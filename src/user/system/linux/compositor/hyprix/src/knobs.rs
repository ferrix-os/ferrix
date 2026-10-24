//! Knobs a test boot sets in hyprix's environment, read once.
//!
//! * `HYPRIX_HOLD_FRAME_MS=<n>`: every frame is held `n` milliseconds after
//!   its frame callbacks go, so input arrives while one is drawn
//!   (`cargo xtask test-compositor --boot locked-held`).
//! * `HYPRIX_INPUT_TRACE=1`: a line for each pass that read a device and for
//!   each relative motion carried out, so a boot can count the motions in
//!   and out.
//! * `HYPRIX_DIRECT_SCANOUT=0`: a fullscreen client's buffer is copied as
//!   before, never handed to the card (`crate::frame::scanned_out`): the
//!   way back on a machine where showing it directly misbehaves, and the
//!   "before" of a measurement. Hyprland's own `render:direct_scanout`
//!   stays its KMS-plane option with its default of 0, which this is not.
//! * `HYPRIX_SYNC_FLIP=1`: a card's page flips and dirty flushes are made
//!   on the loop and waited for, as before they went to a thread of their
//!   own (`crate::flush::FlipThread`): the "before" of a measurement, and
//!   the way back where the thread misbehaves.

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

/// Whether a client's buffer may be handed to the card (on unless
/// `HYPRIX_DIRECT_SCANOUT=0`).
pub(crate) fn direct_scanout() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("HYPRIX_DIRECT_SCANOUT").map_or(true, |value| value != "0"))
}

/// Whether input is traced.
pub(crate) fn trace_input() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| std::env::var("HYPRIX_INPUT_TRACE").is_ok_and(|value| value == "1"))
}

/// Whether flips are made on the loop and waited for (`HYPRIX_SYNC_FLIP=1`).
pub(crate) fn sync_flip() -> bool {
    static SYNC: OnceLock<bool> = OnceLock::new();
    *SYNC.get_or_init(|| std::env::var("HYPRIX_SYNC_FLIP").is_ok_and(|value| value == "1"))
}
