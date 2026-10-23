//! The kernel's part in the Pixel 7's USB device controller: a device node a
//! ring-3 driver is started on, and nothing else.
//!
//! The phone's USB-C port is a Synopsys DWC3 (`usb@11210000`, and the
//! `synopsys,dwc3` child that shares its window), which the phone's tree
//! puts in device mode. `src/user/system/native/drivers/usb/usbdev` drives it, presenting a USB serial
//! port to whatever the phone is plugged into
//! (`docs/vendor/google/pixel7/USB-HANDOVER.md`).
//!
//! What the chip shares with it -- the HSI0 power domain the controller, its
//! PHY and HSI0's clock unit sit in, and the PMU's PHY isolation control --
//! the kernel would have to turn on, as `platform::st::stm32mp1::usb` turns on the DK
//! board's clocks and regulators. It does not: those are power domains, and
//! writing one needs the owner's word first. ABL runs fastboot over this
//! controller until it hands over, and this module relies on it leaving them
//! on. It checks that it did, reading only: the domain says it is on, and the
//! core answers with a DWC3's identity. Otherwise the controller is left
//! alone and the boot says why.
//!
//! The node is the controller's one register window and its interrupt. Its
//! memory is not snooped (the tree gives no `dma-coherent`), so the driver
//! pins what it shares with `PIN_COHERENT`.

use alloc::format;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_fdt::{Fdt, Node};
use ferrix_native_abi::types::TREE_GS201_DWC3;

use crate::device::DmaShape;
use crate::discovery::board::{self, BoardBinding, BoardDevice};
use crate::hooks::Full;
use crate::mmio::Mmio;
use crate::vmap;

/// The USB controller, as the device registry is told about it.
static BINDING: BoardBinding = BoardBinding {
    binding: TREE_GS201_DWC3,
    label: "usb",
    device: "the phone's USB device controller",
    prepare: board_device,
    clock: None,
};

/// Register the binding with the device registry.
///
/// # Errors
///
/// [`Full`] when the registry's list of board bindings is.
pub fn install() -> Result<(), Full> {
    board::register_board(&BINDING)
}

/// The wrapper's `compatible`, whose window and interrupt the DWC3 core
/// shares.
const WRAPPER_COMPATIBLE: &str = "samsung,exynos9-dwusb";

/// `pd-hsi0`'s status word in the PMU, and the page it is in. Bit 0 is set
/// while the domain is on, as `pd-disp`'s and `pd-dpu`'s read on the phone.
const PD_HSI0_PAGE: u64 = 0x1806_2000;
const PD_HSI0_STATUS: u64 = 0x84;

/// The core's identity register, and the product numbers in its upper half
/// that name a DWC3 (`DWC_usb3`, `DWC_usb31`, `DWC_usb32`), from Linux's
/// `drivers/usb/dwc3/core.h`.
const GSNPSID: u64 = 0xC120;
const DWC3_PRODUCTS: [u32; 3] = [0x5533, 0x3331, 0x3332];

/// [`prepare`], as the registry asks for it.
fn board_device(tree: &Fdt<'_>) -> Result<Option<BoardDevice>, &'static str> {
    let Some(wrapper) = tree
        .compatible_nodes(WRAPPER_COMPATIBLE)
        .find(Node::is_enabled)
    else {
        return Ok(None);
    };
    let window = wrapper
        .reg()
        .next()
        .ok_or("the USB controller has no registers")?;
    let interrupt = tree
        .gic_interrupt_of(&wrapper, 0)
        .ok_or("the USB controller's interrupt does not reach the GIC")?;
    let identity = prepare(window.address, window.size)?;
    Ok(Some(BoardDevice {
        registers: alloc::vec![(window.address, window.size)],
        interrupt,
        dma: DmaShape {
            contiguous: false,
            coherent: false,
        },
        summary: format!(
            "DWC3 {identity:#010x} at {:#x}, left on by ABL",
            window.address
        ),
    }))
}

/// Check that the controller is powered and answers, reading only, and
/// return its identity.
///
/// # Errors
///
/// When HSI0 is off, or the core's identity is not a DWC3's.
fn prepare(base: u64, size: u64) -> Result<u32, &'static str> {
    let pmu = Window::map(PD_HSI0_PAGE, PAGE_SIZE)?;
    if pmu.mmio.read32(PD_HSI0_STATUS) & 1 == 0 {
        return Err("HSI0 is powered down, and turning it on waits for the owner");
    }
    drop(pmu);
    if size <= GSNPSID {
        return Err("the USB controller's window is too small for a DWC3");
    }
    let core = Window::map(base, size)?;
    let identity = core.mmio.read32(GSNPSID);
    if !DWC3_PRODUCTS.contains(&(identity >> 16)) {
        return Err("the USB controller does not answer as a DWC3");
    }
    Ok(identity)
}

/// A device register window the kernel maps only while it looks.
struct Window {
    at: u64,
    mmio: Mmio,
}

impl Window {
    fn map(phys: u64, len: u64) -> Result<Window, &'static str> {
        let at = vmap::map_device(phys, len).map_err(|_| "device registers could not be mapped")?;
        Ok(Window {
            at,
            mmio: Mmio::at(at),
        })
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = vmap::unmap_device(self.at);
    }
}
