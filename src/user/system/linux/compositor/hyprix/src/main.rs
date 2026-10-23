//! The compositor's command line.
//!
//! Everything it does is in the library beside this, so a test can run the
//! compositor in a thread rather than as a process and compare the pixels it
//! drew.

use std::io::Write;

use hyprix::Options;

fn main() {
    // Before any thread or child: a session running as its user gets its
    // devices from `sessiond` over a channel this takes out of the
    // environment, so nothing the compositor starts inherits it.
    if compositor_seat::adopt() {
        report("hyprix: seat0's devices come from sessiond");
    }
    // And the lock channel beside it: the session's locks then go only on
    // authd's grant (docs/AUTH.md §3.7).
    match hyprix::grants::adopt() {
        Some(true) => report("hyprix: the session's locks go on authd's grant, through sessiond"),
        Some(false) => report(
            "hyprix: sessiond named a lock channel that could not be taken; this session's locks \
             are refused",
        ),
        None => {}
    }
    // And the session's token, which lode trusts a screenshot provider by
    // (docs/LOGGING.md §19): out of the environment before any child.
    let lode = hyprix::lodeshot::adopt();
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => Options { lode, ..options },
        Err(message) => {
            report(&format!("hyprix: {message}"));
            report(Options::USAGE);
            std::process::exit(2);
        }
    };
    match hyprix::run_with(&options, &mut |line| report(line)) {
        Ok(line) => report(&line),
        Err(error) => {
            report(&format!("hyprix: failed: {error}"));
            std::process::exit(1);
        }
    }
}

/// Say one line on standard output.
///
/// A compositor's own output is a log, not a client's, so it goes out
/// whatever happens to the screen; the display test reads it.
fn report(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
