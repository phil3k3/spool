//! Per-channel level metering for `spool record`'s live view.
//!
//! Peak: instantaneous peak with a hold-and-fall ballistic (falls 20 dB/s).
//! RMS: exponential average with a 300 ms time constant (VU-like).
//! Both keep falling in wall-clock time when no audio arrives, so meters drop to silence
//! when playback stops instead of freezing.

use std::time::Instant;

const PEAK_FALL_DB_PER_S: f64 = 20.0;
const RMS_TIME_CONSTANT_S: f64 = 0.3;
/// Floor reported for silence.
pub const FLOOR_DB: f32 = -120.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    pub peak_db: f32,
    pub rms_db: f32,
}

pub struct Meter {
    peak: Vec<f64>,
    mean_square: Vec<f64>,
    /// Samples at or above full scale since the last reset.
    pub clipped: u64,
    last: Instant,
}

fn db(amplitude: f64) -> f32 {
    if amplitude <= 1e-6 { FLOOR_DB } else { (20.0 * amplitude.log10()).max(FLOOR_DB as f64) as f32 }
}

impl Meter {
    pub fn new() -> Self {
        Self { peak: Vec::new(), mean_square: Vec::new(), clipped: 0, last: Instant::now() }
    }

    /// Peak fall over `dt`. (RMS decays through its exponential average in `feed`.)
    fn decay_peaks(&mut self, dt: f64) {
        let peak_factor = 10f64.powf(-PEAK_FALL_DB_PER_S * dt / 20.0);
        self.peak.iter_mut().for_each(|p| *p *= peak_factor);
    }

    /// Feeds interleaved samples. `gain` scales them first (post-fader metering).
    pub fn feed(&mut self, samples: &[f32], channels: usize, sample_rate: f64, gain: f32) {
        // The ring only delivers validated formats; the cap keeps allocation bounded anyway.
        let channels = channels.clamp(1, spool_ring::MAX_CHANNELS as usize);
        if self.peak.len() != channels {
            self.peak = vec![0.0; channels];
            self.mean_square = vec![0.0; channels];
        }
        let frames = samples.len() / channels;
        if frames == 0 {
            return;
        }
        let dt = frames as f64 / sample_rate.max(1.0);
        self.decay_peaks(dt);
        let alpha = 1.0 - (-dt / RMS_TIME_CONSTANT_S).exp();
        for c in 0..channels {
            let (mut pk, mut sum) = (0f64, 0f64);
            for f in 0..frames {
                let s = (samples[f * channels + c] * gain).abs() as f64;
                if s >= 1.0 {
                    self.clipped += 1;
                }
                pk = pk.max(s);
                sum += s * s;
            }
            self.peak[c] = self.peak[c].max(pk);
            self.mean_square[c] += alpha * (sum / frames as f64 - self.mean_square[c]);
        }
        self.last = Instant::now();
    }

    /// Current levels, decayed for the time since audio last arrived.
    pub fn levels(&self) -> Vec<Level> {
        let idle = self.last.elapsed().as_secs_f64();
        let peak_factor = 10f64.powf(-PEAK_FALL_DB_PER_S * idle / 20.0);
        let rms_factor = (-idle / RMS_TIME_CONSTANT_S).exp();
        self.peak
            .iter()
            .zip(&self.mean_square)
            .map(|(&p, &m)| Level { peak_db: db(p * peak_factor), rms_db: db((m * rms_factor).sqrt()) })
            .collect()
    }

    pub fn reset_clipped(&mut self) {
        self.clipped = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, frames: usize, channels: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let s = amp * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48000.0).sin();
                std::iter::repeat_n(s, channels)
            })
            .collect()
    }

    #[test]
    fn full_scale_sine_reads_0_db_peak_and_minus_3_db_rms() {
        let mut m = Meter::new();
        // 2 s so the 300 ms RMS average settles.
        m.feed(&sine(1.0, 96000, 2), 2, 48000.0, 1.0);
        for l in m.levels() {
            assert!(l.peak_db > -0.1 && l.peak_db <= 0.01, "peak {}", l.peak_db);
            assert!((l.rms_db - -3.01).abs() < 0.1, "rms {}", l.rms_db);
        }
    }

    #[test]
    fn small_chunks_read_the_same_as_one_big_chunk() {
        // The daemon feeds ~5 ms chunks; RMS must not depend on chunking.
        let mut m = Meter::new();
        let s = sine(0.5, 96000, 2);
        for chunk in s.chunks(240 * 2) {
            m.feed(chunk, 2, 48000.0, 1.0);
        }
        let l = m.levels();
        assert!((l[0].rms_db - -9.03).abs() < 0.1, "rms {}", l[0].rms_db);
        assert!((l[0].peak_db - -6.02).abs() < 0.2, "peak {}", l[0].peak_db);
    }

    #[test]
    fn gain_and_channels_are_independent() {
        let mut m = Meter::new();
        let mut s = sine(0.5, 96000, 2);
        s.iter_mut().skip(1).step_by(2).for_each(|x| *x = 0.0); // right channel silent
        m.feed(&s, 2, 48000.0, 0.5);
        let l = m.levels();
        assert!((l[0].peak_db - -12.04).abs() < 0.1, "left {}", l[0].peak_db); // 0.25 = -12 dB
        assert_eq!(l[1].peak_db, FLOOR_DB);
    }

    #[test]
    fn peak_falls_after_audio_stops_and_clipping_counts() {
        let mut m = Meter::new();
        m.feed(&[1.0, 1.0], 2, 48000.0, 1.0);
        assert_eq!(m.clipped, 2);
        // One second of silence: peak falls ~20 dB.
        m.feed(&vec![0.0; 96000], 2, 48000.0, 1.0);
        let l = m.levels();
        assert!((l[0].peak_db - -20.0).abs() < 0.5, "{}", l[0].peak_db);
    }
}
