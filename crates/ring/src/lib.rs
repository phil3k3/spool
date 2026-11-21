//! Lock-free single-producer / single-consumer ring of interleaved `f32` samples living in
//! POSIX shared memory.
//!
//! * **Producer**: the AudioServerPlugIn driver, inside `coreaudiod` (user `_coreaudiod`).
//!   It creates the shared-memory object. Its write path ([`Producer::write`]) is real-time
//!   safe: atomics and `memcpy` only, no allocation, locks or syscalls, and it never blocks.
//!   When the ring is full the *newest* samples are dropped and counted.
//! * **Consumer**: the recorder daemon (logged-in user). It attaches to the existing object,
//!   advances the read index and publishes a heartbeat so the producer only spends cycles
//!   when someone is listening.
//!
//! The `coreaudiod` sandbox profile contains `(allow ipc-posix-shm)`, which is why this
//! transport works without Mach/XPC plumbing.
//!
//! Positions are monotonically increasing `u64` sample counters (not frames), so wraparound
//! of the index itself never happens in practice. The producer always writes whole frames.
//!
//! Format changes (sample rate / channel count) are published through a small seqlock
//! together with the sample position where the new format begins, so the consumer can
//! finish the old segment before switching.
//!
//! Every value the other process writes is treated as untrusted: indices are range-checked,
//! the format is validated, gain is clamped and seqlock reads are bounded, so a corrupt or
//! hostile header can cause dropped audio but never out-of-bounds access, a hang or an
//! absurd allocation.
//!
//! # Access control
//!
//! The object has to be mode 0666: the producer is `_coreaudiod`, the consumer is the
//! logged-in user, and macOS shm objects support neither `fchmod` nor `fchown` (both fail
//! with `EINVAL`), so the mode is fixed at creation. Any local process can therefore read
//! (and write) the audio stream. What *is* enforced:
//!
//! * The producer only adopts an existing object that it owns itself; a pre-created
//!   (squatted) object is replaced, or creation fails.
//! * The consumer only attaches to an object owned by `_coreaudiod` or by itself (the
//!   latter for test producers such as `examples/tone.rs`).
//! * The consumer refuses to attach while another consumer is heartbeating, so two
//!   daemons (e.g. of two logged-in users) do not silently split the stream.

use std::ffi::CString;
use std::io;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// Default shared-memory object name. macOS limits names to 31 bytes.
pub const DEFAULT_NAME: &str = "/spool.ring.v1";
/// Magic tag written last during initialisation.
pub const MAGIC: u64 = u64::from_le_bytes(*b"SPOOLRNG");
pub const VERSION: u32 = 1;
/// 2^21 samples = 8 MiB, ~21.8 s of stereo audio at 48 kHz.
pub const DEFAULT_CAPACITY: u32 = 1 << 21;
/// Size reserved for the header; data starts at this offset.
pub const HEADER_BYTES: usize = 4096;
/// Upper bound on channels accepted from the shared header.
pub const MAX_CHANNELS: u32 = 64;
/// Sample-rate range accepted from the shared header.
pub const MIN_SAMPLE_RATE: f64 = 8_000.0;
pub const MAX_SAMPLE_RATE: f64 = 768_000.0;
/// User the driver's producer runs as.
pub const PRODUCER_USER: &str = "_coreaudiod";

/// How long a consumer heartbeat stays valid.
const HEARTBEAT_TIMEOUT_NS: u64 = 2_000_000_000;
/// Seqlock read attempts before giving up (the writer holds it for a few stores only).
const SEQLOCK_RETRIES: u32 = 10_000;

#[repr(C, align(64))]
struct Line<T>(T);

#[repr(C)]
struct Header {
    magic: AtomicU64,
    version: AtomicU32,
    capacity: AtomicU32,

    /// Seqlock guarding the `fmt_*` fields. Odd while being written.
    fmt_seq: AtomicU32,
    fmt_channels: AtomicU32,
    fmt_rate_bits: AtomicU64,
    fmt_start: AtomicU64,

    /// Bumped every time a producer (re)initialises the object.
    producer_epoch: AtomicU64,
    /// Set by a producer that is about to unlink this object; the consumer must reopen.
    stale: AtomicU32,
    /// 1 while the device is doing IO.
    running: AtomicU32,
    /// Device volume as an amplitude factor (f32 bits) and mute flag, published by the
    /// producer so the consumer can choose raw vs. post-fader recording.
    gain_bits: AtomicU32,
    muted: AtomicU32,

    write: Line<AtomicU64>,
    read: Line<AtomicU64>,
    /// Consumer liveness, in `mach_absolute_time` ticks.
    consumer_heartbeat: Line<AtomicU64>,
    dropped: Line<AtomicU64>,
    /// Samples the producer did not write because no consumer heartbeat was fresh.
    /// Appended in a compatible way: older producers leave it 0 (the header is zeroed).
    skipped: Line<AtomicU64>,
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_BYTES);

/// Stream format of a segment of the ring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Format {
    pub sample_rate: f64,
    pub channels: u32,
}

impl Format {
    /// True if the format is within the limits every consumer can handle. Formats read
    /// from the ring are always valid.
    pub fn is_valid(&self) -> bool {
        (1..=MAX_CHANNELS).contains(&self.channels)
            && self.sample_rate.is_finite()
            && (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&self.sample_rate)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FormatRecord {
    seq: u32,
    format: Format,
    start: u64,
}

/// Snapshot of the shared counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub write_pos: u64,
    pub read_pos: u64,
    pub dropped_samples: u64,
    /// Samples not written because the consumer looked dead (heartbeat lapsed).
    pub skipped_samples: u64,
    pub producer_epoch: u64,
    pub running: bool,
    pub gain: f32,
    pub muted: bool,
}

// ---------------------------------------------------------------------------------------
// Time

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebase) -> i32;
}

/// Monotonic host time in ticks. Real-time safe (reads the commpage, no syscall).
#[inline]
pub fn host_ticks() -> u64 {
    // SAFETY: no preconditions.
    unsafe { mach_absolute_time() }
}

/// Converts nanoseconds to host ticks.
pub fn ns_to_ticks(ns: u64) -> u64 {
    let mut tb = MachTimebase { numer: 0, denom: 0 };
    // SAFETY: valid out-pointer.
    unsafe { mach_timebase_info(&mut tb) };
    if tb.numer == 0 {
        return ns;
    }
    ((ns as u128 * tb.denom as u128) / tb.numer as u128) as u64
}

// ---------------------------------------------------------------------------------------
// Mapping

struct Mapping {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain shared memory; all shared fields are atomics, and the sample
// region is accessed according to the SPSC protocol.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn map(fd: libc::c_int, len: usize) -> io::Result<Self> {
        // SAFETY: fd is a valid shm descriptor; we request a fresh shared mapping.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { base: NonNull::new(p.cast()).unwrap(), len })
    }

    fn header(&self) -> &Header {
        // SAFETY: every mapping is at least HEADER_BYTES long, page aligned, and Header
        // consists only of atomics, for which any bit pattern is valid.
        unsafe { &*(self.base.as_ptr() as *const Header) }
    }

    fn data(&self) -> *mut f32 {
        // SAFETY: HEADER_BYTES < len.
        unsafe { self.base.as_ptr().add(HEADER_BYTES) as *mut f32 }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: base/len came from mmap.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

fn c_name(name: &str) -> io::Result<CString> {
    if !name.starts_with('/') || name.len() > 31 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "shm name must start with '/' and be <= 31 bytes"));
    }
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in shm name"))
}

struct Fd(libc::c_int);
impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: we own the descriptor.
        unsafe { libc::close(self.0) };
    }
}

fn shm_open(name: &CString, flags: libc::c_int, mode: libc::mode_t) -> io::Result<Fd> {
    // SAFETY: valid C string. shm_open is variadic; mode is promoted to c_uint.
    let fd = unsafe { libc::shm_open(name.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(Fd(fd)) }
}

/// Size and owner of a shm object.
fn fd_stat(fd: &Fd) -> io::Result<(usize, libc::uid_t)> {
    // SAFETY: zeroed stat is a valid out-parameter.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.0, &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((st.st_size as usize, st.st_uid))
}

fn euid() -> libc::uid_t {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() }
}

/// UID of [`PRODUCER_USER`], if that account exists.
pub fn producer_uid() -> Option<libc::uid_t> {
    let name = CString::new(PRODUCER_USER).ok()?;
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: zeroed passwd is a valid out-parameter; buf/len describe a writable buffer.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwnam_r(name.as_ptr(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut result) };
    (rc == 0 && !result.is_null()).then_some(pw.pw_uid)
}

fn total_bytes(capacity: u32) -> usize {
    HEADER_BYTES + capacity as usize * std::mem::size_of::<f32>()
}

/// macOS rounds shared-memory objects up to the VM page size (16 KiB on Apple Silicon),
/// so `fstat` may report a little more than was requested.
fn size_matches(actual: usize, capacity: u32) -> bool {
    let want = total_bytes(capacity);
    actual >= want && actual - want < 64 * 1024
}

/// Removes the shared-memory object (used by uninstallers and tests).
pub fn unlink(name: &str) -> io::Result<()> {
    let c = c_name(name)?;
    // SAFETY: valid C string.
    if unsafe { libc::shm_unlink(c.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads a consistent format record. `None` if the seqlock stays busy (a writer died
/// mid-update or the header is being tampered with); never spins unboundedly.
/// The returned format is not validated; see [`Format::is_valid`].
fn load_format(h: &Header) -> Option<FormatRecord> {
    for _ in 0..SEQLOCK_RETRIES {
        let s1 = h.fmt_seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            std::hint::spin_loop();
            continue;
        }
        let channels = h.fmt_channels.load(Ordering::Relaxed);
        let rate = f64::from_bits(h.fmt_rate_bits.load(Ordering::Relaxed));
        let start = h.fmt_start.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        if h.fmt_seq.load(Ordering::Relaxed) == s1 {
            return Some(FormatRecord { seq: s1, format: Format { sample_rate: rate, channels }, start });
        }
    }
    None
}

fn stats(h: &Header) -> Stats {
    // The producer publishes scalar² in [0, 1]; anything else is corruption. Clamping here
    // keeps a hostile header from blasting the monitor output or filling files with NaN.
    let gain = f32::from_bits(h.gain_bits.load(Ordering::Relaxed));
    let gain = if gain.is_finite() { gain.clamp(0.0, 1.0) } else { 0.0 };
    Stats {
        write_pos: h.write.0.load(Ordering::Acquire),
        read_pos: h.read.0.load(Ordering::Acquire),
        dropped_samples: h.dropped.0.load(Ordering::Relaxed),
        skipped_samples: h.skipped.0.load(Ordering::Relaxed),
        producer_epoch: h.producer_epoch.load(Ordering::Relaxed),
        running: h.running.load(Ordering::Relaxed) != 0,
        gain,
        muted: h.muted.load(Ordering::Relaxed) != 0,
    }
}

fn lost_samples(h: &Header) -> u64 {
    h.dropped.0.load(Ordering::Relaxed).wrapping_add(h.skipped.0.load(Ordering::Relaxed))
}

fn heartbeat_fresh(h: &Header, timeout: u64) -> bool {
    let hb = h.consumer_heartbeat.0.load(Ordering::Relaxed);
    hb != 0 && host_ticks().wrapping_sub(hb) < timeout
}

// ---------------------------------------------------------------------------------------
// Producer

/// Writing side, owned by the driver.
pub struct Producer {
    map: Mapping,
    capacity: u64,
    heartbeat_timeout: u64,
}

impl Producer {
    /// Creates (or adopts a compatible existing) shared-memory ring. Not real-time safe.
    ///
    /// The object is created world read/write (0666) because the producer (`_coreaudiod`)
    /// and the consumer (logged-in user) are different users; see the crate docs for what
    /// that implies. macOS cannot change a shm object's mode later, so the creation mode
    /// (after umask; `coreaudiod` runs with umask 0) is final.
    ///
    /// An existing object is only adopted if this process's user owns it. Anyone else's
    /// object (e.g. one pre-created to capture the stream) is unlinked and replaced; if that
    /// is not permitted, creation fails rather than writing audio into it.
    pub fn create(name: &str, capacity: u32, format: Format) -> io::Result<Self> {
        if !capacity.is_power_of_two() || capacity < 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "capacity must be a power of two >= 1024"));
        }
        if !format.is_valid() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported format"));
        }
        let cname = c_name(name)?;
        let want = total_bytes(capacity);

        // Try to adopt an existing object (e.g. after a coreaudiod restart while the daemon
        // keeps its mapping). Keeping positions monotonic lets the consumer carry on.
        if let Ok(fd) = shm_open(&cname, libc::O_RDWR, 0) {
            let (size, owner) = fd_stat(&fd)?;
            if owner == euid() && size >= HEADER_BYTES {
                let map = Mapping::map(fd.0, size)?;
                let h = map.header();
                let compatible = size_matches(size, capacity)
                    && h.magic.load(Ordering::Acquire) == MAGIC
                    && h.version.load(Ordering::Relaxed) == VERSION
                    && h.capacity.load(Ordering::Relaxed) == capacity;
                if compatible {
                    let p = Self::from_map(map, capacity);
                    let h = p.map.header();
                    h.stale.store(0, Ordering::Relaxed);
                    h.running.store(0, Ordering::Relaxed);
                    h.producer_epoch.fetch_add(1, Ordering::Release);
                    p.set_format(format);
                    return Ok(p);
                }
                // Tell any consumer still mapped to this object to reopen.
                h.stale.store(1, Ordering::Release);
            }
            // Not ours: never write into it. Consumers reject it too (wrong owner).
            drop(fd);
            // SAFETY: valid C string.
            unsafe { libc::shm_unlink(cname.as_ptr()) };
        }

        // O_EXCL: if a foreign object survived the unlink above, fail instead of using it.
        let fd = shm_open(&cname, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o666).map_err(|e| {
            if e.raw_os_error() == Some(libc::EEXIST) {
                io::Error::new(e.kind(), format!("{name} exists and is owned by another user; cannot replace it"))
            } else {
                e
            }
        })?;
        // SAFETY: fd valid; on macOS a shm object can only be sized once, right after creation.
        if unsafe { libc::ftruncate(fd.0, want as libc::off_t) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::shm_unlink(cname.as_ptr()) };
            return Err(e);
        }
        let map = Mapping::map(fd.0, want)?;
        // Pre-fault every page now so the real-time thread never takes a page fault.
        // SAFETY: the mapping is `want` bytes long and writable.
        unsafe { std::ptr::write_bytes(map.base.as_ptr(), 0, want) };
        let h = map.header();
        h.version.store(VERSION, Ordering::Relaxed);
        h.capacity.store(capacity, Ordering::Relaxed);
        h.gain_bits.store(1.0f32.to_bits(), Ordering::Relaxed);
        h.producer_epoch.store(1, Ordering::Relaxed);
        let p = Self::from_map(map, capacity);
        p.set_format(format);
        p.map.header().magic.store(MAGIC, Ordering::Release);
        Ok(p)
    }

    fn from_map(map: Mapping, capacity: u32) -> Self {
        Self { map, capacity: capacity as u64, heartbeat_timeout: ns_to_ticks(HEARTBEAT_TIMEOUT_NS) }
    }

    /// Publishes a new stream format, starting at the current write position.
    /// Must not race with [`write`](Self::write); call it while IO is stopped.
    pub fn set_format(&self, format: Format) {
        let h = self.map.header();
        let start = h.write.0.load(Ordering::Relaxed);
        // Single writer, so plain stores suffice. Deriving the odd value from an even base
        // also repairs a sequence left odd by a producer that died mid-update.
        let even = h.fmt_seq.load(Ordering::Relaxed) & !1;
        h.fmt_seq.store(even.wrapping_add(1), Ordering::Relaxed);
        // Orders the odd sequence before the data stores (ARM reorders stores otherwise,
        // letting a reader accept a torn record).
        fence(Ordering::Release);
        h.fmt_channels.store(format.channels, Ordering::Relaxed);
        h.fmt_rate_bits.store(format.sample_rate.to_bits(), Ordering::Relaxed);
        h.fmt_start.store(start, Ordering::Relaxed);
        h.fmt_seq.store(even.wrapping_add(2), Ordering::Release);
    }

    pub fn set_running(&self, running: bool) {
        self.map.header().running.store(running as u32, Ordering::Relaxed);
    }

    pub fn set_gain(&self, gain: f32, muted: bool) {
        let h = self.map.header();
        h.gain_bits.store(gain.to_bits(), Ordering::Relaxed);
        h.muted.store(muted as u32, Ordering::Relaxed);
    }

    /// True if a consumer published a heartbeat recently. Real-time safe.
    #[inline]
    pub fn consumer_alive(&self) -> bool {
        heartbeat_fresh(self.map.header(), self.heartbeat_timeout)
    }

    /// Counts `n` samples the caller chose not to write because
    /// [`consumer_alive`](Self::consumer_alive) was false. Lets a consumer whose heartbeat
    /// lapsed (process stalled or suspended) see that audio is missing. Real-time safe.
    #[inline]
    pub fn count_skipped(&self, n: usize) {
        self.map.header().skipped.0.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Appends samples. Returns `false` (and counts them as dropped) if they do not fit.
    /// Real-time safe: no allocation, locks or syscalls.
    #[inline]
    pub fn write(&self, samples: &[f32]) -> bool {
        self.write_with(samples.len(), |dst, offset| {
            dst.copy_from_slice(&samples[offset..offset + dst.len()]);
        })
    }

    /// Appends `n` zero samples. Real-time safe.
    #[inline]
    pub fn write_silence(&self, n: usize) -> bool {
        self.write_with(n, |dst, _| dst.fill(0.0))
    }

    #[inline]
    fn write_with(&self, n: usize, mut fill: impl FnMut(&mut [f32], usize)) -> bool {
        let h = self.map.header();
        let n64 = n as u64;
        let w = h.write.0.load(Ordering::Relaxed);
        let r = h.read.0.load(Ordering::Acquire);
        let used = w.wrapping_sub(r);
        // `used > capacity` means the consumer wrote garbage; treat as full.
        if used > self.capacity || n64 > self.capacity - used {
            h.dropped.0.fetch_add(n64, Ordering::Relaxed);
            return false;
        }
        let mask = self.capacity - 1;
        let start = (w & mask) as usize;
        let first = n.min(self.capacity as usize - start);
        let data = self.map.data();
        // SAFETY: start + first <= capacity and n - first <= capacity; the consumer does not
        // touch [w, w + n) until we publish the new write index below.
        unsafe {
            fill(std::slice::from_raw_parts_mut(data.add(start), first), 0);
            if first < n {
                fill(std::slice::from_raw_parts_mut(data, n - first), first);
            }
        }
        h.write.0.store(w + n64, Ordering::Release);
        true
    }

    pub fn stats(&self) -> Stats {
        stats(self.map.header())
    }
}

// ---------------------------------------------------------------------------------------
// Consumer

/// Result of a [`Consumer::read`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chunk {
    /// Number of samples copied into the output buffer (always whole frames).
    pub samples: usize,
    /// Format of those samples.
    pub format: Format,
    /// True if samples were lost before this chunk (resync after overrun or corruption).
    pub discontinuity: bool,
}

/// Reading side, owned by the recorder daemon.
pub struct Consumer {
    map: Mapping,
    capacity: u64,
    current: FormatRecord,
    epoch: u64,
    pending_discontinuity: bool,
    /// `dropped + skipped` as last seen; a change means the producer lost audio.
    lost: u64,
}

impl Consumer {
    /// Attaches to an existing ring created by the driver. Skips any stale data.
    ///
    /// Only objects owned by [`PRODUCER_USER`] or by this process's user are accepted, and
    /// attaching fails while another consumer is heartbeating.
    pub fn open(name: &str) -> io::Result<Self> {
        let cname = c_name(name)?;
        let fd = shm_open(&cname, libc::O_RDWR, 0)?;
        let (size, owner) = fd_stat(&fd)?;
        let invalid = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        if owner != euid() && Some(owner) != producer_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{name} is owned by uid {owner}, not {PRODUCER_USER}; refusing to attach (planted object?)"),
            ));
        }
        if size < HEADER_BYTES {
            return Err(invalid("shared memory too small"));
        }
        let map = Mapping::map(fd.0, size)?;
        let h = map.header();
        if h.magic.load(Ordering::Acquire) != MAGIC {
            return Err(invalid("bad magic (driver not initialised yet?)"));
        }
        if h.version.load(Ordering::Relaxed) != VERSION {
            return Err(invalid("ring version mismatch"));
        }
        let capacity = h.capacity.load(Ordering::Relaxed);
        if !capacity.is_power_of_two() || capacity < 1024 || !size_matches(size, capacity) {
            return Err(invalid("ring capacity does not match object size"));
        }
        // A single read index cannot be shared: a second consumer would silently take half
        // of the stream from the first (e.g. two logged-in users each running spoold).
        if heartbeat_fresh(h, ns_to_ticks(HEARTBEAT_TIMEOUT_NS)) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another consumer is attached (another spoold, possibly of another user)",
            ));
        }
        let w = h.write.0.load(Ordering::Acquire);
        let current = load_format(h).ok_or_else(|| invalid("format record busy or corrupt"))?;
        if !current.format.is_valid() {
            return Err(invalid("unsupported format in ring header"));
        }
        h.read.0.store(w, Ordering::Release);
        let epoch = h.producer_epoch.load(Ordering::Relaxed);
        let lost = lost_samples(h);
        let c = Self { map, capacity: capacity as u64, current, epoch, pending_discontinuity: false, lost };
        c.heartbeat();
        Ok(c)
    }

    /// Must be called regularly (well under 2 s apart) or the producer stops writing.
    pub fn heartbeat(&self) {
        self.map.header().consumer_heartbeat.0.store(host_ticks().max(1), Ordering::Relaxed);
    }

    /// True if the producer abandoned this object; the caller should reopen.
    pub fn is_stale(&self) -> bool {
        self.map.header().stale.load(Ordering::Acquire) != 0
    }

    pub fn format(&self) -> Format {
        self.current.format
    }

    pub fn stats(&self) -> Stats {
        stats(self.map.header())
    }

    /// Copies available samples into `out`. Never returns samples of two formats at once:
    /// if a format change is pending, the old segment is drained first and the next call
    /// returns data in the new format.
    pub fn read(&mut self, out: &mut [f32]) -> Chunk {
        let h = self.map.header();
        let epoch = h.producer_epoch.load(Ordering::Acquire);
        if epoch != self.epoch {
            // Producer restarted on the same object; positions continued, but samples in
            // between may be missing.
            self.epoch = epoch;
            self.pending_discontinuity = true;
        }
        // Samples dropped (ring full) or skipped (heartbeat lapsed) leave a hole in the
        // stream that the positions do not show.
        let lost = lost_samples(h);
        if lost != self.lost {
            self.lost = lost;
            self.pending_discontinuity = true;
        }
        // Load write *before* the format: the producer publishes a format before writing
        // data in it, so acquiring `write` guarantees we see the matching format.
        let w = h.write.0.load(Ordering::Acquire);
        let mut r = h.read.0.load(Ordering::Relaxed);
        let Some(fmt) = load_format(h) else {
            // Seqlock stuck: deliver nothing this time instead of hanging the caller.
            return self.chunk(0);
        };
        if !fmt.format.is_valid() {
            // Data in an unusable format: skip it rather than hand it on.
            h.read.0.store(w, Ordering::Release);
            self.pending_discontinuity = true;
            return self.chunk(0);
        }

        let mut limit = w;
        if fmt.seq != self.current.seq {
            let single_step = fmt.seq == self.current.seq.wrapping_add(2);
            if single_step && r < fmt.start && fmt.start <= w {
                // Drain the rest of the old segment first.
                limit = fmt.start;
            } else {
                if r < fmt.start {
                    // Data of an unknown intermediate format: skip it.
                    r = fmt.start.min(w);
                    self.pending_discontinuity = true;
                }
                self.current = fmt;
            }
        }

        let avail = limit.wrapping_sub(r);
        if avail > self.capacity {
            // Overrun or corrupt index: resync to the newest data.
            h.read.0.store(w, Ordering::Release);
            self.pending_discontinuity = true;
            return self.chunk(0);
        }
        let channels = self.current.format.channels.clamp(1, MAX_CHANNELS) as usize;
        let n = (avail as usize).min(out.len());
        let n = n - n % channels;
        if n == 0 {
            if r != h.read.0.load(Ordering::Relaxed) {
                h.read.0.store(r, Ordering::Release);
            }
            return self.chunk(0);
        }
        let mask = self.capacity - 1;
        let start = (r & mask) as usize;
        let first = n.min(self.capacity as usize - start);
        let data = self.map.data();
        // SAFETY: ranges are within capacity; the producer does not write to [r, r + n) until
        // we publish the new read index.
        unsafe {
            std::ptr::copy_nonoverlapping(data.add(start), out.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(data, out.as_mut_ptr().add(first), n - first);
        }
        h.read.0.store(r + n as u64, Ordering::Release);
        self.chunk(n)
    }

    fn chunk(&mut self, samples: usize) -> Chunk {
        let discontinuity = samples > 0 && std::mem::take(&mut self.pending_discontinuity);
        Chunk { samples, format: self.current.format, discontinuity }
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        self.map.header().consumer_heartbeat.0.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const STEREO: Format = Format { sample_rate: 48000.0, channels: 2 };

    fn unique_name() -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        format!("/spt.{}.{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    struct Ring {
        name: String,
        p: Producer,
        c: Consumer,
    }
    impl Drop for Ring {
        fn drop(&mut self) {
            let _ = unlink(&self.name);
        }
    }
    fn ring(cap: u32) -> Ring {
        let name = unique_name();
        let p = Producer::create(&name, cap, STEREO).unwrap();
        let c = Consumer::open(&name).unwrap();
        Ring { name, p, c }
    }

    #[test]
    fn roundtrip_and_wraparound() {
        let mut r = ring(1024);
        let mut out = vec![0.0; 1024];
        let mut next_w = 0.0f32;
        let mut next_r = 0.0f32;
        // 700-sample writes against a 1024 ring force wraparound at many offsets.
        for _ in 0..50 {
            let block: Vec<f32> = (0..700).map(|i| next_w + i as f32).collect();
            assert!(r.p.write(&block));
            next_w += 700.0;
            let ch = r.c.read(&mut out);
            assert_eq!(ch.samples, 700);
            assert!(!ch.discontinuity);
            for &s in &out[..ch.samples] {
                assert_eq!(s, next_r);
                next_r += 1.0;
            }
        }
    }

    #[test]
    fn full_ring_drops_newest_and_counts() {
        let mut r = ring(1024);
        assert!(r.p.write(&[1.0; 1000]));
        assert!(!r.p.write(&[2.0; 100]));
        assert_eq!(r.p.stats().dropped_samples, 100);
        let mut out = vec![0.0; 2048];
        let ch = r.c.read(&mut out);
        assert_eq!(ch.samples, 1000);
        assert!(out[..1000].iter().all(|&s| s == 1.0));
    }

    #[test]
    fn reads_whole_frames_only() {
        let mut r = ring(1024);
        r.p.write(&[0.5; 10]);
        let mut out = vec![0.0; 7];
        assert_eq!(r.c.read(&mut out).samples, 6);
        assert_eq!(r.c.read(&mut out).samples, 4);
    }

    #[test]
    fn format_change_drains_old_segment_first() {
        let mut r = ring(4096);
        r.p.write(&[1.0; 8]);
        let mono = Format { sample_rate: 96000.0, channels: 1 };
        r.p.set_format(mono);
        r.p.write(&[2.0; 3]);
        let mut out = vec![0.0; 64];
        let a = r.c.read(&mut out);
        assert_eq!((a.samples, a.format), (8, STEREO));
        let b = r.c.read(&mut out);
        assert_eq!((b.samples, b.format), (3, mono));
        assert!(out[..3].iter().all(|&s| s == 2.0));
    }

    #[test]
    fn corrupt_read_index_is_contained() {
        let mut r = ring(1024);
        r.p.write(&[1.0; 16]);
        r.c.map.header().read.0.store(u64::MAX / 2, Ordering::Relaxed);
        // Producer treats the ring as full instead of writing out of bounds.
        assert!(!r.p.write(&[1.0; 16]));
        // Consumer resyncs.
        let mut out = vec![0.0; 64];
        assert_eq!(r.c.read(&mut out).samples, 0);
        assert!(r.p.write(&[3.0; 4]));
        let ch = r.c.read(&mut out);
        assert_eq!(ch.samples, 4);
        assert!(ch.discontinuity);
    }

    #[test]
    fn producer_restart_adopts_existing_object() {
        let name = unique_name();
        let p = Producer::create(&name, 1024, STEREO).unwrap();
        let mut c = Consumer::open(&name).unwrap();
        p.write(&[1.0; 4]);
        drop(p);
        let p2 = Producer::create(&name, 1024, STEREO).unwrap();
        assert!(!c.is_stale());
        p2.write(&[2.0; 4]);
        let mut out = vec![0.0; 16];
        // The restart republishes the format, so the old segment is drained first.
        let a = c.read(&mut out);
        assert_eq!(&out[..a.samples], &[1.0; 4]);
        assert!(a.discontinuity);
        let b = c.read(&mut out);
        assert_eq!(&out[..b.samples], &[2.0; 4]);
        drop(p2);
        // Incompatible capacity: old object is marked stale and replaced.
        let _p3 = Producer::create(&name, 2048, STEREO).unwrap();
        assert!(c.is_stale());
        let _ = unlink(&name);
    }

    #[test]
    fn stuck_seqlock_does_not_hang_and_producer_repairs_it() {
        let mut r = ring(1024);
        r.p.write(&[1.0; 4]);
        let h = r.p.map.header();
        h.fmt_seq.store(h.fmt_seq.load(Ordering::Relaxed) | 1, Ordering::Relaxed);
        let mut out = vec![0.0; 16];
        assert_eq!(r.c.read(&mut out).samples, 0, "must give up instead of spinning");
        assert!(Consumer::open(&r.name).is_err());
        // The next set_format leaves the sequence even again.
        r.p.set_format(STEREO);
        assert_eq!(h.fmt_seq.load(Ordering::Relaxed) & 1, 0);
        r.p.write(&[2.0; 4]);
        assert!(r.c.read(&mut out).samples > 0);
    }

    #[test]
    fn hostile_format_and_gain_are_contained() {
        let mut r = ring(1024);
        r.p.write(&[1.0; 8]);
        let mut out = vec![0.0; 64];
        for bad in [
            Format { sample_rate: 48000.0, channels: 0 },
            Format { sample_rate: 48000.0, channels: u32::MAX },
            Format { sample_rate: f64::NAN, channels: 2 },
            Format { sample_rate: 1e300, channels: 2 },
        ] {
            r.p.set_format(bad);
            r.p.write(&[1.0; 8]);
            let ch = r.c.read(&mut out);
            assert!(ch.samples == 0 || ch.format.is_valid(), "{bad:?} leaked: {ch:?}");
            assert!(r.c.format().is_valid());
        }
        r.p.set_format(STEREO);
        r.p.write(&[3.0; 4]);
        let ch = r.c.read(&mut out);
        assert_eq!((ch.samples, ch.format, ch.discontinuity), (4, STEREO, true));

        let h = r.c.map.header();
        for (bits, want) in [(f32::NAN.to_bits(), 0.0), (1e9f32.to_bits(), 1.0), ((-1.0f32).to_bits(), 0.0)] {
            h.gain_bits.store(bits, Ordering::Relaxed);
            assert_eq!(r.c.stats().gain, want);
        }
        assert!(Producer::create("/spt.bad", 1024, Format { sample_rate: 0.0, channels: 2 }).is_err());
    }

    #[test]
    fn second_consumer_is_refused_until_first_leaves() {
        let name = unique_name();
        let _p = Producer::create(&name, 1024, STEREO).unwrap();
        let c = Consumer::open(&name).unwrap();
        let err = Consumer::open(&name).err().expect("second consumer must be refused");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(c);
        let _c2 = Consumer::open(&name).expect("free again after the first consumer left");
        let _ = unlink(&name);
    }

    #[test]
    fn lost_samples_are_reported_as_discontinuity() {
        let mut r = ring(1024);
        let mut out = vec![0.0; 2048];
        r.p.write(&[1.0; 1000]);
        assert!(!r.p.write(&[2.0; 100])); // ring full: dropped
        assert!(r.c.read(&mut out).discontinuity);
        r.p.write(&[1.0; 4]);
        assert!(!r.c.read(&mut out).discontinuity);
        r.p.count_skipped(8); // heartbeat lapsed for a cycle
        r.p.write(&[1.0; 4]);
        let ch = r.c.read(&mut out);
        assert!(ch.discontinuity);
        assert_eq!(r.c.stats().skipped_samples, 8);
    }

    #[test]
    fn heartbeat_gates_producer() {
        let name = unique_name();
        let p = Producer::create(&name, 1024, STEREO).unwrap();
        assert!(!p.consumer_alive());
        let c = Consumer::open(&name).unwrap();
        assert!(p.consumer_alive());
        drop(c);
        assert!(!p.consumer_alive());
        let _ = unlink(&name);
    }

    #[test]
    fn concurrent_threads_preserve_order() {
        let name = unique_name();
        let p = Producer::create(&name, 1 << 12, STEREO).unwrap();
        let mut c = Consumer::open(&name).unwrap();
        let total: u64 = 2_000_000;
        let prod = std::thread::spawn(move || {
            let mut next = 0u64;
            let mut buf = [0.0f32; 256];
            while next < total {
                for (i, s) in buf.iter_mut().enumerate() {
                    *s = ((next + i as u64) % 1_000_000) as f32;
                }
                if p.write(&buf) {
                    next += buf.len() as u64;
                } else {
                    std::thread::yield_now();
                }
            }
            p
        });
        let mut out = vec![0.0f32; 1000];
        let mut seen = 0u64;
        while seen < total {
            let ch = c.read(&mut out);
            for &s in &out[..ch.samples] {
                assert_eq!(s, (seen % 1_000_000) as f32);
                seen += 1;
            }
            if ch.samples == 0 {
                std::thread::yield_now();
            }
        }
        let _p = prod.join().unwrap();
        let _ = unlink(&name);
    }
}
