//! XPC transport spike, driver side (feature `xpc-probe`, off by default; see docs/XPC.md).
//!
//! On Initialize, a helper thread looks up the broker's Mach service and deposits a probe
//! page (a Mach memory entry holding magic, pid and uid). Every outcome is logged once, so
//! one `log show` answers: where is the plug-in hosted, may it look up a LaunchDaemon's
//! service from its sandbox, and can it send a memory entry. Audio still goes through the
//! shared-memory ring; nothing here touches the IO path.

use crate::log;
use spool_mach::{PROBE_SIZE, SERVICE_NAME, Shared};
use std::ffi::{CStr, c_char};

unsafe extern "C" {
    fn getprogname() -> *const c_char;
}

pub fn spawn() {
    let spawned = std::thread::Builder::new().name("Spool XPC probe".into()).spawn(run);
    if let Err(e) = spawned {
        log(&format!("Spool: XPC probe: cannot start thread: {e}"));
    }
}

fn run() {
    // SAFETY: trivial libc calls; getprogname returns a static string.
    let (pid, uid) = unsafe { (libc::getpid(), libc::geteuid()) };
    let prog = unsafe { CStr::from_ptr(getprogname()) }.to_string_lossy().into_owned();
    log(&format!("Spool: XPC probe: plug-in hosted by {prog} (pid {pid}, uid {uid})"));

    let service = match spool_mach::look_up(SERVICE_NAME) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("Spool: XPC probe: FAIL lookup of {SERVICE_NAME}: {e}"));
            return;
        }
    };
    log(&format!("Spool: XPC probe: ok looked up {SERVICE_NAME}"));

    let (mem, entry) = match Shared::create(PROBE_SIZE) {
        Ok(x) => x,
        Err(e) => {
            log(&format!("Spool: XPC probe: FAIL cannot create memory entry: {e}"));
            return;
        }
    };
    mem.write_probe(pid as u32, uid);
    match spool_mach::send_deposit(&service, &entry, PROBE_SIZE, 1000) {
        Ok(()) => log("Spool: XPC probe: PASS deposited the probe page with the broker"),
        Err(e) => log(&format!("Spool: XPC probe: FAIL deposit: {e}")),
    }
    // `mem` is unmapped here; the broker's copy of the entry keeps the pages alive.
}
