//! `spool`: command-line control for the recorder daemon.

mod live;
mod text;

use spool_proto::{Request, Response, Status, call, default_socket_path};
use std::io::IsTerminal;
use std::path::PathBuf;

const USAGE: &str = "\
spool - control the Spool recorder

USAGE:
    spool start [--device <DEVICE>] [--record] [--dir <PATH>]
                                 Forward Spool's audio to DEVICE so you hear it.
                                 DEVICE: name, part of a name, or UID (`spool devices`).
                                 The first time without --device you are asked to pick
                                 one; it is remembered. --record also starts recording.
    spool record [--dir <PATH>] [-d]
                                 Start recording (also starts listening if stopped) and
                                 show live levels, time and file size; Ctrl-C stops the
                                 recording. -d/--detach: start and return immediately.
    spool stop [--record]        Stop forwarding and recording; --record stops only recording
    spool split                  Close the current file and continue in a new one
    spool status [--json] [-w]   Show daemon and device state; -w/--watch: live view
    spool devices                List output devices

OPTIONS:
    --socket <PATH>              Daemon socket [default: ~/Library/Application Support/Spool/spoold.sock]
";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket = default_socket_path();
    if let Some(i) = args.iter().position(|a| a == "--socket") {
        if i + 1 >= args.len() {
            die("--socket needs a value");
        }
        socket = PathBuf::from(args.remove(i + 1));
        args.remove(i);
    }
    let json = args.iter().any(|a| a == "--json");
    args.retain(|a| a != "--json");
    let interactive = std::io::stdout().is_terminal() && !json;
    let watch = args.iter().any(|a| a == "-w" || a == "--watch");
    let detach = args.iter().any(|a| a == "-d" || a == "--detach");
    args.retain(|a| !["-w", "--watch", "-d", "--detach"].contains(&a.as_str()));
    let live_mode = match args.first().map(String::as_str) {
        Some("record") if interactive && !detach => Some(live::Mode::Record),
        Some("status") if watch => Some(live::Mode::Watch),
        _ => None,
    };
    if live_mode.is_some() {
        live::catch_ctrl_c();
    }

    let req = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["start", opts @ ..] => {
            let o = parse_opts("start", opts, &["--device", "--dir"], &["--record"]);
            Request::Start { device: o.value("--device"), record: o.flag("--record"), dir: o.dir() }
        }
        ["record", opts @ ..] => Request::Record { dir: parse_opts("record", opts, &["--dir"], &[]).dir() },
        ["stop", opts @ ..] => Request::Stop { record_only: parse_opts("stop", opts, &[], &["--record"]).flag("--record") },
        ["devices"] => Request::Devices,
        ["split"] => Request::Split,
        ["status"] | [] => Request::Status,
        ["-h" | "--help" | "help"] => {
            print!("{USAGE}");
            return;
        }
        _ => die(&format!("unrecognised command\n\n{USAGE}")),
    };

    let mut resp = call(&socket, &req);
    if let Ok(Response::Error { code: Some(code), .. }) = &resp
        && code == spool_proto::ERR_NO_DEVICE {
            resp = choose_device_and_retry(&socket, &req);
        }
    // Device names and messages can contain terminal escapes (see `text`).
    if !json
        && let Ok(r) = &mut resp
    {
        text::clean_response(r);
    }

    match resp {
        Ok(Response::Ok { status, .. }) if json => println!("{}", serde_json(&status)),
        Ok(Response::Ok { message: None, .. }) if live_mode.is_some() => {}
        Ok(Response::Ok { status, message: None }) => print_status(&status),
        Ok(Response::Ok { message: Some(m), .. }) => {
            if live_mode != Some(live::Mode::Record) {
                println!("{m}");
            }
        }
        Ok(Response::Devices { devices }) => {
            for d in devices {
                let tags = [(d.is_default, "default"), (!d.selectable, "this is Spool")]
                    .iter()
                    .filter(|(on, _)| *on)
                    .map(|(_, t)| *t)
                    .collect::<Vec<_>>();
                let tags = if tags.is_empty() { String::new() } else { format!("  ({})", tags.join(", ")) };
                println!("{}{tags}\n    uid: {}", d.name, d.uid);
            }
        }
        Ok(Response::Error { message, .. }) => die(&message),
        Err(e) => die(&format!(
            "cannot reach spoold at {}: {e}\nIs the daemon running? (launchctl list | grep spool)",
            socket.display()
        )),
    }
    if let Some(mode) = live_mode {
        live::run(&socket, mode);
    }
}

/// First `start`/`record` without a remembered device: list devices and, in a terminal,
/// ask which one to listen through, then retry with it (the daemon remembers it).
fn choose_device_and_retry(socket: &std::path::Path, req: &Request) -> std::io::Result<Response> {
    use std::io::{BufRead, IsTerminal, Write};
    let mut listing = call(socket, &Request::Devices)?;
    text::clean_response(&mut listing);
    let devices = match listing {
        Response::Devices { devices } => devices.into_iter().filter(|d| d.selectable).collect::<Vec<_>>(),
        other => return Ok(other),
    };
    if devices.is_empty() {
        die("no output devices found to listen through");
    }
    let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
    if !std::io::stdin().is_terminal() {
        die(&format!(
            "no monitoring device chosen yet; pass one with --device (it is remembered):\n  {}",
            names.join("\n  ")
        ));
    }
    println!("Which device should Spool play through? (remembered for next time)");
    for (i, d) in devices.iter().enumerate() {
        println!("  {}) {}{}", i + 1, d.name, if d.is_default { "  (current system output)" } else { "" });
    }
    let pick = loop {
        print!("Choose 1-{}: ", devices.len());
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            die("no device chosen");
        }
        match line.trim().parse::<usize>() {
            Ok(n) if (1..=devices.len()).contains(&n) => break &devices[n - 1],
            _ => println!("Please enter a number between 1 and {}.", devices.len()),
        }
    };
    // Exact name, unless another device shares it; then the unique UID.
    let dup = devices.iter().filter(|d| d.name == pick.name).count() > 1;
    let device = Some(if dup { pick.uid.clone() } else { pick.name.clone() });
    let retry = match req {
        Request::Start { record, dir, .. } => Request::Start { device, record: *record, dir: dir.clone() },
        Request::Record { dir } => Request::Start { device, record: true, dir: dir.clone() },
        other => other.clone(),
    };
    call(socket, &retry)
}

struct Opts(Vec<(String, Option<String>)>);

impl Opts {
    fn value(&self, name: &str) -> Option<String> {
        self.0.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| v.clone())
    }
    fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|(k, _)| k == name)
    }
    fn dir(&self) -> Option<PathBuf> {
        self.value("--dir").map(|d| std::fs::canonicalize(&d).unwrap_or_else(|_| PathBuf::from(d)))
    }
}

/// Parses `--key value` options and bare `--flag`s for one subcommand.
fn parse_opts(cmd: &str, opts: &[&str], valued: &[&str], flags: &[&str]) -> Opts {
    let mut out = Vec::new();
    let mut it = opts.iter();
    while let Some(&opt) = it.next() {
        if valued.contains(&opt) {
            let v = it.next().unwrap_or_else(|| die(&format!("{opt} needs a value")));
            out.push((opt.to_string(), Some(v.to_string())));
        } else if flags.contains(&opt) {
            out.push((opt.to_string(), None));
        } else {
            die(&format!("unknown option {opt:?} for {cmd}\n\n{USAGE}"));
        }
    }
    Opts(out)
}

fn serde_json(s: &Status) -> String {
    spool_proto::to_json(s)
}

fn print_status(s: &Status) {
    let yn = |b: bool| if b { "yes" } else { "no" };
    let state = match (s.monitor.is_some(), s.recording) {
        (_, true) => "recording",
        (true, false) => "listening",
        (false, false) => "stopped",
    };
    println!("state:           {state}");
    println!("driver attached: {}", yn(s.attached));
    if s.attached {
        println!("device format:   {} Hz, {} ch", s.sample_rate, s.channels);
        println!("device running:  {}", yn(s.device_running));
    }
    println!("recording:       {}{}", yn(s.recording), if s.post_fader { " (post-fader)" } else { "" });
    println!("output dir:      {}", s.output_dir.display());
    if let Some(f) = &s.current_file {
        let secs = if s.sample_rate > 0.0 { s.frames_in_file as f64 / s.sample_rate } else { 0.0 };
        println!("current file:    {} ({secs:.1} s)", f.display());
    } else if s.recording {
        println!("current file:    (waiting for audio)");
    }
    if s.dropped_samples > 0 || s.discontinuities > 0 {
        println!("dropped samples: {}  discontinuities: {}", s.dropped_samples, s.discontinuities);
    }
    match &s.monitor {
        None => println!("forwarding to:   (off)"),
        Some(m) if m.active => {
            println!(
                "forwarding to:   {} ({} Hz, {} ch) {}",
                m.device,
                m.sample_rate,
                m.channels,
                if m.playing { "playing" } else { "idle" }
            );
            println!(
                "                 latency {:.0}/{} ms, drift correction {:+} ppm, underruns {}",
                m.latency_ms, m.target_latency_ms, m.correction_ppm, m.underruns
            );
        }
        Some(m) => println!("forwarding to:   {} (not open)", m.device),
    }
    if let Some(e) = s.monitor.as_ref().and_then(|m| m.error.as_ref()) {
        println!("device error:    {e}");
    }
    if let Some(e) = &s.last_error {
        println!("last error:      {e}");
    }
}

fn die(msg: &str) -> ! {
    eprintln!("spool: {msg}");
    std::process::exit(1);
}
