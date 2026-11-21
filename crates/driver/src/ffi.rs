//! Hand-written bindings for the parts of CoreFoundation and `AudioServerPlugIn.h` the driver
//! uses. Layouts and constants were checked against the macOS 15 SDK headers.
//!
//! Invariants for everything in this module:
//! * All structs are `#[repr(C)]` mirrors of the SDK definitions; field order matters.
//! * Function-pointer tables must list entries in exactly the header order.

#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types, clippy::upper_case_acronyms, dead_code)]

use std::ffi::c_void;

pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type Boolean = u8;
pub type HRESULT = i32;
pub type ULONG = u32;
pub type pid_t = i32;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFUUIDRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFPropertyListRef = *const c_void;

pub const fn fcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

// ---- COM / CFPlugIn --------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CFUUIDBytes(pub [u8; 16]);

pub const S_OK: HRESULT = 0;
pub const E_NOINTERFACE: HRESULT = 0x8000_0004u32 as i32;

pub const IUnknownUUID: [u8; 16] =
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46];
pub const kAudioServerPlugInTypeUUID: [u8; 16] =
    [0x44, 0x3A, 0xBA, 0xB8, 0xE7, 0xB3, 0x49, 0x1A, 0xB9, 0x85, 0xBE, 0xB9, 0x18, 0x70, 0x30, 0xDB];
pub const kAudioServerPlugInDriverInterfaceUUID: [u8; 16] =
    [0xEE, 0xA5, 0x77, 0x3D, 0xCC, 0x43, 0x49, 0xF1, 0x8E, 0x00, 0x8F, 0x96, 0xE7, 0xD2, 0x3B, 0x17];

// ---- CoreFoundation ----------------------------------------------------------------------

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean;
    pub fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
    pub fn CFUUIDCreateFromUUIDBytes(alloc: CFAllocatorRef, bytes: CFUUIDBytes) -> CFUUIDRef;
    pub fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        num_bytes: isize,
        encoding: u32,
        is_external_representation: Boolean,
    ) -> CFStringRef;
}

/// Creates a +1 retained CFString.
pub fn cfstr(s: &str) -> CFStringRef {
    // SAFETY: bytes/len describe a valid UTF-8 buffer.
    unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, kCFStringEncodingUTF8, 0) }
}

// ---- Core Audio types ----------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: u32,
    pub mScope: u32,
    pub mElement: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct AudioValueRange {
    pub mMinimum: f64,
    pub mMaximum: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct AudioStreamRangedDescription {
    pub mFormat: AudioStreamBasicDescription,
    pub mSampleRateRange: AudioValueRange,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SMPTETime {
    pub mSubframes: i16,
    pub mSubframeDivisor: i16,
    pub mCounter: u32,
    pub mType: u32,
    pub mFlags: u32,
    pub mHours: i16,
    pub mMinutes: i16,
    pub mSeconds: i16,
    pub mFrames: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioTimeStamp {
    pub mSampleTime: f64,
    pub mHostTime: u64,
    pub mRateScalar: f64,
    pub mWordClockTime: u64,
    pub mSMPTETime: SMPTETime,
    pub mFlags: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioServerPlugInIOCycleInfo {
    pub mIOCycleCounter: u64,
    pub mNominalIOBufferFrameSize: u32,
    pub mCurrentTime: AudioTimeStamp,
    pub mInputTime: AudioTimeStamp,
    pub mOutputTime: AudioTimeStamp,
    pub mMainHostTicksPerFrame: f64,
    pub mDeviceHostTicksPerFrame: f64,
}

#[repr(C)]
pub struct AudioServerPlugInClientInfo {
    pub mClientID: u32,
    pub mProcessID: pid_t,
    pub mIsNativeEndian: Boolean,
    pub mBundleID: CFStringRef,
}

#[repr(C)]
pub struct AudioChannelDescription {
    pub mChannelLabel: u32,
    pub mChannelFlags: u32,
    pub mCoordinates: [f32; 3],
}

// Compile-time layout checks against the C sizes.
const _: () = {
    assert!(std::mem::size_of::<AudioStreamBasicDescription>() == 40);
    assert!(std::mem::size_of::<AudioStreamRangedDescription>() == 56);
    assert!(std::mem::size_of::<SMPTETime>() == 24);
    assert!(std::mem::size_of::<AudioTimeStamp>() == 64);
    assert!(std::mem::size_of::<AudioServerPlugInIOCycleInfo>() == 16 + 3 * 64 + 16);
    assert!(std::mem::size_of::<AudioChannelDescription>() == 20);
};

// ---- Host / driver interfaces ----------------------------------------------------------

pub type AudioServerPlugInHostRef = *const AudioServerPlugInHostInterface;
pub type AudioServerPlugInDriverRef = *const *const AudioServerPlugInDriverInterface;

#[repr(C)]
pub struct AudioServerPlugInHostInterface {
    pub PropertiesChanged: unsafe extern "C" fn(
        AudioServerPlugInHostRef,
        AudioObjectID,
        u32,
        *const AudioObjectPropertyAddress,
    ) -> OSStatus,
    pub CopyFromStorage:
        unsafe extern "C" fn(AudioServerPlugInHostRef, CFStringRef, *mut CFPropertyListRef) -> OSStatus,
    pub WriteToStorage: unsafe extern "C" fn(AudioServerPlugInHostRef, CFStringRef, CFPropertyListRef) -> OSStatus,
    pub DeleteFromStorage: unsafe extern "C" fn(AudioServerPlugInHostRef, CFStringRef) -> OSStatus,
    pub RequestDeviceConfigurationChange:
        unsafe extern "C" fn(AudioServerPlugInHostRef, AudioObjectID, u64, *mut c_void) -> OSStatus,
}

type Drv = AudioServerPlugInDriverRef;
type Addr = *const AudioObjectPropertyAddress;

#[repr(C)]
pub struct AudioServerPlugInDriverInterface {
    pub _reserved: *mut c_void,
    pub QueryInterface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
    pub AddRef: unsafe extern "C" fn(*mut c_void) -> ULONG,
    pub Release: unsafe extern "C" fn(*mut c_void) -> ULONG,
    pub Initialize: unsafe extern "C" fn(Drv, AudioServerPlugInHostRef) -> OSStatus,
    pub CreateDevice: unsafe extern "C" fn(
        Drv,
        CFDictionaryRef,
        *const AudioServerPlugInClientInfo,
        *mut AudioObjectID,
    ) -> OSStatus,
    pub DestroyDevice: unsafe extern "C" fn(Drv, AudioObjectID) -> OSStatus,
    pub AddDeviceClient: unsafe extern "C" fn(Drv, AudioObjectID, *const AudioServerPlugInClientInfo) -> OSStatus,
    pub RemoveDeviceClient: unsafe extern "C" fn(Drv, AudioObjectID, *const AudioServerPlugInClientInfo) -> OSStatus,
    pub PerformDeviceConfigurationChange: unsafe extern "C" fn(Drv, AudioObjectID, u64, *mut c_void) -> OSStatus,
    pub AbortDeviceConfigurationChange: unsafe extern "C" fn(Drv, AudioObjectID, u64, *mut c_void) -> OSStatus,
    pub HasProperty: unsafe extern "C" fn(Drv, AudioObjectID, pid_t, Addr) -> Boolean,
    pub IsPropertySettable: unsafe extern "C" fn(Drv, AudioObjectID, pid_t, Addr, *mut Boolean) -> OSStatus,
    pub GetPropertyDataSize:
        unsafe extern "C" fn(Drv, AudioObjectID, pid_t, Addr, u32, *const c_void, *mut u32) -> OSStatus,
    pub GetPropertyData: unsafe extern "C" fn(
        Drv,
        AudioObjectID,
        pid_t,
        Addr,
        u32,
        *const c_void,
        u32,
        *mut u32,
        *mut c_void,
    ) -> OSStatus,
    pub SetPropertyData:
        unsafe extern "C" fn(Drv, AudioObjectID, pid_t, Addr, u32, *const c_void, u32, *const c_void) -> OSStatus,
    pub StartIO: unsafe extern "C" fn(Drv, AudioObjectID, u32) -> OSStatus,
    pub StopIO: unsafe extern "C" fn(Drv, AudioObjectID, u32) -> OSStatus,
    pub GetZeroTimeStamp: unsafe extern "C" fn(Drv, AudioObjectID, u32, *mut f64, *mut u64, *mut u64) -> OSStatus,
    pub WillDoIOOperation:
        unsafe extern "C" fn(Drv, AudioObjectID, u32, u32, *mut Boolean, *mut Boolean) -> OSStatus,
    pub BeginIOOperation: unsafe extern "C" fn(
        Drv,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
    pub DoIOOperation: unsafe extern "C" fn(
        Drv,
        AudioObjectID,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
        *mut c_void,
        *mut c_void,
    ) -> OSStatus,
    pub EndIOOperation: unsafe extern "C" fn(
        Drv,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
}

// SAFETY: the table holds only function pointers and a null `_reserved` field; it is immutable.
unsafe impl Sync for AudioServerPlugInDriverInterface {}

// ---- Constants -----------------------------------------------------------------------------

pub const kAudioHardwareNoError: OSStatus = 0;
pub const kAudioHardwareNotRunningError: OSStatus = fcc(b"stop") as i32;
pub const kAudioHardwareUnspecifiedError: OSStatus = fcc(b"what") as i32;
pub const kAudioHardwareUnknownPropertyError: OSStatus = fcc(b"who?") as i32;
pub const kAudioHardwareBadPropertySizeError: OSStatus = fcc(b"!siz") as i32;
pub const kAudioHardwareIllegalOperationError: OSStatus = fcc(b"nope") as i32;
pub const kAudioHardwareBadObjectError: OSStatus = fcc(b"!obj") as i32;
pub const kAudioHardwareUnsupportedOperationError: OSStatus = fcc(b"unop") as i32;
pub const kAudioDeviceUnsupportedFormatError: OSStatus = fcc(b"!dat") as i32;

pub const kAudioObjectUnknown: AudioObjectID = 0;
pub const kAudioObjectPlugInObject: AudioObjectID = 1;

pub const kAudioObjectPropertyScopeGlobal: u32 = fcc(b"glob");
pub const kAudioObjectPropertyScopeInput: u32 = fcc(b"inpt");
pub const kAudioObjectPropertyScopeOutput: u32 = fcc(b"outp");
pub const kAudioObjectPropertyScopeWildcard: u32 = fcc(b"****");
pub const kAudioObjectPropertyElementMain: u32 = 0;

// Classes
pub const kAudioObjectClassID: u32 = fcc(b"aobj");
pub const kAudioPlugInClassID: u32 = fcc(b"aplg");
pub const kAudioDeviceClassID: u32 = fcc(b"adev");
pub const kAudioStreamClassID: u32 = fcc(b"astr");
pub const kAudioLevelControlClassID: u32 = fcc(b"levl");
pub const kAudioVolumeControlClassID: u32 = fcc(b"vlme");
pub const kAudioBooleanControlClassID: u32 = fcc(b"togl");
pub const kAudioMuteControlClassID: u32 = fcc(b"mute");

// AudioObject
pub const kAudioObjectPropertyBaseClass: u32 = fcc(b"bcls");
pub const kAudioObjectPropertyClass: u32 = fcc(b"clas");
pub const kAudioObjectPropertyOwner: u32 = fcc(b"stdv");
pub const kAudioObjectPropertyName: u32 = fcc(b"lnam");
pub const kAudioObjectPropertyModelName: u32 = fcc(b"lmod");
pub const kAudioObjectPropertyManufacturer: u32 = fcc(b"lmak");
pub const kAudioObjectPropertyElementName: u32 = fcc(b"lchn");
pub const kAudioObjectPropertyOwnedObjects: u32 = fcc(b"ownd");
pub const kAudioObjectPropertyIdentify: u32 = fcc(b"iden");
pub const kAudioObjectPropertySerialNumber: u32 = fcc(b"snum");
pub const kAudioObjectPropertyFirmwareVersion: u32 = fcc(b"fwvn");
pub const kAudioObjectPropertyControlList: u32 = fcc(b"ctrl");
pub const kAudioObjectPropertyCustomPropertyInfoList: u32 = fcc(b"cust");

// PlugIn
pub const kAudioPlugInPropertyDeviceList: u32 = fcc(b"dev#");
pub const kAudioPlugInPropertyTranslateUIDToDevice: u32 = fcc(b"uidd");
pub const kAudioPlugInPropertyBoxList: u32 = fcc(b"box#");
pub const kAudioPlugInPropertyResourceBundle: u32 = fcc(b"rsrc");

// Device
pub const kAudioDevicePropertyConfigurationApplication: u32 = fcc(b"capp");
pub const kAudioDevicePropertyDeviceUID: u32 = fcc(b"uid ");
pub const kAudioDevicePropertyModelUID: u32 = fcc(b"muid");
pub const kAudioDevicePropertyTransportType: u32 = fcc(b"tran");
pub const kAudioDevicePropertyRelatedDevices: u32 = fcc(b"akin");
pub const kAudioDevicePropertyClockDomain: u32 = fcc(b"clkd");
pub const kAudioDevicePropertyDeviceIsAlive: u32 = fcc(b"livn");
pub const kAudioDevicePropertyDeviceIsRunning: u32 = fcc(b"goin");
pub const kAudioDevicePropertyDeviceCanBeDefaultDevice: u32 = fcc(b"dflt");
pub const kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: u32 = fcc(b"sflt");
pub const kAudioDevicePropertyLatency: u32 = fcc(b"ltnc");
pub const kAudioDevicePropertyStreams: u32 = fcc(b"stm#");
pub const kAudioDevicePropertySafetyOffset: u32 = fcc(b"saft");
pub const kAudioDevicePropertyNominalSampleRate: u32 = fcc(b"nsrt");
pub const kAudioDevicePropertyAvailableNominalSampleRates: u32 = fcc(b"nsr#");
pub const kAudioDevicePropertyIsHidden: u32 = fcc(b"hidn");
pub const kAudioDevicePropertyPreferredChannelsForStereo: u32 = fcc(b"dch2");
pub const kAudioDevicePropertyPreferredChannelLayout: u32 = fcc(b"srnd");
pub const kAudioDevicePropertyZeroTimeStampPeriod: u32 = fcc(b"ring");
pub const kAudioDevicePropertyClockIsStable: u32 = fcc(b"cstb");
pub const kAudioDeviceTransportTypeVirtual: u32 = fcc(b"virt");

// Stream
pub const kAudioStreamPropertyIsActive: u32 = fcc(b"sact");
pub const kAudioStreamPropertyDirection: u32 = fcc(b"sdir");
pub const kAudioStreamPropertyTerminalType: u32 = fcc(b"term");
pub const kAudioStreamPropertyStartingChannel: u32 = fcc(b"schn");
pub const kAudioStreamPropertyLatency: u32 = kAudioDevicePropertyLatency;
pub const kAudioStreamPropertyVirtualFormat: u32 = fcc(b"sfmt");
pub const kAudioStreamPropertyAvailableVirtualFormats: u32 = fcc(b"sfma");
pub const kAudioStreamPropertyPhysicalFormat: u32 = fcc(b"pft ");
pub const kAudioStreamPropertyAvailablePhysicalFormats: u32 = fcc(b"pfta");
pub const kAudioStreamTerminalTypeSpeaker: u32 = fcc(b"spkr");

// Controls
pub const kAudioControlPropertyScope: u32 = fcc(b"cscp");
pub const kAudioControlPropertyElement: u32 = fcc(b"celm");
pub const kAudioLevelControlPropertyScalarValue: u32 = fcc(b"lcsv");
pub const kAudioLevelControlPropertyDecibelValue: u32 = fcc(b"lcdv");
pub const kAudioLevelControlPropertyDecibelRange: u32 = fcc(b"lcdr");
pub const kAudioLevelControlPropertyConvertScalarToDecibels: u32 = fcc(b"lcsd");
pub const kAudioLevelControlPropertyConvertDecibelsToScalar: u32 = fcc(b"lcds");
pub const kAudioBooleanControlPropertyValue: u32 = fcc(b"bcvl");

// Formats
pub const kAudioFormatLinearPCM: u32 = fcc(b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;
pub const kAudioFormatFlagsNativeFloatPacked: u32 = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;

pub const kAudioChannelLayoutTag_UseChannelDescriptions: u32 = 0;
pub const kAudioChannelLabel_Left: u32 = 1;
pub const kAudioChannelLabel_Right: u32 = 2;
pub const kAudioChannelLabel_Mono: u32 = 42;
pub const kAudioChannelLabel_Discrete_0: u32 = 1 << 16;

// IO operations
pub const kAudioServerPlugInIOOperationWriteMix: u32 = fcc(b"rite");
