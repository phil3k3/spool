//! Live terminal view for `spool record` and `spool status --watch`.
//!
//! Polls the daemon ~10×/s and redraws a small block in place with ANSI escapes:
//! level meters per channel, elapsed time, file size, disk space, forwarding health.

use spool_proto::{Request, Response, Status, call};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Ctrl-C stops the recording.
    Record,
    /// Ctrl-C just leaves the view.
    Watch,
}

const METER_MIN_DB: f32 = -60.0;

struct Style {
    color: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
}

fn terminal_width() -> usize {
    // SAFETY: zeroed winsize is a valid out-parameter for TIOCGWINSZ.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
}

pub fn human_bytes(b: u64) -> String {
    let b = b as f64;
    match b {
        b if b >= 1e12 => format!("{:.1} TB", b / 1e12),
        b if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        b if b >= 1e6 => format!("{:.1} MB", b / 1e6),
        b if b >= 1e3 => format!("{:.0} KB", b / 1e3),
        b => format!("{b:.0} B"),
    }
}

pub fn clock(secs: f64) -> String {
    let t = secs.max(0.0);
    let h = (t / 3600.0) as u64;
    let m = ((t % 3600.0) / 60.0) as u64;
    let s = t % 60.0;
    format!("{h:02}:{m:02}:{s:04.1}")
}

fn rough_duration(secs: f64) -> String {
    match secs {
        s if s >= 2.0 * 3600.0 => format!("{:.0} h", s / 3600.0),
        s if s >= 120.0 => format!("{:.0} min", s / 60.0),
        s => format!("{s:.0} s"),
    }
}

fn channel_label(i: usize, n: usize) -> String {
    match (n, i) {
        (2, 0) => "L".into(),
        (2, 1) => "R".into(),
        (1, _) => "M".into(),
        _ => format!("{}", i + 1),
    }
}

/// Meter bar: RMS as a filled bar, peak as a marker; green / yellow / red zones.
fn meter(st: &Style, rms_db: f32, peak_db: f32, width: usize) -> String {
    let pos = |db: f32| (((db - METER_MIN_DB) / -METER_MIN_DB).clamp(0.0, 1.0) * width as f32).round() as usize;
    let (fill, peak) = (pos(rms_db), pos(peak_db).min(width.saturating_sub(1)));
    let zone = |i: usize| {
        let db = METER_MIN_DB + (i as f32 + 0.5) / width as f32 * -METER_MIN_DB;
        if db >= -6.0 { "31" } else if db >= -18.0 { "33" } else { "32" }
    };
    let mut out = String::new();
    for i in 0..width {
        let cell = if i < fill {
            st.paint(zone(i), "█")
        } else if i == peak && peak_db > METER_MIN_DB {
            st.paint(zone(i), "▏")
        } else {
            st.dim("·")
        };
        out.push_str(&cell);
    }
    out
}

fn fmt_db(db: f32) -> String {
    if db <= -119.0 { "  -inf".into() } else { format!("{db:6.1}") }
}

fn render(s: &Status, mode: Mode, st: &Style, tick: u64) -> Vec<String> {
    let mut lines = Vec::new();
    let rate = s.sample_rate.max(1.0);
    let secs = s.frames_in_file as f64 / rate;
    let file_name = s
        .current_file
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());

    // Header: state, elapsed, file.
    let header = match (s.recording, &file_name) {
        (true, Some(f)) => {
            let dot = if tick % 10 < 6 { st.red("●") } else { " ".into() };
            format!("{dot} {}  {}  {}", st.bold(&st.red("REC")), st.bold(&clock(secs)), f)
        }
        (true, None) => format!("{} {}  waiting for audio…", st.red("○"), st.bold(&st.red("REC"))),
        (false, _) if s.monitor.is_some() => format!("{} {}", st.green("▶"), st.bold("LISTENING")),
        (false, _) => format!("■ {}", st.bold("STOPPED")),
    };
    lines.push(header);

    if !s.attached {
        lines.push(st.red("  driver not attached — is Spool installed? (spool status)"));
        return lines;
    }

    // Format line.
    let bytes_per_s = rate * s.channels.max(1) as f64 * 4.0;
    lines.push(format!(
        "  {} kHz · {} ch · 32-bit float WAV · {}/s{}",
        format_args!("{:.1}", rate / 1000.0),
        s.channels,
        human_bytes(bytes_per_s as u64),
        if s.post_fader { " · post-fader" } else { "" }
    ));

    // Size / disk line.
    let free = s.disk_free_bytes.map(|f| {
        let left = f as f64 / bytes_per_s;
        format!("free {} (~{} left)", human_bytes(f), rough_duration(left))
    });
    let size = if s.recording { format!("size {}", human_bytes(s.file_bytes)) } else { "not recording".into() };
    lines.push(format!("  {}{}", size, free.map(|f| format!(" · {f}")).unwrap_or_default()));

    // Meters.
    let width = terminal_width().saturating_sub(30).clamp(10, 48);
    if s.levels.is_empty() || !s.device_running {
        lines.push(st.dim("  no audio — play something to Spool"));
    }
    if s.device_running {
        for (i, l) in s.levels.iter().take(8).enumerate() {
            lines.push(format!(
                "  {} {} {} dB {}",
                st.bold(&channel_label(i, s.levels.len())),
                meter(st, l.rms_db, l.peak_db, width),
                fmt_db(l.rms_db),
                st.dim(&format!("pk {}", fmt_db(l.peak_db).trim_start()))
            ));
        }
    }

    // Forwarding.
    lines.push(match &s.monitor {
        Some(m) if m.active => format!(
            "  {} {} · {:.0} ms · drift {:+} ppm · underruns {}",
            st.green("▶"),
            m.device,
            m.latency_ms,
            m.correction_ppm,
            m.underruns
        ),
        Some(m) => format!("  {} {} {}", st.red("▶"), m.device, st.red(m.error.as_deref().unwrap_or("not open"))),
        None => st.dim("  ▶ not listening (spool start)"),
    });

    // Problems.
    let problems = [
        (s.dropped_samples, "dropped samples"),
        (s.discontinuities, "gaps"),
        (s.clipped_samples, "clipped"),
    ];
    let shown: Vec<String> = problems.iter().filter(|(n, _)| *n > 0).map(|(n, t)| format!("{n} {t}")).collect();
    if !shown.is_empty() {
        lines.push(format!("  {}", st.red(&shown.join(" · "))));
    }
    if let Some(e) = &s.last_error {
        lines.push(format!("  {}", st.red(e)));
    }

    lines.push(st.dim(match mode {
        Mode::Record => "  Ctrl-C stops recording",
        Mode::Watch => "  Ctrl-C to exit",
    }));
    lines
}

/// Catches Ctrl-C from here on. Call before the first request so an early Ctrl-C (e.g. while
/// the daemon opens the output device) still ends cleanly instead of killing the CLI.
pub fn catch_ctrl_c() {
    // SAFETY: handler only stores to an atomic.
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
}

/// Runs the live view until Ctrl-C (or the recording stops elsewhere, in record mode).
pub fn run(socket: &Path, mode: Mode) {
    let st = Style { color: std::env::var_os("NO_COLOR").is_none() };
    catch_ctrl_c();
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?25l"); // hide cursor
    let mut drawn = 0usize;
    let mut last: Option<Status> = None;
    let started = Instant::now();
    let mut tick = 0u64;

    let end_note = loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            break None;
        }
        let status = match call(socket, &Request::Status) {
            Ok(Response::Ok { mut status, .. }) => {
                crate::text::clean_status(&mut status);
                status
            }
            Ok(Response::Error { message, .. }) => break Some(crate::text::clean(&message)),
            Ok(_) => break Some("unexpected response".into()),
            Err(e) => break Some(format!("lost connection to spoold: {e}")),
        };
        if mode == Mode::Record && !status.recording && started.elapsed() > Duration::from_millis(500) {
            break Some("Recording was stopped elsewhere.".into());
        }
        let lines = render(&status, mode, &st, tick);
        if drawn > 0 {
            let _ = write!(out, "\x1b[{drawn}F");
        }
        for l in &lines {
            let _ = writeln!(out, "\x1b[2K{l}");
        }
        let _ = write!(out, "\x1b[J");
        let _ = out.flush();
        drawn = lines.len();
        last = Some(status);
        tick += 1;
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = write!(out, "\x1b[?25h"); // show cursor
    let _ = out.flush();

    if let Some(note) = end_note {
        println!("{note}");
        return;
    }
    if mode == Mode::Record {
        // Ctrl-C: stop the recording, keep listening, and summarise.
        match call(socket, &Request::Stop { record_only: true }) {
            Ok(Response::Ok { mut message, .. }) => {
                message = message.map(|m| crate::text::clean(&m));
                if let Some(s) = last.filter(|s| s.frames_in_file > 0) {
                    let secs = s.frames_in_file as f64 / s.sample_rate.max(1.0);
                    println!("Recorded {} · {}.", clock(secs), human_bytes(s.file_bytes));
                }
                if let Some(m) = message {
                    println!("{m}");
                }
            }
            Ok(Response::Error { message, .. }) => eprintln!("spool: {}", crate::text::clean(&message)),
            Ok(_) => {}
            Err(e) => eprintln!("spool: cannot stop recording: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_proto::{ChannelLevel, MonitorStatus};

    fn status() -> Status {
        Status {
            attached: true,
            device_running: true,
            recording: true,
            current_file: Some("/x/Spool-2026-01-01_00-00-00.wav".into()),
            sample_rate: 48000.0,
            channels: 2,
            frames_in_file: 48000 * 83 + 24000,
            file_bytes: 31_900_000,
            disk_free_bytes: Some(212_000_000_000),
            levels: vec![ChannelLevel { peak_db: -6.0, rms_db: -12.0 }, ChannelLevel { peak_db: -3.0, rms_db: -15.0 }],
            monitor: Some(MonitorStatus {
                device: "MacBook Pro Speakers".into(),
                active: true,
                playing: true,
                latency_ms: 51.0,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn renders_recording_view() {
        let lines = render(&status(), Mode::Record, &Style { color: false }, 0);
        let text = lines.join("\n");
        assert!(text.contains("REC  00:01:23.5  Spool-2026-01-01_00-00-00.wav"), "{text}");
        assert!(text.contains("48.0 kHz · 2 ch · 32-bit float WAV · 384 KB/s"), "{text}");
        assert!(text.contains("size 31.9 MB · free 212.0 GB (~153 h left)"), "{text}");
        assert!(text.contains("  L ") && text.contains("  R "), "{text}");
        assert!(text.contains("-12.0 dB pk -6.0"), "{text}");
        assert!(text.contains("MacBook Pro Speakers · 51 ms"), "{text}");
        assert!(!text.contains("dropped"), "{text}");
    }

    #[test]
    fn shows_problems_and_idle_states() {
        let mut s = status();
        s.dropped_samples = 12;
        s.clipped_samples = 3;
        let text = render(&s, Mode::Record, &Style { color: false }, 0).join("\n");
        assert!(text.contains("12 dropped samples · 3 clipped"), "{text}");

        s.device_running = false;
        s.current_file = None;
        let text = render(&s, Mode::Record, &Style { color: false }, 0).join("\n");
        assert!(text.contains("waiting for audio"), "{text}");
        assert!(text.contains("no audio — play something to Spool"), "{text}");
    }

    #[test]
    fn meter_scales_and_formats() {
        let st = Style { color: false };
        let full = meter(&st, 0.0, 0.0, 10);
        assert_eq!(full, "██████████");
        let half = meter(&st, -30.0, -12.0, 10);
        assert_eq!(half, "█████···▏·");
        assert_eq!(fmt_db(-120.0), "  -inf");
        assert_eq!(clock(3725.25), "01:02:05.2");
        assert_eq!(human_bytes(1_234_567), "1.2 MB");
    }
}
