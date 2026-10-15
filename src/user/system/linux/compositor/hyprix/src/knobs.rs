//! Knobs a test boot sets in hyprix's environment, read once.
//!
//! * `HYPRIX_HOLD_FRAME_MS=<n>`: every frame is held `n` milliseconds after
//!   its frame callbacks go, so input arrives while one is drawn
//!   (`cargo xtask test-compositor --boot locked-held`).
//! * `HYPRIX_INPUT_TRACE=1`: a line for each pass that read a device and for
//!   each relative motion carried out, so a boot can count the motions in
//!   and out.

use std::sync::OnceLock;
use std::time::Duration;

/// How long each frame is held, if a test asked.
pub(crate) fn hold_frame() -> Option<Duration> {
    static HOLD: OnceLock<Option<Duration>> = OnceLock::new();
    *HOLD.get_or_init(|| {
        std::env::var("HYPRIX_HOLD_FRAME_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|millis| *millis > 0)
            .map(Duration::from_millis)
    })
}

/// Whether input is traced.
pub(crate) fn trace_input() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| std::env::var("HYPRIX_INPUT_TRACE").is_ok_and(|value| value == "1"))
}
