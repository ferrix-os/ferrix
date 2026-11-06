//! The kernel's part in an STM32MP157's GPU: its clock and its reset line,
//! then a device node a ring-3 driver is started on (`docs/GPU.md` §6.2, G1).
//!
//! The STM32MP157 carries a Vivante GC400T at `0x5900_0000`, the tree's
//! `gpu@59000000` with `compatible = "vivante,gc"`. The driver, `src/user/system/native/drivers/gpu/gc400`,
//! programs everything inside the core: its own clock control and soft reset,
//! its interrupt enables, and the front end that fetches command buffers.
//! What it cannot do is what the rest of the chip shares, for the reason
//! `stm32mp1` gives for the display: the RCC's clock gates and resets. So the
//! kernel does these things, once, while it publishes device nodes, and
//! nothing more:
//!
//! * checks that the GPU's core clock -- PLL2's Q output, which firmware set
//!   up and which feeds nothing but the GPU in Linux's clock tree -- is
//!   running, and says how fast;
//! * turns on the GPU's clocks: one bit of the RCC gates both the bus clock
//!   and the core clock;
//! * pulses its reset line, so the driver starts from the core's reset state
//!   whatever firmware or an earlier boot left in it.
//!
//! The RCC's offsets and bits are Linux's `drivers/clk/stm32/clk-stm32mp1.c`
//! and RM0436's register map: the gate is
//! `K_MGATE(G_GPU, RCC_AHB6ENSETR, 5, 0)`, which both `GPU` (the bus clock,
//! from `ck_axi`) and `GPU_K` (the core clock, from `pll2_q`) name; the
//! reset is `GPU_R`, 3269 in `include/dt-bindings/reset/stm32mp1-resets.h`,
//! which is the set register `0x198` times eight plus bit 5, and its clear
//! register is four bytes on (`RCC_CLR`).
//!
//! What the driver then gets is a node with one aperture, the core's
//! registers, minted a page as RM0436's memory map places it though the
//! tree's `reg` says 0x800, and its interrupt.

use alloc::format;
use core::fmt;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_fdt::{Fdt, GicInterrupt, Node};
use ferrix_native_abi::types::TREE_STM32_GPU;

use crate::device::DmaShape;
use crate::discovery::board::{BoardBinding, BoardDevice};
use crate::mmio::Mmio;
use crate::{timer, vmap};

/// The GPU, as the device registry is told about it
/// (`crate::platform::st::stm32mp1::install`).
pub(crate) static BINDING: BoardBinding = BoardBinding {
    binding: TREE_STM32_GPU,
    label: "gpu",
    device: "the board's GPU",
    prepare: board_device,
    clock: None,
};

/// [`prepare`], as the registry asks for it.
fn board_device(tree: &Fdt<'_>) -> Result<Option<BoardDevice>, &'static str> {
    let Some(prepared) = prepare(tree)? else {
        return Ok(None);
    };
    Ok(Some(BoardDevice {
        registers: alloc::vec![prepared.registers],
        interrupt: prepared.interrupt,
        // The core does not snoop the caches, and with its MMU off its front
        // end reads one run of physical addresses, as the LTDC scans one out.
        dma: DmaShape {
            contiguous: true,
            coherent: false,
        },
        summary: format!("{prepared}"),
    }))
}

/// The GPU's `compatible`.
const GPU_COMPATIBLE: &str = "vivante,gc";
/// The RCC's.
const RCC_COMPATIBLE: &str = "st,stm32mp1-rcc";
/// Where the STM32MP157's GC400T is: the one Vivante core whose clock and
/// reset this module knows.
const GPU_BASE: u64 = 0x5900_0000;

// The RCC's registers.
const RCC_HSICFGR: u64 = 0x18;
const RCC_RCK12SELR: u64 = 0x28;
const RCC_PLL2CR: u64 = 0x94;
const RCC_PLL2CFGR1: u64 = 0x98;
const RCC_PLL2CFGR2: u64 = 0x9C;
const RCC_PLL2FRACR: u64 = 0xA0;
/// The AHB6 peripherals' reset set register, and its clear register.
const RCC_AHB6RSTSETR: u64 = 0x198;
const RCC_AHB6RSTCLRR: u64 = 0x19C;
/// RM0436's `RCC_MP_AHB6ENSETR`: the processors' enables for the AHB6
/// peripherals.
const RCC_AHB6ENSETR: u64 = 0x218;
/// `AHB6ENSETR`'s `GPUEN` and `AHB6RSTSETR`/`AHB6RSTCLRR`'s `GPURST`.
const GPU_BIT: u32 = 1 << 5;
/// `PLL2CR`: on, locked, Q output enabled (`PLLON`, `PLLRDY`, `DIVQEN`).
const PLL_ON: u32 = 1 << 0;
const PLL_READY: u32 = 1 << 1;
const PLL_Q_ENABLE: u32 = 1 << 5;
/// `PLL2FRACR`'s `FRACLE`: the fraction is in use.
const FRAC_ENABLE: u32 = 1 << 16;

/// The HSI's undivided rate.
const HSI_HZ: u64 = 64_000_000;

/// How long the reset is held and how long the core is left after it:
/// etnaviv waits a microsecond on each side of the release (32 and 128
/// cycles of the slowest clock); ten is as cheap here and leaves a margin.
const RESET_NANOS: u64 = 10_000;

/// What the kernel prepared, for the device node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prepared {
    /// The core's registers, the whole page RM0436 gives them.
    pub(crate) registers: (u64, u64),
    /// Its interrupt.
    pub(crate) interrupt: GicInterrupt,
    /// PLL2's Q output, the core clock.
    pub(crate) core_hz: u64,
}

impl fmt::Display for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GC400 at {:#x}, interrupt {}, core clock {}.{:03} MHz from PLL2 Q, out of reset",
            self.registers.0,
            self.interrupt.id,
            self.core_hz / 1_000_000,
            (self.core_hz / 1000) % 1000
        )
    }
}

/// Find the board's GPU, and prepare it for a driver.
///
/// `Ok(None)` on a machine with no enabled Vivante core at the STM32MP157's
/// address, which is every machine but an STM32MP157 board: QEMU emulates
/// none. `Err` names what stopped the kernel from handing the core over;
/// nothing is written to the RCC until every check has passed, so an `Err`
/// leaves the GPU as firmware left it.
pub(crate) fn prepare(tree: &Fdt<'_>) -> Result<Option<Prepared>, &'static str> {
    let Some(gpu) = tree.compatible_nodes(GPU_COMPATIBLE).find(Node::is_enabled) else {
        return Ok(None);
    };
    let reg = gpu.reg().next().ok_or("the GPU has no registers")?;
    if reg.address != GPU_BASE {
        return Err("the GPU is not the STM32MP157's, whose clock this kernel knows");
    }
    let interrupt = tree
        .gic_interrupt_of(&gpu, 0)
        .ok_or("the GPU's interrupt does not reach the GIC")?;

    let rcc_node = tree
        .compatible_nodes(RCC_COMPATIBLE)
        .next()
        .ok_or("no RCC")?;
    let rcc_reg = rcc_node.reg().next().ok_or("the RCC has no registers")?;
    let rcc = Window::map(rcc_reg.address, PAGE_SIZE)?;
    let core_hz = pll2_q(tree, rcc.mmio)?;

    // Clocks on, then the reset pulsed: the set and clear registers take
    // ones and leave the other bits alone. The clock goes first because a
    // reset released into a stopped clock is not a reset: the core's
    // flip-flops only clear on edges.
    rcc.mmio.write32(RCC_AHB6ENSETR, GPU_BIT);
    rcc.mmio.write32(RCC_AHB6RSTSETR, GPU_BIT);
    spin(RESET_NANOS);
    rcc.mmio.write32(RCC_AHB6RSTCLRR, GPU_BIT);
    spin(RESET_NANOS);
    drop(rcc);

    Ok(Some(Prepared {
        registers: (reg.address, PAGE_SIZE),
        interrupt,
        core_hz,
    }))
}

/// The rate of PLL2's Q output, from the RCC's registers and the device
/// tree's HSE frequency: `ref / (M + 1) * (N + 1 + frac / 8192) / (Q + 1)`,
/// Linux's `pll_recalc_rate`, as `stm32mp1`'s `pll4_q` reads PLL4. PLL1 and
/// PLL2 share one reference mux, `RCK12SELR`, which chooses between the HSI
/// and the HSE only.
fn pll2_q(tree: &Fdt<'_>, rcc: Mmio) -> Result<u64, &'static str> {
    let control = rcc.read32(RCC_PLL2CR);
    if control & (PLL_ON | PLL_READY) != PLL_ON | PLL_READY {
        return Err("PLL2 is not running");
    }
    if control & PLL_Q_ENABLE == 0 {
        return Err("PLL2's Q output, the GPU's core clock, is not enabled");
    }
    let reference = match rcc.read32(RCC_RCK12SELR) & 0x3 {
        0 => HSI_HZ >> (rcc.read32(RCC_HSICFGR) & 0x3),
        1 => hse_hz(tree).ok_or("PLL2 runs from the HSE, whose rate the tree does not give")?,
        _ => return Err("PLL2's reference is not a clock"),
    };
    let config = rcc.read32(RCC_PLL2CFGR1);
    let m = u64::from((config >> 16) & 0x3F) + 1;
    let n = u64::from(config & 0x1FF) + 1;
    let fraction = rcc.read32(RCC_PLL2FRACR);
    let frac = if fraction & FRAC_ENABLE != 0 {
        u64::from((fraction >> 3) & 0x1FFF)
    } else {
        0
    };
    let q = u64::from((rcc.read32(RCC_PLL2CFGR2) >> 8) & 0x7F) + 1;
    let vco = reference * n / m + reference * frac / (m * 8192);
    Ok(vco / q)
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
