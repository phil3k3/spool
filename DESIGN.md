# Design Document: Virtual Audio Output Device with Built-in Recording (macOS, Rust)

Status: Draft
Platform: macOS (target 13+; process-tap feature requires 14.2+)
Language: Rust

## 1. Goal

Build a virtual audio **output** device for macOS, selectable in Sound settings and in any application's output picker (like BlackHole), that additionally:

- records everything sent to it directly to disk,
- lets the user hear the audio (monitoring) without a Multi-Output Device,
- behaves like a normal output device (volume, mute, media keys),
- is configurable at runtime instead of at compile time.

### Non-goals

- Virtual camera, virtual disk or block device (different mechanisms: CoreMediaIO, FSKit/DriverKit).
- Per-application routing inside the device (see optional extension in 9).
- Kernel extensions. Only an AudioServerPlugIn is used.

## 2. Background: Limitations of BlackHole

BlackHole is the reference loopback driver. Its weaknesses define our feature set.

| Limitation | Our answer |
|---|---|
| No recording to file. Needs a second app to capture. Listed as an unimplemented request upstream. | Built-in recorder daemon (4.2) |
| User cannot hear routed audio. Workaround is a Multi-Output Device, which has no volume control and requires the built-in output to be listed first. | Built-in monitoring with drift correction (4.3) |
| No volume or mute controls on the device. Media keys do nothing. | Volume and mute control objects (4.1) |
| Name, bundle ID, icon, hidden flag and channel count are compile-time constants. Each channel count is a separate driver build. | Runtime configuration (4.4) |
| Clock sync with other devices listed as in development. | Adaptive resampling plus stable timestamps (4.3, 4.5) |
| Root install, `coreaudiod` restart, sometimes manual approval. Often documented with `killall -9`. | Signed, notarized `.pkg` and safe restart (6) |
| GPL license. Commercial use needs a license from the author. | Implement from Apple's documentation and the NullAudio sample, not from BlackHole source (8) |
| Reports of problems on newer macOS (anecdotal, unconfirmed). Many Tahoe audio glitches appear to be `coreaudiod` issues affecting all devices. | Test matrix across macOS versions (7) |

Note: reports of Tahoe-specific breakage should be treated as unconfirmed until reproduced.

## 3. Architecture

Three components.

```
 Apps ──► [ coreaudiod ]
              │  loads
              ▼
   ┌────────────────────────┐        shared memory ring        ┌──────────────────────┐
   │ driver (cdylib)        │ ───────────────────────────────► │ recorder daemon      │
   │ AudioServerPlugIn      │   (lock-free SPSC, mach/XPC      │ (user-space binary)  │
   │ real-time IO only      │    for control + wakeups)        │ encode → disk        │
   └────────────────────────┘                                  │ monitor → real device│
                                                               └──────────────────────┘
                                                                         ▲
                                                               CLI / menu bar control
```

### 3.1 Cargo workspace layout

```
.
├── Cargo.toml                # workspace
├── crates/
│   ├── driver/               # cdylib: AudioServerPlugIn (loaded by coreaudiod)
│   ├── daemon/               # recorder + monitor binary
│   ├── ring/                 # shared lock-free ring buffer + shared-memory protocol
│   ├── proto/                # control protocol types (XPC/Mach messages)
│   └── ctl/                  # CLI for start/stop/config
├── bundle/
│   └── Info.plist            # driver bundle metadata, declared Mach services
├── installer/                # pkg build, signing, notarization, uninstall
└── DESIGN.md
```

### 3.2 Driver (crate `driver`)

- Built as a `cdylib`, packaged as `<Name>.driver` in `/Library/Audio/Plug-Ins/HAL/`.
- Exports the AudioServerPlugIn factory function and implements the `AudioServerPlugInDriverInterface` vtable (hand-written, using `coreaudio-sys` types).
- Publishes one plug-in, one device, one output stream, plus control objects.
- `DoIOOperation` copies frames into the shared ring and returns. Nothing else.

### 3.3 Daemon (crate `daemon`)

- Plain user-space Rust binary, run as a launchd agent.
- Drains the ring, encodes (WAV via `hound`, FLAC/Opus via encoder crates), writes to disk.
- Optionally forwards audio to a physical output device (monitoring).
- Receives control commands over XPC/Mach (start/stop recording, config, monitor device).

## 4. Features

### 4.1 Standard device behavior

- Expose `kAudioVolumeControlClassID` and `kAudioMuteControlClassID` objects on the output stream so system volume and media keys work.
- Apply volume and mute in the driver before writing to the ring (or flag them in metadata so the recorder can choose raw vs. post-fader recording).
- Format: 32-bit float, non-interleaved or interleaved consistently with Core Audio's native format.

### 4.2 Recording to disk

- Driver writes into a lock-free single-producer single-consumer ring in shared memory.
- Daemon reads, encodes, writes. All file I/O, allocation and encoding happen outside the real-time thread.
- Formats: WAV first; FLAC and Opus later.
- Features: start/stop at runtime, file rotation by size or duration, timestamped file names, configurable output directory.
- Overrun handling: if the daemon falls behind, the ring drops oldest or newest frames and increments a counter exposed over the control channel. The driver never blocks.

### 4.3 Monitoring (replaces Multi-Output Device)

- Daemon opens an AUHAL output unit on a user-chosen physical device and plays the ring contents.
- Clock drift between the virtual device and the hardware is corrected by an adaptive resampler (ring fill level drives the resampling ratio, e.g. with `rubato`).
- Latency target and fill level are configurable.
- Result: the user selects only our device as system output and still hears audio, with working volume.

### 4.4 Runtime configuration

- Sample rate, channel count and device name changeable at runtime through custom properties or a config file; the daemon asks the driver to republish the device (`RequestDeviceConfigurationChange`).
- No per-variant builds.
- Supported sample rates: at least 44.1, 48, 88.2, 96, 176.4, 192 kHz.

### 4.5 Timing

- `GetZeroTimeStamp` derived from `mach_absolute_time` with a stable anchor.
- Timeline stays continuous across rate changes and stream restarts.
- Reports a consistent seed so clients do not see time jumps.

## 5. Real-time and Safety Constraints

The driver runs inside `coreaudiod`. A crash or stall there breaks system audio for the whole machine.

- `panic = "abort"` for the driver, or `catch_unwind` at every `extern "C"` boundary. A panic must never unwind into C.
- On the IO thread: no allocation, no locks, no syscalls that can block, no logging, no file or network access.
- Add an allocation-detecting test allocator in CI to fail any allocation on the IO path.
- Keep all `unsafe` isolated in a small FFI layer with documented invariants.
- The plug-in runs in the `coreaudiod` sandbox. Shared memory and Mach/XPC services must be declared in the bundle's `Info.plist` (`AudioServerPlugIn_MachServices`).

## 6. Installation, Signing, Distribution

- Driver bundle installs to `/Library/Audio/Plug-Ins/HAL/`. Requires root.
- Daemon installed as a launchd agent or daemon plist.
- Ship a signed (Developer ID) and notarized `.pkg` with a postinstall step that restarts CoreAudio via `sudo launchctl kickstart -kp system/com.apple.audio.coreaudiod`, falling back to `sudo killall coreaudiod` (SIGTERM; launchd restarts it) where SIP refuses kickstart, as on macOS 27. Avoid recommending `killall -9`.
- Provide a proper uninstaller (removes driver, daemon, launchd plist).
- Document that restarting `coreaudiod` briefly interrupts all audio on the machine.

## 7. Testing

- Unit tests for the ring buffer (loom for concurrency, property tests for wraparound).
- Allocation-free IO-path test (see 5).
- Integration: play known test tones to the device, record, compare samples bit-exact (WAV, 32-bit float).
- Drift test: long runs (hours) with monitoring to a hardware device, assert bounded latency and no xruns.
- Stress: rapid device start/stop, sample-rate changes, many clients, `coreaudiod` restarts.
- Matrix: macOS 13, 14, 15, 26 (Tahoe); Intel and Apple Silicon.
- Validate with Audio MIDI Setup, QuickTime, a DAW, and a browser.

## 8. Licensing and References

- BlackHole is GPL-licensed. To keep this project's license free of that constraint, implement from Apple's documentation and the NullAudio sample driver. Read BlackHole for understanding only.
- References:
  - Apple: Creating an Audio Server Driver Plug-in (AudioServerPlugIn headers and NullAudio sample).
  - BlackHole README and wiki: https://github.com/ExistentialAudio/BlackHole
  - BlackHole discussion #896 (silent-buffer report on macOS 26.6, unconfirmed): https://github.com/ExistentialAudio/BlackHole/discussions/896
  - Apple Community thread on audio glitches after Tahoe: https://discussions.apple.com/thread/256140785
- No mature Rust crate exists for AudioServerPlugIn. Expect hand-written vtables over `coreaudio-sys`.

## 9. Optional Extensions

- **Process taps (macOS 14.2+):** a second input source in the same daemon using Core Audio process taps (`AudioHardwareCreateProcessTap`, via `cidre`), so users can record system or per-app audio without changing their output device.
- **ScreenCaptureKit** as an alternative capture path.
- **Menu bar UI** for start/stop, level meters, monitor device selection.
- **Multiple devices** published from one driver (e.g. stereo and 16-channel).

## 10. Milestones

1. Prototype: Path A recorder using process taps writing WAV (validates the disk pipeline, no driver needed).
2. Driver skeleton: device appears in Sound settings, silent loopback into the ring.
3. Daemon drains the ring to WAV; end-to-end recording works.
4. Volume and mute controls.
5. Monitoring with drift correction.
6. Runtime configuration (rate, channels, name).
7. Installer, signing, notarization, uninstaller.
8. Hardening: allocation test, stress and drift tests, macOS matrix.

## 11. Open Questions

- ~~Shared memory vs. XPC data path for the ring~~ Resolved: POSIX shared memory. `/System/Library/Sandbox/Profiles/com.apple.audio.coreaudiod.sb` contains an unconditional `(allow ipc-posix-shm)`, so no Mach services are needed for the data path. Control between CLI and daemon uses a Unix socket (both run as the same user). Still to confirm on a real install: the driver may be hosted in `Core Audio Driver Service` rather than `coreaudiod` itself.
- Whether volume is applied before or after recording (raw vs. post-fader), and whether this is user-selectable.
- Which encoder crates to standardize on for FLAC and Opus.
- Minimum macOS version to support.
