//! Spool AudioServerPlugIn: a virtual loopback device. What apps play to its output is
//! streamed to the recorder daemon through a shared-memory ring, and comes back out of its
//! input for any app that records from Spool.
//!
//! Object tree:
//!
//! ```text
//! PlugIn (1)
//! └── Device (2)              "Spool", output and input
//!     ├── Stream (3)          output, f32 interleaved
//!     ├── Volume control (4)  output scope, master element
//!     ├── Mute control (5)    output scope, master element
//!     └── Stream (6)          input, same format as stream 3 (loopback)
//! ```
//!
//! The loopback is pre-fader: volume and mute change what you hear through the monitor, not
//! what apps reading Spool's input receive (PRO.md §3.5).
//!
//! Threading model: the HAL calls property functions from arbitrary threads and IO functions
//! from its real-time IO thread. All shared state is atomics; the only lock (`control`)
//! serialises StartIO/StopIO and is never taken on the IO path.
//!
//! Every `extern "C"` entry point is wrapped in [`guard`], so a panic is turned into an
//! error code instead of unwinding into `coreaudiod` (release builds additionally use
//! `panic = "abort"`).

#![allow(non_upper_case_globals)] // Core Audio constant names are kept verbatim.

mod ffi;
mod loopback;
#[cfg(feature = "xpc-probe")]
mod xpc_probe;

use ffi::*;
use loopback::Loopback;
use spool_ring::{Format, Producer};
use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Mutex, OnceLock};

// ---- Configuration ---------------------------------------------------------------------

pub const DEVICE_NAME: &str = "Spool";
pub const MANUFACTURER: &str = "Spool";
pub const DEVICE_UID: &str = "SpoolDevice_UID";
pub const MODEL_UID: &str = "SpoolDevice_ModelUID";
pub const CHANNELS: u32 = 2;
pub const SAMPLE_RATES: [f64; 6] = [44100.0, 48000.0, 88200.0, 96000.0, 176400.0, 192000.0];
pub const DEFAULT_SAMPLE_RATE: f64 = 48000.0;
/// Frames per period of the device's virtual timeline (`kAudioDevicePropertyZeroTimeStampPeriod`).
pub const ZERO_TS_PERIOD: u32 = 16384;
/// Volume control range.
pub const MIN_DB: f32 = -96.0;
pub const MAX_DB: f32 = 0.0;

pub const PLUGIN_ID: AudioObjectID = kAudioObjectPlugInObject;
pub const DEVICE_ID: AudioObjectID = 2;
pub const STREAM_ID: AudioObjectID = 3;
pub const VOLUME_ID: AudioObjectID = 4;
pub const MUTE_ID: AudioObjectID = 5;
pub const INPUT_STREAM_ID: AudioObjectID = 6;

fn ring_name() -> String {
    if cfg!(test) {
        format!("/spool.test.{}", std::process::id())
    } else {
        spool_ring::DEFAULT_NAME.to_string()
    }
}

// ---- State ---------------------------------------------------------------------------------

struct State {
    host: AtomicPtr<AudioServerPlugInHostInterface>,
    refcount: AtomicU32,
    sample_rate_bits: AtomicU64,
    host_ticks_per_sec_bits: AtomicU64,
    volume_scalar_bits: AtomicU32,
    muted: AtomicBool,
    stream_active: AtomicBool,
    input_active: AtomicBool,
    io_clients: AtomicU32,
    control: Mutex<()>,
    ring: OnceLock<Producer>,
    /// Output-to-input buffer, allocated in Initialize (never on the IO thread).
    loopback: OnceLock<Loopback>,
    /// Latest requested sample rate (f64 bits, 0 = none) and whether a helper is running.
    pending_rate_bits: AtomicU64,
    rate_worker: AtomicBool,
    // IO-thread state.
    anchor_host_time: AtomicU64,
    /// Sample time expected at the start of the next WriteMix (f64 bits); NaN = unknown.
    next_sample_bits: AtomicU64,
}

static STATE: State = State {
    host: AtomicPtr::new(std::ptr::null_mut()),
    refcount: AtomicU32::new(1),
    sample_rate_bits: AtomicU64::new(DEFAULT_SAMPLE_RATE.to_bits()),
    host_ticks_per_sec_bits: AtomicU64::new(1_000_000_000f64.to_bits()),
    volume_scalar_bits: AtomicU32::new(1.0f32.to_bits()),
    muted: AtomicBool::new(false),
    stream_active: AtomicBool::new(true),
    input_active: AtomicBool::new(true),
    io_clients: AtomicU32::new(0),
    control: Mutex::new(()),
    ring: OnceLock::new(),
    loopback: OnceLock::new(),
    pending_rate_bits: AtomicU64::new(0),
    rate_worker: AtomicBool::new(false),
    anchor_host_time: AtomicU64::new(0),
    next_sample_bits: AtomicU64::new(f64::NAN.to_bits()),
};

impl State {
    fn active_flag(&self, stream: AudioObjectID) -> &AtomicBool {
        if stream == INPUT_STREAM_ID { &self.input_active } else { &self.stream_active }
    }
    fn sample_rate(&self) -> f64 {
        f64::from_bits(self.sample_rate_bits.load(Relaxed))
    }
    fn volume(&self) -> f32 {
        f32::from_bits(self.volume_scalar_bits.load(Relaxed))
    }
    fn format(&self) -> Format {
        Format { sample_rate: self.sample_rate(), channels: CHANNELS }
    }
    /// Creates the shared-memory ring if it does not exist yet. Not real-time safe.
    fn ensure_ring(&self) {
        if self.ring.get().is_some() {
            return;
        }
        match Producer::create(&ring_name(), spool_ring::DEFAULT_CAPACITY, self.format()) {
            Ok(p) => {
                p.set_gain(scalar_to_gain(self.volume()), self.muted.load(Relaxed));
                let _ = self.ring.set(p);
            }
            Err(e) => log(&format!("Spool: cannot create shared-memory ring: {e}")),
        }
    }
    /// Allocates the loopback buffer if needed. Not real-time safe.
    fn ensure_loopback(&self) {
        let _ = self.loopback.get_or_init(|| Loopback::new(CHANNELS));
    }
    fn clear_loopback(&self) {
        if let Some(lb) = self.loopback.get() {
            lb.clear();
        }
    }
    fn publish_gain(&self) {
        if let Some(r) = self.ring.get() {
            r.set_gain(scalar_to_gain(self.volume()), self.muted.load(Relaxed));
        }
    }
    fn notify(&self, object: AudioObjectID, selectors: &[u32]) {
        let host = self.host.load(Acquire);
        if host.is_null() {
            return;
        }
        let addrs: Vec<_> = selectors
            .iter()
            .map(|&s| AudioObjectPropertyAddress {
                mSelector: s,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            })
            .collect();
        // SAFETY: host pointer was provided by the HAL in Initialize and stays valid.
        unsafe { ((*host).PropertiesChanged)(host, object, addrs.len() as u32, addrs.as_ptr()) };
    }
    /// Asks the host to schedule a sample-rate change (applied in PerformDeviceConfigurationChange).
    ///
    /// The request must not be made synchronously from within a property call, so it runs on
    /// a helper thread. Requests are coalesced: at most one helper exists, and it forwards
    /// only the latest rate, so a client spamming SetPropertyData cannot make `coreaudiod`
    /// churn threads. Spawning uses `Builder::spawn`, which reports failure instead of
    /// panicking (a panic aborts `coreaudiod` under `panic = "abort"`).
    fn request_rate_change(&self, rate: f64) -> OSStatus {
        if self.host.load(Acquire).is_null() {
            return kAudioHardwareNotRunningError;
        }
        self.pending_rate_bits.store(rate.to_bits(), Release);
        if self.rate_worker.swap(true, AcqRel) {
            return kAudioHardwareNoError; // the running helper picks it up
        }
        let spawned = std::thread::Builder::new().name("Spool rate change".into()).spawn(rate_change_worker);
        if spawned.is_err() {
            self.rate_worker.store(false, Release);
            return kAudioHardwareUnspecifiedError;
        }
        kAudioHardwareNoError
    }
}

fn rate_change_worker() {
    let s = &STATE;
    loop {
        // 0 bits = 0.0 Hz, never a valid request.
        let bits = s.pending_rate_bits.swap(0, AcqRel);
        let host = s.host.load(Acquire);
        if bits != 0 && !host.is_null() {
            // SAFETY: see `notify`.
            unsafe {
                ((*host).RequestDeviceConfigurationChange)(host, DEVICE_ID, f64::from_bits(bits) as u64, std::ptr::null_mut())
            };
        }
        s.rate_worker.store(false, Release);
        // A request stored after our swap saw `rate_worker == true` and returned; handle it
        // unless a new helper has already taken over.
        if s.pending_rate_bits.load(Acquire) == 0 || s.rate_worker.swap(true, AcqRel) {
            return;
        }
    }
}

fn log(msg: &str) {
    if let Ok(c) = std::ffi::CString::new(msg) {
        // SAFETY: format string and argument are valid C strings.
        unsafe { libc::syslog(libc::LOG_NOTICE, c"%s".as_ptr(), c.as_ptr()) };
    }
}

// Volume curve: amplitude = scalar², so dB = 40·log10(scalar).
// All three are NaN-safe: `f32::clamp` passes NaN through, and any HAL client can send one.
pub fn scalar_to_db(s: f32) -> f32 {
    if s.is_nan() || s <= 0.0 { MIN_DB } else { (40.0 * s.log10()).clamp(MIN_DB, MAX_DB) }
}
pub fn db_to_scalar(db: f32) -> f32 {
    if db.is_nan() || db <= MIN_DB { 0.0 } else { 10f32.powf(db.min(MAX_DB) / 40.0) }
}
fn scalar_to_gain(s: f32) -> f32 {
    let s = if s.is_finite() { s.clamp(0.0, 1.0) } else { 0.0 };
    s * s
}

// ---- Property values -----------------------------------------------------------------------

/// A serialised property value. Owns a CF object if it holds one.
struct Val {
    bytes: Vec<u8>,
    item: usize,
    list: bool,
    cf: CFTypeRef,
}

impl Val {
    fn pod<T: Copy>(v: T) -> Val {
        Self::list_of(std::slice::from_ref(&v), false)
    }
    fn list<T: Copy>(v: &[T]) -> Val {
        Self::list_of(v, true)
    }
    fn list_of<T: Copy>(v: &[T], list: bool) -> Val {
        let len = std::mem::size_of_val(v);
        // SAFETY: T is Copy plain data; we only read its bytes.
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, len) }.to_vec();
        Val { bytes, item: std::mem::size_of::<T>(), list, cf: std::ptr::null() }
    }
    fn u32(v: u32) -> Val {
        Self::pod(v)
    }
    fn string(s: &str) -> Val {
        let cf = cfstr(s);
        let mut v = Self::pod(cf as usize);
        v.cf = cf;
        v
    }
    fn size(&self) -> u32 {
        self.bytes.len() as u32
    }
    /// Copies into the caller's buffer. Lists are truncated to whole items that fit.
    fn write(mut self, out: *mut c_void, avail: u32) -> Result<u32, OSStatus> {
        let n = if self.list {
            (avail as usize / self.item.max(1)) * self.item
        } else if (avail as usize) < self.bytes.len() {
            return Err(kAudioHardwareBadPropertySizeError);
        } else {
            self.bytes.len()
        };
        let n = n.min(self.bytes.len());
        if out.is_null() && n > 0 {
            return Err(kAudioHardwareIllegalOperationError);
        }
        // SAFETY: caller guarantees `avail` writable bytes at `out`; n <= avail.
        unsafe { std::ptr::copy_nonoverlapping(self.bytes.as_ptr(), out as *mut u8, n) };
        // Ownership of a CF object passes to the caller.
        self.cf = std::ptr::null();
        Ok(n as u32)
    }
}

impl Drop for Val {
    fn drop(&mut self) {
        if !self.cf.is_null() {
            // SAFETY: we own a +1 reference.
            unsafe { CFRelease(self.cf) };
        }
    }
}

fn asbd(rate: f64) -> AudioStreamBasicDescription {
    let bytes = 4 * CHANNELS;
    AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
        mBytesPerPacket: bytes,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes,
        mChannelsPerFrame: CHANNELS,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

fn available_formats() -> Vec<AudioStreamRangedDescription> {
    SAMPLE_RATES
        .iter()
        .map(|&r| AudioStreamRangedDescription {
            mFormat: asbd(r),
            mSampleRateRange: AudioValueRange { mMinimum: r, mMaximum: r },
        })
        .collect()
}

fn channel_layout() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&kAudioChannelLayoutTag_UseChannelDescriptions.to_ne_bytes());
    v.extend_from_slice(&0u32.to_ne_bytes()); // bitmap
    v.extend_from_slice(&CHANNELS.to_ne_bytes());
    for ch in 0..CHANNELS {
        let label = match (CHANNELS, ch) {
            (1, _) => kAudioChannelLabel_Mono,
            (_, 0) => kAudioChannelLabel_Left,
            (_, 1) => kAudioChannelLabel_Right,
            _ => kAudioChannelLabel_Discrete_0 | ch,
        };
        v.extend_from_slice(&label.to_ne_bytes());
        v.extend_from_slice(&0u32.to_ne_bytes()); // flags
        v.extend_from_slice(&[0u8; 12]); // coordinates
    }
    v
}

/// Returns the value of a property, or an error if the object does not have it.
/// `qualifier` is only used by kAudioPlugInPropertyTranslateUIDToDevice.
fn get(obj: AudioObjectID, a: &AudioObjectPropertyAddress, qualifier: Option<CFStringRef>) -> Result<Val, OSStatus> {
    let s = &STATE;
    let unknown = Err(kAudioHardwareUnknownPropertyError);
    let common = |base: u32, class: u32, owner: AudioObjectID| match a.mSelector {
        kAudioObjectPropertyBaseClass => Some(Val::u32(base)),
        kAudioObjectPropertyClass => Some(Val::u32(class)),
        kAudioObjectPropertyOwner => Some(Val::u32(owner)),
        _ => None,
    };
    match obj {
        PLUGIN_ID => {
            if let Some(v) = common(kAudioObjectClassID, kAudioPlugInClassID, kAudioObjectUnknown) {
                return Ok(v);
            }
            Ok(match a.mSelector {
                kAudioObjectPropertyManufacturer => Val::string(MANUFACTURER),
                kAudioObjectPropertyOwnedObjects | kAudioPlugInPropertyDeviceList => Val::list(&[DEVICE_ID]),
                kAudioPlugInPropertyTranslateUIDToDevice => {
                    let ours = cfstr(DEVICE_UID);
                    // SAFETY: both are valid CFStrings (qualifier checked by caller).
                    let hit = qualifier.is_some_and(|q| !q.is_null() && unsafe { CFEqual(q, ours) } != 0);
                    unsafe { CFRelease(ours) };
                    Val::u32(if hit { DEVICE_ID } else { kAudioObjectUnknown })
                }
                kAudioPlugInPropertyResourceBundle => Val::string(""),
                _ => return unknown,
            })
        }
        DEVICE_ID => {
            if let Some(v) = common(kAudioObjectClassID, kAudioDeviceClassID, PLUGIN_ID) {
                return Ok(v);
            }
            let (owned, streams): (&[u32], &[u32]) = match a.mScope {
                kAudioObjectPropertyScopeInput => (&[INPUT_STREAM_ID], &[INPUT_STREAM_ID]),
                kAudioObjectPropertyScopeOutput => (&[STREAM_ID, VOLUME_ID, MUTE_ID], &[STREAM_ID]),
                _ => (&[STREAM_ID, VOLUME_ID, MUTE_ID, INPUT_STREAM_ID], &[STREAM_ID, INPUT_STREAM_ID]),
            };
            Ok(match a.mSelector {
                kAudioObjectPropertyName => Val::string(DEVICE_NAME),
                kAudioObjectPropertyManufacturer => Val::string(MANUFACTURER),
                kAudioObjectPropertyOwnedObjects => Val::list(owned),
                kAudioDevicePropertyDeviceUID => Val::string(DEVICE_UID),
                kAudioDevicePropertyModelUID => Val::string(MODEL_UID),
                kAudioDevicePropertyTransportType => Val::u32(kAudioDeviceTransportTypeVirtual),
                kAudioDevicePropertyRelatedDevices => Val::list(&[DEVICE_ID]),
                kAudioDevicePropertyClockDomain => Val::u32(0),
                kAudioDevicePropertyDeviceIsAlive => Val::u32(1),
                kAudioDevicePropertyDeviceIsRunning => Val::u32((s.io_clients.load(Relaxed) > 0) as u32),
                kAudioDevicePropertyDeviceCanBeDefaultDevice => Val::u32(1),
                kAudioDevicePropertyDeviceCanBeDefaultSystemDevice => Val::u32(1),
                // Both scopes: Spool adds nothing to the HAL's own buffering (PRO.md §3.4).
                kAudioDevicePropertyLatency => Val::u32(0),
                kAudioDevicePropertyStreams => Val::list(streams),
                kAudioObjectPropertyControlList => Val::list(&[VOLUME_ID, MUTE_ID]),
                kAudioDevicePropertySafetyOffset => Val::u32(0),
                kAudioDevicePropertyNominalSampleRate => Val::pod(s.sample_rate()),
                kAudioDevicePropertyAvailableNominalSampleRates => {
                    let r: Vec<_> =
                        SAMPLE_RATES.iter().map(|&r| AudioValueRange { mMinimum: r, mMaximum: r }).collect();
                    Val::list(&r)
                }
                kAudioDevicePropertyIsHidden => Val::u32(0),
                kAudioDevicePropertyPreferredChannelsForStereo => {
                    Val::list(&[1u32, if CHANNELS > 1 { 2 } else { 1 }])
                }
                kAudioDevicePropertyPreferredChannelLayout => {
                    let mut v = Val::list::<u8>(&channel_layout());
                    v.list = false;
                    v
                }
                kAudioDevicePropertyZeroTimeStampPeriod => Val::u32(ZERO_TS_PERIOD),
                kAudioDevicePropertyConfigurationApplication => Val::string("com.apple.audio.AudioMIDISetup"),
                _ => return unknown,
            })
        }
        STREAM_ID | INPUT_STREAM_ID => {
            if let Some(v) = common(kAudioObjectClassID, kAudioStreamClassID, DEVICE_ID) {
                return Ok(v);
            }
            let input = obj == INPUT_STREAM_ID;
            Ok(match a.mSelector {
                kAudioObjectPropertyOwnedObjects => Val::list::<u32>(&[]),
                kAudioStreamPropertyIsActive => Val::u32(s.active_flag(obj).load(Relaxed) as u32),
                kAudioStreamPropertyDirection => Val::u32(input as u32), // 0 = output, 1 = input
                kAudioStreamPropertyTerminalType => {
                    Val::u32(if input { kAudioStreamTerminalTypeLine } else { kAudioStreamTerminalTypeSpeaker })
                }
                kAudioStreamPropertyStartingChannel => Val::u32(1),
                kAudioStreamPropertyLatency => Val::u32(0),
                kAudioStreamPropertyVirtualFormat | kAudioStreamPropertyPhysicalFormat => {
                    Val::pod(asbd(s.sample_rate()))
                }
                kAudioStreamPropertyAvailableVirtualFormats | kAudioStreamPropertyAvailablePhysicalFormats => {
                    Val::list(&available_formats())
                }
                _ => return unknown,
            })
        }
        VOLUME_ID => {
            if let Some(v) = common(kAudioLevelControlClassID, kAudioVolumeControlClassID, DEVICE_ID) {
                return Ok(v);
            }
            Ok(match a.mSelector {
                kAudioObjectPropertyOwnedObjects => Val::list::<u32>(&[]),
                kAudioControlPropertyScope => Val::u32(kAudioObjectPropertyScopeOutput),
                kAudioControlPropertyElement => Val::u32(kAudioObjectPropertyElementMain),
                kAudioLevelControlPropertyScalarValue => Val::pod(s.volume()),
                kAudioLevelControlPropertyDecibelValue => Val::pod(scalar_to_db(s.volume())),
                kAudioLevelControlPropertyDecibelRange => {
                    Val::pod(AudioValueRange { mMinimum: MIN_DB as f64, mMaximum: MAX_DB as f64 })
                }
                // In/out conversions: the size is an f32; the value is computed in GetPropertyData.
                kAudioLevelControlPropertyConvertScalarToDecibels
                | kAudioLevelControlPropertyConvertDecibelsToScalar => Val::pod(0f32),
                _ => return unknown,
            })
        }
        MUTE_ID => {
            if let Some(v) = common(kAudioBooleanControlClassID, kAudioMuteControlClassID, DEVICE_ID) {
                return Ok(v);
            }
            Ok(match a.mSelector {
                kAudioObjectPropertyOwnedObjects => Val::list::<u32>(&[]),
                kAudioControlPropertyScope => Val::u32(kAudioObjectPropertyScopeOutput),
                kAudioControlPropertyElement => Val::u32(kAudioObjectPropertyElementMain),
                kAudioBooleanControlPropertyValue => Val::u32(s.muted.load(Relaxed) as u32),
                _ => return unknown,
            })
        }
        _ => Err(kAudioHardwareBadObjectError),
    }
}

fn settable(obj: AudioObjectID, sel: u32) -> bool {
    matches!(
        (obj, sel),
        (DEVICE_ID, kAudioDevicePropertyNominalSampleRate)
            | (STREAM_ID | INPUT_STREAM_ID, kAudioStreamPropertyIsActive)
            | (STREAM_ID | INPUT_STREAM_ID, kAudioStreamPropertyVirtualFormat)
            | (STREAM_ID | INPUT_STREAM_ID, kAudioStreamPropertyPhysicalFormat)
            | (VOLUME_ID, kAudioLevelControlPropertyScalarValue)
            | (VOLUME_ID, kAudioLevelControlPropertyDecibelValue)
            | (MUTE_ID, kAudioBooleanControlPropertyValue)
    )
}

/// Reads a `T` from a property data buffer after checking its size.
fn read_in<T: Copy>(data: *const c_void, size: u32) -> Result<T, OSStatus> {
    if (size as usize) < std::mem::size_of::<T>() || data.is_null() {
        return Err(kAudioHardwareBadPropertySizeError);
    }
    // SAFETY: size checked; read_unaligned tolerates any alignment.
    Ok(unsafe { std::ptr::read_unaligned(data as *const T) })
}

fn set(obj: AudioObjectID, a: &AudioObjectPropertyAddress, data: *const c_void, size: u32) -> Result<(), OSStatus> {
    let s = &STATE;
    if get(obj, a, None).is_err() {
        return Err(kAudioHardwareUnknownPropertyError);
    }
    if !settable(obj, a.mSelector) {
        return Err(kAudioHardwareUnsupportedOperationError);
    }
    let change_rate = |rate: f64| -> Result<(), OSStatus> {
        if !SAMPLE_RATES.contains(&rate) {
            return Err(kAudioDeviceUnsupportedFormatError);
        }
        if rate != s.sample_rate() {
            let e = s.request_rate_change(rate);
            if e != 0 {
                return Err(e);
            }
        }
        Ok(())
    };
    match (obj, a.mSelector) {
        (DEVICE_ID, kAudioDevicePropertyNominalSampleRate) => change_rate(read_in::<f64>(data, size)?),
        (STREAM_ID | INPUT_STREAM_ID, kAudioStreamPropertyIsActive) => {
            s.active_flag(obj).store(read_in::<u32>(data, size)? != 0, Relaxed);
            s.notify(obj, &[kAudioStreamPropertyIsActive]);
            Ok(())
        }
        // Both streams share one format, so a format set on either changes the device rate.
        (STREAM_ID | INPUT_STREAM_ID, _) => {
            let f: AudioStreamBasicDescription = read_in(data, size)?;
            let want = asbd(f.mSampleRate);
            if f != want {
                return Err(kAudioDeviceUnsupportedFormatError);
            }
            change_rate(f.mSampleRate)
        }
        (VOLUME_ID, sel) => {
            let v: f32 = read_in(data, size)?;
            if !v.is_finite() {
                return Err(kAudioHardwareIllegalOperationError);
            }
            let scalar = if sel == kAudioLevelControlPropertyScalarValue {
                v.clamp(0.0, 1.0)
            } else {
                db_to_scalar(v.clamp(MIN_DB, MAX_DB))
            };
            s.volume_scalar_bits.store(scalar.to_bits(), Relaxed);
            s.publish_gain();
            s.notify(VOLUME_ID, &[kAudioLevelControlPropertyScalarValue, kAudioLevelControlPropertyDecibelValue]);
            Ok(())
        }
        (MUTE_ID, _) => {
            s.muted.store(read_in::<u32>(data, size)? != 0, Relaxed);
            s.publish_gain();
            s.notify(MUTE_ID, &[kAudioBooleanControlPropertyValue]);
            Ok(())
        }
        _ => Err(kAudioHardwareUnsupportedOperationError),
    }
}

// ---- Entry points --------------------------------------------------------------------------

/// Runs `f`, converting a panic into `fallback`.
#[inline(always)]
fn guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

fn status(r: Result<(), OSStatus>) -> OSStatus {
    r.err().unwrap_or(kAudioHardwareNoError)
}

#[repr(transparent)]
struct InterfacePtr(*const AudioServerPlugInDriverInterface);
// SAFETY: points to an immutable static.
unsafe impl Sync for InterfacePtr {}

static INTERFACE: AudioServerPlugInDriverInterface = AudioServerPlugInDriverInterface {
    _reserved: std::ptr::null_mut(),
    QueryInterface: query_interface,
    AddRef: add_ref,
    Release: release,
    Initialize: initialize,
    CreateDevice: create_device,
    DestroyDevice: destroy_device,
    AddDeviceClient: add_device_client,
    RemoveDeviceClient: remove_device_client,
    PerformDeviceConfigurationChange: perform_config_change,
    AbortDeviceConfigurationChange: abort_config_change,
    HasProperty: has_property,
    IsPropertySettable: is_property_settable,
    GetPropertyDataSize: get_property_data_size,
    GetPropertyData: get_property_data,
    SetPropertyData: set_property_data,
    StartIO: start_io,
    StopIO: stop_io,
    GetZeroTimeStamp: get_zero_time_stamp,
    WillDoIOOperation: will_do_io_operation,
    BeginIOOperation: begin_io_operation,
    DoIOOperation: do_io_operation,
    EndIOOperation: end_io_operation,
};

static INTERFACE_PTR: InterfacePtr = InterfacePtr(&INTERFACE);

fn driver_ref() -> AudioServerPlugInDriverRef {
    &INTERFACE_PTR as *const InterfacePtr as AudioServerPlugInDriverRef
}

fn check_driver(d: AudioServerPlugInDriverRef) -> Result<(), OSStatus> {
    if d == driver_ref() { Ok(()) } else { Err(kAudioHardwareBadObjectError) }
}

/// CFPlugIn factory, named in Info.plist under `CFPlugInFactories`.
///
/// # Safety
/// Called by CFPlugIn with a valid `CFUUIDRef`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SpoolDriverFactory(_alloc: CFAllocatorRef, type_uuid: CFUUIDRef) -> *mut c_void {
    guard(std::ptr::null_mut(), || {
        if type_uuid.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: valid CFUUIDRef per contract.
        let bytes = unsafe { CFUUIDGetUUIDBytes(type_uuid) };
        if bytes.0 == kAudioServerPlugInTypeUUID { driver_ref() as *mut c_void } else { std::ptr::null_mut() }
    })
}

unsafe extern "C" fn query_interface(d: *mut c_void, uuid: CFUUIDBytes, out: *mut *mut c_void) -> HRESULT {
    guard(E_NOINTERFACE, || {
        if check_driver(d as _).is_err() || out.is_null() {
            return E_NOINTERFACE;
        }
        if uuid.0 == IUnknownUUID || uuid.0 == kAudioServerPlugInDriverInterfaceUUID {
            STATE.refcount.fetch_add(1, Relaxed);
            // SAFETY: out checked non-null.
            unsafe { *out = d };
            S_OK
        } else {
            unsafe { *out = std::ptr::null_mut() };
            E_NOINTERFACE
        }
    })
}

unsafe extern "C" fn add_ref(d: *mut c_void) -> ULONG {
    guard(0, || {
        if check_driver(d as _).is_err() {
            return 0;
        }
        STATE.refcount.fetch_add(1, Relaxed).saturating_add(1)
    })
}

unsafe extern "C" fn release(d: *mut c_void) -> ULONG {
    guard(0, || {
        if check_driver(d as _).is_err() {
            return 0;
        }
        // The driver is a static singleton; the count is informational only.
        let mut cur = STATE.refcount.load(Relaxed);
        loop {
            let next = cur.saturating_sub(1);
            match STATE.refcount.compare_exchange_weak(cur, next, Relaxed, Relaxed) {
                Ok(_) => return next,
                Err(c) => cur = c,
            }
        }
    })
}

unsafe extern "C" fn initialize(d: AudioServerPlugInDriverRef, host: AudioServerPlugInHostRef) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status(check_driver(d).map(|_| {
            STATE.host.store(host as *mut _, Release);
            let ticks_per_sec = spool_ring::ns_to_ticks(1_000_000_000) as f64;
            STATE.host_ticks_per_sec_bits.store(ticks_per_sec.to_bits(), Relaxed);
            STATE.ensure_ring();
            STATE.ensure_loopback();
            #[cfg(feature = "xpc-probe")]
            if !cfg!(test) {
                xpc_probe::spawn();
            }
        }))
    })
}

unsafe extern "C" fn create_device(
    _d: AudioServerPlugInDriverRef,
    _desc: CFDictionaryRef,
    _client: *const AudioServerPlugInClientInfo,
    _out: *mut AudioObjectID,
) -> OSStatus {
    kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn destroy_device(_d: AudioServerPlugInDriverRef, _dev: AudioObjectID) -> OSStatus {
    kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn add_device_client(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _c: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || status(check_device(d, dev)))
}

unsafe extern "C" fn remove_device_client(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _c: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || status(check_device(d, dev)))
}

fn check_device(d: AudioServerPlugInDriverRef, dev: AudioObjectID) -> Result<(), OSStatus> {
    check_driver(d)?;
    if dev == DEVICE_ID { Ok(()) } else { Err(kAudioHardwareBadObjectError) }
}

unsafe extern "C" fn perform_config_change(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    action: u64,
    _info: *mut c_void,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status(check_device(d, dev).and_then(|_| {
            let rate = action as f64;
            if !SAMPLE_RATES.contains(&rate) {
                return Err(kAudioHardwareBadObjectError);
            }
            let _g = STATE.control.lock().unwrap_or_else(|e| e.into_inner());
            STATE.sample_rate_bits.store(rate.to_bits(), Relaxed);
            // IO is stopped during a configuration change, so this cannot race the writer.
            if let Some(r) = STATE.ring.get() {
                r.set_format(STATE.format());
            }
            // Frames stored at the old rate must not come out of the input at the new one.
            STATE.clear_loopback();
            Ok(())
        }))
    })
}

unsafe extern "C" fn abort_config_change(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _action: u64,
    _info: *mut c_void,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || status(check_device(d, dev)))
}

/// # Safety
/// `addr` must be null or valid.
unsafe fn addr_ref<'a>(addr: *const AudioObjectPropertyAddress) -> Result<&'a AudioObjectPropertyAddress, OSStatus> {
    // SAFETY: caller contract.
    unsafe { addr.as_ref() }.ok_or(kAudioHardwareIllegalOperationError)
}

fn qualifier_cfstring(size: u32, data: *const c_void) -> Option<CFStringRef> {
    read_in::<CFStringRef>(data, size).ok()
}

unsafe extern "C" fn has_property(
    d: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _pid: pid_t,
    addr: *const AudioObjectPropertyAddress,
) -> Boolean {
    guard(0, || {
        let has = check_driver(d).is_ok() && unsafe { addr_ref(addr) }.is_ok_and(|a| get(obj, a, None).is_ok());
        has as Boolean
    })
}

unsafe extern "C" fn is_property_settable(
    d: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _pid: pid_t,
    addr: *const AudioObjectPropertyAddress,
    out: *mut Boolean,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status((|| {
            check_driver(d)?;
            let a = unsafe { addr_ref(addr) }?;
            get(obj, a, None)?;
            if out.is_null() {
                return Err(kAudioHardwareIllegalOperationError);
            }
            // SAFETY: checked non-null.
            unsafe { *out = settable(obj, a.mSelector) as u8 };
            Ok(())
        })())
    })
}

unsafe extern "C" fn get_property_data_size(
    d: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _pid: pid_t,
    addr: *const AudioObjectPropertyAddress,
    qsize: u32,
    qdata: *const c_void,
    out: *mut u32,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status((|| {
            check_driver(d)?;
            let a = unsafe { addr_ref(addr) }?;
            let v = get(obj, a, qualifier_cfstring(qsize, qdata))?;
            if out.is_null() {
                return Err(kAudioHardwareIllegalOperationError);
            }
            // SAFETY: checked non-null.
            unsafe { *out = v.size() };
            Ok(())
        })())
    })
}

unsafe extern "C" fn get_property_data(
    d: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _pid: pid_t,
    addr: *const AudioObjectPropertyAddress,
    qsize: u32,
    qdata: *const c_void,
    in_size: u32,
    out_size: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status((|| {
            check_driver(d)?;
            let a = unsafe { addr_ref(addr) }?;
            if out_size.is_null() {
                return Err(kAudioHardwareIllegalOperationError);
            }
            let mut v = get(obj, a, qualifier_cfstring(qsize, qdata))?;
            if obj == VOLUME_ID {
                // In/out conversions read their input from the output buffer.
                let convert: Option<fn(f32) -> f32> = match a.mSelector {
                    kAudioLevelControlPropertyConvertScalarToDecibels => Some(|x| scalar_to_db(x.clamp(0.0, 1.0))),
                    kAudioLevelControlPropertyConvertDecibelsToScalar => Some(db_to_scalar),
                    _ => None,
                };
                if let Some(f) = convert {
                    v = Val::pod(f(read_in::<f32>(out, in_size)?));
                }
            }
            let n = v.write(out, in_size)?;
            // SAFETY: checked non-null.
            unsafe { *out_size = n };
            Ok(())
        })())
    })
}

unsafe extern "C" fn set_property_data(
    d: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _pid: pid_t,
    addr: *const AudioObjectPropertyAddress,
    _qsize: u32,
    _qdata: *const c_void,
    in_size: u32,
    data: *const c_void,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status((|| {
            check_driver(d)?;
            let a = unsafe { addr_ref(addr) }?;
            set(obj, a, data, in_size)
        })())
    })
}

unsafe extern "C" fn start_io(d: AudioServerPlugInDriverRef, dev: AudioObjectID, _client: u32) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status(check_device(d, dev).map(|_| {
            let s = &STATE;
            let _g = s.control.lock().unwrap_or_else(|e| e.into_inner());
            if s.io_clients.load(Relaxed) == 0 {
                s.ensure_ring();
                s.ensure_loopback();
                // No IO is running, so nothing reads or writes the buffer concurrently.
                s.clear_loopback();
                s.anchor_host_time.store(spool_ring::host_ticks(), Relaxed);
                s.next_sample_bits.store(f64::NAN.to_bits(), Relaxed);
                if let Some(r) = s.ring.get() {
                    r.set_running(true);
                }
            }
            s.io_clients.fetch_add(1, Relaxed);
        }))
    })
}

unsafe extern "C" fn stop_io(d: AudioServerPlugInDriverRef, dev: AudioObjectID, _client: u32) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        status(check_device(d, dev).and_then(|_| {
            let s = &STATE;
            let _g = s.control.lock().unwrap_or_else(|e| e.into_inner());
            let n = s.io_clients.load(Relaxed);
            if n == 0 {
                return Err(kAudioHardwareIllegalOperationError);
            }
            s.io_clients.store(n - 1, Relaxed);
            if n == 1
                && let Some(r) = s.ring.get() {
                    r.set_running(false);
                }
            Ok(())
        }))
    })
}

/// Device timeline: one zero time stamp every `ZERO_TS_PERIOD` frames since the anchor taken
/// at StartIO. Computed directly from the host clock, so it never drifts or jumps.
fn zero_time_stamp(now: u64) -> (f64, u64) {
    let s = &STATE;
    let ticks_per_sec = f64::from_bits(s.host_ticks_per_sec_bits.load(Relaxed));
    let ticks_per_period = ticks_per_sec / s.sample_rate() * ZERO_TS_PERIOD as f64;
    let anchor = s.anchor_host_time.load(Relaxed);
    let periods = (now.saturating_sub(anchor) as f64 / ticks_per_period).floor();
    (periods * ZERO_TS_PERIOD as f64, anchor + (periods * ticks_per_period) as u64)
}

unsafe extern "C" fn get_zero_time_stamp(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _client: u32,
    out_sample: *mut f64,
    out_host: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        if check_device(d, dev).is_err() || out_sample.is_null() || out_host.is_null() || out_seed.is_null() {
            return kAudioHardwareIllegalOperationError;
        }
        let (sample, host) = zero_time_stamp(spool_ring::host_ticks());
        // SAFETY: checked non-null.
        unsafe {
            *out_sample = sample;
            *out_host = host;
            *out_seed = 1;
        }
        kAudioHardwareNoError
    })
}

unsafe extern "C" fn will_do_io_operation(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _client: u32,
    op: u32,
    will_do: *mut Boolean,
    in_place: *mut Boolean,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        if check_device(d, dev).is_err() || will_do.is_null() || in_place.is_null() {
            return kAudioHardwareIllegalOperationError;
        }
        // SAFETY: checked non-null.
        unsafe {
            *will_do = matches!(op, kAudioServerPlugInIOOperationWriteMix | kAudioServerPlugInIOOperationReadInput) as u8;
            *in_place = 1;
        }
        kAudioHardwareNoError
    })
}

unsafe extern "C" fn begin_io_operation(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _client: u32,
    _op: u32,
    _frames: u32,
    _cycle: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || status(check_device(d, dev)))
}

unsafe extern "C" fn end_io_operation(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _client: u32,
    _op: u32,
    _frames: u32,
    _cycle: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || status(check_device(d, dev)))
}

/// Real-time path: no allocation, locks or syscalls.
///
/// - `WriteMix` on the output stream: copy the final mix into the shared ring (recording and
///   monitoring) and into the loopback buffer at the cycle's output time.
/// - `ReadInput` on the input stream: fill the HAL's buffer from the loopback buffer at the
///   cycle's input time.
unsafe extern "C" fn do_io_operation(
    d: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    stream: AudioObjectID,
    _client: u32,
    op: u32,
    frames: u32,
    cycle: *const AudioServerPlugInIOCycleInfo,
    main: *mut c_void,
    _secondary: *mut c_void,
) -> OSStatus {
    guard(kAudioHardwareUnspecifiedError, || {
        if check_device(d, dev).is_err() || !matches!(stream, STREAM_ID | INPUT_STREAM_ID) {
            return kAudioHardwareBadObjectError;
        }
        if main.is_null() {
            return kAudioHardwareNoError;
        }
        // SAFETY: the cycle info pointer is either null or valid for the call.
        let cycle = unsafe { cycle.as_ref() };
        let len = frames as usize * CHANNELS as usize;
        match (op, stream) {
            (kAudioServerPlugInIOOperationWriteMix, STREAM_ID) => {
                // SAFETY: the HAL provides `frames` frames of our virtual format (f32 × CHANNELS).
                let samples = unsafe { std::slice::from_raw_parts(main as *const f32, len) };
                write_mix(samples, cycle.map(|c| c.mOutputTime.mSampleTime));
            }
            (kAudioServerPlugInIOOperationReadInput, INPUT_STREAM_ID) => {
                // SAFETY: as above; the buffer is ours to fill for this operation.
                let out = unsafe { std::slice::from_raw_parts_mut(main as *mut f32, len) };
                read_input(out, cycle.map(|c| c.mInputTime.mSampleTime));
            }
            _ => {}
        }
        kAudioHardwareNoError
    })
}

fn read_input(out: &mut [f32], sample_time: Option<f64>) {
    match (STATE.loopback.get(), sample_time) {
        (Some(lb), Some(t)) => lb.read(t, out),
        _ => out.fill(0.0),
    }
}

fn write_mix(samples: &[f32], sample_time: Option<f64>) {
    let s = &STATE;
    if let (Some(lb), Some(t)) = (s.loopback.get(), sample_time) {
        lb.write(t, samples);
    }
    let frames = samples.len() / CHANNELS as usize;
    let expected = f64::from_bits(s.next_sample_bits.load(Relaxed));

    if let Some(ring) = s.ring.get() {
        if ring.consumer_alive() {
            // Keep the recording's timeline intact across HAL overloads: fill skipped
            // cycles (up to half a second) with silence.
            if let Some(t) = sample_time {
                let gap = t - expected;
                if gap >= 1.0 && gap < s.sample_rate() * 0.5 {
                    ring.write_silence(gap as usize * CHANNELS as usize);
                }
            }
            ring.write(samples);
        } else {
            // Lets a consumer whose heartbeat lapsed notice the hole (one atomic add).
            ring.count_skipped(samples.len());
        }
    }
    if let Some(t) = sample_time {
        s.next_sample_bits.store((t + frames as f64).to_bits(), Relaxed);
    }
}

#[cfg(test)]
mod tests;
