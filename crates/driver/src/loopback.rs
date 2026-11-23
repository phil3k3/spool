//! Loopback buffer: carries what apps play to Spool's output to Spool's input (PRO.md §3.3).
//!
//! The buffer is indexed by **sample time** on the device timeline, not by separate read and
//! write positions. `WriteMix` stores the frames for output time *T*; `ReadInput` for input
//! time *T* returns them. Output and input share the device clock, so nothing needs to be
//! resampled or synchronised, and the delay is whatever the HAL puts between the two times.
//!
//! **Only fresh frames come out.** The buffer remembers where the latest write ended
//! (`written_until`). A read returns a frame only if its time lies in the last capacity's worth
//! of writes, `[written_until − CAPACITY, written_until)`; anything else reads as silence. Within
//! that window every slot holds the frame for exactly its own time, because a write that skips
//! ahead (a HAL overload) zeroes the frames it skipped. Without this, a reader that joins while
//! audio is already playing, and keeps reading after it stops, would hear frames that were
//! written one wrap earlier and never read.
//!
//! Frames are also cleared once read, so a span that was already heard doesn't come out twice.
//!
//! Samples are stored as `AtomicU32` (f32 bits) and accessed with `Relaxed` ordering, which
//! compiles to plain loads and stores on arm64 and x86-64. That keeps the buffer sound even if
//! the HAL turns out to call `ReadInput` and `WriteMix` from different threads (PRO.md §11.1),
//! with no locks on the IO path. `written_until` is stored with `Release` after the samples and
//! loaded with `Acquire` before them.

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering::{Acquire, Relaxed, Release}};

/// Capacity in frames. A power of two, ≥ 4 × the largest HAL buffer and ≥ the zero time stamp
/// period, so the output can run this far ahead of the input before frames are overwritten.
pub const CAPACITY_FRAMES: usize = 65_536;
const CAP: i64 = CAPACITY_FRAMES as i64;
const MASK: u64 = CAPACITY_FRAMES as u64 - 1;
/// `written_until` when nothing has been written since the last clear.
const NOTHING: i64 = i64::MIN;

pub struct Loopback {
    channels: usize,
    samples: Box<[AtomicU32]>,
    /// Sample time just past the latest write, or `NOTHING`.
    written_until: AtomicI64,
}

/// Whole-frame sample time. The HAL's times are whole numbers in practice; anything else maps
/// consistently on both sides. Saturating for out-of-range values, NaN -> 0.
fn frame_time(t: f64) -> i64 {
    t as i64
}

impl Loopback {
    /// Allocates and zeroes the buffer. Not real-time safe.
    pub fn new(channels: u32) -> Self {
        let channels = channels.max(1) as usize;
        let samples = (0..CAPACITY_FRAMES * channels).map(|_| AtomicU32::new(0)).collect();
        Loopback { channels, samples, written_until: AtomicI64::new(NOTHING) }
    }

    /// First sample of the frame at sample time `t`.
    fn slot(&self, t: i64) -> usize {
        (t as u64 & MASK) as usize * self.channels
    }

    fn frame(&self, t: i64) -> &[AtomicU32] {
        let at = self.slot(t);
        &self.samples[at..at + self.channels]
    }

    /// Stores interleaved `samples` (whole frames) for output time `t`. Real-time safe.
    pub fn write(&self, t: f64, samples: &[f32]) {
        let base = frame_time(t);
        let frames = (samples.len() / self.channels) as i64;
        let end = base.saturating_add(frames);
        // A skip ahead (HAL overload) leaves a hole. Zero the part of it that will be inside the
        // fresh window after this write, so the hole reads as silence, not as the last wrap.
        let prev = self.written_until.load(Relaxed);
        if prev != NOTHING && base > prev {
            let from = prev.max(end.saturating_sub(CAP));
            for ft in from..base {
                for s in self.frame(ft) {
                    s.store(0, Relaxed);
                }
            }
        }
        for (i, frame) in samples.chunks_exact(self.channels).enumerate() {
            for (slot, &v) in self.frame(base.wrapping_add(i as i64)).iter().zip(frame) {
                slot.store(v.to_bits(), Relaxed);
            }
        }
        self.written_until.store(end, Release);
    }

    /// Fills `out` (whole frames) with what was written for input time `t`, and clears what it
    /// read. Frames outside the fresh window read as silence. Real-time safe.
    pub fn read(&self, t: f64, out: &mut [f32]) {
        let base = frame_time(t);
        let end = self.written_until.load(Acquire);
        let start = end.saturating_sub(CAP);
        for (i, frame) in out.chunks_exact_mut(self.channels).enumerate() {
            let ft = base.wrapping_add(i as i64);
            let fresh = end != NOTHING && ft >= start && ft < end;
            for (slot, v) in self.frame(ft).iter().zip(frame) {
                let bits = slot.swap(0, Relaxed);
                *v = if fresh { f32::from_bits(bits) } else { 0.0 };
            }
        }
    }

    /// Zeroes the whole buffer and forgets the last write. Not real-time: called when IO starts
    /// and on format changes, while no IO runs.
    pub fn clear(&self) {
        for s in self.samples.iter() {
            s.store(0, Relaxed);
        }
        self.written_until.store(NOTHING, Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(frames: usize, ch: usize, seed: f32) -> Vec<f32> {
        (0..frames * ch).map(|i| seed + i as f32 * 1e-4).collect()
    }

    #[test]
    fn reads_what_was_written_at_the_same_sample_time() {
        let lb = Loopback::new(2);
        let mut t = 5_000.0;
        for (cycle, frames) in [512usize, 512, 128, 4096, 33, 1].into_iter().enumerate() {
            let data = ramp(frames, 2, cycle as f32);
            lb.write(t, &data);
            let mut out = vec![9.0; data.len()];
            lb.read(t, &mut out);
            assert_eq!(out, data, "cycle {cycle}, {frames} frames");
            t += frames as f64;
        }
    }

    #[test]
    fn output_can_lead_input_by_several_buffers() {
        let lb = Loopback::new(2);
        let data: Vec<Vec<f32>> = (0..8).map(|c| ramp(256, 2, c as f32)).collect();
        for (c, d) in data.iter().enumerate() {
            lb.write(c as f64 * 256.0, d);
        }
        for (c, d) in data.iter().enumerate() {
            let mut out = vec![0.0; d.len()];
            lb.read(c as f64 * 256.0, &mut out);
            assert_eq!(&out, d);
        }
    }

    #[test]
    fn unwritten_span_reads_as_silence() {
        let lb = Loopback::new(2);
        lb.write(0.0, &ramp(64, 2, 1.0));
        let mut out = vec![9.0; 128];
        lb.read(1_000.0, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn zero_on_read_hides_audio_from_the_previous_wrap() {
        let lb = Loopback::new(2);
        let data = ramp(100, 2, 0.5);
        lb.write(10.0, &data);
        let mut out = vec![0.0; data.len()];
        lb.read(10.0, &mut out);
        assert_eq!(out, data);
        // Same slots one wrap later, nothing new written: silence, not a repeat.
        lb.read(10.0 + CAPACITY_FRAMES as f64, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn wraps_in_the_middle_of_a_buffer() {
        for ch in [1u32, 2, 3, 16] {
            let lb = Loopback::new(ch);
            let t = (CAPACITY_FRAMES - 7) as f64 + 3.0 * CAPACITY_FRAMES as f64;
            let data = ramp(512, ch as usize, 2.0);
            lb.write(t, &data);
            let mut out = vec![0.0; data.len()];
            lb.read(t, &mut out);
            assert_eq!(out, data, "{ch} channels");
        }
    }

    #[test]
    fn clear_drops_pending_audio() {
        let lb = Loopback::new(2);
        lb.write(0.0, &ramp(64, 2, 1.0));
        lb.clear();
        let mut out = vec![9.0; 128];
        lb.read(0.0, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn oversized_and_odd_inputs_do_not_panic() {
        let lb = Loopback::new(2);
        // More than the capacity: the later frames win, like a ring.
        let big = ramp(CAPACITY_FRAMES + 10, 2, 0.0);
        lb.write(0.0, &big);
        let mut out = vec![0.0; 20];
        lb.read(CAPACITY_FRAMES as f64, &mut out);
        assert_eq!(out, big[CAPACITY_FRAMES * 2..]);
        // Time 0 was overwritten by time CAPACITY: silence, not the newer frame.
        lb.read(0.0, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        // Partial trailing frame is ignored; odd sample times are tolerated.
        lb.write(-3.0, &[1.0, 2.0, 3.0]);
        let mut out = [0.0; 3];
        lb.read(-3.0, &mut out);
        assert_eq!(out, [1.0, 2.0, 0.0]);
        for t in [f64::NAN, f64::INFINITY, -1e300, 1e300] {
            lb.write(t, &[1.0, 1.0]);
            lb.read(t, &mut [0.0; 2]);
        }
    }

    /// docs/g1-stale-audio.html: a reader joins while audio plays and keeps reading after it
    /// stops. Past the last write it must hear silence, not the frames left from one wrap ago.
    #[test]
    fn reader_that_outlives_the_writer_hears_silence_not_the_last_wrap() {
        let lb = Loopback::new(2);
        let block = 512;
        let joins = 2 * CAPACITY_FRAMES as i64; // nobody reads before this time
        let stops = joins + 8 * block as i64; // nothing is written from this time on
        let mut t = 0;
        while t < stops {
            lb.write(t as f64, &ramp(block, 2, 1.0 + t as f32));
            t += block as i64;
        }
        // The reader is live up to the last write...
        let mut out = vec![0.0; block * 2];
        let mut t = joins;
        while t < stops {
            lb.read(t as f64, &mut out);
            assert_eq!(out, ramp(block, 2, 1.0 + t as f32), "live at {t}");
            t += block as i64;
        }
        // ...and gets silence for a whole wrap after it, where slots still hold unread audio.
        while t < stops + CAPACITY_FRAMES as i64 {
            lb.read(t as f64, &mut out);
            assert!(out.iter().all(|&s| s == 0.0), "replayed old audio at {t}");
            t += block as i64;
        }
    }

    #[test]
    fn a_skipped_span_reads_as_silence() {
        let lb = Loopback::new(2);
        // One full wrap that nobody reads.
        for c in 0..(CAPACITY_FRAMES / 512) {
            lb.write((c * 512) as f64, &ramp(512, 2, 3.0));
        }
        // Then the HAL skips [CAP + 512, CAP + 1024).
        let cap = CAPACITY_FRAMES as f64;
        lb.write(cap, &ramp(512, 2, 4.0));
        lb.write(cap + 1024.0, &ramp(512, 2, 5.0));
        let mut out = vec![9.0; 1024];
        lb.read(cap + 512.0, &mut out);
        assert!(out.iter().all(|&s| s == 0.0), "skipped span replayed the last wrap");
        lb.read(cap + 1024.0, &mut out);
        assert_eq!(out, ramp(512, 2, 5.0));
    }

    #[test]
    fn nothing_written_reads_as_silence() {
        let lb = Loopback::new(2);
        let mut out = vec![9.0; 64];
        lb.read(0.0, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }
}
