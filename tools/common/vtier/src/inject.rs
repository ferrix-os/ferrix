//! Faults on demand, for a control: the trigger is a file outside the mount,
//! named by `VTIER_TEST_TRIGGER`. `flip` flips one stored bit at the next read,
//! `stall` hangs every device call from now on. Removed after it fires, for `flip`.
use std::path::PathBuf;

fn trigger() -> Option<PathBuf> {
    std::env::var_os("VTIER_TEST_TRIGGER").map(PathBuf::from)
}
fn says(what: &str) -> bool {
    trigger()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|t| t.trim() == what)
}
/// Whether to flip a bit now; once for each trigger written.
#[must_use]
pub fn flip_now() -> bool {
    let now = says("flip");
    if now && let Some(p) = trigger() {
        let _ = std::fs::remove_file(p);
    }
    now
}
/// Whether the device is to hang.
#[must_use]
pub fn stall_now() -> bool {
    says("stall")
}
