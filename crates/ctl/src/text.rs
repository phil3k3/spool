//! Makes daemon-supplied text safe to print to a terminal.
//!
//! Device names come from hardware and from the network (AirPlay receivers advertise their
//! own names, Bluetooth devices name themselves), and they also end up in error messages.
//! Printed raw, an escape sequence in such a name could rewrite the screen, set the window
//! title or, in some terminals, inject input. `--json` output is left untouched (JSON
//! escapes control characters itself).

use spool_proto::{Response, Status};
use std::path::PathBuf;

/// Replaces control characters and bidirectional overrides with visible escapes.
pub fn clean(s: &str) -> String {
    if !s.chars().any(unsafe_char) {
        return s.to_string();
    }
    s.chars().map(|c| if unsafe_char(c) { c.escape_unicode().to_string() } else { c.to_string() }).collect()
}

fn unsafe_char(c: char) -> bool {
    // is_control: C0, DEL and C1 (includes ESC and the 8-bit CSI). The rest are bidi
    // embeddings/overrides/isolates, which can make text display differently from its content.
    c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

fn clean_path(p: &mut PathBuf) {
    if let Some(s) = p.to_str()
        && s.chars().any(unsafe_char)
    {
        *p = PathBuf::from(clean(s));
    }
}

fn clean_opt(s: &mut Option<String>) {
    if let Some(v) = s {
        *v = clean(v);
    }
}

pub fn clean_status(s: &mut Status) {
    clean_path(&mut s.output_dir);
    if let Some(p) = &mut s.current_file {
        clean_path(p);
    }
    clean_opt(&mut s.last_error);
    if let Some(m) = &mut s.monitor {
        m.device = clean(&m.device);
        clean_opt(&mut m.error);
    }
}

pub fn clean_response(r: &mut Response) {
    match r {
        Response::Ok { status, message } => {
            clean_status(status);
            clean_opt(message);
        }
        Response::Devices { devices } => {
            for d in devices {
                d.name = clean(&d.name);
                d.uid = clean(&d.uid);
            }
        }
        Response::Error { message, .. } => *message = clean(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_controls_and_bidi() {
        assert_eq!(clean("MacBook Pro Speakers"), "MacBook Pro Speakers");
        assert_eq!(clean("Café ♫"), "Café ♫");
        assert_eq!(clean("evil\x1b]0;pwned\x07"), "evil\\u{1b}]0;pwned\\u{7}");
        assert_eq!(clean("a\u{9b}31m"), "a\\u{9b}31m");
        assert_eq!(clean("x\u{202E}gpj"), "x\\u{202e}gpj");
        assert_eq!(clean("line\nbreak"), "line\\u{a}break");
    }
}
