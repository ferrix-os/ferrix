use crate::device::DeviceNode;
use crate::device::DmaShape;
use crate::device::Location;
use crate::device::Reserved;
use crate::fallible;
use crate::hooks::Full;
use crate::hooks::Hooks;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use ferrix_bootinfo::BootView;
use ferrix_fdt::Fdt;
use ferrix_fdt::GicInterrupt;
#[derive(Debug)]
pub struct BoardDevice {
    pub registers: Vec<(u64, u64)>,
    pub interrupt: GicInterrupt,
    pub dma: DmaShape,
    pub summary: String,
}
pub type Prepare = fn(&Fdt<'_>) -> Result<Option<BoardDevice>, &'static str>;
pub type Clock = fn(u64, bool) -> Result<u64, &'static str>;
#[derive(Debug)]
pub struct BoardBinding {
    pub binding: u16,
    pub label: &'static str,
    pub device: &'static str,
    pub prepare: Prepare,
    pub clock: Option<Clock>,
}
pub fn register_board(binding: &'static BoardBinding) -> Result<(), Full> {
    panic!("shim: discovery::board::register_board")
}
pub fn board_clock(node: &DeviceNode, hz: u64, set: bool) -> Option<Result<u64, &'static str>> {
    None
}
