//! In-process tests that drive the plug-in through its vtable exactly like the HAL would,
//! with a fake host. The driver is a global singleton, so everything runs in one test.

use super::*;
use spool_ring::Consumer;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

/// Counts allocations made on a thread while it is "armed", to prove the IO path never
/// allocates (DESIGN.md §5). Other threads, like the rate-change helper, are not counted.
struct CountingAlloc;
static RT_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            RT_ALLOCATIONS.fetch_add(1, Relaxed);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Runs `f` and fails if it allocated on this thread.
fn no_alloc<R>(what: &str, f: impl FnOnce() -> R) -> R {
    let before = RT_ALLOCATIONS.load(Relaxed);
    ARMED.with(|a| a.set(true));
    let r = f();
    ARMED.with(|a| a.set(false));
    assert_eq!(RT_ALLOCATIONS.load(Relaxed), before, "{what} allocated on the IO path");
    r
}

static NOTIFICATIONS: StdMutex<Vec<(AudioObjectID, u32)>> = StdMutex::new(Vec::new());

unsafe extern "C" fn host_properties_changed(
    _h: AudioServerPlugInHostRef,
    obj: AudioObjectID,
    n: u32,
    addrs: *const AudioObjectPropertyAddress,
) -> OSStatus {
    let addrs = unsafe { std::slice::from_raw_parts(addrs, n as usize) };
    let mut v = NOTIFICATIONS.lock().unwrap();
    v.extend(addrs.iter().map(|a| (obj, a.mSelector)));
    0
}
unsafe extern "C" fn host_copy(_h: AudioServerPlugInHostRef, _k: CFStringRef, out: *mut CFPropertyListRef) -> OSStatus {
    unsafe { *out = std::ptr::null() };
    0
}
unsafe extern "C" fn host_write(_h: AudioServerPlugInHostRef, _k: CFStringRef, _d: CFPropertyListRef) -> OSStatus {
    0
}
unsafe extern "C" fn host_delete(_h: AudioServerPlugInHostRef, _k: CFStringRef) -> OSStatus {
    0
}
/// Like the real HAL: stop IO, perform the change, restart IO.
unsafe extern "C" fn host_request_change(
    _h: AudioServerPlugInHostRef,
    dev: AudioObjectID,
    action: u64,
    info: *mut c_void,
) -> OSStatus {
    let d = driver();
    unsafe { ((**d).PerformDeviceConfigurationChange)(d, dev, action, info) }
}

static HOST: AudioServerPlugInHostInterface = AudioServerPlugInHostInterface {
    PropertiesChanged: host_properties_changed,
    CopyFromStorage: host_copy,
    WriteToStorage: host_write,
    DeleteFromStorage: host_delete,
    RequestDeviceConfigurationChange: host_request_change,
};

fn driver() -> AudioServerPlugInDriverRef {
    let uuid = unsafe { CFUUIDCreateFromUUIDBytes(std::ptr::null(), CFUUIDBytes(kAudioServerPlugInTypeUUID)) };
    let p = unsafe { SpoolDriverFactory(std::ptr::null(), uuid) };
    unsafe { CFRelease(uuid) };
    assert!(!p.is_null());
    p as AudioServerPlugInDriverRef
}

fn vt() -> &'static AudioServerPlugInDriverInterface {
    unsafe { &**driver() }
}

fn addr(sel: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: sel, mScope: scope, mElement: 0 }
}
fn glob(sel: u32) -> AudioObjectPropertyAddress {
    addr(sel, kAudioObjectPropertyScopeGlobal)
}

fn get_raw(obj: AudioObjectID, a: AudioObjectPropertyAddress, q: Option<CFStringRef>) -> Result<Vec<u8>, OSStatus> {
    let (qs, qd) = match &q {
        Some(s) => (std::mem::size_of::<CFStringRef>() as u32, s as *const CFStringRef as *const c_void),
        None => (0, std::ptr::null()),
    };
    let mut size = 0u32;
    let e = unsafe { (vt().GetPropertyDataSize)(driver(), obj, 0, &a, qs, qd, &mut size) };
    if e != 0 {
        return Err(e);
    }
    let mut buf = vec![0u8; size as usize];
    let mut got = 0u32;
    let e = unsafe { (vt().GetPropertyData)(driver(), obj, 0, &a, qs, qd, size, &mut got, buf.as_mut_ptr().cast()) };
    if e != 0 {
        return Err(e);
    }
    assert_eq!(got, size);
    Ok(buf)
}

fn get<T: Copy>(obj: AudioObjectID, a: AudioObjectPropertyAddress) -> T {
    let b = get_raw(obj, a, None).unwrap_or_else(|e| panic!("get {:?} on {obj}: {e}", a));
    assert_eq!(b.len(), std::mem::size_of::<T>());
    unsafe { std::ptr::read_unaligned(b.as_ptr() as *const T) }
}

fn get_list<T: Copy>(obj: AudioObjectID, a: AudioObjectPropertyAddress) -> Vec<T> {
    let b = get_raw(obj, a, None).unwrap();
    assert_eq!(b.len() % std::mem::size_of::<T>(), 0);
    b.chunks_exact(std::mem::size_of::<T>()).map(|c| unsafe { std::ptr::read_unaligned(c.as_ptr() as *const T) }).collect()
}

fn get_string_eq(obj: AudioObjectID, sel: u32, want: &str) {
    let s: CFStringRef = get(obj, glob(sel));
    let w = cfstr(want);
    assert!(unsafe { CFEqual(s, w) } != 0, "string property {sel:#x} on {obj} != {want}");
    unsafe {
        CFRelease(s);
        CFRelease(w);
    }
}

fn set<T: Copy>(obj: AudioObjectID, a: AudioObjectPropertyAddress, v: T) -> OSStatus {
    unsafe {
        (vt().SetPropertyData)(
            driver(),
            obj,
            0,
            &a,
            0,
            std::ptr::null(),
            std::mem::size_of::<T>() as u32,
            &v as *const T as *const c_void,
        )
    }
}

fn write_mix(samples: &mut [f32], sample_time: f64) -> OSStatus {
    let frames = (samples.len() / CHANNELS as usize) as u32;
    let mut cycle = AudioServerPlugInIOCycleInfo::default();
    cycle.mOutputTime.mSampleTime = sample_time;
    unsafe {
        (vt().DoIOOperation)(
            driver(),
            DEVICE_ID,
            STREAM_ID,
            1,
            kAudioServerPlugInIOOperationWriteMix,
            frames,
            &cycle,
            samples.as_mut_ptr().cast(),
            std::ptr::null_mut(),
        )
    }
}

fn do_io(op: u32, stream: AudioObjectID, buf: &mut [f32], output_time: f64, input_time: f64) -> OSStatus {
    let frames = (buf.len() / CHANNELS as usize) as u32;
    let mut cycle = AudioServerPlugInIOCycleInfo::default();
    cycle.mOutputTime.mSampleTime = output_time;
    cycle.mInputTime.mSampleTime = input_time;
    unsafe {
        (vt().DoIOOperation)(driver(), DEVICE_ID, stream, 1, op, frames, &cycle, buf.as_mut_ptr().cast(), std::ptr::null_mut())
    }
}

/// ReadInput on the input stream at `t`, checked to be allocation-free. The buffer starts as
/// NaN so a frame the driver failed to fill is caught.
fn read_input(frames: usize, t: f64) -> Vec<f32> {
    let mut buf = vec![f32::NAN; frames * CHANNELS as usize];
    let st = no_alloc("ReadInput", || do_io(kAudioServerPlugInIOOperationReadInput, INPUT_STREAM_ID, &mut buf, 0.0, t));
    assert_eq!(st, 0);
    buf
}

fn read_all(c: &mut Consumer) -> Vec<f32> {
    let mut out = vec![0.0; 1 << 16];
    let mut all = Vec::new();
    loop {
        let ch = c.read(&mut out);
        if ch.samples == 0 {
            return all;
        }
        all.extend_from_slice(&out[..ch.samples]);
    }
}

#[test]
fn driver_behaves_like_a_hal_device() {
    let _ = spool_ring::unlink(&ring_name());
    let d = driver();

    // Factory rejects unknown plug-in types.
    let other = unsafe { CFUUIDCreateFromUUIDBytes(std::ptr::null(), CFUUIDBytes([7; 16])) };
    assert!(unsafe { SpoolDriverFactory(std::ptr::null(), other) }.is_null());
    unsafe { CFRelease(other) };

    // COM plumbing.
    let mut out: *mut c_void = std::ptr::null_mut();
    let iid = CFUUIDBytes(kAudioServerPlugInDriverInterfaceUUID);
    assert_eq!(unsafe { (vt().QueryInterface)(d as _, iid, &mut out) }, S_OK);
    assert_eq!(out, d as *mut c_void);
    assert_eq!(unsafe { (vt().QueryInterface)(d as _, CFUUIDBytes([1; 16]), &mut out) }, E_NOINTERFACE);

    assert_eq!(unsafe { (vt().Initialize)(d, &HOST) }, 0);

    // Object tree.
    assert_eq!(get_list::<u32>(PLUGIN_ID, glob(kAudioPlugInPropertyDeviceList)), vec![DEVICE_ID]);
    assert_eq!(get::<u32>(DEVICE_ID, glob(kAudioObjectPropertyClass)), kAudioDeviceClassID);
    assert_eq!(get::<u32>(DEVICE_ID, glob(kAudioObjectPropertyOwner)), PLUGIN_ID);
    // Scope-correct children: global lists everything, each scope only its own objects.
    let input = kAudioObjectPropertyScopeInput;
    let output = kAudioObjectPropertyScopeOutput;
    assert_eq!(
        get_list::<u32>(DEVICE_ID, glob(kAudioObjectPropertyOwnedObjects)),
        vec![STREAM_ID, VOLUME_ID, MUTE_ID, INPUT_STREAM_ID]
    );
    assert_eq!(get_list::<u32>(DEVICE_ID, addr(kAudioObjectPropertyOwnedObjects, input)), vec![INPUT_STREAM_ID]);
    assert_eq!(get_list::<u32>(DEVICE_ID, addr(kAudioObjectPropertyOwnedObjects, output)), vec![STREAM_ID, VOLUME_ID, MUTE_ID]);
    assert_eq!(get_list::<u32>(DEVICE_ID, glob(kAudioDevicePropertyStreams)), vec![STREAM_ID, INPUT_STREAM_ID]);
    assert_eq!(get_list::<u32>(DEVICE_ID, addr(kAudioDevicePropertyStreams, input)), vec![INPUT_STREAM_ID]);
    assert_eq!(get_list::<u32>(DEVICE_ID, addr(kAudioDevicePropertyStreams, output)), vec![STREAM_ID]);
    for scope in [input, output] {
        assert_eq!(get::<u32>(DEVICE_ID, addr(kAudioDevicePropertyDeviceCanBeDefaultDevice, scope)), 1);
        assert_eq!(get::<u32>(DEVICE_ID, addr(kAudioDevicePropertyLatency, scope)), 0);
        assert_eq!(get::<u32>(DEVICE_ID, addr(kAudioDevicePropertySafetyOffset, scope)), 0);
    }
    assert_eq!(get::<u32>(STREAM_ID, glob(kAudioStreamPropertyDirection)), 0);
    assert_eq!(get::<u32>(STREAM_ID, glob(kAudioStreamPropertyTerminalType)), kAudioStreamTerminalTypeSpeaker);

    // Input stream: direction 1, line terminal, owned by the device, same format as the output.
    assert_eq!(get::<u32>(INPUT_STREAM_ID, glob(kAudioObjectPropertyClass)), kAudioStreamClassID);
    assert_eq!(get::<u32>(INPUT_STREAM_ID, glob(kAudioObjectPropertyOwner)), DEVICE_ID);
    assert_eq!(get::<u32>(INPUT_STREAM_ID, glob(kAudioStreamPropertyDirection)), 1);
    assert_eq!(get::<u32>(INPUT_STREAM_ID, glob(kAudioStreamPropertyTerminalType)), kAudioStreamTerminalTypeLine);
    assert_eq!(get::<u32>(INPUT_STREAM_ID, glob(kAudioStreamPropertyStartingChannel)), 1);
    assert_eq!(get::<u32>(INPUT_STREAM_ID, glob(kAudioStreamPropertyLatency)), 0);
    for sel in [kAudioStreamPropertyVirtualFormat, kAudioStreamPropertyPhysicalFormat] {
        assert_eq!(
            get::<AudioStreamBasicDescription>(INPUT_STREAM_ID, glob(sel)),
            get::<AudioStreamBasicDescription>(STREAM_ID, glob(sel))
        );
    }
    assert_eq!(
        get_list::<AudioStreamRangedDescription>(INPUT_STREAM_ID, glob(kAudioStreamPropertyAvailableVirtualFormats)).len(),
        SAMPLE_RATES.len()
    );
    // IsActive is per stream.
    assert_eq!(set(INPUT_STREAM_ID, glob(kAudioStreamPropertyIsActive), 0u32), 0);
    assert_eq!(get::<u32>(INPUT_STREAM_ID, glob(kAudioStreamPropertyIsActive)), 0);
    assert_eq!(get::<u32>(STREAM_ID, glob(kAudioStreamPropertyIsActive)), 1);
    assert_eq!(set(INPUT_STREAM_ID, glob(kAudioStreamPropertyIsActive), 1u32), 0);
    assert!(NOTIFICATIONS.lock().unwrap().contains(&(INPUT_STREAM_ID, kAudioStreamPropertyIsActive)));
    // A format the device does not offer is refused on the input stream too.
    let mut odd = get::<AudioStreamBasicDescription>(INPUT_STREAM_ID, glob(kAudioStreamPropertyVirtualFormat));
    odd.mChannelsPerFrame = 7;
    assert_eq!(set(INPUT_STREAM_ID, glob(kAudioStreamPropertyVirtualFormat), odd), kAudioDeviceUnsupportedFormatError);
    assert_eq!(get::<u32>(VOLUME_ID, glob(kAudioObjectPropertyClass)), kAudioVolumeControlClassID);
    assert_eq!(get::<u32>(VOLUME_ID, glob(kAudioObjectPropertyBaseClass)), kAudioLevelControlClassID);
    assert_eq!(get::<u32>(MUTE_ID, glob(kAudioObjectPropertyClass)), kAudioMuteControlClassID);
    get_string_eq(DEVICE_ID, kAudioObjectPropertyName, DEVICE_NAME);
    get_string_eq(DEVICE_ID, kAudioDevicePropertyDeviceUID, DEVICE_UID);
    get_string_eq(PLUGIN_ID, kAudioObjectPropertyManufacturer, MANUFACTURER);

    // UID translation uses the qualifier.
    let uid = cfstr(DEVICE_UID);
    let b = get_raw(PLUGIN_ID, glob(kAudioPlugInPropertyTranslateUIDToDevice), Some(uid)).unwrap();
    assert_eq!(u32::from_ne_bytes(b.try_into().unwrap()), DEVICE_ID);
    unsafe { CFRelease(uid) };
    let nope = cfstr("nope");
    let b = get_raw(PLUGIN_ID, glob(kAudioPlugInPropertyTranslateUIDToDevice), Some(nope)).unwrap();
    assert_eq!(u32::from_ne_bytes(b.try_into().unwrap()), kAudioObjectUnknown);
    unsafe { CFRelease(nope) };

    // Formats.
    let f: AudioStreamBasicDescription = get(STREAM_ID, glob(kAudioStreamPropertyVirtualFormat));
    assert_eq!((f.mSampleRate, f.mChannelsPerFrame, f.mBitsPerChannel, f.mBytesPerFrame), (48000.0, 2, 32, 8));
    let fs: Vec<AudioStreamRangedDescription> = get_list(STREAM_ID, glob(kAudioStreamPropertyAvailablePhysicalFormats));
    assert_eq!(fs.len(), SAMPLE_RATES.len());
    let rates: Vec<AudioValueRange> = get_list(DEVICE_ID, glob(kAudioDevicePropertyAvailableNominalSampleRates));
    assert_eq!(rates.iter().map(|r| r.mMinimum).collect::<Vec<_>>(), SAMPLE_RATES);
    let layout = get_raw(DEVICE_ID, addr(kAudioDevicePropertyPreferredChannelLayout, kAudioObjectPropertyScopeOutput), None).unwrap();
    assert_eq!(layout.len(), 12 + 20 * CHANNELS as usize);

    // Lists are truncated to what fits; scalars reject short buffers.
    let a = glob(kAudioObjectPropertyOwnedObjects);
    let mut buf = [0u32; 1];
    let mut got = 0;
    assert_eq!(unsafe { (vt().GetPropertyData)(d, DEVICE_ID, 0, &a, 0, std::ptr::null(), 4, &mut got, buf.as_mut_ptr().cast()) }, 0);
    assert_eq!((got, buf[0]), (4, STREAM_ID));
    let a = glob(kAudioDevicePropertyNominalSampleRate);
    assert_eq!(
        unsafe { (vt().GetPropertyData)(d, DEVICE_ID, 0, &a, 0, std::ptr::null(), 4, &mut got, buf.as_mut_ptr().cast()) },
        kAudioHardwareBadPropertySizeError
    );

    // Unknown properties and objects.
    let bogus = glob(fcc(b"zzzz"));
    assert_eq!(unsafe { (vt().HasProperty)(d, DEVICE_ID, 0, &bogus) }, 0);
    assert_eq!(unsafe { (vt().HasProperty)(d, DEVICE_ID, 0, &glob(kAudioObjectPropertyName)) }, 1);
    assert_eq!(unsafe { (vt().HasProperty)(d, 99, 0, &glob(kAudioObjectPropertyName)) }, 0);
    let mut settable = 9u8;
    assert_eq!(unsafe { (vt().IsPropertySettable)(d, DEVICE_ID, 0, &glob(kAudioObjectPropertyName), &mut settable) }, 0);
    assert_eq!(settable, 0);
    assert_eq!(unsafe { (vt().IsPropertySettable)(d, VOLUME_ID, 0, &glob(kAudioLevelControlPropertyScalarValue), &mut settable) }, 0);
    assert_eq!(settable, 1);
    assert_eq!(set(DEVICE_ID, glob(kAudioObjectPropertyName), 0u32), kAudioHardwareUnsupportedOperationError);

    // Volume and mute: values, conversions, notifications.
    assert_eq!(set(VOLUME_ID, glob(kAudioLevelControlPropertyScalarValue), 0.5f32), 0);
    assert_eq!(get::<f32>(VOLUME_ID, glob(kAudioLevelControlPropertyScalarValue)), 0.5);
    let db = get::<f32>(VOLUME_ID, glob(kAudioLevelControlPropertyDecibelValue));
    assert!((db - scalar_to_db(0.5)).abs() < 1e-4);
    assert_eq!(set(VOLUME_ID, glob(kAudioLevelControlPropertyDecibelValue), MAX_DB), 0);
    assert_eq!(get::<f32>(VOLUME_ID, glob(kAudioLevelControlPropertyScalarValue)), 1.0);
    // Non-finite values are rejected (f32::clamp would pass NaN through).
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(set(VOLUME_ID, glob(kAudioLevelControlPropertyScalarValue), bad), kAudioHardwareIllegalOperationError);
        assert_eq!(set(VOLUME_ID, glob(kAudioLevelControlPropertyDecibelValue), bad), kAudioHardwareIllegalOperationError);
    }
    assert_eq!(get::<f32>(VOLUME_ID, glob(kAudioLevelControlPropertyScalarValue)), 1.0);
    assert_eq!(scalar_to_db(f32::NAN), MIN_DB);
    assert_eq!(db_to_scalar(f32::NAN), 0.0);
    let mut io = -20.0f32;
    let mut got = 0;
    let a = glob(kAudioLevelControlPropertyConvertDecibelsToScalar);
    assert_eq!(unsafe { (vt().GetPropertyData)(d, VOLUME_ID, 0, &a, 0, std::ptr::null(), 4, &mut got, (&mut io as *mut f32).cast()) }, 0);
    assert!((scalar_to_db(io) - -20.0).abs() < 1e-3);
    assert_eq!(set(MUTE_ID, glob(kAudioBooleanControlPropertyValue), 1u32), 0);
    assert_eq!(get::<u32>(MUTE_ID, glob(kAudioBooleanControlPropertyValue)), 1);
    {
        let n = NOTIFICATIONS.lock().unwrap();
        assert!(n.contains(&(VOLUME_ID, kAudioLevelControlPropertyScalarValue)));
        assert!(n.contains(&(MUTE_ID, kAudioBooleanControlPropertyValue)));
    }
    let mut consumer = Consumer::open(&ring_name()).expect("driver created the ring");
    assert!(consumer.stats().muted);
    assert_eq!(set(MUTE_ID, glob(kAudioBooleanControlPropertyValue), 0u32), 0);
    assert!(!consumer.stats().muted);

    // IO: WriteMix data reaches the ring bit-exact.
    let mut will = 0u8;
    let mut inplace = 0u8;
    unsafe { (vt().WillDoIOOperation)(d, DEVICE_ID, 1, kAudioServerPlugInIOOperationWriteMix, &mut will, &mut inplace) };
    assert_eq!(will, 1);
    unsafe { (vt().WillDoIOOperation)(d, DEVICE_ID, 1, kAudioServerPlugInIOOperationReadInput, &mut will, &mut inplace) };
    assert_eq!((will, inplace), (1, 1));
    unsafe { (vt().WillDoIOOperation)(d, DEVICE_ID, 1, fcc(b"mixo"), &mut will, &mut inplace) };
    assert_eq!(will, 0);

    assert_eq!(unsafe { (vt().StartIO)(d, DEVICE_ID, 1) }, 0);
    assert_eq!(get::<u32>(DEVICE_ID, glob(kAudioDevicePropertyDeviceIsRunning)), 1);
    assert!(consumer.stats().running);
    let mut expected = Vec::new();
    let mut t = 1000.0;
    for cycle in 0..20 {
        let mut block: Vec<f32> = (0..512 * 2).map(|i| (cycle * 10_000 + i) as f32 * 1e-5).collect();
        expected.extend_from_slice(&block);
        assert_eq!(write_mix(&mut block, t), 0);
        t += 512.0;
    }
    assert_eq!(read_all(&mut consumer), expected);

    // Loopback: what was played at output time T comes out of the input at input time T,
    // bit-exact, and neither operation allocates.
    let base = 1000.0;
    let frames = 512;
    for cycle in 0..20 {
        let got = read_input(frames, base + (cycle * frames) as f64);
        let want = &expected[cycle * frames * 2..(cycle + 1) * frames * 2];
        assert_eq!(got, want, "loopback cycle {cycle}");
    }
    // Zero-on-read: reading the same span again gives silence, not a repeat.
    assert!(read_input(frames, base).iter().all(|&s| s == 0.0));
    // Never written: silence.
    assert!(read_input(256, 40_000.0).iter().all(|&s| s == 0.0));
    // The output leads the input: several cycles in flight, buffer sizes the HAL may use.
    let mut t2 = 20_000.0;
    let mut sent = Vec::new();
    for (i, n) in [32usize, 4096, 333, 128, 1].into_iter().enumerate() {
        let mut block: Vec<f32> = (0..n * 2).map(|j| (i * 7919 + j) as f32 * 1e-6).collect();
        sent.push((t2, block.clone()));
        let st = no_alloc("WriteMix", || do_io(kAudioServerPlugInIOOperationWriteMix, STREAM_ID, &mut block, t2, 0.0));
        assert_eq!(st, 0);
        t2 += n as f64;
    }
    let _ = read_all(&mut consumer); // the ring tap got the same frames
    for (t, block) in &sent {
        assert_eq!(&read_input(block.len() / 2, *t), block, "frames at {t}");
    }
    // Pre-fader: volume and mute do not change what apps reading Spool receive.
    assert_eq!(set(VOLUME_ID, glob(kAudioLevelControlPropertyScalarValue), 0.25f32), 0);
    assert_eq!(set(MUTE_ID, glob(kAudioBooleanControlPropertyValue), 1u32), 0);
    let mut block = vec![0.75f32; 64];
    write_mix(&mut block, 30_000.0);
    assert_eq!(read_input(32, 30_000.0), block);
    assert_eq!(set(MUTE_ID, glob(kAudioBooleanControlPropertyValue), 0u32), 0);
    assert_eq!(set(VOLUME_ID, glob(kAudioLevelControlPropertyScalarValue), 1.0f32), 0);
    let _ = read_all(&mut consumer);
    // Wrong stream for the operation: ignored, buffer untouched. Unknown stream: refused.
    let mut buf = vec![5.0f32; 8];
    assert_eq!(do_io(kAudioServerPlugInIOOperationReadInput, STREAM_ID, &mut buf, 0.0, 30_000.0), 0);
    assert_eq!(do_io(kAudioServerPlugInIOOperationWriteMix, INPUT_STREAM_ID, &mut buf, 31_000.0, 0.0), 0);
    assert!(buf.iter().all(|&s| s == 5.0));
    assert!(read_input(4, 31_000.0).iter().all(|&s| s == 0.0));
    assert_eq!(do_io(kAudioServerPlugInIOOperationReadInput, 99, &mut buf, 0.0, 0.0), kAudioHardwareBadObjectError);
    // Null cycle info on ReadInput: silence instead of a guess.
    let mut buf = vec![5.0f32; 8];
    let st = unsafe {
        (vt().DoIOOperation)(d, DEVICE_ID, INPUT_STREAM_ID, 1, kAudioServerPlugInIOOperationReadInput, 4, std::ptr::null(), buf.as_mut_ptr().cast(), std::ptr::null_mut())
    };
    assert_eq!(st, 0);
    assert!(buf.iter().all(|&s| s == 0.0));

    // A skipped cycle is filled with silence so the recording keeps its timeline.
    let mut a1 = vec![1.0f32; 64];
    let mut a2 = vec![2.0f32; 64];
    write_mix(&mut a1, t);
    write_mix(&mut a2, t + 32.0 + 100.0); // 100 frames skipped
    let got = read_all(&mut consumer);
    assert_eq!(got.len(), 64 + 200 + 64);
    assert!(got[..64].iter().all(|&s| s == 1.0));
    assert!(got[64..264].iter().all(|&s| s == 0.0));
    assert!(got[264..].iter().all(|&s| s == 2.0));

    // Zero time stamps advance in whole periods and are monotonic.
    let (mut prev_s, mut prev_h) = (-1.0, 0);
    for _ in 0..5 {
        let (mut st, mut ht, mut seed) = (0.0, 0u64, 0u64);
        assert_eq!(unsafe { (vt().GetZeroTimeStamp)(d, DEVICE_ID, 1, &mut st, &mut ht, &mut seed) }, 0);
        assert_eq!(st % ZERO_TS_PERIOD as f64, 0.0);
        assert!(st >= prev_s && ht >= prev_h);
        assert_eq!(seed, 1);
        (prev_s, prev_h) = (st, ht);
    }
    let anchor = STATE.anchor_host_time.load(Relaxed);
    let one_sec = spool_ring::ns_to_ticks(1_000_000_000);
    // One second at 48 kHz = 2.93 periods of 16384 frames -> the last whole period is #2.
    let (s1, h1) = zero_time_stamp(anchor + one_sec);
    assert_eq!(s1, 2.0 * ZERO_TS_PERIOD as f64);
    assert!(h1 > anchor && h1 <= anchor + one_sec);

    // A reader that joins while audio plays and keeps reading after it stops hears silence past
    // the last write, not the unread frames from one wrap earlier (docs/g1-stale-audio.html).
    // IO stays running throughout, so StartIO's clear doesn't help here.
    let cap = loopback::CAPACITY_FRAMES as f64;
    let first = 200_000.0;
    let mut t = first;
    while t < first + 2.0 * cap {
        let mut block = vec![0.125f32; 1024];
        let st = no_alloc("WriteMix", || do_io(kAudioServerPlugInIOOperationWriteMix, STREAM_ID, &mut block, t, 0.0));
        assert_eq!(st, 0);
        t += 512.0;
    }
    let _ = read_all(&mut consumer);
    let last = t;
    assert_eq!(read_input(512, last - 512.0), vec![0.125f32; 1024], "live before the writer stops");
    let mut t = last;
    while t < last + cap {
        assert!(read_input(512, t).iter().all(|&s| s == 0.0), "replayed old audio at {t}");
        t += 512.0;
    }
    // A skipped cycle (HAL overload) reads as silence too, and the gap fill doesn't allocate.
    // First a full wrap nobody reads, so every slot holds old audio.
    let mut t = last + cap;
    let wrap_end = t + cap;
    while t < wrap_end {
        assert_eq!(write_mix(&mut vec![0.125f32; 1024], t), 0);
        t += 512.0;
    }
    let mut block = vec![0.25f32; 1024];
    assert_eq!(no_alloc("WriteMix", || do_io(kAudioServerPlugInIOOperationWriteMix, STREAM_ID, &mut block, wrap_end + 512.0, 0.0)), 0);
    assert!(read_input(512, wrap_end).iter().all(|&s| s == 0.0), "skipped cycle replayed old audio");
    assert_eq!(read_input(512, wrap_end + 512.0), vec![0.25f32; 1024]);
    let _ = read_all(&mut consumer);

    // Audio left in the loopback when IO stops does not come out after the next StartIO.
    let mut block = vec![0.5f32; 64];
    write_mix(&mut block, 50_000.0);
    assert_eq!(unsafe { (vt().StopIO)(d, DEVICE_ID, 1) }, 0);
    assert_eq!(unsafe { (vt().StartIO)(d, DEVICE_ID, 1) }, 0);
    assert!(read_input(32, 50_000.0).iter().all(|&s| s == 0.0));
    let _ = read_all(&mut consumer);
    // Audio written before a rate change does not come out at the new rate.
    write_mix(&mut block, 60_000.0);
    let _ = read_all(&mut consumer);

    assert_eq!(unsafe { (vt().StopIO)(d, DEVICE_ID, 1) }, 0);
    assert!(!consumer.stats().running);
    assert_eq!(unsafe { (vt().StopIO)(d, DEVICE_ID, 1) }, kAudioHardwareIllegalOperationError);

    // Sample-rate change goes through the host and is published to the ring.
    assert_eq!(set(DEVICE_ID, glob(kAudioDevicePropertyNominalSampleRate), 12345.0f64), kAudioDeviceUnsupportedFormatError);
    assert_eq!(set(DEVICE_ID, glob(kAudioDevicePropertyNominalSampleRate), 96000.0f64), 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while get::<f64>(DEVICE_ID, glob(kAudioDevicePropertyNominalSampleRate)) != 96000.0 {
        assert!(Instant::now() < deadline, "rate change not performed");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(get::<AudioStreamBasicDescription>(STREAM_ID, glob(kAudioStreamPropertyPhysicalFormat)).mSampleRate, 96000.0);
    assert_eq!(get::<AudioStreamBasicDescription>(INPUT_STREAM_ID, glob(kAudioStreamPropertyPhysicalFormat)).mSampleRate, 96000.0);
    assert!(read_input(32, 60_000.0).iter().all(|&s| s == 0.0), "loopback cleared on rate change");
    // A format set on the input stream changes the rate of both streams.
    let f48 = get::<AudioStreamBasicDescription>(INPUT_STREAM_ID, glob(kAudioStreamPropertyVirtualFormat));
    let want = AudioStreamBasicDescription { mSampleRate: 48000.0, ..f48 };
    assert_eq!(set(INPUT_STREAM_ID, glob(kAudioStreamPropertyVirtualFormat), want), 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while get::<f64>(DEVICE_ID, glob(kAudioDevicePropertyNominalSampleRate)) != 48000.0 {
        assert!(Instant::now() < deadline, "rate change from the input stream not performed");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(get::<AudioStreamBasicDescription>(STREAM_ID, glob(kAudioStreamPropertyVirtualFormat)).mSampleRate, 48000.0);
    assert_eq!(set(DEVICE_ID, glob(kAudioDevicePropertyNominalSampleRate), 96000.0f64), 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while get::<f64>(DEVICE_ID, glob(kAudioDevicePropertyNominalSampleRate)) != 96000.0 {
        assert!(Instant::now() < deadline, "rate change back not performed");
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe { (vt().StartIO)(d, DEVICE_ID, 1) };
    let mut block = vec![0.25f32; 32];
    write_mix(&mut block, 0.0);
    let mut out = vec![0.0; 64];
    let ch = consumer.read(&mut out);
    assert_eq!(ch.format.sample_rate, 96000.0);
    assert_eq!(&out[..ch.samples], &block[..]);
    unsafe { (vt().StopIO)(d, DEVICE_ID, 1) };

    // Producer stops writing once the consumer goes away.
    drop(consumer);
    let before = STATE.ring.get().unwrap().stats().write_pos;
    unsafe { (vt().StartIO)(d, DEVICE_ID, 1) };
    write_mix(&mut block, 100.0);
    unsafe { (vt().StopIO)(d, DEVICE_ID, 1) };
    assert_eq!(STATE.ring.get().unwrap().stats().write_pos, before);

    let _ = spool_ring::unlink(&ring_name());
}

#[test]
fn volume_curve_roundtrips() {
    for s in [0.01f32, 0.1, 0.25, 0.5, 0.75, 1.0] {
        assert!((db_to_scalar(scalar_to_db(s)) - s).abs() < 1e-4, "{s}");
    }
    assert_eq!(scalar_to_db(0.0), MIN_DB);
    assert_eq!(db_to_scalar(MIN_DB), 0.0);
}
