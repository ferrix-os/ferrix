//! The kernel's part in an STM32MP15 board's SD card: finding SDMMC1 as
//! firmware left it, and a device node a ring-3 driver is started on
//! (`docs/CHROME.md` §10).
//!
//! The DK boards' microSD slot is wired to SDMMC1, the tree's
//! `mmc@58005000` with `compatible = "st,stm32-sdmmc2"` and its interrupt
//! GIC SPI 49 (Linux's `arch/arm/boot/dts/st/stm32mp151.dtsi`). The driver,
//! `src/user/system/native/drivers/block/stm32-sdmmc`, programs the
//! controller and the card.
//!
//! Unlike the display, the USB host and the GPU, this one is handed over
//! with **no write at all**. U-Boot read the card to boot, so the controller
//! is clocked, out of reset, its pins muxed and the card powered; changing
//! any of that would be undoing what the boot depended on. So the kernel
//! only reads, and leaves the controller alone, with the reason on the
//! console, unless it finds:
//!
//! * SDMMC1's clock gate open: `RCC_MP_AHB6ENSETR` (0x218) bit 16, which a
//!   read of the set register returns (Linux's `clk-stm32mp1.c`,
//!   `K_MGATE(G_SDMMC1, RCC_AHB6ENSETR, 16, 0)`);
//! * its reset released: `RCC_AHB6RSTSETR` (0x198) bit 16, `SDMMC1_R` = 3280
//!   in `include/dt-bindings/reset/stm32mp1-resets.h`, 0x198 times eight
//!   plus sixteen;
//! * its kernel clock, `sdmmc1_k`, on a source whose rate the registers
//!   give: `RCC_SDMMC12CKSELR` (0x8F4) bits 2:0 choose `ck_axi`, `pll3_r`,
//!   `pll4_p` or `ck_hsi` (`sdmmc12_src`, `K_MMUX(M_SDMMC12,
//!   RCC_SDMMC12CKSELR, 0, 3, 0)`). PLL4's P output -- what TF-A gives
//!   SDMMC1 on a DK board, 99 MHz -- is `vco / (DIVP + 1)` with `DIVP` in
//!   `PLL4CFGR2` bits 6:0, taken only when `PLL4CR` says the PLL is on,
//!   locked, and its P output enabled (`DIVPEN`, bit 4); the HSI is 64 MHz
//!   over `HSICFGR`'s divider. The bus clock and PLL3's R output are not
//!   computed here, and a controller on either is left alone rather than
//!   given a guessed rate.
//!
//! The rate is the node's clock (`device_clock`): a driver asks it, and is
//! refused if it asks to set it. Register offsets and bits are RM0436's, as
//! Linux's clock driver names them.
//!
//! What the driver then gets is a node with one aperture, the controller's
//! page at `0x5800_5000`, and its interrupt.
//!
//! The controller has an internal DMA, programmed from that same page, and
//! nothing in front of SDMMC1 checks the addresses it is given: a driver
//! that turned it on would be trusted with all of memory, as every
//! untranslated device's is (`docs/certification/VULNERABILITY-ANALYSIS.md`, V-03).
//! The driver in this tree does not; that is its property, not the
//! kernel's.

use alloc::format;
use core::fmt;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_fdt::{Fdt, GicInterrupt, Node};
use ferrix_native_abi::types::TREE_STM32_SDMMC;
use ferrix_sync::Once;

use super::{
    HSI_HZ, Mhz, PLL_ON, PLL_READY, RCC_HSICFGR, RCC_PLL4CFGR2, RCC_PLL4CR, RCC_RCK4SELR, Window,
    hse_hz, vco,
};
use crate::device::DmaShape;
use crate::discovery::board::{BoardBinding, BoardDevice};

/// SDMMC1, as the device registry is told about it
/// (`crate::platform::st::stm32mp1::install`).
pub(crate) static BINDING: BoardBinding = BoardBinding {
    binding: TREE_STM32_SDMMC,
    label: "sdmmc",
    device: "the board's SD card",
    prepare: board_device,
    clock: Some(kernel_clock),
};

/// The kernel clock's rate, found by [`prepare`].
static RATE: Once<u64> = Once::new();

/// [`prepare`], as the registry asks for it.
fn board_device(tree: &Fdt<'_>) -> Result<Option<BoardDevice>, &'static str> {
    let Some(prepared) = prepare(tree)? else {
        return Ok(None);
    };
    let _ = RATE.call_once(|| prepared.kernel_hz);
    Ok(Some(BoardDevice {
        registers: alloc::vec![prepared.registers],
        interrupt: prepared.interrupt,
        // The driver moves data through the FIFO and pins nothing; were
        // memory pinned, the controller's DMA does not snoop the caches.
        dma: DmaShape {
            contiguous: false,
            coherent: false,
        },
        summary: format!("{prepared}"),
    }))
}

/// The node's clock: the rate, whatever `hz` was asked; never set.
fn kernel_clock(_hz: u64, set: bool) -> Result<u64, &'static str> {
    if set {
        return Err("the SD card's kernel clock is firmware's and is not set");
    }
    RATE.get().copied().ok_or("the SD card was not handed over")
}

/// The controller's `compatible`.
const SDMMC_COMPATIBLE: &str = "st,stm32-sdmmc2";
/// The RCC's.
const RCC_COMPATIBLE: &str = "st,stm32mp1-rcc";
/// SDMMC1, the DK boards' microSD slot: the one instance this module knows.
const SDMMC1_BASE: u64 = 0x5800_5000;

/// `RCC_AHB6RSTSETR`: read, the AHB6 peripherals held in reset.
const RCC_AHB6RSTSETR: u64 = 0x198;
/// `RCC_MP_AHB6ENSETR`: read, the AHB6 peripherals' clock gates.
const RCC_AHB6ENSETR: u64 = 0x218;
/// `RCC_SDMMC12CKSELR`: SDMMC1 and SDMMC2's kernel clock source.
const RCC_SDMMC12CKSELR: u64 = 0x8F4;
/// SDMMC1's bit in both AHB6 registers.
const SDMMC1_BIT: u32 = 1 << 16;
/// `SDMMC12CKSELR`'s sources.
const FROM_PLL4_P: u32 = 2;
const FROM_HSI: u32 = 3;
/// `PLL4CR.DIVPEN`: the P output is enabled.
const PLL_P_ENABLE: u32 = 1 << 4;
/// `PLL4CFGR2.DIVP`: P's divider, less one.
const DIVP_MASK: u32 = 0x7F;

/// What the kernel found, for the device node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prepared {
    /// The controller's registers, the page RM0436 gives them.
    pub(crate) registers: (u64, u64),
    /// Its interrupt.
    pub(crate) interrupt: GicInterrupt,
    /// The kernel clock's rate.
    pub(crate) kernel_hz: u64,
    /// Where the kernel clock comes from.
    pub(crate) source: &'static str,
}

impl fmt::Display for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SDMMC1 at {:#x}, interrupt {}, kernel clock {} MHz from {}, as firmware left it",
            self.registers.0,
            self.interrupt.id,
            Mhz(self.kernel_hz),
            self.source
        )
    }
}

/// Find SDMMC1 and check it is running as firmware left it; nothing is
/// written.
///
/// `Ok(None)` on a machine with no enabled SDMMC1, which is every machine
/// but an STM32MP15 board whose tree enables the slot. `Err` names why it
/// is left alone.
pub(crate) fn prepare(tree: &Fdt<'_>) -> Result<Option<Prepared>, &'static str> {
    let Some(node) = tree
        .compatible_nodes(SDMMC_COMPATIBLE)
        .filter(Node::is_enabled)
        .find(|node| {
            node.reg()
                .next()
                .is_some_and(|reg| reg.address == SDMMC1_BASE)
        })
    else {
        return Ok(None);
    };
    let interrupt = tree
        .gic_interrupt_of(&node, 0)
        .ok_or("SDMMC1's interrupt does not reach the GIC")?;
    let rcc_node = tree
        .compatible_nodes(RCC_COMPATIBLE)
        .next()
        .ok_or("no RCC")?;
    let rcc_reg = rcc_node.reg().next().ok_or("the RCC has no registers")?;
    let rcc = Window::map(rcc_reg.address, PAGE_SIZE)?;
    let r = rcc.mmio;
    if r.read32(RCC_AHB6ENSETR) & SDMMC1_BIT == 0 {
        return Err("SDMMC1's clock is off");
    }
    if r.read32(RCC_AHB6RSTSETR) & SDMMC1_BIT != 0 {
        return Err("SDMMC1 is held in reset");
    }
    let (kernel_hz, source) = match r.read32(RCC_SDMMC12CKSELR) & 0x7 {
        FROM_PLL4_P => {
            let control = r.read32(RCC_PLL4CR);
            if control & (PLL_ON | PLL_READY | PLL_P_ENABLE) != PLL_ON | PLL_READY | PLL_P_ENABLE {
                return Err("SDMMC1 runs from PLL4's P output, which is off");
            }
            let reference = match r.read32(RCC_RCK4SELR) & 0x3 {
                0 => HSI_HZ >> (r.read32(RCC_HSICFGR) & 0x3),
                1 => hse_hz(tree)
                    .ok_or("PLL4 runs from the HSE, whose rate the tree does not give")?,
                2 => super::CSI_HZ,
                _ => return Err("PLL4's reference is not a clock"),
            };
            let p = u64::from(r.read32(RCC_PLL4CFGR2) & DIVP_MASK) + 1;
            (vco(r, reference) / p, "PLL4 P")
        }
        FROM_HSI => (HSI_HZ >> (r.read32(RCC_HSICFGR) & 0x3), "the HSI"),
        _ => return Err("SDMMC1's kernel clock is on a source whose rate is not read here"),
    };
    drop(rcc);
    if kernel_hz == 0 {
        return Err("SDMMC1's kernel clock reads as stopped");
    }
    Ok(Some(Prepared {
        registers: (SDMMC1_BASE, PAGE_SIZE),
        interrupt,
        kernel_hz,
        source,
    }))
}
