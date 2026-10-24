//! Where the settings writer is absent (`docs/SETTINGS-WRITER.md` §12.8)
//! there is no trusted prompt: no channel, nothing to draw, no key of its
//! own. The frame's loop asks the same questions of this as of
//! `prompt.rs`, and every answer is "nothing".

use std::os::fd::RawFd;
use std::time::Instant;

use crate::overlay::Picture;

/// Nothing is adopted: there is no prompt channel in this build.
#[must_use]
pub const fn adopt() -> Option<bool> {
    None
}

/// No prompt, ever.
#[derive(Debug, Default)]
pub(crate) struct Prompts;

#[expect(
    clippy::unused_self,
    clippy::needless_pass_by_ref_mut,
    reason = "the same calls as prompt.rs's, each answering nothing"
)]
impl Prompts {
    pub(crate) const fn adopted() -> Prompts {
        Prompts
    }
    pub(crate) const fn take_said(&mut self) -> Vec<String> {
        Vec::new()
    }
    pub(crate) const fn raw_fd(&self) -> Option<RawFd> {
        None
    }
    pub(crate) const fn up(&self) -> bool {
        false
    }
    pub(crate) const fn asking(&self) -> bool {
        false
    }
    pub(crate) const fn read(&mut self, _locked: bool, _screens: bool) -> bool {
        false
    }
    pub(crate) fn note_held(&mut self, _held: impl IntoIterator<Item = u32>) {}
    pub(crate) const fn trusted_key(&mut self, _code: u16, _pressed: bool, _repeat: bool) -> bool {
        false
    }
    pub(crate) const fn key(&mut self, _code: u16, _pressed: bool, _keysym: Option<&str>) -> bool {
        false
    }
    pub(crate) const fn button(&mut self, _button: u32, _pressed: bool, _at: (f64, f64)) -> bool {
        false
    }
    pub(crate) const fn lock_taken(&mut self) {}
    pub(crate) const fn next_wake(&self) -> Option<Instant> {
        None
    }
    pub(crate) const fn tick(&mut self, _now: Instant) -> bool {
        false
    }
    pub(crate) const fn begin_frame(&mut self) {}
    pub(crate) const fn picture(
        &mut self,
        _size: (u32, u32),
        _scale: f64,
        _origin: (i64, i64),
    ) -> Option<Picture> {
        None
    }
}
