//! Minimal Mach IPC for the XPC transport spike (see `docs/XPC.md`).
//!
//! What the spike needs, and nothing more:
//!
//! * [`look_up`] / [`check_in`]: find or register a launchd Mach service.
//! * [`Shared`]: memory created as a Mach *memory entry*. The entry is a port, not a name in
//!   a global namespace, so only a process that is handed the port can map the memory. This
//!   is what replaces the world-readable `/spool.ring.v1` later.
//! * Two message types: [`send_deposit`] (driver → broker, carries a memory entry) and
//!   [`fetch`] (client → broker, the reply carries the entry). The broker side is
//!   [`receive`] + [`reply_fetch`]. Received messages carry an audit trailer, so the broker
//!   learns the sender's pid and euid from the kernel instead of trusting the message.
//!
//! Plain Mach messages instead of libxpc: the same kernel mechanism, but no dispatch queues
//! or blocks inside `coreaudiod`, and no Objective-C runtime. Everything here is hand-written
//! FFI over libSystem, like the driver's Core Audio bindings. None of it is real-time safe:
//! call it from helper threads only.

#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString, c_char};
use std::fmt;
use std::mem::size_of;
use std::ptr::{addr_of_mut, read_unaligned};

pub type mach_port_t = u32;
pub type kern_return_t = i32;

/// Reverse-DNS prefix of every Spool identifier, resolved at build time by
/// `scripts/spool-id.sh` from the `origin` remote (or `$SPOOL_ID_PREFIX`).
pub const ID_PREFIX: &str = env!("SPOOL_ID_PREFIX");

/// launchd Mach service of the broker, `<prefix>.spool-broker`. The same name is generated
/// into the broker's LaunchDaemon plist and the driver's `Info.plist`
/// (`AudioServerPlugIn_MachServices`) by `scripts/build-bundle.sh`.
pub const SERVICE_NAME: &str = concat!(env!("SPOOL_ID_PREFIX"), ".spool-broker");

/// Contents of the spike's probe page: magic, then the writer's pid and uid (u32 each).
pub const PROBE_MAGIC: u64 = u64::from_le_bytes(*b"SPOOLPRB");
pub const PROBE_SIZE: u64 = 16 * 1024;

pub const MSG_DEPOSIT: i32 = 0x5350_0001;
pub const MSG_FETCH: i32 = 0x5350_0002;
pub const MSG_FETCH_REPLY: i32 = 0x5350_0003;

/// Largest memory entry the broker will map (the ring is ~8 MiB).
pub const MAX_SHARED_BYTES: u64 = 64 << 20;

const KERN_SUCCESS: kern_return_t = 0;
const MACH_PORT_NULL: mach_port_t = 0;

const MACH_MSG_TYPE_PORT_SEND: u32 = 17; // = MOVE_SEND, as seen by a receiver
const MACH_MSG_TYPE_PORT_SEND_ONCE: u32 = 18; // = MOVE_SEND_ONCE
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSG_TYPE_MAKE_SEND_ONCE: u32 = 21;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;

const MACH_SEND_MSG: i32 = 0x1;
const MACH_RCV_MSG: i32 = 0x2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_RCV_TIMEOUT: i32 = 0x100;
/// MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) | MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT)
const MACH_RCV_TRAILER_AUDIT: i32 = 3 << 24;

const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const VM_FLAGS_ANYWHERE: i32 = 1;
const VM_PROT_READ_WRITE: i32 = 0x3;
const VM_INHERIT_NONE: u32 = 2;
const MAP_MEM_NAMED_CREATE: i32 = 0x02_0000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Header {
    bits: u32,
    size: u32,
    remote: mach_port_t,
    local: mach_port_t,
    voucher: mach_port_t,
    id: i32,
}

/// `mach_msg_port_descriptor_t`. The C bitfields `pad2:16, disposition:8, type:8` are laid
/// out low to high, which on little-endian is these three fields in this order.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct PortDescriptor {
    name: mach_port_t,
    pad1: u32,
    pad2: u16,
    disposition: u8,
    kind: u8,
}

/// A complex message carrying one port plus the size of the memory behind it.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct PortMsg {
    header: Header,
    descriptor_count: u32,
    port: PortDescriptor,
    size: u64,
}

/// `mach_msg_audit_trailer_t`.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct AuditTrailer {
    kind: u32,
    size: u32,
    seqno: u32,
    sender: [u32; 2],
    audit: [u32; 8],
}

#[repr(C, align(8))]
struct RecvBuf([u8; 256]);

/// Reads the audit trailer the kernel appended after the message body (receives must ask
/// for it with `MACH_RCV_TRAILER_AUDIT`).
fn sender_of(buf: &RecvBuf, hdr: &Header) -> Sender {
    let at = (hdr.size as usize).next_multiple_of(4);
    if at + size_of::<AuditTrailer>() > buf.0.len() {
        return Sender { pid: -1, euid: u32::MAX };
    }
    // SAFETY: in bounds.
    let t: AuditTrailer = unsafe { read_unaligned(buf.0.as_ptr().add(at).cast()) };
    // audit_token_t: [auid, euid, egid, ruid, rgid, pid, asid, pidversion]
    Sender { pid: t.audit[5] as i32, euid: t.audit[1] }
}

const _: () = assert!(size_of::<Header>() == 24);
const _: () = assert!(size_of::<PortDescriptor>() == 12);
const _: () = assert!(size_of::<PortMsg>() == 48);
const _: () = assert!(size_of::<AuditTrailer>() == 52);

unsafe extern "C" {
    static mach_task_self_: mach_port_t;
    static bootstrap_port: mach_port_t;
    fn bootstrap_look_up(bp: mach_port_t, name: *const c_char, sp: *mut mach_port_t) -> kern_return_t;
    fn bootstrap_check_in(bp: mach_port_t, name: *const c_char, sp: *mut mach_port_t) -> kern_return_t;
    fn bootstrap_strerror(kr: kern_return_t) -> *const c_char;
    fn mach_msg(
        msg: *mut Header,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: mach_port_t,
        timeout: u32,
        notify: mach_port_t,
    ) -> kern_return_t;
    fn mach_msg_destroy(msg: *mut Header);
    fn mach_port_allocate(task: mach_port_t, right: u32, name: *mut mach_port_t) -> kern_return_t;
    fn mach_port_insert_right(task: mach_port_t, name: mach_port_t, poly: mach_port_t, poly_type: u32) -> kern_return_t;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> kern_return_t;
    fn mach_port_mod_refs(task: mach_port_t, name: mach_port_t, right: u32, delta: i32) -> kern_return_t;
    fn mach_make_memory_entry_64(
        task: mach_port_t,
        size: *mut u64,
        offset: u64,
        permission: i32,
        handle: *mut mach_port_t,
        parent: mach_port_t,
    ) -> kern_return_t;
    fn mach_vm_map(
        task: mach_port_t,
        addr: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        object: mach_port_t,
        offset: u64,
        copy: i32,
        cur_protection: i32,
        max_protection: i32,
        inheritance: u32,
    ) -> kern_return_t;
    fn mach_vm_deallocate(task: mach_port_t, addr: u64, size: u64) -> kern_return_t;
}

fn task() -> mach_port_t {
    // SAFETY: initialised by libSystem before main; never changes.
    unsafe { mach_task_self_ }
}

// ---- Errors -----------------------------------------------------------------------------

/// A failed Mach or bootstrap call, or a malformed reply.
#[derive(Debug, Clone)]
pub struct Error {
    pub op: &'static str,
    /// Kernel or bootstrap return code; 0 for protocol errors.
    pub kr: kern_return_t,
    detail: Option<String>,
}

impl Error {
    fn kr(op: &'static str, kr: kern_return_t) -> Self {
        Self { op, kr, detail: None }
    }
    fn protocol(op: &'static str, detail: impl Into<String>) -> Self {
        Self { op, kr: 0, detail: Some(detail.into()) }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(d) = &self.detail {
            return write!(f, "{}: {d}", self.op);
        }
        // SAFETY: bootstrap_strerror returns a static string for any code (it covers Mach
        // errors as well as bootstrap ones).
        let s = unsafe { CStr::from_ptr(bootstrap_strerror(self.kr)) }.to_string_lossy();
        write!(f, "{}: {s} ({:#x})", self.op, self.kr)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(op: &'static str, kr: kern_return_t) -> Result<()> {
    if kr == KERN_SUCCESS { Ok(()) } else { Err(Error::kr(op, kr)) }
}

// ---- Ports ------------------------------------------------------------------------------

/// A send (or send-once) right owned by this process. Dropping it releases the right.
#[derive(Debug)]
pub struct Port(mach_port_t);

impl Port {
    pub fn name(&self) -> mach_port_t {
        self.0
    }
    fn into_raw(self) -> mach_port_t {
        let n = self.0;
        std::mem::forget(self);
        n
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        if self.0 != MACH_PORT_NULL {
            // SAFETY: we own one user reference on this name.
            unsafe { mach_port_deallocate(task(), self.0) };
        }
    }
}

/// A receive right owned by this process.
#[derive(Debug)]
pub struct ReceivePort(mach_port_t);

impl ReceivePort {
    /// Allocates a fresh receive right (reply ports, tests).
    pub fn new() -> Result<Self> {
        let mut name = MACH_PORT_NULL;
        // SAFETY: out pointer valid.
        check("mach_port_allocate", unsafe { mach_port_allocate(task(), MACH_PORT_RIGHT_RECEIVE, &mut name) })?;
        Ok(Self(name))
    }
    /// Makes a send right to this port.
    pub fn make_send(&self) -> Result<Port> {
        // SAFETY: we hold the receive right for this name.
        check("mach_port_insert_right", unsafe {
            mach_port_insert_right(task(), self.0, self.0, MACH_MSG_TYPE_MAKE_SEND)
        })?;
        Ok(Port(self.0))
    }
}

impl Drop for ReceivePort {
    fn drop(&mut self) {
        // SAFETY: we own the receive right.
        unsafe { mach_port_mod_refs(task(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

fn c_name(name: &str) -> Result<CString> {
    CString::new(name).map_err(|_| Error::protocol("service name", "contains NUL"))
}

/// Looks up a launchd Mach service. Fails with `BOOTSTRAP_UNKNOWN_SERVICE` if nothing is
/// registered under `name`, or with a permission error if the sandbox forbids the lookup.
pub fn look_up(name: &str) -> Result<Port> {
    let c = c_name(name)?;
    let mut port = MACH_PORT_NULL;
    // SAFETY: valid C string and out pointer.
    check("bootstrap_look_up", unsafe { bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut port) })?;
    Ok(Port(port))
}

/// Claims the receive right for a service declared in this process's launchd plist.
pub fn check_in(name: &str) -> Result<ReceivePort> {
    let c = c_name(name)?;
    let mut port = MACH_PORT_NULL;
    // SAFETY: valid C string and out pointer.
    check("bootstrap_check_in", unsafe { bootstrap_check_in(bootstrap_port, c.as_ptr(), &mut port) })?;
    Ok(ReceivePort(port))
}

// ---- Shared memory ----------------------------------------------------------------------

/// A mapping of memory that is shared through a Mach memory entry. Unmapped on drop.
#[derive(Debug)]
pub struct Shared {
    addr: u64,
    len: u64,
}

impl Shared {
    /// Creates `len` bytes of zeroed memory, maps it, and returns the mapping together with a
    /// memory-entry port that lets another process map the same pages.
    pub fn create(len: u64) -> Result<(Self, Port)> {
        let mut size = len;
        let mut entry = MACH_PORT_NULL;
        // SAFETY: out pointers valid; NAMED_CREATE makes a new VM object of `size` bytes.
        check("mach_make_memory_entry_64", unsafe {
            mach_make_memory_entry_64(
                task(),
                &mut size,
                0,
                MAP_MEM_NAMED_CREATE | VM_PROT_READ_WRITE,
                &mut entry,
                MACH_PORT_NULL,
            )
        })?;
        let entry = Port(entry);
        let mapping = Self::map(&entry, len)?;
        Ok((mapping, entry))
    }

    /// Maps `len` bytes of a memory entry received from another process.
    pub fn map(entry: &Port, len: u64) -> Result<Self> {
        if len == 0 || len > MAX_SHARED_BYTES {
            return Err(Error::protocol("mach_vm_map", format!("refusing to map {len} bytes")));
        }
        let mut addr = 0u64;
        // SAFETY: out pointer valid. The kernel checks `len` against the entry's size.
        check("mach_vm_map", unsafe {
            mach_vm_map(
                task(),
                &mut addr,
                len,
                0,
                VM_FLAGS_ANYWHERE,
                entry.0,
                0,
                0, // copy = false: share the pages
                VM_PROT_READ_WRITE,
                VM_PROT_READ_WRITE,
                VM_INHERIT_NONE,
            )
        })?;
        Ok(Self { addr, len })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.addr as *mut u8
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Writes the probe page header: magic, pid, uid.
    pub fn write_probe(&self, pid: u32, uid: u32) {
        let p = self.as_ptr();
        // SAFETY: the mapping is at least PROBE_SIZE bytes (callers create it that way) and
        // page-aligned, so these aligned writes are in bounds.
        unsafe {
            (p.add(8) as *mut u32).write_volatile(pid);
            (p.add(12) as *mut u32).write_volatile(uid);
            (p as *mut u64).write_volatile(PROBE_MAGIC);
        }
    }

    /// Reads the probe page header, if the magic matches.
    pub fn read_probe(&self) -> Option<(u32, u32)> {
        if self.len < 16 {
            return None;
        }
        let p = self.as_ptr();
        // SAFETY: at least 16 bytes, page-aligned. The other side may write concurrently;
        // volatile reads of plain integers are fine for a probe.
        unsafe {
            if (p as *const u64).read_volatile() != PROBE_MAGIC {
                return None;
            }
            Some(((p.add(8) as *const u32).read_volatile(), (p.add(12) as *const u32).read_volatile()))
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: we mapped exactly this range.
        unsafe { mach_vm_deallocate(task(), self.addr, self.len) };
    }
}

// ---- Messages ---------------------------------------------------------------------------

/// Who sent a message, from the kernel's audit trailer (not from the message body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sender {
    pub pid: i32,
    pub euid: u32,
}

#[derive(Debug)]
pub enum Request {
    /// Driver → broker: here is the memory.
    Deposit { entry: Port, size: u64, sender: Sender },
    /// Client → broker: send me the memory. Answer with [`reply_fetch`].
    Fetch { reply: Port, sender: Sender },
    /// A message that is not one of ours. Its rights have already been released.
    Rejected { id: i32, sender: Sender },
}

fn port_msg(remote: mach_port_t, remote_disposition: u32, id: i32, port: mach_port_t, size: u64) -> PortMsg {
    PortMsg {
        header: Header {
            bits: MACH_MSGH_BITS_COMPLEX | remote_disposition,
            size: size_of::<PortMsg>() as u32,
            remote,
            local: MACH_PORT_NULL,
            voucher: MACH_PORT_NULL,
            id,
        },
        descriptor_count: 1,
        port: PortDescriptor {
            name: port,
            pad1: 0,
            pad2: 0,
            disposition: MACH_MSG_TYPE_COPY_SEND as u8,
            kind: MACH_MSG_PORT_DESCRIPTOR,
        },
        size,
    }
}

fn send(op: &'static str, msg: *mut Header, size: usize, timeout_ms: u32) -> Result<()> {
    // SAFETY: msg points to a fully initialised message of `size` bytes.
    check(op, unsafe {
        mach_msg(msg, MACH_SEND_MSG | MACH_SEND_TIMEOUT, size as u32, 0, MACH_PORT_NULL, timeout_ms, MACH_PORT_NULL)
    })
}

/// Sends a memory entry to the broker. Never blocks longer than `timeout_ms`.
pub fn send_deposit(service: &Port, entry: &Port, size: u64, timeout_ms: u32) -> Result<()> {
    let mut m = port_msg(service.0, MACH_MSG_TYPE_COPY_SEND, MSG_DEPOSIT, entry.0, size);
    send("send deposit", addr_of_mut!(m).cast(), size_of::<PortMsg>(), timeout_ms)
}

/// Answer to [`fetch`].
#[derive(Debug)]
pub struct Fetched {
    /// The memory entry and its size; `None` if the broker has nothing yet.
    pub entry: Option<(Port, u64)>,
    /// Who actually answered, from the kernel. Anyone can register the service name in
    /// their own login session, so a real client must check this (docs/XPC.md, S1).
    pub responder: Sender,
}

/// Asks the broker for the deposited memory.
pub fn fetch(service: &Port, timeout_ms: u32) -> Result<Fetched> {
    let reply = ReceivePort::new()?;
    let mut req = Header {
        bits: MACH_MSG_TYPE_COPY_SEND | (MACH_MSG_TYPE_MAKE_SEND_ONCE << 8),
        size: size_of::<Header>() as u32,
        remote: service.0,
        local: reply.0,
        voucher: MACH_PORT_NULL,
        id: MSG_FETCH,
    };
    send("send fetch", &mut req, size_of::<Header>(), timeout_ms)?;

    let mut buf = RecvBuf([0; 256]);
    let h = buf.0.as_mut_ptr().cast::<Header>();
    // SAFETY: buffer is 256 bytes and 8-aligned.
    check("receive fetch reply", unsafe {
        mach_msg(
            h,
            MACH_RCV_MSG | MACH_RCV_TIMEOUT | MACH_RCV_TRAILER_AUDIT,
            0,
            buf.0.len() as u32,
            reply.0,
            timeout_ms,
            MACH_PORT_NULL,
        )
    })?;
    // SAFETY: the kernel wrote at least a header.
    let hdr: Header = unsafe { read_unaligned(h) };
    let responder = sender_of(&buf, &hdr);
    let ok = hdr.id == MSG_FETCH_REPLY
        && hdr.bits & MACH_MSGH_BITS_COMPLEX != 0
        && hdr.size as usize == size_of::<PortMsg>();
    if !ok {
        // SAFETY: a received message; destroying releases any rights it carried.
        unsafe { mach_msg_destroy(h) };
        return Err(Error::protocol("fetch", format!("unexpected reply id {:#x}", hdr.id)));
    }
    // SAFETY: size checked above.
    let m: PortMsg = unsafe { read_unaligned(h.cast()) };
    let port = m.port;
    if m.descriptor_count != 1 || port.kind != MACH_MSG_PORT_DESCRIPTOR {
        unsafe { mach_msg_destroy(h) };
        return Err(Error::protocol("fetch", "malformed reply"));
    }
    if port.name == MACH_PORT_NULL {
        return Ok(Fetched { entry: None, responder });
    }
    if port.disposition as u32 != MACH_MSG_TYPE_PORT_SEND {
        unsafe { mach_msg_destroy(h) };
        return Err(Error::protocol("fetch", "reply carries the wrong kind of right"));
    }
    Ok(Fetched { entry: Some((Port(port.name), m.size)), responder })
}

/// Waits for the next request on `port` (`None` = wait forever). Errors are kernel errors
/// only; a malformed message comes back as [`Request::Rejected`].
pub fn receive(port: &ReceivePort, timeout_ms: Option<u32>) -> Result<Request> {
    let mut buf = RecvBuf([0; 256]);
    let h = buf.0.as_mut_ptr().cast::<Header>();
    let mut opts = MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT;
    if timeout_ms.is_some() {
        opts |= MACH_RCV_TIMEOUT;
    }
    // SAFETY: buffer is 256 bytes and 8-aligned.
    check("receive", unsafe {
        mach_msg(h, opts, 0, buf.0.len() as u32, port.0, timeout_ms.unwrap_or(0), MACH_PORT_NULL)
    })?;
    // SAFETY: the kernel wrote a header.
    let hdr: Header = unsafe { read_unaligned(h) };
    let sender = sender_of(&buf, &hdr);

    let complex = hdr.bits & MACH_MSGH_BITS_COMPLEX != 0;
    match hdr.id {
        MSG_DEPOSIT if complex && hdr.size as usize == size_of::<PortMsg>() => {
            // SAFETY: size checked.
            let m: PortMsg = unsafe { read_unaligned(h.cast()) };
            let port = m.port;
            if m.descriptor_count == 1
                && port.kind == MACH_MSG_PORT_DESCRIPTOR
                && port.disposition as u32 == MACH_MSG_TYPE_PORT_SEND
                && port.name != MACH_PORT_NULL
            {
                return Ok(Request::Deposit { entry: Port(port.name), size: m.size, sender });
            }
        }
        MSG_FETCH
            if !complex
                && hdr.size as usize == size_of::<Header>()
                && hdr.bits & 0x1f == MACH_MSG_TYPE_PORT_SEND_ONCE
                && hdr.remote != MACH_PORT_NULL =>
        {
            return Ok(Request::Fetch { reply: Port(hdr.remote), sender });
        }
        _ => {}
    }
    // SAFETY: a received message; destroying it releases every right it carried.
    unsafe { mach_msg_destroy(h) };
    Ok(Request::Rejected { id: hdr.id, sender })
}

/// Answers a [`Request::Fetch`]. `None` tells the client nothing has been deposited.
pub fn reply_fetch(reply: Port, entry: Option<(&Port, u64)>) -> Result<()> {
    let (name, size) = entry.map_or((MACH_PORT_NULL, 0), |(p, s)| (p.0, s));
    let mut m = port_msg(reply.into_raw(), MACH_MSG_TYPE_PORT_SEND_ONCE, MSG_FETCH_REPLY, name, size);
    // A send-once reply never waits for queue space; the timeout only bounds the call.
    send("send fetch reply", addr_of_mut!(m).cast(), size_of::<PortMsg>(), 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Sender {
        // SAFETY: trivial libc calls.
        Sender { pid: unsafe { libc::getpid() }, euid: unsafe { libc::geteuid() } }
    }

    #[test]
    fn memory_entry_is_shared_both_ways() {
        let (a, entry) = Shared::create(PROBE_SIZE).unwrap();
        let b = Shared::map(&entry, PROBE_SIZE).unwrap();
        assert_ne!(a.as_ptr(), b.as_ptr());
        a.write_probe(7, 8);
        assert_eq!(b.read_probe(), Some((7, 8)));
        b.write_probe(9, 10);
        assert_eq!(a.read_probe(), Some((9, 10)));
    }

    #[test]
    fn map_rejects_absurd_sizes() {
        let (_a, entry) = Shared::create(PROBE_SIZE).unwrap();
        assert!(Shared::map(&entry, 0).is_err());
        assert!(Shared::map(&entry, MAX_SHARED_BYTES + 1).is_err());
        // Larger than the entry: the kernel refuses.
        assert!(Shared::map(&entry, PROBE_SIZE * 4).is_err());
    }

    #[test]
    fn deposit_carries_entry_and_audit_token() {
        let rx = ReceivePort::new().unwrap();
        let tx = rx.make_send().unwrap();
        let (mem, entry) = Shared::create(PROBE_SIZE).unwrap();
        mem.write_probe(1234, 42);
        send_deposit(&tx, &entry, PROBE_SIZE, 100).unwrap();
        match receive(&rx, Some(1000)).unwrap() {
            Request::Deposit { entry, size, sender } => {
                assert_eq!(size, PROBE_SIZE);
                assert_eq!(sender, me());
                let m = Shared::map(&entry, size).unwrap();
                assert_eq!(m.read_probe(), Some((1234, 42)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn fetch_round_trip() {
        let rx = ReceivePort::new().unwrap();
        let tx = rx.make_send().unwrap();
        let (mem, entry) = Shared::create(PROBE_SIZE).unwrap();
        mem.write_probe(5, 6);

        let broker = std::thread::spawn(move || {
            for give in [false, true] {
                match receive(&rx, Some(2000)).unwrap() {
                    Request::Fetch { reply, sender } => {
                        assert_eq!(sender, me());
                        reply_fetch(reply, give.then_some((&entry, PROBE_SIZE))).unwrap();
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        });

        let first = fetch(&tx, 2000).unwrap();
        assert!(first.entry.is_none());
        assert_eq!(first.responder, me());
        let (port, size) = fetch(&tx, 2000).unwrap().entry.expect("entry");
        assert_eq!(size, PROBE_SIZE);
        assert_eq!(Shared::map(&port, size).unwrap().read_probe(), Some((5, 6)));
        broker.join().unwrap();
    }

    #[test]
    fn foreign_messages_are_rejected() {
        let rx = ReceivePort::new().unwrap();
        let tx = rx.make_send().unwrap();
        let mut h = Header {
            bits: MACH_MSG_TYPE_COPY_SEND,
            size: size_of::<Header>() as u32,
            remote: tx.0,
            id: 0x1234,
            ..Default::default()
        };
        send("send", &mut h, size_of::<Header>(), 100).unwrap();
        assert!(matches!(receive(&rx, Some(1000)).unwrap(), Request::Rejected { id: 0x1234, .. }));
        // A deposit with the right id but no port.
        let mut h = Header { id: MSG_DEPOSIT, ..h };
        send("send", &mut h, size_of::<Header>(), 100).unwrap();
        assert!(matches!(receive(&rx, Some(1000)).unwrap(), Request::Rejected { id: MSG_DEPOSIT, .. }));
    }

    #[test]
    fn service_name_uses_the_prefix() {
        assert!(!ID_PREFIX.is_empty());
        assert_eq!(SERVICE_NAME, format!("{ID_PREFIX}.spool-broker"));
    }

    #[test]
    fn unknown_service_fails_cleanly() {
        let e = look_up(concat!(env!("SPOOL_ID_PREFIX"), ".spool-broker.does-not-exist")).unwrap_err();
        assert_ne!(e.kr, 0);
        assert!(e.to_string().contains("bootstrap_look_up"));
    }
}
