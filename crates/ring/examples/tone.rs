//! Stand-in for the driver: writes a 440 Hz stereo sine into a ring in real time.
//! Lets you exercise `spoold` without installing the driver:
//!
//!     cargo run -p spool-ring --example tone -- /spool.tone 5 [amplitude]
//!     spoold --ring /spool.tone --record
use spool_ring::{Format, Producer};
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "/spool.tone".into());
    let seconds: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let amplitude: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0.5);
    let fmt = Format { sample_rate: 48000.0, channels: 2 };
    let p = Producer::create(&name, spool_ring::DEFAULT_CAPACITY, fmt).expect("create ring");
    p.set_running(true);
    let block = 480; // 10 ms
    let mut buf = vec![0f32; block * 2];
    let (mut n, start) = (0u64, Instant::now());
    let total = (seconds * fmt.sample_rate) as u64;
    let mut written = 0u64;
    while n < total {
        for f in 0..block {
            let s = (2.0 * std::f64::consts::PI * 440.0 * (n + f as u64) as f64 / fmt.sample_rate).sin() as f32 * amplitude as f32;
            buf[2 * f] = s;
            buf[2 * f + 1] = -s;
        }
        if p.consumer_alive() && p.write(&buf) {
            written += block as u64;
        }
        n += block as u64;
        let due = start + Duration::from_secs_f64(n as f64 / fmt.sample_rate);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
    }
    p.set_running(false);
    let st = p.stats();
    println!("tone: wrote {written} frames, dropped {} samples", st.dropped_samples);
    let _ = spool_ring::unlink(&name);
}
