use crate::arch;
use crate::fallible;
use crate::fallible::AllocError;
use crate::iommu;
use crate::irq;
use crate::mmio::Mmio;
use crate::sync::SpinLock;
use crate::vmap;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::MemKind;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_fdt::GicInterrupt;
use ferrix_fdt::Trigger as TreeTrigger;
use ferrix_native_abi::types::APERTURE_BAR_64;
use ferrix_native_abi::types::APERTURE_NOT_BAR;
use ferrix_native_abi::types::APERTURE_PREFETCHABLE;
use ferrix_native_abi::types::APERTURE_WHOLE_PAGES;
use ferrix_native_abi::types::ApertureInfo;
use ferrix_native_abi::types::DEVICE_NOT_PCI;
use ferrix_native_abi::types::DEVICE_TREE_BLOCKS;
use ferrix_native_abi::types::DEVICE_VIRTIO_PCI;
use ferrix_native_abi::types::DeviceBlock;
use ferrix_native_abi::types::DeviceInfo;
use ferrix_native_abi::types::PCI_CLASS_XHCI;
use ferrix_native_abi::types::TREE_FIRMWARE_FB;
use ferrix_native_abi::types::TREE_GS201_DWC3;
use ferrix_native_abi::types::TREE_STM32_USBH;
use ferrix_native_abi::types::TREE_X86_POWER;
use ferrix_native_abi::types::USB_INPUT_FUNCTIONS;
use ferrix_pci::Address;
use ferrix_pci::ConfigSpace as _;
use ferrix_pci::bar::Bar;
use ferrix_pci::bar::Region;
use ferrix_pci::capability::Capability;
use ferrix_pci::capability::MSIX_ENTRY_SIZE;
use ferrix_pci::capability::MsiX;
use ferrix_pci::header::BAR0;
use ferrix_pci::header::COMMAND;
use ferrix_pci::header::COMMAND_BUS_MASTER;
use ferrix_pci::header::COMMAND_INTERRUPT_DISABLE;
use ferrix_pci::header::COMMAND_MEMORY_SPACE;
use ferrix_pci::header::Identity;
use ferrix_pci::msi;
use ferrix_pci::msi::Msi;
use ferrix_pci::msix;
use ferrix_pci::msix::CAPABILITY_CONTROL;
use ferrix_pci::msix::CONTROL_ENABLE;
use ferrix_pci::msix::CONTROL_FUNCTION_MASK;
use ferrix_pci::msix::ENTRY_ADDRESS_HIGH;
use ferrix_pci::msix::ENTRY_ADDRESS_LOW;
use ferrix_pci::msix::ENTRY_DATA;
use ferrix_pci::msix::ENTRY_VECTOR_CONTROL;
use ferrix_pci::msix::VECTOR_CONTROL_MASKED;
use ferrix_pci::virtio::Location as VirtioLocation;
use ferrix_pci::virtio::SharedMemory;
use ferrix_pci::virtio::Transport;
use ferrix_pci::window::Writable;
use ferrix_sync::IrqSpinLock;
use ferrix_sync::IrqSpinLockGuard;
use ferrix_sync::Once;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Aperture {}
impl Aperture {
    pub const fn cacheable(self) -> bool {
        false
    }
    pub const fn whole_pages(self) -> bool {
        true
    }
    pub const fn phys(self) -> u64 {
        0
    }
    pub const fn len(self) -> u64 {
        0
    }
    pub fn info(self) -> ApertureInfo {
        panic!("shim: device::Aperture::info")
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Vector {}
impl Vector {
    pub fn coalesces(self) -> bool {
        false
    }
    pub fn mask(self) -> Result<(), &'static str> {
        Err("no interrupts in ring 3")
    }
    pub fn unmask(self) -> Result<(), &'static str> {
        Err("no interrupts in ring 3")
    }
    pub fn reads_masked(self) -> Option<bool> {
        None
    }
    pub const fn number(self) -> u32 {
        0
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Location {
    Pci(Address),
    VirtioMmio(u64),
    Tree(u64),
}
impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
#[derive(Debug, Default)]
pub struct Reserved {}
impl Reserved {
    pub fn of(view: &BootView<'_>, reads: &[&[(u64, u64)]]) -> Self {
        panic!("shim: device::Reserved::of")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterBlock {
    pub phys: u64,
    pub offset: u32,
    pub length: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioBlocks {
    pub common: RegisterBlock,
    pub notify: RegisterBlock,
    pub isr: RegisterBlock,
    pub device: Option<RegisterBlock>,
    pub notify_multiplier: u32,
}
#[derive(Debug, Clone, Copy)]
pub struct Seen<'a> {
    pub config_phys: Option<u64>,
    pub identity: &'a Identity,
    pub transport: Option<&'a Transport>,
    pub subsystem: (u16, u16),
    pub secondary_bus: Option<u8>,
    pub host_visible: Option<&'a SharedMemory>,
    pub intx: Option<GicInterrupt>,
    pub msi: Option<Msi>,
    pub msix_held_off: bool,
    pub config: &'a ConfigFound,
}
#[derive(Debug, Clone, Copy)]
pub struct ConfigFound {
    pub bytes: u16,
    pub command: u16,
    pub writable: Writable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciFunction {
    pub vendor: u16,
    pub device: u16,
    pub class: u32,
    pub revision: u8,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
    pub secondary_bus: Option<u8>,
    pub config_phys: Option<u64>,
    pub virtio: Option<VirtioBlocks>,
    pub msix_table_size: u16,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub phys: u64,
    pub len: u64,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DmaShape {
    pub contiguous: bool,
    pub coherent: bool,
}
#[derive(Debug)]
pub struct DeviceNode {
    /// Where it is: all linuxd knows of it; devmgr holds its handle.
    location: Location,
    function: Option<PciFunction>,
    index: usize,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FramebufferMode {
    pub width: u16,
    pub height: u16,
    pub stride: u32,
    pub layout: u16,
}
impl DeviceNode {
    pub const fn empty(location: Location) -> Self {
        DeviceNode {
            location,
            function: None,
            index: 0,
        }
    }
    pub fn power() -> Self {
        panic!("shim: device::DeviceNode::power")
    }
    pub fn may_copy_lent(&self) -> bool {
        false
    }
}
impl DeviceNode {
    pub const fn reaches_memory(&self) -> bool {
        false
    }
    pub fn may_have_client_nodes(&self) -> bool {
        false
    }
    pub const fn framebuffer_mode(&self) -> Option<FramebufferMode> {
        None
    }
    pub fn pci(
        address: Address,
        seen: &Seen<'_>,
        regions: &[Region],
        msix: Option<&(Capability, MsiX)>,
        decoding: bool,
        reserved: &Reserved,
    ) -> Self {
        panic!("shim: device::DeviceNode::pci")
    }
    pub fn host_visible(&self) -> Option<Window> {
        None
    }
    pub const fn location(&self) -> Location {
        self.location
    }
    pub const fn index(&self) -> usize {
        self.index
    }
    pub const fn binding(&self) -> u16 {
        0
    }
    /// The input core's limit on control channels: a USB host has one per keyboard
    /// or mouse it finds.
    pub fn input_functions(&self) -> usize {
        match &self.function {
            Some(function) if function.class == PCI_CLASS_XHCI => USB_INPUT_FUNCTIONS,
            _ => 1,
        }
    }
    pub const fn reads_log(&self) -> bool {
        false
    }
    /// x86-64's devices snoop, and the driver holds its own buffers' shape: what a
    /// core does with this is say so to its client.
    pub const fn dma_shape(&self) -> DmaShape {
        DmaShape {
            contiguous: false,
            coherent: true,
        }
    }
    pub fn bind_board(&mut self, binding: u16, dma: DmaShape) {}
    pub const fn pci_function(&self) -> Option<&PciFunction> {
        self.function.as_ref()
    }
    pub fn describe(&self) -> DeviceInfo {
        panic!("shim: device::DeviceNode::describe")
    }
    pub fn enable_dma(&self) -> Result<(), &'static str> {
        panic!("shim: device::DeviceNode::enable_dma")
    }
    pub fn disable_dma(&self) -> Result<(), &'static str> {
        panic!("shim: device::DeviceNode::disable_dma")
    }
    pub fn config_phys(&self) -> Option<u64> {
        None
    }
    pub fn aperture_info(&self, index: usize) -> Option<ApertureInfo> {
        None
    }
    pub fn is_refused(&self) -> bool {
        false
    }
    pub const fn yielded(&self) -> bool {
        false
    }
    pub fn verify_config(&self) -> Result<(), Breach> {
        panic!("shim: device::DeviceNode::verify_config")
    }
    pub fn hello_accepted(&self) {}
    pub fn breach(&self) -> Option<Breach> {
        None
    }
    pub fn apertures(&self) -> &[Aperture] {
        panic!("shim: device::DeviceNode::apertures")
    }
    pub fn domain(&self) -> Result<Arc<iommu::Domain>, AllocError> {
        panic!("shim: device::DeviceNode::domain")
    }
    pub fn isolated_marked(&self) -> bool {
        false
    }
    pub fn interrupts_isolated(&self) -> bool {
        false
    }
    pub fn may_take_vectors_and_pins(&self) -> bool {
        false
    }
    pub fn domain_made(&self) -> Option<Arc<iommu::Domain>> {
        None
    }
    pub fn aperture(&self, phys: u64, len: u64) -> Option<Aperture> {
        None
    }
    pub fn vector(&self, index: usize) -> Option<Vector> {
        None
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Register {
    Command,
    Bar(u8),
    MsiControl,
    MsiAddress,
    MsiData,
    MsixControl,
    MsixAddress(u16),
    MsixData(u16),
}
impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Breach {
    pub register: Register,
}
static DEVICES: Once<Vec<Arc<DeviceNode>>> = Once::new();
/// The kernel's enumeration, from the boot page: once, at linuxd's start.
pub fn set_devices(facts: impl Iterator<Item = ferrix_native_abi::lift::DeviceFact>) {
    let _ = DEVICES.call_once(|| {
        facts
            .enumerate()
            .filter_map(|(index, fact)| {
                let address = Address::new(
                    (fact.location >> 16) as u16,
                    (fact.location >> 8) as u8,
                    ((fact.location >> 3) & 0x1f) as u8,
                    (fact.location & 7) as u8,
                )?;
                Some(Arc::new(DeviceNode {
                    location: Location::Pci(address),
                    function: Some(PciFunction {
                        vendor: fact.vendor,
                        device: fact.device,
                        class: fact.class,
                        revision: fact.revision,
                        subsystem_vendor: fact.subsystem_vendor,
                        subsystem: fact.subsystem,
                        secondary_bus: (fact.secondary_bus != 0xff).then_some(fact.secondary_bus),
                        config_phys: None,
                        virtio: None,
                        msix_table_size: 0,
                    }),
                    index,
                }))
            })
            .collect()
    });
}
pub fn devices() -> &'static [Arc<DeviceNode>] {
    DEVICES.get().map_or(&[], Vec::as_slice)
}
pub static FORCED_INTEL_GPU: AtomicUsize = AtomicUsize::new(0);
pub static FORCED_UNTRANSLATED: AtomicBool = AtomicBool::new(false);
