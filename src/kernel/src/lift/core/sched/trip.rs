//! The ring-trip measure is the kernel's (docs/microkernel/M5-SHIM-MAP.md: N).
use super::WaitQueue;
pub fn answered(_: &WaitQueue) {}
pub fn bell(_: &WaitQueue, _: &WaitQueue) {}
pub fn on_ring(_: &WaitQueue, _: &WaitQueue) {}
pub fn queued(_: &WaitQueue) {}
pub fn reader_running(_: &WaitQueue) {}
pub fn ring_running(_: &WaitQueue) {}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    RootInstall,
    RootUninstall,
    DeviceInterrupt,
}
pub fn count(what: Count) {}
pub fn interrupt_queued(port: &WaitQueue) {}
pub fn port_rung(port: &WaitQueue) {}
pub fn port_taken(port: &WaitQueue, interrupt: bool) {}
