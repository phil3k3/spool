//! Control protocol between `spool` (CLI) and `spoold` (recorder daemon).
//!
//! Transport: a Unix domain socket in the user's Application Support directory, one
//! JSON-encoded [`Request`] per line, answered by one JSON-encoded [`Response`] line.
//! (The design doc suggests XPC/Mach; both endpoints run as the same user, so a socket
//! with 0600 permissions is sufficient and far simpler. The driver is not involved.)

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Default socket path: `~/Library/Application Support/Spool/spoold.sock`.
pub fn default_socket_path() -> PathBuf {
    support_dir().join("spoold.sock")
}

/// `~/Library/Application Support/Spool`.
pub fn support_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/Spool")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Start forwarding the Spool stream to an output device (name, UID or unique name
    /// fragment; default: the remembered one, error if none). `record` also starts
    /// recording, like a following `Record`.
    Start {
        #[serde(default)]
        device: Option<String>,
        #[serde(default)]
        record: bool,
        #[serde(default)]
        dir: Option<PathBuf>,
    },
    /// Start recording. `dir` overrides the output directory for this and later recordings.
    Record {
        #[serde(default)]
        dir: Option<PathBuf>,
    },
    /// Stop recording, and forwarding unless `record_only`.
    Stop {
        #[serde(default)]
        record_only: bool,
    },
    Status,
    /// Close the current file and continue in a new one.
    Split,
    /// List output devices usable with `Start { device }`.
    Devices,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub name: String,
    pub uid: String,
    /// Current system default output.
    pub is_default: bool,
    /// False for Spool itself (monitoring to it would loop).
    pub selectable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
// One message per request; boxing `Status` would only add noise.
#[allow(clippy::large_enum_variant)]
pub enum Response {
    Ok {
        status: Status,
        /// One-line description of what the command changed (or that nothing needed to).
        #[serde(default)]
        message: Option<String>,
    },
    Devices { devices: Vec<Device> },
    Error {
        message: String,
        /// Machine-readable reason, e.g. [`ERR_NO_DEVICE`].
        #[serde(default)]
        code: Option<String>,
    },
}

/// `start`/`record` without `--device` and no remembered device. Interactive clients
/// can ask the user to pick one and retry with `device` set.
pub const ERR_NO_DEVICE: &str = "no_device";

impl Response {
    pub fn error(message: impl Into<String>) -> Self {
        Response::Error { message: message.into(), code: None }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// Daemon is attached to the driver's shared-memory ring.
    pub attached: bool,
    /// The virtual device is currently doing IO (some app is playing to it).
    pub device_running: bool,
    pub recording: bool,
    pub output_dir: PathBuf,
    pub current_file: Option<PathBuf>,
    pub sample_rate: f64,
    pub channels: u32,
    /// Frames written to the current file.
    pub frames_in_file: u64,
    /// Samples the driver had to drop because the daemon fell behind (since driver start).
    pub dropped_samples: u64,
    /// Number of discontinuities (lost data) seen while recording.
    pub discontinuities: u64,
    pub post_fader: bool,
    pub last_error: Option<String>,
    #[serde(default)]
    pub monitor: Option<MonitorStatus>,
    /// Per-channel levels of what is being recorded (decaying meters).
    #[serde(default)]
    pub levels: Vec<ChannelLevel>,
    /// Samples at or above full scale in the current recording.
    #[serde(default)]
    pub clipped_samples: u64,
    /// Size of the current file on disk.
    #[serde(default)]
    pub file_bytes: u64,
    /// Free space on the volume holding `output_dir`.
    #[serde(default)]
    pub disk_free_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ChannelLevel {
    /// Peak with 20 dB/s fall, dBFS.
    pub peak_db: f32,
    /// RMS, 300 ms average, dBFS.
    pub rms_db: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MonitorStatus {
    /// Selected device (as configured).
    pub device: String,
    /// The output stream is open.
    pub active: bool,
    /// Audio is currently flowing (queue primed).
    pub playing: bool,
    pub sample_rate: u32,
    pub channels: u16,
    pub target_latency_ms: u32,
    pub latency_ms: f64,
    /// Clock-drift correction currently applied, in ppm.
    pub correction_ppm: i64,
    pub underruns: u64,
    pub error: Option<String>,
}

/// Sends one request and waits for the response.
pub fn call(socket: &std::path::Path, req: &Request) -> io::Result<Response> {
    let mut s = UnixStream::connect(socket)?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut r = BufReader::new(s);
    let mut resp = String::new();
    r.read_line(&mut resp)?;
    serde_json::from_str(&resp).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Pretty JSON for scripting (`spool status --json`).
pub fn to_json(s: &Status) -> String {
    serde_json::to_string_pretty(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
        let r: Request = serde_json::from_str(r#"{"cmd":"start","record":true}"#).unwrap();
        assert_eq!(r, Request::Start { device: None, record: true, dir: None });
        let r: Request = serde_json::from_str(r#"{"cmd":"stop"}"#).unwrap();
        assert_eq!(r, Request::Stop { record_only: false });
        let r: Request = serde_json::from_str(r#"{"cmd":"record","dir":"/x"}"#).unwrap();
        assert_eq!(r, Request::Record { dir: Some("/x".into()) });
    }
}
