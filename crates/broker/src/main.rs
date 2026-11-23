//! `spool-broker`: the system-domain Mach service of the XPC transport spike (docs/XPC.md).
//!
//! Run by launchd from `/Library/LaunchDaemons/<prefix>.spool-broker.plist` (see
//! scripts/spool-id.sh), on
//! demand. It only answers two questions for the spike:
//!
//! 1. Can the driver, inside the Core Audio sandbox, reach a LaunchDaemon's service and send
//!    it a memory entry? (`deposit`, logged with the sender's pid and uid)
//! 2. Can a process of the logged-in user fetch that entry and map it? (`spool-broker --probe`)
//!
//! Spike only: there is no authentication and the memory is a probe page, never audio.

use spool_mach::{Port, Request, SERVICE_NAME, Shared};
use std::ffi::{CStr, CString};
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
usage: spool-broker            serve (launchd runs this; run by hand it registers in your
                               login session only, which the driver cannot see)
       spool-broker --probe    fetch the driver's probe page through the broker and report

Logs: log stream --predicate 'eventMessage CONTAINS \"Spool\"'";

fn log(msg: &str) {
    eprintln!("{msg}");
    if let Ok(c) = CString::new(msg) {
        // SAFETY: format string and argument are valid C strings.
        unsafe { libc::syslog(libc::LOG_NOTICE, c"%s".as_ptr(), c.as_ptr()) };
    }
}

fn user_name(uid: u32) -> String {
    // SAFETY: getpwuid's static buffer is only used here, and this process is single-threaded.
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return format!("uid {uid}");
    }
    // SAFETY: pw_name is a valid C string while pw is.
    unsafe { CStr::from_ptr((*pw).pw_name) }.to_string_lossy().into_owned()
}

fn describe(mem: &Shared) -> String {
    match mem.read_probe() {
        Some((pid, uid)) => format!("probe page written by pid {pid} ({})", user_name(uid)),
        None => "no probe magic (unexpected contents)".into(),
    }
}

fn serve() -> ExitCode {
    let port = match spool_mach::check_in(SERVICE_NAME) {
        Ok(p) => p,
        Err(e) => {
            log(&format!("Spool broker: cannot check in as {SERVICE_NAME}: {e} (start it through launchd)"));
            return ExitCode::FAILURE;
        }
    };
    // SAFETY: trivial libc calls.
    let (pid, uid) = unsafe { (libc::getpid(), libc::geteuid()) };
    log(&format!("Spool broker: serving {SERVICE_NAME} (pid {pid}, {})", user_name(uid)));

    let mut stored: Option<(Port, u64)> = None;
    loop {
        match spool_mach::receive(&port, None) {
            Ok(Request::Deposit { entry, size, sender }) => {
                let verdict = match Shared::map(&entry, size) {
                    Ok(m) => describe(&m),
                    Err(e) => format!("cannot map it: {e}"),
                };
                log(&format!(
                    "Spool broker: deposit from pid {} ({}): {size} bytes, {verdict}",
                    sender.pid,
                    user_name(sender.euid)
                ));
                stored = Some((entry, size));
            }
            Ok(Request::Fetch { reply, sender }) => {
                log(&format!(
                    "Spool broker: fetch from pid {} ({}): {}",
                    sender.pid,
                    user_name(sender.euid),
                    if stored.is_some() { "handing over the entry" } else { "nothing deposited yet" }
                ));
                if let Err(e) = spool_mach::reply_fetch(reply, stored.as_ref().map(|(p, s)| (p, *s))) {
                    log(&format!("Spool broker: reply failed: {e}"));
                }
            }
            Ok(Request::Rejected { id, sender }) => {
                log(&format!("Spool broker: ignored message {id:#x} from pid {}", sender.pid));
            }
            Err(e) => {
                log(&format!("Spool broker: receive failed: {e}"));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn probe() -> ExitCode {
    // SAFETY: trivial libc calls.
    let (pid, uid) = unsafe { (libc::getpid(), libc::geteuid()) };
    println!("probe: running as pid {pid} ({})", user_name(uid));
    let svc = match spool_mach::look_up(SERVICE_NAME) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL  lookup of {SERVICE_NAME}: {e}");
            println!("      Is the broker installed? sudo launchctl print system/{SERVICE_NAME}");
            return ExitCode::FAILURE;
        }
    };
    println!("ok    looked up {SERVICE_NAME}");
    let fetched = match spool_mach::fetch(&svc, 2000) {
        Ok(f) => f,
        Err(e) => {
            println!("FAIL  fetch: {e}");
            return ExitCode::FAILURE;
        }
    };
    let r = fetched.responder;
    let who = user_name(r.euid);
    println!("ok    answered by pid {} ({who})", r.pid);
    if who != "nobody" {
        println!("WARN  expected the LaunchDaemon (user nobody). Something in your session registered");
        println!("      the name and shadows the real broker; a real client must refuse this.");
    }
    match fetched.entry {
        None => {
            println!("FAIL  the broker has nothing: the driver never reached it.");
            println!("      Check the driver's line in: log show --last 10m --predicate 'eventMessage CONTAINS \"Spool\"'");
            ExitCode::FAILURE
        }
        Some((entry, size)) => match Shared::map(&entry, size) {
            Err(e) => {
                println!("FAIL  got an entry but cannot map it: {e}");
                ExitCode::FAILURE
            }
            Ok(m) => {
                println!("ok    mapped {size} bytes: {}", describe(&m));
                if m.read_probe().is_some() {
                    println!("PASS  driver → broker → user process works");
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
        },
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => serve(),
        ["--probe"] => probe(),
        ["-h" | "--help"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
