//! Audio thread: drains the driver's ring and fans the stream out, without ever blocking on
//! devices or disk.
//!
//! ```text
//!                     ┌──► meter (levels for status)
//! ring ──► audio ─────┼──► MonitorSink ──► output device        (non-blocking queue)
//!          thread     └──► Chunk channel ──► control thread ──► WAV files
//! ```
//!
//! The heartbeat that keeps the driver writing comes from this thread, so slow work on the
//! control thread (opening an AirPlay device, a stalled disk) cannot make the driver stop
//! and silently lose audio. If the control thread falls behind, recording data waits in the
//! channel (bounded by [`MAX_QUEUED_SAMPLES`]; beyond that it is dropped and flagged as a
//! discontinuity) while monitoring keeps playing.

use crate::meter::Meter;
use crate::monitor::MonitorSink;
use spool_ring::{Consumer, Format, Stats};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Drain interval; keeps the monitor queue topped up well inside its latency target.
const POLL: Duration = Duration::from_millis(5);
const ATTACH_RETRY: Duration = Duration::from_secs(1);
/// How far recording may fall behind before audio is dropped: 16 Mi samples (64 MiB),
/// ~87 s of 48 kHz stereo, ~22 s at 192 kHz.
pub const MAX_QUEUED_SAMPLES: usize = 16 << 20;
/// Read buffer, in samples.
const READ_SAMPLES: usize = 1 << 16;

/// Audio for the recorder, in ring order.
pub struct Chunk {
    pub samples: Vec<f32>,
    pub format: Format,
    /// Device gain (amplitude, 0 if muted) when the chunk was read.
    pub gain: f32,
    /// Audio was lost before this chunk (ring overrun, producer restart, backlog full).
    pub discontinuity: bool,
}

pub enum Cmd {
    /// Feed this monitor (None = stop feeding).
    SetMonitor(Option<MonitorSink>),
}

/// What the control thread may read about the ring.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub attached: bool,
    pub stats: Stats,
    pub format: Option<Format>,
    pub error: Option<String>,
}

/// State shared between the audio thread and the control thread.
pub struct Shared {
    pub snapshot: Mutex<Snapshot>,
    pub meter: Mutex<Meter>,
    /// Set by the control thread while it records; chunks are only sent then.
    pub recording: AtomicBool,
    /// Samples sent to the control thread and not yet taken by it.
    pub queued: AtomicUsize,
    /// Samples dropped because the recording backlog was full.
    pub backlog_dropped: AtomicU64,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(Snapshot::default()),
            meter: Mutex::new(Meter::new()),
            recording: AtomicBool::new(false),
            queued: AtomicUsize::new(0),
            backlog_dropped: AtomicU64::new(0),
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn meter(&self) -> std::sync::MutexGuard<'_, Meter> {
        self.meter.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Config {
    pub ring_name: String,
    /// Meter what ends up in the file (raw mix unless post-fader).
    pub post_fader: bool,
}

/// Starts the audio thread. It runs until `quit` is set or the control side hangs up.
pub fn spawn<M: From<Chunk> + Send + 'static>(
    cfg: Config,
    shared: Arc<Shared>,
    out: Sender<M>,
    cmds: Receiver<Cmd>,
    quit: &'static AtomicBool,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("spoold audio".into()).spawn(move || {
        let mut a = Audio {
            cfg,
            shared,
            consumer: None,
            last_attach_attempt: None,
            error: None,
            sink: None,
            pending_discontinuity: false,
        };
        let mut buf = vec![0f32; READ_SAMPLES];
        while !quit.load(Ordering::Relaxed) {
            loop {
                match cmds.try_recv() {
                    Ok(Cmd::SetMonitor(s)) => a.sink = s,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            a.ensure_attached();
            if a.drain(&mut buf, &out).is_err() {
                return; // control thread gone
            }
            a.publish();
            std::thread::sleep(POLL);
        }
    })
}

struct Audio {
    cfg: Config,
    shared: Arc<Shared>,
    consumer: Option<Consumer>,
    last_attach_attempt: Option<Instant>,
    error: Option<String>,
    sink: Option<MonitorSink>,
    /// Recording audio was dropped (backlog full); flag the next chunk sent.
    pending_discontinuity: bool,
}

impl Audio {
    fn ensure_attached(&mut self) {
        if self.consumer.as_ref().is_some_and(|c| c.is_stale()) {
            eprintln!("spoold: driver replaced the ring, reattaching");
            self.consumer = None;
        }
        if self.consumer.is_some() || self.last_attach_attempt.is_some_and(|t| t.elapsed() < ATTACH_RETRY) {
            return;
        }
        self.last_attach_attempt = Some(Instant::now());
        let name = &self.cfg.ring_name;
        match Consumer::open(name) {
            Ok(c) => {
                eprintln!("spoold: attached to {name} ({:?})", c.format());
                self.consumer = Some(c);
                self.error = None;
            }
            Err(e) => {
                let hint = if e.kind() == std::io::ErrorKind::NotFound { " (is the Spool driver installed?)" } else { "" };
                let msg = format!("cannot attach to driver ring {name}: {e}{hint}");
                if self.error.as_deref() != Some(&msg) {
                    eprintln!("spoold: {msg}");
                }
                self.error = Some(msg);
            }
        }
    }

    /// Reads everything available and fans it out. Err if the control thread hung up.
    fn drain<M: From<Chunk>>(&mut self, buf: &mut [f32], out: &Sender<M>) -> Result<(), ()> {
        let Some(c) = self.consumer.as_mut() else { return Ok(()) };
        c.heartbeat();
        let stats = c.stats();
        let gain = if stats.muted { 0.0 } else { stats.gain };
        if let Some(s) = &self.sink {
            // Monitoring always follows the device volume, so volume keys work.
            s.set_gain(gain);
        }
        let meter_gain = if self.cfg.post_fader { gain } else { 1.0 };
        loop {
            let ch = c.read(buf);
            if ch.samples == 0 {
                return Ok(());
            }
            let samples = &buf[..ch.samples];
            let (channels, rate) = (ch.format.channels as usize, ch.format.sample_rate);
            self.shared.meter().feed(samples, channels, rate, meter_gain);
            if let Some(s) = self.sink.as_mut().filter(|s| s.input == ch.format) {
                s.push(samples);
            }
            if !self.shared.recording.load(Ordering::Acquire) {
                continue;
            }
            let discontinuity = ch.discontinuity || std::mem::take(&mut self.pending_discontinuity);
            if self.shared.queued.load(Ordering::Acquire) + samples.len() > MAX_QUEUED_SAMPLES {
                self.shared.backlog_dropped.fetch_add(samples.len() as u64, Ordering::Relaxed);
                self.pending_discontinuity = true;
                continue;
            }
            self.shared.queued.fetch_add(samples.len(), Ordering::AcqRel);
            let chunk = Chunk { samples: samples.to_vec(), format: ch.format, gain, discontinuity };
            out.send(chunk.into()).map_err(|_| ())?;
        }
    }

    fn publish(&self) {
        let snap = Snapshot {
            attached: self.consumer.is_some(),
            stats: self.consumer.as_ref().map(|c| c.stats()).unwrap_or_default(),
            format: self.consumer.as_ref().map(|c| c.format()),
            error: self.error.clone(),
        };
        *self.shared.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = snap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_ring::Producer;
    use std::sync::mpsc;

    static QUIT: AtomicBool = AtomicBool::new(false);
    const STEREO: Format = Format { sample_rate: 48000.0, channels: 2 };

    /// The control thread not reading for longer than the heartbeat timeout (2 s) must not
    /// cost audio: the audio thread keeps the heartbeat alive and the backlog queues up.
    #[test]
    fn control_stall_longer_than_heartbeat_loses_nothing() {
        let name = format!("/spd.{}", std::process::id());
        let _ = spool_ring::unlink(&name);
        let p = Producer::create(&name, spool_ring::DEFAULT_CAPACITY, STEREO).unwrap();
        let shared = Shared::new();
        shared.recording.store(true, Ordering::Release);
        let (tx, rx) = mpsc::channel::<Chunk>();
        let (_cmd_tx, cmd_rx) = mpsc::channel();
        let cfg = Config { ring_name: name.clone(), post_fader: false };
        let t = spawn(cfg, shared.clone(), tx, cmd_rx, &QUIT).unwrap();

        let t0 = Instant::now();
        while !p.consumer_alive() {
            assert!(t0.elapsed() < Duration::from_secs(3), "audio thread never attached");
            std::thread::sleep(Duration::from_millis(5));
        }
        // 3 s of audio in real time, 10 ms per cycle, gated like the driver does it.
        let block = [0.25f32; 480 * 2];
        let (mut written, start) = (0usize, Instant::now());
        for i in 1..=300u32 {
            if p.consumer_alive() {
                assert!(p.write(&block));
                written += block.len();
            } else {
                p.count_skipped(block.len());
            }
            let due = start + Duration::from_millis(10) * i;
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        std::thread::sleep(Duration::from_millis(100)); // let the last cycle drain
        QUIT.store(true, Ordering::Relaxed);
        t.join().unwrap();

        let chunks: Vec<Chunk> = rx.try_iter().collect();
        let got: usize = chunks.iter().map(|c| c.samples.len()).sum();
        let stats = p.stats();
        let _ = spool_ring::unlink(&name);
        assert_eq!(stats.skipped_samples, 0, "heartbeat lapsed while control thread was stalled");
        assert_eq!(got, written);
        assert_eq!(written, 300 * block.len());
        assert!(chunks.iter().all(|c| !c.discontinuity && c.format == STEREO));
        assert_eq!(shared.queued.load(Ordering::Relaxed), got, "backlog accounting");
    }
}
