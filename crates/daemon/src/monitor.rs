//! Monitoring: plays the Spool stream to a physical output device, replacing a
//! Multi-Output Device.
//!
//! ```text
//! drain loop ──push──► rtrb queue ──► [Core: drift-correcting resampler + gain] ──► cpal/AUHAL
//! (daemon thread)                      (output device's real-time callback)
//! ```
//!
//! The virtual device's clock (host time) and the hardware's crystal differ by tens of ppm,
//! and the hardware may run at another sample rate. [`Core`] resamples with a ratio of
//! `in_rate / out_rate`, nudged by a proportional controller on the smoothed queue fill
//! so latency stays at the target instead of drifting into underruns or growing delay.
//!
//! The device's volume and mute (set by volume keys on Spool) are applied here, with a
//! per-buffer ramp to avoid zipper noise.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use spool_ring::Format;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};

/// UID of the Spool virtual device (must match `crates/driver`). Monitoring to it would
/// feed the stream back into itself.
pub const SPOOL_UID: &str = "SpoolDevice_UID";

/// Maximum resampling correction (±0.5 %, ~9 cents; inaudible for monitoring).
const MAX_CORRECTION: f64 = 0.005;
/// Correction per unit of relative fill error.
const KP: f64 = 0.01;
/// Per-callback smoothing of the fill level (~1 s time constant at 10 ms callbacks).
const FILL_SMOOTHING: f64 = 0.01;

/// Counters shared between the real-time callback and the daemon.
#[derive(Default)]
pub struct Shared {
    gain_bits: AtomicU32,
    pub underruns: AtomicU64,
    pub playing: AtomicBool,
    pub failed: AtomicBool,
    /// Smoothed queue fill, in input frames.
    pub fill_frames: AtomicU64,
    /// Current correction in parts per million.
    pub correction_ppm: AtomicI64,
}

impl Shared {
    fn new() -> Arc<Self> {
        let s = Self::default();
        s.gain_bits.store(1.0f32.to_bits(), Relaxed);
        Arc::new(s)
    }
}

/// Resampler, drift controller and channel mapper. Real-time safe: `render` performs no
/// allocation, locking or syscalls.
pub struct Core {
    rx: rtrb::Consumer<f32>,
    in_ch: usize,
    out_ch: usize,
    base_step: f64,
    target: f64,
    /// Last four input frames (oldest first), for cubic interpolation between [1] and [2].
    hist: Vec<f32>,
    mu: f64,
    primed: bool,
    fill_avg: f64,
    gain: f32,
    shared: Arc<Shared>,
}

impl Core {
    pub fn new(rx: rtrb::Consumer<f32>, input: Format, out_rate: f64, out_ch: usize, target_frames: f64, shared: Arc<Shared>) -> Self {
        let in_ch = input.channels.max(1) as usize;
        Self {
            rx,
            in_ch,
            out_ch: out_ch.max(1),
            base_step: input.sample_rate / out_rate,
            target: target_frames.max(1.0),
            hist: vec![0.0; 4 * in_ch],
            mu: 0.0,
            primed: false,
            fill_avg: 0.0,
            gain: f32::from_bits(shared.gain_bits.load(Relaxed)),
            shared,
        }
    }

    fn available(&self) -> usize {
        self.rx.slots() / self.in_ch
    }

    /// Shifts the next input frame into the history. False if the queue is empty.
    fn pop_frame(&mut self) -> bool {
        let Ok(chunk) = self.rx.read_chunk(self.in_ch) else { return false };
        let (a, b) = chunk.as_slices();
        let ch = self.in_ch;
        self.hist.copy_within(ch.., 0);
        self.hist[3 * ch..3 * ch + a.len()].copy_from_slice(a);
        self.hist[3 * ch + a.len()..].copy_from_slice(b);
        chunk.commit_all();
        true
    }

    fn discard(&mut self, frames: usize) {
        if let Ok(c) = self.rx.read_chunk(frames * self.in_ch) {
            c.commit_all();
        }
    }

    #[inline]
    fn sample(&self, ch: usize, mu: f32) -> f32 {
        let c = self.in_ch;
        let (y0, y1, y2, y3) = (self.hist[ch], self.hist[c + ch], self.hist[2 * c + ch], self.hist[3 * c + ch]);
        // Catmull-Rom cubic Hermite between y1 and y2.
        let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
        let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let cc = -0.5 * y0 + 0.5 * y2;
        ((a * mu + b) * mu + cc) * mu + y1
    }

    #[inline]
    fn out_sample(&self, out_c: usize, mu: f32) -> f32 {
        if self.out_ch == 1 && self.in_ch > 1 {
            // Downmix to mono.
            (0..self.in_ch).map(|c| self.sample(c, mu)).sum::<f32>() / self.in_ch as f32
        } else if out_c < self.in_ch {
            self.sample(out_c, mu)
        } else if self.in_ch == 1 {
            self.sample(0, mu)
        } else {
            0.0
        }
    }

    /// Fills `out` (interleaved, `out_ch` channels).
    pub fn render(&mut self, out: &mut [f32]) {
        let frames = out.len() / self.out_ch;
        let mut avail = self.available() as f64;

        if !self.primed {
            if avail < self.target {
                out.fill(0.0);
                return;
            }
            self.primed = true;
            // Audio that piled up while the device was opening would otherwise become
            // permanent extra latency: start exactly at the target.
            if avail > self.target {
                self.discard((avail - self.target) as usize);
                avail = self.available() as f64;
            }
            self.fill_avg = avail;
            self.mu = 0.0;
            self.hist.fill(0.0);
            self.shared.playing.store(true, Relaxed);
        }

        // Latency blew up (e.g. the output device stalled): jump back to the target.
        if avail > 4.0 * self.target + frames as f64 {
            self.discard((avail - self.target) as usize);
            self.fill_avg = self.target;
        } else {
            self.fill_avg += (avail - self.fill_avg) * FILL_SMOOTHING;
        }
        let err = (self.fill_avg - self.target) / self.target;
        let correction = (err * KP).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        let step = self.base_step * (1.0 + correction);
        self.shared.fill_frames.store(self.fill_avg as u64, Relaxed);
        self.shared.correction_ppm.store((correction * 1e6) as i64, Relaxed);

        let target_gain = f32::from_bits(self.shared.gain_bits.load(Relaxed));
        let g0 = self.gain;
        let dg = (target_gain - g0) / frames.max(1) as f32;

        for f in 0..frames {
            while self.mu >= 1.0 {
                if !self.pop_frame() {
                    // Underrun: silence the rest and wait until the queue refills.
                    out[f * self.out_ch..].fill(0.0);
                    self.primed = false;
                    self.shared.playing.store(false, Relaxed);
                    self.shared.underruns.fetch_add(1, Relaxed);
                    self.gain = target_gain;
                    return;
                }
                self.mu -= 1.0;
            }
            let g = g0 + dg * f as f32;
            let mu = self.mu as f32;
            for c in 0..self.out_ch {
                out[f * self.out_ch + c] = self.out_sample(c, mu) * g;
            }
            self.mu += step;
        }
        self.gain = target_gain;
    }
}

/// An output device as seen by `spool devices`.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub uid: String,
    pub is_default: bool,
}

fn uid_of(d: &cpal::Device) -> String {
    d.id().map(|id| id.id().to_string()).unwrap_or_default()
}

pub fn list_output_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default_uid = host.default_output_device().map(|d| uid_of(&d));
    let Ok(devs) = host.output_devices() else { return Vec::new() };
    devs.map(|d| {
        let uid = uid_of(&d);
        DeviceInfo { name: d.to_string(), is_default: Some(&uid) == default_uid.as_ref(), uid }
    })
    .collect()
}

/// Checks that `selector` names a usable output device; returns its display name.
pub fn resolve_device(selector: &str) -> Result<String, String> {
    find_device(selector).map(|d| d.to_string())
}

/// Finds an output device by exact name, UID, or unique case-insensitive name fragment.
fn find_device(selector: &str) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let devs: Vec<_> = host.output_devices().map_err(|e| e.to_string())?.collect();
    let exact = devs.iter().find(|d| d.to_string() == selector || uid_of(d) == selector);
    let dev = match exact {
        Some(d) => d.clone(),
        None => {
            let needle = selector.to_lowercase();
            let hits: Vec<_> = devs.iter().filter(|d| d.to_string().to_lowercase().contains(&needle)).collect();
            match hits.as_slice() {
                [d] => (*d).clone(),
                [] => return Err(format!("no output device matches {selector:?} (see `spool devices`)")),
                _ => {
                    let names: Vec<_> = hits.iter().map(|d| d.to_string()).collect();
                    return Err(format!("{selector:?} is ambiguous: {}", names.join(", ")));
                }
            }
        }
    };
    if uid_of(&dev) == SPOOL_UID {
        return Err("cannot monitor to Spool itself (feedback loop); choose your speakers or headphones".into());
    }
    Ok(dev)
}

/// A running monitor stream. Opening and closing it can block (device IO), so it lives on
/// the control thread; the audio thread feeds it through the [`MonitorSink`].
pub struct Monitor {
    _stream: cpal::Stream,
    sink: Option<MonitorSink>,
    pub input: Format,
    pub device_name: String,
    pub out_rate: u32,
    pub out_channels: u16,
    pub shared: Arc<Shared>,
}

/// Input side of a monitor stream: non-blocking, used by the audio thread.
pub struct MonitorSink {
    tx: rtrb::Producer<f32>,
    pub input: Format,
    shared: Arc<Shared>,
    /// Input samples dropped because the queue was full.
    pub overflowed: u64,
}

impl MonitorSink {
    /// Queues interleaved samples in the input format. Drops what does not fit.
    pub fn push(&mut self, samples: &[f32]) {
        let ch = self.input.channels.max(1) as usize;
        let n = ((self.tx.slots() / ch) * ch).min(samples.len());
        if n > 0
            && let Ok(chunk) = self.tx.write_chunk_uninit(n)
        {
            chunk.fill_from_iter(samples[..n].iter().copied());
        }
        self.overflowed += (samples.len() - n) as u64;
    }

    /// Device volume (amplitude, 0 if muted); ramped in the output callback.
    pub fn set_gain(&self, gain: f32) {
        self.shared.gain_bits.store(gain.to_bits(), Relaxed);
    }
}

impl Monitor {
    pub fn open(selector: &str, input: Format, latency_ms: u32) -> Result<Self, String> {
        // Sizes the queue from the format, so it must be sane (the ring guarantees this).
        if !input.is_valid() {
            return Err(format!("unsupported stream format {input:?}"));
        }
        let dev = find_device(selector)?;
        let name = dev.to_string();
        let supported = dev.default_output_config().map_err(|e| format!("{name}: {e}"))?;
        let mut config = supported.config();
        config.buffer_size = cpal::BufferSize::Default;
        let out_rate = config.sample_rate;
        let out_ch = config.channels as usize;

        let in_ch = input.channels.max(1) as usize;
        let target = input.sample_rate * latency_ms as f64 / 1000.0;
        // One second of headroom beyond the target.
        let cap = ((target + input.sample_rate) as usize) * in_ch;
        let (tx, rx) = rtrb::RingBuffer::new(cap);
        let shared = Shared::new();
        let mut core = Core::new(rx, input, out_rate as f64, out_ch, target, shared.clone());

        let err_shared = shared.clone();
        let stream = dev
            .build_output_stream::<f32, _, _>(
                config,
                move |data: &mut [f32], _| core.render(data),
                move |e| {
                    eprintln!("spoold: monitor stream error: {e}");
                    err_shared.failed.store(true, Relaxed);
                },
                None,
            )
            .map_err(|e| format!("{name}: cannot open output stream: {e}"))?;
        stream.play().map_err(|e| format!("{name}: cannot start output stream: {e}"))?;
        let sink = MonitorSink { tx, input, shared: shared.clone(), overflowed: 0 };
        Ok(Self {
            _stream: stream,
            sink: Some(sink),
            input,
            device_name: name,
            out_rate,
            out_channels: out_ch as u16,
            shared,
        })
    }

    /// Hands out the input side (once) for the audio thread.
    pub fn take_sink(&mut self) -> Option<MonitorSink> {
        self.sink.take()
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Relaxed)
    }

    pub fn latency_now_ms(&self) -> f64 {
        self.shared.fill_frames.load(Relaxed) as f64 * 1000.0 / self.input.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(in_rate: f64, in_ch: u32, out_rate: f64, out_ch: usize, target: f64) -> (rtrb::Producer<f32>, Core, Arc<Shared>) {
        let (tx, rx) = rtrb::RingBuffer::new(1 << 20);
        let shared = Shared::new();
        let c = Core::new(rx, Format { sample_rate: in_rate, channels: in_ch }, out_rate, out_ch, target, shared.clone());
        (tx, c, shared)
    }

    fn push(tx: &mut rtrb::Producer<f32>, s: &[f32]) {
        for &x in s {
            tx.push(x).unwrap();
        }
    }

    #[test]
    fn waits_for_target_fill_then_plays_unity_ratio_exactly() {
        let (mut tx, mut c, shared) = core(48000.0, 2, 48000.0, 2, 100.0);
        push(&mut tx, &[0.5; 2 * 50]);
        let mut out = vec![1.0; 2 * 64];
        c.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "must not start below target fill");
        assert!(!shared.playing.load(Relaxed));

        // A ramp at exactly the target fill: no drift correction, step is exactly 1 and the
        // interpolator returns input samples verbatim, delayed by its 3-frame history.
        let ramp: Vec<f32> = (0..2000).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let (mut tx, mut c, _) = core(48000.0, 2, 48000.0, 2, 2000.0);
        push(&mut tx, &ramp);
        let mut out = vec![0.0; 2 * 512];
        c.render(&mut out);
        for k in 3..512 {
            assert_eq!(out[2 * k], (k - 3) as f32);
            assert_eq!(out[2 * k + 1], -((k - 3) as f32));
        }
    }

    #[test]
    fn converts_sample_rate_preserving_pitch() {
        let (mut tx, mut c, _) = core(48000.0, 1, 44100.0, 1, 48000.0);
        let f = 1000.0;
        let sine: Vec<f32> = (0..48000).map(|n| (2.0 * std::f64::consts::PI * f * n as f64 / 48000.0).sin() as f32).collect();
        push(&mut tx, &sine);
        let mut out = vec![0.0; 22050];
        c.render(&mut out);
        // Count zero crossings over the last 0.4 s of output: 1 kHz -> ~800 crossings.
        let tail = &out[4410..];
        let crossings = tail.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let secs = tail.len() as f64 / 44100.0;
        let freq = crossings as f64 / 2.0 / secs;
        assert!((freq - 1000.0).abs() < 5.0, "got {freq} Hz");
    }

    #[test]
    fn controller_absorbs_clock_drift() {
        // Producer runs 200 ppm fast relative to the output clock.
        let in_rate = 48000.0 * (1.0 + 200e-6);
        let (mut tx, mut c, shared) = core(48000.0, 2, 48000.0, 2, 2400.0); // 50 ms target
        let mut produced = 0.0f64;
        let mut out = vec![0.0; 2 * 480];
        for _ in 0..30_000 {
            // 10 ms of producer audio, 10 ms of output callback (5 minutes total).
            produced += in_rate * 0.01;
            let n = produced.floor();
            produced -= n;
            for _ in 0..n as usize {
                tx.push(0.1).unwrap();
                tx.push(0.1).unwrap();
            }
            c.render(&mut out);
        }
        let fill = shared.fill_frames.load(Relaxed) as f64;
        assert!((fill - 2400.0).abs() < 2400.0 * 0.1, "fill drifted to {fill}");
        assert_eq!(shared.underruns.load(Relaxed), 0);
        let ppm = shared.correction_ppm.load(Relaxed);
        assert!((150..=250).contains(&ppm), "correction {ppm} ppm should track +200 ppm drift");
    }

    #[test]
    fn priming_drops_backlog_down_to_target_latency() {
        let (mut tx, mut c, shared) = core(48000.0, 2, 48000.0, 2, 2400.0);
        push(&mut tx, &vec![0.1; 2 * 6000]); // 125 ms queued while the device opened
        let mut out = vec![0.0; 2 * 480];
        c.render(&mut out);
        let left = c.available();
        assert!((1900..=2400).contains(&left), "queue {left} frames after priming");
        assert_eq!(shared.correction_ppm.load(Relaxed), 0);
    }

    #[test]
    fn underrun_silences_and_reprimes() {
        let (mut tx, mut c, shared) = core(48000.0, 2, 48000.0, 2, 100.0);
        push(&mut tx, &[0.5; 2 * 150]);
        let mut out = vec![0.0; 2 * 400];
        c.render(&mut out);
        assert_eq!(shared.underruns.load(Relaxed), 1);
        assert!(!shared.playing.load(Relaxed));
        assert!(out[2 * 200..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn applies_gain_with_ramp_and_maps_channels() {
        let (mut tx, mut c, shared) = core(48000.0, 2, 48000.0, 1, 1000.0);
        push(&mut tx, &[1.0, 0.0].repeat(1000));
        shared.gain_bits.store(0.0f32.to_bits(), Relaxed);
        let mut out = vec![0.0; 100];
        c.render(&mut out);
        // Mono downmix of (1, 0) = 0.5, ramping from gain 1 to 0 (after 3 frames of history).
        assert!(out[5] > 0.45 && out[99] < 0.01, "{:?}", &out[..8]);
        c.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }
}
