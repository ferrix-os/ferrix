//! linuxd's watchdog: none. The deadline, its task and the chipset's timer are
//! ring 0's (`src/kernel/src/watchdog.rs`, `docs/POSTMORTEM.md` §6.11); in
//! linuxd `/dev/watchdog` is never served, so its open is `ENXIO` and every
//! stop on purpose finds nothing to disarm.

/// The device is open already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

pub fn served() -> bool {
    false
}
pub fn previous_reset() -> bool {
    false
}
pub fn hardware() -> bool {
    false
}
pub fn open(_pid: u32, _name: &[u8]) -> Result<(), Busy> {
    Err(Busy)
}
pub fn feed() -> bool {
    false
}
pub fn set_timeout(_seconds: u32) -> Option<u32> {
    None
}
pub fn timeout() -> u32 {
    0
}
pub fn time_left() -> Option<u64> {
    None
}
pub fn armed() -> bool {
    false
}
pub fn enable() {}
pub fn disable(_pid: u32) {}
pub fn release(_pid: u32, _magic: bool) {}
pub fn stop_for_shutdown(_why: &str) {}
