//! The kernel's part in an STM32MP15 DK board's USB host: clocks, resets,
//! regulators and the PHY, then a device node a ring-3 driver is started on.
//!
//! The DK boards' four USB-A sockets hang off a Microchip USB2514B hub, wired
//! to port 1 of the chip's EHCI controller through the first port of its
//! USB PHY controller, `USBPHYC` (`docs/INPUT.md` §7). The driver,
//! `src/user/system/native/drivers/usb/usbhid`, programs the EHCI controller and everything on the bus.
//! What it cannot do is what the rest of the chip shares, for the reason
//! `stm32mp1` gives for the display -- the RCC's clock gates and resets, and
//! the PWR block's regulators -- or what serves two controllers, as the PHY's
//! PLL serves the EHCI host and the OTG controller alike. So the kernel does
//! these things, once, while it publishes device nodes, and nothing more:
//!
//! * turns on the EHCI controller's and the PHY controller's clocks, and
//!   releases both from reset;
//! * turns on the PWR block's 1.8 V and 1.1 V regulators, which feed the PHY,
//!   and waits for them to say they are ready;
//! * starts the PHY's PLL from the HSE, which is the PHY's clock whenever the
//!   RCC's `USBCKSELR` has not been changed from its reset value.
//!
//! Every value is what U-Boot's `usb start` writes on this board, read back
//! with `md` on 2026-09-23. The PHY's analogue tuning, which Linux and U-Boot
//! write from the tree's `st,tune-*` properties, is left at its reset value:
//! it trims the high-speed eye, and the one high-speed link here is a
//! centimetre of board to the hub.
//!
//! What the driver then gets is a node with one aperture, the EHCI
//! controller's registers, and its interrupt.

use alloc::format;
use core::fmt;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_fdt::{Fdt, GicInterrupt, Node};
use ferrix_native_abi::types::TREE_STM32_USBH;

use crate::device::DmaShape;
use crate::discovery::board::{BoardBinding, BoardDevice};
use crate::mmio::Mmio;
use crate::{timer, vmap};

/// The USB host, as the device registry is told about it
/// (`crate::platform::st::stm32mp1::install`).
pub(crate) static BINDING: BoardBinding = BoardBinding {
    binding: TREE_STM32_USBH,
    label: "usb",
    device: "the board's USB host",
    prepare: board_device,
    clock: None,
};

/// [`prepare`], as the registry asks for it.
fn board_device(tree: &Fdt<'_>) -> Result<Option<BoardDevice>, &'static str> {
    let Some(prepared) = prepare(tree)? else {
        return Ok(None);
    };
    Ok(Some(BoardDevice {
        registers: alloc::vec![prepared.ehci],
        interrupt: prepared.interrupt,
        // EHCI walks lists of descriptors a page at a time, and does not
        // snoop.
        dma: DmaShape {
            contiguous: false,
            coherent: false,
        },
        summary: format!("{prepared}"),
    }))
}

/// The EHCI controller's `compatible`.
const EHCI_COMPATIBLE: &str = "generic-ehci";
/// The PHY controller's.
const USBPHYC_COMPATIBLE: &str = "st,stm32mp1-usbphyc";
/// The RCC's.
const RCC_COMPATIBLE: &str = "st,stm32mp1-rcc";
/// The PWR block's regulators', whose register this module writes.
const PWR_REGULATORS_COMPATIBLE: &str = "st,stm32mp1,pwr-reg";

/// The PWR block's registers (RM0436's memory map; the regulators' node is a
/// child of a `syscon` that gives no `reg` of its own to a reader like this).
const PWR_BASE: u64 = 0x5000_1000;
/// `PWR_CR3`: the regulators' enables and ready flags.
const PWR_CR3: u64 = 0x0C;
const REG18_ENABLE: u32 = 1 << 28;
const REG18_READY: u32 = 1 << 29;
const REG11_ENABLE: u32 = 1 << 30;
const REG11_READY: u32 = 1 << 31;

// The RCC's registers, from Linux's `drivers/clk/stm32/clk-stm32mp1.c` and
// `include/dt-bindings/reset/stm32mp1-resets.h`: a reset id is the set
// register's offset times eight plus the bit, so the tree's `resets` cells
// name them too.
const RCC_APB4RSTCLRR: u64 = 0x184;
const RCC_AHB6RSTCLRR: u64 = 0x19C;
const RCC_APB4ENSETR: u64 = 0x200;
const RCC_AHB6ENSETR: u64 = 0x218;
const RCC_USBCKSELR: u64 = 0x91C;
/// `AHB6ENSETR` and `AHB6RSTCLRR`: the EHCI (and OHCI) controller.
const USBH_BIT: u32 = 1 << 24;
/// `APB4ENSETR` and `APB4RSTCLRR`: the PHY controller.
const USBPHY_BIT: u32 = 1 << 16;
/// `USBCKSELR`'s `USBPHYSRC`, bits 1:0: 0 is `hse_ker_ck`.
const USBPHYSRC_MASK: u32 = 0x3;

/// The PHY controller's PLL register, and its fields.
const USBPHYC_PLL: u64 = 0x000;
const PLL_NDIV_MASK: u32 = 0x7F;
const PLL_FRACIN_SHIFT: u32 = 10;
const PLL_ENABLE: u32 = 1 << 26;
const PLL_STROBE_BYPASS: u32 = 1 << 28;
const PLL_FRACTION_CONTROL: u32 = 1 << 29;
const PLL_DITHER_0: u32 = 1 << 30;
const PLL_DITHER_1: u32 = 1 << 31;
/// The VCO rate the PLL is set up for, Linux's `PLL_FVCO_MHZ`.
const PLL_VCO_HZ: u64 = 2_880_000_000;
/// The input rates the PLL takes.
const PLL_INPUT_HZ: core::ops::RangeInclusive<u64> = 19_200_000..=38_400_000;
/// How long the PLL takes to lock once enabled: Linux waits 100 µs, U-Boot
/// 200.
const PLL_LOCK_NANOS: u64 = 200_000;
/// How long a regulator may take to say it is ready: Linux polls for 20 ms.
const REGULATOR_NANOS: u64 = 20_000_000;

/// What the kernel prepared, for the device node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prepared {
    /// The EHCI controller's registers, the whole page RM0436 gives it.
    pub(crate) ehci: (u64, u64),
    /// Its interrupt.
    pub(crate) interrupt: GicInterrupt,
    /// The PHY's input clock.
    pub(crate) reference_hz: u64,
    /// The PLL register as written.
    pub(crate) pll: u32,
}

impl fmt::Display for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "EHCI at {:#x}, PHY PLL {:#010x} from {} MHz",
            self.ehci.0,
            self.pll,
            self.reference_hz / 1_000_000
        )
    }
}

/// Find the board's USB host, and prepare it for a driver.
///
/// `Ok(None)` on a machine with no enabled EHCI controller wired to an
/// STM32MP1 PHY controller, which is every machine but an STM32MP15 board.
/// `Err` names what stopped the kernel from handing the host over; what it
/// did before that stays done, and none of it is harmful to leave: clocks
/// on, resets released, regulators on.
pub(crate) fn prepare(tree: &Fdt<'_>) -> Result<Option<Prepared>, &'static str> {
    let Some((ehci, phy)) = host_and_phy(tree) else {
        return Ok(None);
    };
    let ehci_reg = ehci
        .reg()
        .next()
        .ok_or("the EHCI controller has no registers")?;
    let phy_reg = phy
        .reg()
        .next()
        .ok_or("the USB PHY controller has no registers")?;
    let interrupt = tree
        .gic_interrupt_of(&ehci, 0)
        .ok_or("the EHCI controller's interrupt does not reach the GIC")?;
    if tree
        .compatible_nodes(PWR_REGULATORS_COMPATIBLE)
        .next()
        .is_none()
    {
        return Err("no STM32MP1 PWR regulators");
    }

    let rcc_node = tree
        .compatible_nodes(RCC_COMPATIBLE)
        .next()
        .ok_or("no RCC")?;
    let rcc_reg = rcc_node.reg().next().ok_or("the RCC has no registers")?;
    let rcc = Window::map(rcc_reg.address, PAGE_SIZE)?;
    if rcc.mmio.read32(RCC_USBCKSELR) & USBPHYSRC_MASK != 0 {
        return Err("the USB PHY's clock is not the HSE");
    }
    let reference_hz = hse_hz(tree).ok_or("the tree gives no HSE rate")?;
    let pll = pll_value(reference_hz).ok_or("the HSE is outside what the USB PHY's PLL takes")?;

    // Clocks on, then out of reset: the set and clear registers take ones
    // and leave the other bits alone.
    rcc.mmio.write32(RCC_AHB6ENSETR, USBH_BIT);
    rcc.mmio.write32(RCC_APB4ENSETR, USBPHY_BIT);
    rcc.mmio.write32(RCC_AHB6RSTCLRR, USBH_BIT);
    rcc.mmio.write32(RCC_APB4RSTCLRR, USBPHY_BIT);
    drop(rcc);

    regulators_on()?;

    let phy = Window::map(phy_reg.address, PAGE_SIZE)?;
    // Programmed with the PLL off, then enabled: the dividers are sampled as
    // it starts. A PLL left running by firmware is stopped first, as Linux
    // does, since its dividers are not known to be these.
    let running = phy.mmio.read32(USBPHYC_PLL);
    if running & PLL_ENABLE != 0 {
        phy.mmio.write32(USBPHYC_PLL, running & !PLL_ENABLE);
        spin(PLL_LOCK_NANOS);
    }
    phy.mmio.write32(USBPHYC_PLL, pll);
    phy.mmio.write32(USBPHYC_PLL, pll | PLL_ENABLE);
    spin(PLL_LOCK_NANOS);

    Ok(Some(Prepared {
        ehci: (ehci_reg.address, PAGE_SIZE),
        interrupt,
        reference_hz,
        pll: pll | PLL_ENABLE,
    }))
}

/// The enabled EHCI controller whose `phys` names a port of an enabled
/// STM32MP1 PHY controller, and that controller.
fn host_and_phy<'a>(tree: &Fdt<'a>) -> Option<(Node<'a>, Node<'a>)> {
    let ehci = tree
        .compatible_nodes(EHCI_COMPATIBLE)
        .find(Node::is_enabled)?;
    let port = ehci.property("phys")?.cells().next()?;
    // The port is a child of the controller: walk the tree keeping the path.
    let mut path: alloc::vec::Vec<Node<'a>> = alloc::vec::Vec::new();
    for node in tree.nodes() {
        path.truncate(node.depth);
        if node.phandle() == Some(port) {
            let parent = path.last().copied()?;
            let usable = parent.is_compatible(USBPHYC_COMPATIBLE) && parent.is_enabled();
            return usable.then_some((ehci, parent));
        }
        path.push(node);
    }
    None
}

/// The PLL register for an input of `reference_hz`, the dividers as Linux's
/// `stm32_usbphyc_get_pll_params` computes them --
/// `VCO = input * 2 * (NDIV + FRAC / 2^16)` for a VCO of 2880 MHz -- with
/// the dither and strobe bypass U-Boot sets. `None` for an input the PLL
/// does not take.
fn pll_value(reference_hz: u64) -> Option<u32> {
    if !PLL_INPUT_HZ.contains(&reference_hz) {
        return None;
    }
    let doubled = reference_hz * 2;
    let ndiv = PLL_VCO_HZ / doubled;
    let frac = (PLL_VCO_HZ << 16) / doubled - (ndiv << 16);
    let ndiv = u32::try_from(ndiv).ok()? & PLL_NDIV_MASK;
    let frac = u32::try_from(frac).ok()? & 0xFFFF;
    let mut value = PLL_DITHER_1 | PLL_DITHER_0 | PLL_STROBE_BYPASS | ndiv;
    if frac != 0 {
        value |= PLL_FRACTION_CONTROL | (frac << PLL_FRACIN_SHIFT);
    }
    Some(value)
}

/// Turn on the 1.8 V regulator and then the 1.1 V one, each waited for.
fn regulators_on() -> Result<(), &'static str> {
    let pwr = Window::map(PWR_BASE, PAGE_SIZE)?;
    for (enable, ready, name) in [
        (
            REG18_ENABLE,
            REG18_READY,
            "the 1.8 V USB regulator did not come up",
        ),
        (
            REG11_ENABLE,
            REG11_READY,
            "the 1.1 V USB regulator did not come up",
        ),
    ] {
        let value = pwr.mmio.read32(PWR_CR3);
        pwr.mmio.write32(PWR_CR3, value | enable);
        let until = timer::now_nanos().saturating_add(REGULATOR_NANOS);
        while pwr.mmio.read32(PWR_CR3) & ready == 0 {
            if timer::now_nanos() >= until {
                return Err(name);
            }
            core::hint::spin_loop();
        }
    }
    Ok(())
}

/// The HSE's rate: the `clock-frequency` of the fixed clock named `clk-hse`.
fn hse_hz(tree: &Fdt<'_>) -> Option<u64> {
    tree.nodes()
        .find(|node| node.name == "clk-hse")?
        .property("clock-frequency")?
        .as_u32()
        .map(u64::from)
}

/// Wait `nanos` on the clock.
fn spin(nanos: u64) {
    let until = timer::now_nanos().saturating_add(nanos);
    while timer::now_nanos() < until {
        core::hint::spin_loop();
    }
}

/// A page of device registers mapped for as long as this lives.
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
