#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mmio {}
impl Mmio {
    pub const fn unmapped() -> Self {
        panic!("shim: mmio::Mmio::unmapped")
    }
    pub const fn at(base: u64) -> Self {
        panic!("shim: mmio::Mmio::at")
    }
    pub fn read16(self, offset: u64) -> u16 {
        0
    }
    pub fn write16(self, offset: u64, value: u16) {}
    pub fn read32(self, offset: u64) -> u32 {
        0
    }
    pub fn write32(self, offset: u64, value: u32) {}
}
