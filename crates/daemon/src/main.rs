//! `spoold`: attaches to the Spool driver's shared-memory ring, records it to disk and
//! optionally plays it to a physical output device (monitoring).
//! Controlled by `spool` over a Unix socket (see `spool-proto`).

mod audio;
mod meter;
mod monitor;
mod recorder;

use monitor::Monitor;
use recorder::{Options, Recorder};
use spool_proto::{ChannelLevel, Device, MonitorStatus, Request, Response, Status};
use spool_ring::Format;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const USAGE: &str = "\
spoold - Spool recorder daemon

USAGE: spoold [OPTIONS]

OPTIONS:
    --dir <PATH>               Output directory [default: ~/Music/Spool]
    --start                    Start forwarding to a device immediately (like `spool start`)
    --device <DEVICE>          Monitoring device for --start [default: the saved one; required if none saved]
    --record                   Start recording immediately
    --max-minutes <N>          Start a new file every N minutes of audio
    --max-mb <N>               Start a new file before it reaches N MB (WAV is capped at ~4 GB)
    --post-fader               Apply the device's volume and mute to the recording
    --prefix <NAME>            File name prefix [default: Spool]
    --monitor-latency-ms <N>   Latency target when playing through a device [default: 50]
    --socket <PATH>            Control socket [default: ~/Library/Application Support/Spool/spoold.sock]
    --state <PATH>             Remembers the last device [default: ~/Library/Application Support/Spool/last-device]
    --ring <NAME>              Shared-memory ring name [default: /spool.ring.v1]
";

const DEFAULT_MONITOR_LATENCY_MS: u32 = 50;
const NO_DEVICE: &str = "no monitoring device chosen yet: run `spool start --device <DEVICE>` (see `spool devices`); it is remembered";
const MONITOR_RETRY: Duration = Duration::from_secs(2);
/// Upper bound on how long the control thread waits for a message before housekeeping.
const CONTROL_TICK: Duration = Duration::from_millis(50);

static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

struct Args {
    opts: Options,
    record: bool,
    start: bool,
    state: PathBuf,
    socket: PathBuf,
    ring: String,
    monitor: Option<String>,
    monitor_latency_ms: Option<u32>,
}

fn parse_args() -> Result<Args, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    let mut a = Args {
        opts: Options {
            dir: home.join("Music/Spool"),
            prefix: "Spool".into(),
            max_seconds: None,
            max_bytes: None,
            post_fader: false,
        },
        record: false,
        start: false,
        state: spool_proto::support_dir().join("last-device"),
        socket: spool_proto::default_socket_path(),
        ring: spool_ring::DEFAULT_NAME.into(),
        monitor: None,
        monitor_latency_ms: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        let num = |s: String| s.parse::<u64>().map_err(|e| format!("bad number {s:?}: {e}"));
        match arg.as_str() {
            "--dir" => a.opts.dir = val()?.into(),
            "--record" => a.record = true,
            "--start" => a.start = true,
            "--state" => a.state = val()?.into(),
            "--max-minutes" => a.opts.max_seconds = Some(num(val()?)? * 60),
            "--max-mb" => a.opts.max_bytes = Some(num(val()?)? * 1_000_000),
            "--post-fader" => a.opts.post_fader = true,
            "--prefix" => a.opts.prefix = val()?,
            "--device" => a.monitor = Some(val()?),
            "--monitor-latency-ms" => a.monitor_latency_ms = Some(num(val()?)?.clamp(5, 2000) as u32),
            "--socket" => a.socket = val()?.into(),
            "--ring" => a.ring = val()?,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
        }
    }
    Ok(a)
}

/// Everything the control thread acts on, in arrival order.
enum Msg {
    Command(Request, mpsc::Sender<Response>),
    Audio(audio::Chunk),
}

impl From<audio::Chunk> for Msg {
    fn from(c: audio::Chunk) -> Self {
        Msg::Audio(c)
    }
}

/// Largest accepted request line; real requests are well under 1 KiB.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// Concurrent control connections; more are refused.
const MAX_CONNECTIONS: usize = 16;
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

/// Takes an exclusive lock next to the socket so only one daemon serves it (and so drains
/// the ring). Held for the life of the process.
fn lock_instance(socket: &Path) -> Result<std::fs::File, String> {
    let mut p = socket.as_os_str().to_owned();
    p.push(".lock");
    let path = PathBuf::from(p);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("cannot open lock file {}: {e}", path.display()))?;
    // SAFETY: valid descriptor.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!("another spoold is already running (lock {} is held)", path.display()));
    }
    Ok(f)
}

/// Creates the socket's directory owner-only. The default support directory is tightened
/// even if it already exists, so the socket is never reachable by others, not even in the
/// window between bind and chmod.
fn prepare_socket_dir(socket: &Path) -> std::io::Result<()> {
    let Some(dir) = socket.parent() else { return Ok(()) };
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    if dir == spool_proto::support_dir() {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Removes a stale socket from a previous run. Refuses to delete anything that is not a
/// socket (a mistyped `--socket` must not delete a file).
fn remove_stale_socket(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket; refusing to replace it", path.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// True if the peer runs as our user (defence in depth on top of the 0600 socket).
fn peer_is_same_user(conn: &UnixStream) -> bool {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: valid descriptor and out-pointers.
    let ok = unsafe { libc::getpeereid(conn.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: no preconditions.
    ok && uid == unsafe { libc::geteuid() }
}

fn spawn_control_server(path: PathBuf, tx: mpsc::Sender<Msg>) -> std::io::Result<()> {
    prepare_socket_dir(&path)?;
    remove_stale_socket(&path)?;
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            if !peer_is_same_user(&conn) || active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                continue; // dropped = closed
            }
            if conn.set_read_timeout(Some(CONN_TIMEOUT)).is_err() || conn.set_write_timeout(Some(CONN_TIMEOUT)).is_err() {
                continue;
            }
            active.fetch_add(1, Ordering::Relaxed);
            let tx = tx.clone();
            let active = active.clone();
            std::thread::spawn(move || {
                struct Slot(Arc<AtomicUsize>);
                impl Drop for Slot {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Relaxed);
                    }
                }
                let _slot = Slot(active);
                let mut line = String::new();
                let Ok(mut w) = conn.try_clone() else { return };
                if BufReader::new(conn.take(MAX_REQUEST_BYTES + 1)).read_line(&mut line).is_err() {
                    return;
                }
                let resp = if line.len() as u64 > MAX_REQUEST_BYTES {
                    Response::error("request too large")
                } else {
                    match serde_json::from_str::<Request>(&line) {
                    Ok(req) => {
                        let (rtx, rrx) = mpsc::channel();
                        if tx.send(Msg::Command(req, rtx)).is_err() {
                            return;
                        }
                        rrx.recv_timeout(Duration::from_secs(10))
                            .unwrap_or(Response::error("daemon busy"))
                    }
                    Err(e) => Response::error(format!("bad request: {e}")),
                    }
                };
                if let Ok(mut s) = serde_json::to_string(&resp) {
                    s.push('\n');
                    let _ = w.write_all(s.as_bytes());
                }
            });
        }
    });
    Ok(())
}

/// Control side: requests, devices and files. Allowed to block; the audio thread (see
/// `audio`) keeps the ring drained, the heartbeat alive and the monitor fed meanwhile.
struct Daemon {
    rec: Recorder,
    audio: Arc<audio::Shared>,
    audio_cmds: mpsc::Sender<audio::Cmd>,
    /// Recording write failure (attach errors come from the audio thread).
    last_error: Option<String>,

    /// File remembering the last device passed to `start --device`.
    state_path: PathBuf,
    /// Device the stream is forwarded to (None = not forwarding).
    monitor_device: Option<String>,
    monitor_latency_ms: u32,
    monitor: Option<Monitor>,
    monitor_error: Option<String>,
    last_monitor_attempt: Option<Instant>,
    /// Underruns from monitor streams that were since closed (status keeps a running total).
    monitor_underruns_closed: u64,
}

impl Daemon {
    fn status(&self) -> Status {
        let snap = self.audio.snapshot();
        let (stats, fmt) = (snap.stats, snap.format);
        let (levels, clipped) = {
            let m = self.audio.meter();
            (m.levels(), m.clipped)
        };
        Status {
            attached: snap.attached,
            device_running: stats.running,
            recording: self.rec.is_recording(),
            output_dir: self.rec.opts.dir.clone(),
            current_file: self.rec.current_file().map(Into::into),
            sample_rate: fmt.map_or(0.0, |f| f.sample_rate),
            channels: fmt.map_or(0, |f| f.channels),
            frames_in_file: self.rec.frames_in_file(),
            dropped_samples: stats.dropped_samples,
            discontinuities: self.rec.discontinuities,
            post_fader: self.rec.opts.post_fader,
            last_error: self.last_error.clone().or(snap.error),
            levels: levels.into_iter().map(|l| ChannelLevel { peak_db: l.peak_db, rms_db: l.rms_db }).collect(),
            clipped_samples: clipped,
            file_bytes: self.rec.file_bytes(),
            disk_free_bytes: disk_free(&self.rec.opts.dir),
            monitor: self.monitor_device.as_ref().map(|dev| {
                let m = self.monitor.as_ref();
                let sh = m.map(|m| &m.shared);
                MonitorStatus {
                    device: m.map_or_else(|| dev.clone(), |m| m.device_name.clone()),
                    active: m.is_some(),
                    playing: sh.is_some_and(|s| s.playing.load(Ordering::Relaxed)),
                    sample_rate: m.map_or(0, |m| m.out_rate),
                    channels: m.map_or(0, |m| m.out_channels),
                    target_latency_ms: self.monitor_latency_ms,
                    latency_ms: m.map_or(0.0, |m| m.latency_now_ms()),
                    correction_ppm: sh.map_or(0, |s| s.correction_ppm.load(Ordering::Relaxed)),
                    underruns: self.monitor_underruns_closed
                        + sh.map_or(0, |s| s.underruns.load(Ordering::Relaxed)),
                    error: self.monitor_error.clone(),
                }
            }),
        }
    }

    fn handle(&mut self, req: Request) -> Response {
        let r: std::io::Result<()> = match req {
            Request::Start { device, record, dir } => match self.start(device, record, dir) {
                Ok(msg) => return self.ok(msg),
                Err(message) => return self.err(message),
            },
            Request::Record { dir } => match self.start(None, true, dir) {
                Ok(msg) => return self.ok(msg),
                Err(message) => return self.err(message),
            },
            Request::Stop { record_only } => {
                let was_listening = self.monitor_device.is_some();
                let was_recording = self.rec.is_recording();
                let saved_before = self.rec.finished.len();
                if let Err(e) = self.rec.stop() {
                    return Response::error(format!("error finalising recording: {e}"));
                }
                let saved = (self.rec.finished.len() > saved_before)
                    .then(|| self.rec.finished.last().map(|p| format!(" Saved {}.", p.display())))
                    .flatten()
                    .unwrap_or_default();
                let msg = if record_only {
                    match (was_recording, self.listening_to()) {
                        (false, _) => "Not recording.".to_string(),
                        (true, Some(d)) => format!("Stopped recording.{saved} Still listening through {d}."),
                        (true, None) => format!("Stopped recording.{saved}"),
                    }
                } else {
                    let _ = self.set_monitor(None);
                    match (was_listening, was_recording) {
                        (false, false) => "Already stopped.".to_string(),
                        (_, true) => format!("Stopped listening and recording.{saved}"),
                        (true, false) => "Stopped.".to_string(),
                    }
                };
                return self.ok(msg);
            }
            Request::Split => {
                if !self.rec.is_recording() {
                    return self.ok("Not recording.".into());
                }
                let before = self.rec.finished.len();
                if let Err(e) = self.rec.split() {
                    return Response::error(e.to_string());
                }
                return self.ok(match self.rec.finished.get(before) {
                    Some(p) => format!("Saved {}. Continuing in a new file.", p.display()),
                    None => "Nothing recorded yet; continuing.".into(),
                });
            }
            Request::Status => Ok(()),
            Request::Devices => {
                let devices = monitor::list_output_devices()
                    .into_iter()
                    .map(|d| Device {
                        selectable: d.uid != monitor::SPOOL_UID,
                        name: d.name,
                        uid: d.uid,
                        is_default: d.is_default,
                    })
                    .collect();
                return Response::Devices { devices };
            }
        };
        match r {
            Ok(()) => self.ok(String::new()),
            Err(e) => Response::error(e.to_string()),
        }
    }

    fn err(&self, message: String) -> Response {
        let code = (message == NO_DEVICE).then(|| spool_proto::ERR_NO_DEVICE.to_string());
        Response::Error { message, code }
    }

    fn ok(&self, message: String) -> Response {
        Response::Ok { status: self.status(), message: (!message.is_empty()).then_some(message) }
    }

    fn listening_to(&self) -> Option<String> {
        let dev = self.monitor_device.as_ref()?;
        Some(self.monitor.as_ref().map_or_else(|| dev.clone(), |m| m.device_name.clone()))
    }

    /// `start` and `record`. States are layered: stopped -> listening -> recording.
    /// Idempotent: repeating a command reports the current state instead of failing.
    fn start(&mut self, device: Option<String>, record: bool, dir: Option<PathBuf>) -> Result<String, String> {
        let mut notes = Vec::new();
        let before = self.listening_to();
        // Keep the current device unless a new one is named; otherwise the saved one.
        let explicit = device.is_some();
        let dev = device.or_else(|| self.monitor_device.clone()).or_else(|| self.last_device()).ok_or(NO_DEVICE)?;
        // Opens (or switches) the device first, so a typo fails before anything changes.
        self.set_monitor(Some(dev.clone()))?;
        if explicit {
            self.remember_device(&dev);
        }
        let now = self.listening_to().unwrap_or(dev);
        match before {
            None => notes.push(format!("Listening through {now}.")),
            // `record` while listening: the listening part is unchanged, don't mention it.
            Some(b) if b == now && record => {}
            Some(b) if b == now => notes.push(format!("Already listening through {now}.")),
            Some(b) => notes.push(format!("Switched listening from {b} to {now}.")),
        }
        if record {
            if self.rec.is_recording() && dir.as_ref().is_none_or(|d| *d == self.rec.opts.dir) {
                notes.push(match self.rec.current_file() {
                    Some(f) => format!("Already recording to {}.", f.display()),
                    None => "Already recording (waiting for audio).".to_string(),
                });
            } else {
                self.start_recording(dir).map_err(|e| format!("cannot start recording: {e}"))?;
                self.audio.meter().reset_clipped();
                notes.push(format!("Recording to {} (file opens when audio arrives).", self.rec.opts.dir.display()));
            }
        }
        Ok(notes.join(" "))
    }

    fn start_recording(&mut self, dir: Option<PathBuf>) -> std::io::Result<()> {
        if let Some(d) = dir {
            if self.rec.current_file().is_some() && d != self.rec.opts.dir {
                self.rec.split()?;
            }
            self.rec.opts.dir = d;
        }
        self.rec.start()
    }

    /// Checks that `dev` names a usable output device (exists, is not Spool).
    fn set_monitor_device_checked(&self, dev: &str) -> Result<(), String> {
        monitor::resolve_device(dev).map(|_| ())
    }

    fn last_device(&self) -> Option<String> {
        let s = std::fs::read_to_string(&self.state_path).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    fn remember_device(&self, dev: &str) {
        let write = || -> std::io::Result<()> {
            if let Some(d) = self.state_path.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&self.state_path, format!("{dev}\n"))
        };
        if let Err(e) = write() {
            eprintln!("spoold: cannot save last device to {}: {e}", self.state_path.display());
        }
    }

    /// Plays the stream through `device` (None = stop playing). Validates immediately so
    /// the caller gets an error for an unknown device or for Spool itself.
    fn set_monitor(&mut self, device: Option<String>) -> Result<(), String> {
        if let Some(dev) = &device {
            // Compare resolved names: "macbook" and "MacBook Pro Speakers" are the same device.
            let resolved = monitor::resolve_device(dev)?;
            let same = self.monitor.as_ref().is_some_and(|m| m.device_name == resolved);
            if !same {
                let fmt = self.audio.snapshot().format.unwrap_or(Format { sample_rate: 48000.0, channels: 2 });
                let m = Monitor::open(dev, fmt, self.monitor_latency_ms)?;
                self.close_monitor();
                eprintln!("spoold: playing through {}", m.device_name);
                self.install_monitor(m);
            }
        } else {
            self.close_monitor();
        }
        self.monitor_error = None;
        self.monitor_device = device;
        Ok(())
    }

    fn install_monitor(&mut self, mut m: Monitor) {
        let _ = self.audio_cmds.send(audio::Cmd::SetMonitor(m.take_sink()));
        self.monitor = Some(m);
    }

    fn close_monitor(&mut self) {
        if let Some(m) = self.monitor.take() {
            let _ = self.audio_cmds.send(audio::Cmd::SetMonitor(None));
            self.monitor_underruns_closed += m.shared.underruns.load(Ordering::Relaxed);
        }
    }

    /// Opens, reopens (device lost, format changed) or closes the monitor stream.
    fn ensure_monitor(&mut self) {
        let Some(dev) = self.monitor_device.clone() else {
            self.close_monitor();
            return;
        };
        let Some(fmt) = self.audio.snapshot().format else { return };
        if let Some(m) = &self.monitor {
            if m.failed() {
                self.monitor_error = Some(format!("{} stopped (unplugged?); retrying", m.device_name));
                self.close_monitor();
            } else if m.input != fmt {
                self.close_monitor();
                self.last_monitor_attempt = None;
            } else {
                return;
            }
        }
        if self.last_monitor_attempt.is_some_and(|t| t.elapsed() < MONITOR_RETRY) {
            return;
        }
        self.last_monitor_attempt = Some(Instant::now());
        match Monitor::open(&dev, fmt, self.monitor_latency_ms) {
            Ok(m) => {
                eprintln!("spoold: monitoring to {} ({} Hz, {} ch)", m.device_name, m.out_rate, m.out_channels);
                self.install_monitor(m);
                self.monitor_error = None;
            }
            Err(e) => {
                if self.monitor_error.as_deref() != Some(&e) {
                    eprintln!("spoold: monitor: {e}");
                }
                self.monitor_error = Some(e);
            }
        }
    }

    /// Writes audio from the audio thread (blocking file IO is fine here).
    fn record(&mut self, c: audio::Chunk) {
        self.audio.queued.fetch_sub(c.samples.len(), Ordering::AcqRel);
        if let Err(e) = self.rec.push(&c.samples, c.format, c.gain, c.discontinuity) {
            eprintln!("spoold: write failed, stopping recording: {e}");
            self.last_error = Some(format!("write failed: {e}"));
            let _ = self.rec.stop();
        }
        self.sync_recording();
    }

    /// Tells the audio thread whether to send audio for recording.
    fn sync_recording(&self) {
        self.audio.recording.store(self.rec.is_recording(), Ordering::Release);
    }
}

/// Free bytes on the volume holding `dir` (or its nearest existing ancestor).
fn disk_free(dir: &std::path::Path) -> Option<u64> {
    let existing = dir.ancestors().find(|p| p.exists())?;
    let c = std::ffi::CString::new(existing.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: valid C string and out-pointer.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    // SAFETY: installing a handler that only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    if let Err(e) = prepare_socket_dir(&args.socket) {
        eprintln!("spoold: cannot create {}: {e}", args.socket.display());
        std::process::exit(1);
    }
    // Single instance: a second daemon would delete this one's socket and split the ring.
    let _instance_lock = match lock_instance(&args.socket) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("spoold: {e}");
            std::process::exit(1);
        }
    };
    let (tx, rx) = mpsc::channel::<Msg>();
    if let Err(e) = spawn_control_server(args.socket.clone(), tx.clone()) {
        eprintln!("spoold: cannot listen on {}: {e}", args.socket.display());
        std::process::exit(1);
    }

    let shared = audio::Shared::new();
    let (audio_cmds, audio_rx) = mpsc::channel();
    let audio_cfg = audio::Config { ring_name: args.ring, post_fader: args.opts.post_fader };
    let audio_thread = match audio::spawn(audio_cfg, shared.clone(), tx, audio_rx, &QUIT) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("spoold: cannot start audio thread: {e}");
            std::process::exit(1);
        }
    };

    let mut d = Daemon {
        rec: Recorder::new(args.opts),
        audio: shared,
        audio_cmds,
        last_error: None,
        state_path: args.state,
        monitor_device: None,
        monitor_latency_ms: args.monitor_latency_ms.unwrap_or(DEFAULT_MONITOR_LATENCY_MS),
        monitor: None,
        monitor_error: None,
        last_monitor_attempt: None,
        monitor_underruns_closed: 0,
    };
    if args.monitor.is_some() && !args.start {
        eprintln!("spoold: --device requires --start");
        std::process::exit(2);
    }
    if args.start {
        let Some(dev) = args.monitor.or_else(|| d.last_device()) else {
            eprintln!("spoold: {NO_DEVICE}");
            std::process::exit(2);
        };
        if let Err(e) = d.set_monitor_device_checked(&dev) {
            eprintln!("spoold: {e}");
            std::process::exit(2);
        }
        d.remember_device(&dev);
        // Opened by ensure_monitor once the driver ring is attached.
        d.monitor_device = Some(dev);
    }
    if args.record
        && let Err(e) = d.rec.start() {
            eprintln!("spoold: cannot start recording: {e}");
            std::process::exit(1);
        }
    eprintln!("spoold: listening on {}, output dir {}", args.socket.display(), d.rec.opts.dir.display());

    d.sync_recording();
    while !QUIT.load(Ordering::Relaxed) {
        match rx.recv_timeout(CONTROL_TICK) {
            Ok(Msg::Command(req, reply)) => {
                let _ = reply.send(d.handle(req));
                d.sync_recording();
            }
            Ok(Msg::Audio(c)) => d.record(c),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if audio_thread.is_finished() && !QUIT.load(Ordering::Relaxed) {
            // Only on a bug (panic). Exit so launchd restarts us instead of running deaf.
            eprintln!("spoold: audio thread stopped unexpectedly; exiting");
            QUIT.store(true, Ordering::Relaxed);
            break;
        }
        d.ensure_monitor();
    }

    eprintln!("spoold: shutting down");
    // The audio thread sees QUIT within one poll; then write what it already sent.
    let _ = audio_thread.join();
    while let Ok(m) = rx.try_recv() {
        if let Msg::Audio(c) = m {
            d.record(c);
        }
    }
    d.close_monitor();
    if let Err(e) = d.rec.stop() {
        eprintln!("spoold: error finalising file: {e}");
    }
    let _ = std::fs::remove_file(&args.socket);
}
