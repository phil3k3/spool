//! File-writing half of the daemon, independent of the shared-memory ring so it can be tested.

use spool_ring::Format;
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// WAV's RIFF sizes are 32-bit; rotate comfortably before 4 GiB.
pub const WAV_HARD_LIMIT_BYTES: u64 = 4_000_000_000;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct Options {
    pub dir: PathBuf,
    pub prefix: String,
    /// Start a new file after this many seconds of audio.
    pub max_seconds: Option<u64>,
    /// Start a new file before it exceeds this many bytes (capped at [`WAV_HARD_LIMIT_BYTES`]).
    pub max_bytes: Option<u64>,
    /// Apply the device's volume/mute to the recording instead of recording the raw mix.
    pub post_fader: bool,
}

struct OpenFile {
    writer: hound::WavWriter<BufWriter<File>>,
    path: PathBuf,
    format: Format,
    frames: u64,
    last_flush: Instant,
}

pub struct Recorder {
    pub opts: Options,
    recording: bool,
    file: Option<OpenFile>,
    pub discontinuities: u64,
    /// Files completed since start, most recent last.
    pub finished: Vec<PathBuf>,
}

impl Recorder {
    pub fn new(opts: Options) -> Self {
        Self { opts, recording: false, file: None, discontinuities: 0, finished: Vec::new() }
    }

    pub fn is_recording(&self) -> bool {
        self.recording
    }
    pub fn current_file(&self) -> Option<&Path> {
        self.file.as_ref().map(|f| f.path.as_path())
    }
    pub fn frames_in_file(&self) -> u64 {
        self.file.as_ref().map_or(0, |f| f.frames)
    }
    /// Bytes of the current file (44-byte WAV header + 32-bit samples).
    pub fn file_bytes(&self) -> u64 {
        self.file.as_ref().map_or(0, |f| 44 + f.frames * 4 * f.format.channels.max(1) as u64)
    }

    /// Arms recording. The file is created when the first audio arrives, so its header
    /// always matches the stream format.
    pub fn start(&mut self) -> io::Result<()> {
        std::fs::create_dir_all(&self.opts.dir)?;
        self.recording = true;
        Ok(())
    }

    pub fn stop(&mut self) -> io::Result<()> {
        self.recording = false;
        self.close()
    }

    /// Closes the current file; recording continues in a new one.
    pub fn split(&mut self) -> io::Result<()> {
        self.close()
    }

    fn close(&mut self) -> io::Result<()> {
        if let Some(f) = self.file.take() {
            f.writer.finalize().map_err(to_io)?;
            if f.frames == 0 {
                let _ = std::fs::remove_file(&f.path);
            } else {
                self.finished.push(f.path);
            }
        }
        Ok(())
    }

    /// Appends interleaved samples. `gain` is the device's amplitude factor (0 if muted).
    pub fn push(&mut self, samples: &[f32], format: Format, gain: f32, discontinuity: bool) -> io::Result<()> {
        if discontinuity {
            self.discontinuities += 1;
        }
        if !self.recording || samples.is_empty() {
            return Ok(());
        }
        let channels = format.channels.max(1) as usize;
        let mut rest = samples;
        while !rest.is_empty() {
            if self.file.as_ref().is_some_and(|f| f.format != format) {
                self.close()?;
            }
            if self.file.is_none() {
                self.file = Some(self.open(format)?);
            }
            // How many frames fit before rotation.
            let f = self.file.as_ref().unwrap();
            let room = self.frames_until_rotation(f).max(1);
            let frames = (rest.len() / channels).min(room as usize);
            let (now, later) = rest.split_at(frames * channels);
            let f = self.file.as_mut().unwrap();
            let g = if self.opts.post_fader { gain } else { 1.0 };
            for &s in now {
                f.writer.write_sample(if g == 1.0 { s } else { s * g }).map_err(to_io)?;
            }
            f.frames += frames as u64;
            if f.last_flush.elapsed() >= FLUSH_INTERVAL {
                // Keeps the header valid on disk so a crash loses at most ~1 s.
                f.writer.flush().map_err(to_io)?;
                f.last_flush = Instant::now();
            }
            if self.frames_until_rotation(self.file.as_ref().unwrap()) == 0 {
                self.close()?;
            }
            rest = later;
        }
        Ok(())
    }

    fn frames_until_rotation(&self, f: &OpenFile) -> u64 {
        let bytes_per_frame = 4 * f.format.channels.max(1) as u64;
        let max_bytes = self.opts.max_bytes.unwrap_or(u64::MAX).min(WAV_HARD_LIMIT_BYTES);
        let by_size = (max_bytes.saturating_sub(44) / bytes_per_frame).max(1);
        let by_time = self.opts.max_seconds.map_or(u64::MAX, |s| (s as f64 * f.format.sample_rate) as u64).max(1);
        by_size.min(by_time).saturating_sub(f.frames)
    }

    fn open(&self, format: Format) -> io::Result<OpenFile> {
        if !format.is_valid() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsupported stream format {format:?}")));
        }
        let spec = hound::WavSpec {
            channels: format.channels as u16,
            sample_rate: format.sample_rate.round() as u32,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let path = unique_path(&self.opts.dir, &self.opts.prefix, &timestamp());
        let file = File::options().write(true).create_new(true).open(&path)?;
        let writer = hound::WavWriter::new(BufWriter::with_capacity(1 << 16, file), spec).map_err(to_io)?;
        Ok(OpenFile { writer, path, format, frames: 0, last_flush: Instant::now() })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn to_io(e: hound::Error) -> io::Error {
    match e {
        hound::Error::IoError(e) => e,
        e => io::Error::other(e),
    }
}

fn unique_path(dir: &Path, prefix: &str, stamp: &str) -> PathBuf {
    let base = dir.join(format!("{prefix}-{stamp}.wav"));
    if !base.exists() {
        return base;
    }
    (1..).map(|i| dir.join(format!("{prefix}-{stamp}-{i}.wav"))).find(|p| !p.exists()).unwrap()
}

/// Local time as `YYYY-MM-DD_HH-MM-SS`.
fn timestamp() -> String {
    // SAFETY: time/localtime_r with valid pointers.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO: Format = Format { sample_rate: 48000.0, channels: 2 };

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spool-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn opts(dir: &Path) -> Options {
        Options { dir: dir.into(), prefix: "t".into(), max_seconds: None, max_bytes: None, post_fader: false }
    }

    fn read(path: &Path) -> (hound::WavSpec, Vec<f32>) {
        let mut r = hound::WavReader::open(path).unwrap();
        (r.spec(), r.samples::<f32>().map(Result::unwrap).collect())
    }

    #[test]
    fn writes_bit_exact_float_wav() {
        let dir = tmpdir("exact");
        let mut rec = Recorder::new(opts(&dir));
        rec.push(&[9.0; 4], STEREO, 1.0, false).unwrap(); // not recording: ignored
        rec.start().unwrap();
        let samples: Vec<f32> = (0..9600).map(|i| (i as f32 * 0.37).sin() * 0.9).collect();
        rec.push(&samples, STEREO, 0.25, false).unwrap();
        rec.stop().unwrap();
        let (spec, got) = read(&rec.finished[0]);
        assert_eq!((spec.channels, spec.sample_rate, spec.bits_per_sample), (2, 48000, 32));
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        assert_eq!(got, samples, "pre-fader recording must be bit-exact");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn post_fader_applies_gain() {
        let dir = tmpdir("fader");
        let mut rec = Recorder::new(Options { post_fader: true, ..opts(&dir) });
        rec.start().unwrap();
        rec.push(&[0.8; 8], STEREO, 0.5, false).unwrap();
        rec.stop().unwrap();
        assert_eq!(read(&rec.finished[0]).1, vec![0.4; 8]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rotates_by_duration_and_on_format_change() {
        let dir = tmpdir("rotate");
        let mut rec = Recorder::new(Options { max_seconds: Some(1), ..opts(&dir) });
        rec.start().unwrap();
        let one_and_half_sec = vec![0.1f32; 72000 * 2];
        rec.push(&one_and_half_sec, STEREO, 1.0, false).unwrap();
        let mono96 = Format { sample_rate: 96000.0, channels: 1 };
        rec.push(&[0.2; 10], mono96, 1.0, true).unwrap();
        rec.stop().unwrap();
        let lens: Vec<_> = rec.finished.iter().map(|p| read(p)).map(|(s, v)| (s.channels, s.sample_rate, v.len())).collect();
        assert_eq!(lens, vec![(2, 48000, 96000), (2, 48000, 48000), (1, 96000, 10)]);
        assert_eq!(rec.discontinuities, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rotates_by_size_and_drops_empty_files() {
        let dir = tmpdir("size");
        let mut rec = Recorder::new(Options { max_bytes: Some(44 + 8 * 100), ..opts(&dir) });
        rec.start().unwrap();
        rec.push(&vec![0.5f32; 250 * 2], STEREO, 1.0, false).unwrap();
        rec.split().unwrap();
        rec.split().unwrap(); // nothing open: no empty file
        rec.stop().unwrap();
        let frames: Vec<_> = rec.finished.iter().map(|p| read(p).1.len() / 2).collect();
        assert_eq!(frames, vec![100, 100, 50]);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }
}
