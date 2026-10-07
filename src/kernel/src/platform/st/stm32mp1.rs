//! The kernel's part in an STM32MP15 DK board's display: clocks, pins and a
//! reset line, then a device node a ring-3 driver is started on.
//!
//! The DK boards drive HDMI from the chip's LTDC through a Silicon Image
//! `SiI9022` bridge on an I2C bus (`docs/DISPLAY.md` §6). The driver,
//! `src/user/system/native/drivers/display/stm32-ltdc`, programs both controllers; what it cannot do is what every
//! peripheral on the chip shares -- the RCC's clock gates and the GPIO banks'
//! pin multiplexing -- and a driver that could write those could stop the
//! memory controller's clock or take the console's pins. So the kernel does
//! exactly these things, once, while it publishes device nodes, and nothing
//! more:
//!
//! * turns on the LTDC's clock and the bridge's I2C controller's, and puts
//!   that controller's kernel clock on the 64 MHz HSI so the driver has one
//!   timing to program;
//! * checks that the LTDC's pixel clock -- PLL4's Q output, which firmware
//!   set up -- is the 74.25 MHz that CEA-861's 720p60 needs, and leaves the
//!   display alone if it is not;
//! * muxes both controllers' pins as their `pinctrl-0` says;
//! * pulses the bridge's reset line.
//!
//! What the driver then gets is a node with two apertures, the LTDC's
//! registers and the I2C controller's, and the LTDC's interrupt.
//!
//! # The pixel clock, on request
//!
//! A monitor's larger modes want other pixel clocks, and the driver asks
//! for them with `device_clock` ([`pixel_clock`]). What the kernel changes
//! is exactly one field: `PLL4CFGR2.DIVQ`, the divider of PLL4's Q output,
//! with that output gated (`PLL4CR.DIVQEN` clear) while it changes, as
//! Linux's clock tree has the `pll4_q` gate and divider. PLL4's VCO and its
//! P and R outputs are left as firmware set them: on a DK board TF-A runs
//! the VCO at 594 MHz with P at 99 MHz for the SD card's SDMMC1 and R at
//! 74.25 MHz (`fdts/stm32mp15xx-dkx.dtsi`, `pll4_cfg1`), and moving the VCO
//! would move those. So the rates on offer are 594 MHz divided by an
//! integer, the nearest to what was asked.
//!
//! Q is the LTDC's pixel clock, and DSI's, and one choice of thirteen kernel
//! clock muxes (the SAIs, SPI4 to SPI6, USART1 to UART8, LPTIM2 and 3,
//! FDCAN: Linux's `clk-stm32mp1.c` parent lists). The kernel changes Q only
//! while none of those consumers is both clocked and on it, which it reads
//! from the RCC each time, so a board whose firmware or secure world uses Q
//! for something else keeps its rate. A reset makes TF-A program the RCC
//! afresh, so nothing set here outlives the boot.
//!
//! The rate has a ceiling. The STM32MP157A/D datasheet (DS12504 Rev 4, table
//! 94) gives the LTDC's output clock 90 MHz at 2.7 to 3.6 V with its pins at
//! high or very high speed, and Linux's LTDC driver refuses modes above
//! 90 MHz. The DK boards' device tree sets those pins to medium speed
//! (`ltdc_pins_a`, `slew-rate = <1>`), for which the table gives no rate,
//! and the board is seen to run 74.25 MHz there; so when the pins the tree
//! asks for are slower than high speed, the ceiling is the rate firmware
//! left, which the boot check has just found to be 74.25 MHz.

mod gpu;
mod sdmmc;
mod usb;

use alloc::format;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_fdt::{Fdt, GicInterrupt, Node};
use ferrix_native_abi::types::TREE_STM32_HDMI;
use ferrix_sync::Once;

use crate::device::DmaShape;
use crate::discovery::board::{self, BoardBinding, BoardDevice};
use crate::hooks::Full;
use crate::mmio::Mmio;
use crate::{power, timer, vmap};

/// Tell the item what this board has, once, at bring-up: the display, the
/// USB host, the GPU and the SD card to the device registry, in the order their nodes are
/// published, and where the firmware keeps its boot mode to power.
///
/// Called from `main.rs` before device enumeration and before any program
/// can call `reboot(2)`. The item names none of this module: it holds only
/// what is registered here. On a machine that is not an STM32MP15 board each
/// binding finds nothing in the tree and publishes nothing, and the boot mode
/// says the machine keeps none, so the registration is made on every machine
/// rather than guessed at.
///
/// # Errors
///
/// [`Full`] when the device registry has no room for another binding.
pub(crate) fn install(view: &BootView<'_>) -> Result<(), Full> {
    board::register_board(&DISPLAY)?;
    board::register_board(&usb::BINDING)?;
    board::register_board(&gpu::BINDING)?;
    board::register_board(&sdmmc::BINDING)?;
    if let Ok(tree) = crate::discovery::fdt::open(view) {
        note_boot_context(&tree);
    }
    power::register_boot_mode(request_boot_mode);
    Ok(())
}

/// The HDMI output, as the device registry is told about it. Its one clock a
/// driver may set is the pixel clock ([`pixel_clock`]).
static DISPLAY: BoardBinding = BoardBinding {
    binding: TREE_STM32_HDMI,
    label: "display",
    device: "the board's HDMI output",
    prepare: board_device,
    clock: Some(pixel_clock),
};

/// [`prepare`], as the registry asks for it.
///
/// Its registers are minted a page each, which is how RM0436's memory map
/// places every peripheral on the chip, though the tree's `reg` says 0x400:
/// nothing else lives in either page.
fn board_device(tree: &Fdt<'_>) -> Result<Option<BoardDevice>, &'static str> {
    let Some(prepared) = prepare(tree)? else {
        return Ok(None);
    };
    Ok(Some(BoardDevice {
        registers: alloc::vec![prepared.ltdc, prepared.i2c],
        interrupt: prepared.interrupt,
        // The LTDC scans out one run of addresses and does not snoop the
        // caches.
        dma: DmaShape {
            contiguous: true,
            coherent: false,
        },
        summary: format!("{prepared}"),
    }))
}

/// The LTDC's `compatible`.
pub(crate) const LTDC_COMPATIBLE: &str = "st,stm32-ltdc";
/// The HDMI bridge's.
const BRIDGE_COMPATIBLE: &str = "sil,sii9022";
/// The RCC's.
const RCC_COMPATIBLE: &str = "st,stm32mp1-rcc";
/// The STM32MP157's pin controller's, whose banks this module knows.
const PINCTRL_COMPATIBLE: &str = "st,stm32mp157-pinctrl";

/// The first GPIO bank's registers; bank `n` (A = 0 .. K = 10) is `n` pages
/// on (RM0436's memory map, and every `gpio@` node of the pin controller).
const GPIO_BASE: u64 = 0x5000_2000;
/// Banks A to K. Bank Z is elsewhere and secure; nothing here uses it.
const GPIO_BANKS: u32 = 11;

// The RCC's registers, from Linux's `drivers/clk/stm32/clk-stm32mp1.c`.
const RCC_HSICFGR: u64 = 0x18;
const RCC_APB4ENSETR: u64 = 0x200;
const RCC_OCRDYR: u64 = 0x808;
const RCC_RCK4SELR: u64 = 0x824;
const RCC_PLL4CR: u64 = 0x894;
const RCC_PLL4CFGR1: u64 = 0x898;
const RCC_PLL4CFGR2: u64 = 0x89C;
const RCC_PLL4FRACR: u64 = 0x8A0;
const RCC_I2C12CKSELR: u64 = 0x8C0;
const RCC_APB1ENSETR: u64 = 0xA00;
const RCC_AHB4ENSETR: u64 = 0xA28;
/// `APB4ENSETR`: the LTDC.
const LTDC_ENABLE: u32 = 1 << 0;
/// `PLL4CR`: on, locked, Q output enabled.
const PLL_ON: u32 = 1 << 0;
const PLL_READY: u32 = 1 << 1;
const PLL_Q_ENABLE: u32 = 1 << 5;
/// `PLL4FRACR`: the fraction is in use.
const FRAC_ENABLE: u32 = 1 << 16;
/// `OCRDYR`: the HSI is running.
const HSI_READY: u32 = 1 << 0;
/// `I2C12CKSELR`'s value for the HSI.
const I2C12_FROM_HSI: u32 = 2;

/// The HSI's undivided rate.
const HSI_HZ: u64 = 64_000_000;
/// The CSI's.
const CSI_HZ: u64 = 4_000_000;
/// The pixel clock 720p60 needs, and how far off it may be: HDMI sinks take
/// half a percent, which is CEA-861's own tolerance.
const PIXEL_HZ: u64 = 74_250_000;
const PIXEL_TOLERANCE_HZ: u64 = PIXEL_HZ / 200;

/// The I2C controllers whose clocks are on the I2C12 mux, by address, with
/// their `APB1ENSETR` bit.
const I2C12: [(u64, u32); 2] = [(0x4001_2000, 1 << 21), (0x4001_3000, 1 << 22)];

// More of the RCC, for the pixel clock.
const RCC_SPI6CKSELR: u64 = 0xC4;
const RCC_UART1CKSELR: u64 = 0xC8;
const RCC_APB5ENSETR: u64 = 0x208;
const RCC_SAI1CKSELR: u64 = 0x8C8;
const RCC_SAI2CKSELR: u64 = 0x8CC;
const RCC_SAI3CKSELR: u64 = 0x8D0;
const RCC_SAI4CKSELR: u64 = 0x8D4;
const RCC_SPI2S45CKSELR: u64 = 0x8E0;
const RCC_UART6CKSELR: u64 = 0x8E4;
const RCC_UART24CKSELR: u64 = 0x8E8;
const RCC_UART35CKSELR: u64 = 0x8EC;
const RCC_UART78CKSELR: u64 = 0x8F0;
const RCC_FDCANCKSELR: u64 = 0x90C;
const RCC_LPTIM23CKSELR: u64 = 0x930;
const RCC_APB2ENSETR: u64 = 0xA08;
const RCC_APB3ENSETR: u64 = 0xA10;
/// `APB4ENSETR`: DSI, whose pixel clock is PLL4's Q output as the LTDC's is.
const DSI_ENABLE: u32 = 1 << 4;
/// `PLL4CFGR2`'s DIVQ field: Q's divider, less one.
const DIVQ_SHIFT: u32 = 8;
const DIVQ_MASK: u32 = 0x7F << DIVQ_SHIFT;
/// The largest divider DIVQ holds.
const DIVQ_MAX: u64 = 128;

/// The LTDC's fastest pixel clock with its pins at high speed or faster:
/// DS12504 Rev 4, table 94.
const LTDC_MAX_HZ: u64 = 90_000_000;
/// `OSPEEDR`'s high speed, the slowest the datasheet gives the 90 MHz for.
const HIGH_SPEED: u32 = 2;

/// Every kernel clock that can run from PLL4's Q output, as Linux's
/// `clk-stm32mp1.c` has them: the mux register, its field's mask, the value
/// that picks `pll4_q`, and the `ENSETR` register and bits of the
/// peripherals behind the mux. Q is only changed while none of these is both
/// on and on Q.
#[rustfmt::skip]
const Q_CONSUMERS: [(&str, u64, u32, u32, u64, u32); 13] = [
    ("SPI4 or SPI5 runs from PLL4's Q output", RCC_SPI2S45CKSELR, 0x7, 1, RCC_APB2ENSETR, (1 << 9) | (1 << 10)),
    ("SPI6 runs from PLL4's Q output", RCC_SPI6CKSELR, 0x7, 1, RCC_APB5ENSETR, 1 << 0),
    ("LPTIM2 or LPTIM3 runs from PLL4's Q output", RCC_LPTIM23CKSELR, 0x7, 1, RCC_APB3ENSETR, (1 << 0) | (1 << 1)),
    ("USART1 runs from PLL4's Q output", RCC_UART1CKSELR, 0x7, 4, RCC_APB5ENSETR, 1 << 4),
    ("USART2 or UART4 runs from PLL4's Q output", RCC_UART24CKSELR, 0x7, 1, RCC_APB1ENSETR, (1 << 14) | (1 << 16)),
    ("USART3 or UART5 runs from PLL4's Q output", RCC_UART35CKSELR, 0x7, 1, RCC_APB1ENSETR, (1 << 15) | (1 << 17)),
    ("USART6 runs from PLL4's Q output", RCC_UART6CKSELR, 0x7, 1, RCC_APB2ENSETR, 1 << 13),
    ("UART7 or UART8 runs from PLL4's Q output", RCC_UART78CKSELR, 0x7, 1, RCC_APB1ENSETR, (1 << 18) | (1 << 19)),
    ("FDCAN runs from PLL4's Q output", RCC_FDCANCKSELR, 0x3, 2, RCC_APB2ENSETR, 1 << 24),
    ("SAI1 or DFSDM runs from PLL4's Q output", RCC_SAI1CKSELR, 0x7, 0, RCC_APB2ENSETR, (1 << 16) | (1 << 21)),
    ("SAI2 runs from PLL4's Q output", RCC_SAI2CKSELR, 0x7, 0, RCC_APB2ENSETR, 1 << 17),
    ("SAI3 runs from PLL4's Q output", RCC_SAI3CKSELR, 0x7, 0, RCC_APB2ENSETR, 1 << 18),
    ("SAI4 runs from PLL4's Q output", RCC_SAI4CKSELR, 0x7, 0, RCC_APB3ENSETR, 1 << 8),
];

/// What the kernel found of the pixel clock at boot, for [`pixel_clock`].
#[derive(Clone, Copy, Debug)]
struct PixelClock {
    /// The RCC's registers.
    rcc: u64,
    /// PLL4's reference and VCO, as found.
    reference_hz: u64,
    vco_hz: u64,
    /// The fastest rate the LTDC's pins are held to.
    ceiling_hz: u64,
}

/// Set once, by a [`prepare`] that handed the display over.
static PIXEL: Once<PixelClock> = Once::new();
/// Held while a rate is being set: one driver sets it, but nothing stops a
/// second thread of it asking at once.
static SETTING: AtomicBool = AtomicBool::new(false);

impl PixelClock {
    /// Q's divider for the rate nearest `hz` at or under the ceiling.
    fn divider(&self, hz: u64) -> u64 {
        let slowest = self.vco_hz.div_ceil(self.ceiling_hz).clamp(1, DIVQ_MAX);
        let near = (self.vco_hz / hz.max(1)).clamp(slowest, DIVQ_MAX);
        [near, (near + 1).min(DIVQ_MAX)]
            .into_iter()
            .min_by_key(|&q| (self.vco_hz / q).abs_diff(hz))
            .unwrap_or(near)
    }
}

/// The rate of the LTDC's pixel clock nearest `hz` that the kernel will
/// make, and with `set`, that rate made: PLL4's Q divider changed with the
/// output gated, and read back. `Err` names why it would not be.
pub(crate) fn pixel_clock(hz: u64, set: bool) -> Result<u64, &'static str> {
    let clock = PIXEL
        .get()
        .ok_or("this machine has no pixel clock the kernel sets")?;
    let q = clock.divider(hz);
    let rate = clock.vco_hz / q;
    if !set {
        return Ok(rate);
    }
    if SETTING.swap(true, Ordering::Acquire) {
        return Err("another pixel clock change is under way");
    }
    let done = set_divider(clock, q);
    SETTING.store(false, Ordering::Release);
    // A line either way: the board's serial log is where a mode that shows
    // nothing is looked into.
    match done {
        Ok(()) => crate::console::println!(
            "  display  pixel clock {} MHz: PLL4's VCO over {q}",
            Mhz(rate)
        ),
        Err(why) => crate::console::println!("  display  pixel clock left alone: {why}"),
    }
    done.map(|()| rate)
}

/// Set PLL4's Q divider to `q`, if PLL4 is still what boot found and
/// nothing but the LTDC runs from Q.
fn set_divider(clock: &PixelClock, q: u64) -> Result<(), &'static str> {
    let rcc = Window::map(clock.rcc, PAGE_SIZE)?;
    let r = rcc.mmio;
    let control = r.read32(RCC_PLL4CR);
    if control & (PLL_ON | PLL_READY | PLL_Q_ENABLE) != PLL_ON | PLL_READY | PLL_Q_ENABLE {
        return Err("PLL4's Q output is off");
    }
    if vco(r, clock.reference_hz) != clock.vco_hz {
        return Err("PLL4's VCO is not the one boot found");
    }
    if r.read32(RCC_APB4ENSETR) & DSI_ENABLE != 0 {
        return Err("DSI is clocked, and its pixel clock is PLL4's Q too");
    }
    for (why, select, mask, pll4_q, enable, bits) in Q_CONSUMERS {
        if r.read32(select) & mask == pll4_q && r.read32(enable) & bits != 0 {
            return Err(why);
        }
    }
    let field = u32::try_from(q - 1).map_err(|_| "no such divider")? << DIVQ_SHIFT;
    let config = r.read32(RCC_PLL4CFGR2);
    if config & DIVQ_MASK == field {
        return Ok(());
    }
    // Gated while the divider changes, so the LTDC never sees a clock
    // pulse cut short; P and R run on untouched.
    r.write32(RCC_PLL4CR, control & !PLL_Q_ENABLE);
    r.write32(RCC_PLL4CFGR2, (config & !DIVQ_MASK) | field);
    r.write32(RCC_PLL4CR, control);
    if r.read32(RCC_PLL4CFGR2) & DIVQ_MASK != field {
        return Err("the RCC did not take PLL4's Q divider");
    }
    Ok(())
}

/// What the kernel prepared, for the device node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Prepared {
    /// The LTDC's registers, the whole page RM0436 gives it.
    pub(crate) ltdc: (u64, u64),
    /// The I2C controller's, likewise.
    pub(crate) i2c: (u64, u64),
    /// The LTDC's interrupt.
    pub(crate) interrupt: GicInterrupt,
    /// The bridge's address on the bus.
    pub(crate) bridge: u32,
    /// The pixel clock found.
    pub(crate) pixel_hz: u64,
    /// The fastest the driver may set it to.
    pub(crate) ceiling_hz: u64,
    /// Pins muxed.
    pub(crate) pins: usize,
}

impl fmt::Display for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "LTDC at {:#x}, HDMI bridge at {:#x} on I2C {:#x}, pixel clock {} MHz (at most {}), {} pins muxed",
            self.ltdc.0,
            self.bridge,
            self.i2c.0,
            Mhz(self.pixel_hz),
            Mhz(self.ceiling_hz),
            self.pins
        )
    }
}

/// A rate in Hz, printed in MHz to the kHz.
pub(crate) struct Mhz(pub(crate) u64);

impl fmt::Display for Mhz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03}", self.0 / 1_000_000, (self.0 / 1000) % 1000)
    }
}

/// Find the board's display, and prepare it for a driver.
///
/// `Ok(None)` on a machine with no enabled LTDC wired to a `SiI9022`, which is
/// every machine but a DK board. `Err` names what stopped the kernel from
/// handing the display over; nothing it did before that is undone, and
/// nothing it did is harmful to leave: clocks on and pins muxed.
pub(crate) fn prepare(tree: &Fdt<'_>) -> Result<Option<Prepared>, &'static str> {
    let Some(ltdc) = tree
        .compatible_nodes(LTDC_COMPATIBLE)
        .find(Node::is_enabled)
    else {
        return Ok(None);
    };
    let Some((i2c, bridge)) = bridge_and_bus(tree) else {
        return Ok(None);
    };
    let ltdc_reg = ltdc.reg().next().ok_or("the LTDC has no registers")?;
    let i2c_reg = i2c
        .reg()
        .next()
        .ok_or("the I2C controller has no registers")?;
    let interrupt = tree
        .gic_interrupt_of(&ltdc, 0)
        .ok_or("the LTDC's interrupt does not reach the GIC")?;
    let bridge_address = bridge
        .reg()
        .next()
        .map(|region| region.address)
        .and_then(|address| u32::try_from(address).ok())
        .ok_or("the bridge has no address")?;
    let (_, i2c_enable) = I2C12
        .iter()
        .find(|(base, _)| *base == i2c_reg.address)
        .copied()
        .ok_or("the bridge is on an I2C controller whose clock this kernel does not know")?;
    // The bank addresses below are this pin controller's.
    if tree.compatible_nodes(PINCTRL_COMPATIBLE).next().is_none() {
        return Err("no STM32MP157 pin controller");
    }

    let rcc_node = tree
        .compatible_nodes(RCC_COMPATIBLE)
        .next()
        .ok_or("no RCC")?;
    let rcc_reg = rcc_node.reg().next().ok_or("the RCC has no registers")?;
    let rcc = Window::map(rcc_reg.address, PAGE_SIZE)?;

    let (reference_hz, vco_hz, pixel_hz) = pll4_q(tree, rcc.mmio)?;
    if pixel_hz.abs_diff(PIXEL_HZ) > PIXEL_TOLERANCE_HZ {
        return Err("PLL4's Q output is not the 74.25 MHz 720p60 needs");
    }
    if rcc.mmio.read32(RCC_HSICFGR) & 0x3 != 0 || rcc.mmio.read32(RCC_OCRDYR) & HSI_READY == 0 {
        return Err("the HSI is not running at 64 MHz");
    }

    // Clocks: the controllers, then every bank a pin is in. The set
    // registers take ones and leave the other bits alone.
    rcc.mmio.write32(RCC_APB4ENSETR, LTDC_ENABLE);
    rcc.mmio.write32(RCC_APB1ENSETR, i2c_enable);
    let selected = rcc.mmio.read32(RCC_I2C12CKSELR);
    rcc.mmio
        .write32(RCC_I2C12CKSELR, (selected & !0x7) | I2C12_FROM_HSI);

    let mut pins = Vec::new();
    collect_pins(tree, &ltdc, &mut pins)?;
    let ltdc_speed = pins.iter().map(|pin| pin.speed).min().unwrap_or(0);
    collect_pins(tree, &i2c, &mut pins)?;
    let reset = reset_line(tree, &bridge)?;
    let mut banks = pins.iter().fold(0_u32, |mask, pin| mask | (1 << pin.bank));
    if let Some((bank, _, _)) = reset {
        banks |= 1 << bank;
    }
    rcc.mmio.write32(RCC_AHB4ENSETR, banks);
    drop(rcc);

    for pin in &pins {
        pin.apply()?;
    }
    if let Some((bank, line, active_low)) = reset {
        pulse_reset(bank, line, active_low)?;
    }

    // Only now is the display handed over, so only now may its clock move.
    let ceiling_hz = if ltdc_speed >= HIGH_SPEED {
        LTDC_MAX_HZ
    } else {
        pixel_hz.min(LTDC_MAX_HZ)
    };
    let _ = PIXEL.call_once(|| PixelClock {
        rcc: rcc_reg.address,
        reference_hz,
        vco_hz,
        ceiling_hz,
    });

    Ok(Some(Prepared {
        ltdc: (ltdc_reg.address, PAGE_SIZE),
        i2c: (i2c_reg.address, PAGE_SIZE),
        interrupt,
        bridge: bridge_address,
        pixel_hz,
        ceiling_hz,
        pins: pins.len(),
    }))
}

/// The enabled `SiI9022` and the I2C controller it is a child of.
fn bridge_and_bus<'a>(tree: &Fdt<'a>) -> Option<(Node<'a>, Node<'a>)> {
    // The nodes on the path to the one being looked at, by depth.
    let mut path: Vec<Node<'a>> = Vec::new();
    for node in tree.nodes() {
        path.truncate(node.depth);
        if node.is_compatible(BRIDGE_COMPATIBLE) && node.is_enabled() {
            let parent = path.last().copied()?;
            return parent.is_enabled().then_some((parent, node));
        }
        path.push(node);
    }
    None
}

/// PLL4's reference, its VCO, and the rate of its Q output, from the RCC's
/// registers and the device tree's HSE frequency: `ref / (M + 1) * (N + 1 +
/// frac / 8192) / (Q + 1)`, Linux's `pll_recalc_rate`.
fn pll4_q(tree: &Fdt<'_>, rcc: Mmio) -> Result<(u64, u64, u64), &'static str> {
    let control = rcc.read32(RCC_PLL4CR);
    if control & (PLL_ON | PLL_READY | PLL_Q_ENABLE) != PLL_ON | PLL_READY | PLL_Q_ENABLE {
        return Err("PLL4's Q output is off");
    }
    let reference = match rcc.read32(RCC_RCK4SELR) & 0x3 {
        0 => HSI_HZ >> (rcc.read32(RCC_HSICFGR) & 0x3),
        1 => hse_hz(tree).ok_or("PLL4 runs from the HSE, whose rate the tree does not give")?,
        2 => CSI_HZ,
        _ => return Err("PLL4's reference is not a clock"),
    };
    let vco = vco(rcc, reference);
    let q = u64::from((rcc.read32(RCC_PLL4CFGR2) & DIVQ_MASK) >> DIVQ_SHIFT) + 1;
    Ok((reference, vco, vco / q))
}

/// PLL4's VCO rate from `reference`, as its M, N and fraction say now.
fn vco(rcc: Mmio, reference: u64) -> u64 {
    let config = rcc.read32(RCC_PLL4CFGR1);
    let m = u64::from((config >> 16) & 0x3F) + 1;
    let n = u64::from(config & 0x1FF) + 1;
    let fraction = rcc.read32(RCC_PLL4FRACR);
    let frac = if fraction & FRAC_ENABLE != 0 {
        u64::from((fraction >> 3) & 0x1FFF)
    } else {
        0
    };
    reference * n / m + reference * frac / (m * 8192)
}

/// The HSE's rate: the `clock-frequency` of the fixed clock named `clk-hse`.
fn hse_hz(tree: &Fdt<'_>) -> Option<u64> {
    tree.nodes()
        .find(|node| node.name == "clk-hse")?
        .property("clock-frequency")?
        .as_u32()
        .map(u64::from)
}

/// One pin, as a `pinmux` cell and its group's properties say.
#[derive(Clone, Copy, Debug)]
struct Pin {
    bank: u32,
    line: u32,
    /// `MODER`: 0 input, 1 output, 2 alternate function, 3 analog.
    mode: u32,
    /// The alternate function, for mode 2.
    function: u32,
    open_drain: bool,
    /// `PUPDR`: 0 none, 1 up, 2 down.
    pull: u32,
    speed: u32,
}

impl Pin {
    /// Write the pin's bank registers, alternate function before mode as
    /// Linux does, so it never drives the wrong function.
    fn apply(&self) -> Result<(), &'static str> {
        let bank = Window::map(GPIO_BASE + u64::from(self.bank) * PAGE_SIZE, PAGE_SIZE)?;
        let r = bank.mmio;
        let two = self.line * 2;
        let update = |offset: u64, shift: u32, width: u32, value: u32| {
            let mask = ((1 << width) - 1) << shift;
            r.write32(
                offset,
                (r.read32(offset) & !mask) | ((value << shift) & mask),
            );
        };
        let (afr, shift) = if self.line < 8 {
            (GPIO_AFRL, self.line * 4)
        } else {
            (GPIO_AFRH, (self.line - 8) * 4)
        };
        update(afr, shift, 4, self.function);
        update(GPIO_OTYPER, self.line, 1, u32::from(self.open_drain));
        update(GPIO_OSPEEDR, two, 2, self.speed);
        update(GPIO_PUPDR, two, 2, self.pull);
        update(GPIO_MODER, two, 2, self.mode);
        Ok(())
    }
}

// A GPIO bank's registers.
const GPIO_MODER: u64 = 0x00;
const GPIO_OTYPER: u64 = 0x04;
const GPIO_OSPEEDR: u64 = 0x08;
const GPIO_PUPDR: u64 = 0x0C;
const GPIO_BSRR: u64 = 0x18;
const GPIO_AFRL: u64 = 0x20;
const GPIO_AFRH: u64 = 0x24;

/// Every pin `node`'s `pinctrl-0` groups name.
fn collect_pins(tree: &Fdt<'_>, node: &Node<'_>, out: &mut Vec<Pin>) -> Result<(), &'static str> {
    let groups = node
        .property("pinctrl-0")
        .ok_or("a display controller has no pinctrl-0")?;
    for phandle in groups.cells() {
        let mut found = false;
        for pins in children(tree, phandle) {
            found = true;
            pins_of(&pins, out)?;
        }
        if !found {
            return Err("a pinctrl-0 group has no pins");
        }
    }
    Ok(())
}

/// The children of the node whose phandle is `phandle`.
fn children<'a>(tree: &Fdt<'a>, phandle: u32) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    let mut parent = None;
    for node in tree.nodes() {
        match parent {
            None if node.phandle() == Some(phandle) => parent = Some(node.depth),
            None => {}
            Some(depth) if node.depth <= depth => break,
            Some(depth) if node.depth == depth + 1 => found.push(node),
            Some(_) => {}
        }
    }
    found
}

/// The pins of one `pins` subnode: its `pinmux` cells, each
/// `STM32_PINMUX(port, line, mode)` -- `(port * 16 + line) << 8 | mode`, with
/// mode 0 GPIO, 1 to 16 alternate functions 0 to 15, 17 analog -- and the
/// group's bias, drive and slew rate.
fn pins_of(pins: &Node<'_>, out: &mut Vec<Pin>) -> Result<(), &'static str> {
    let cells = pins
        .property("pinmux")
        .ok_or("a pin group has no pinmux")?
        .cells();
    let pull = if pins.property("bias-pull-up").is_some() {
        1
    } else if pins.property("bias-pull-down").is_some() {
        2
    } else {
        0
    };
    let open_drain = pins.property("drive-open-drain").is_some();
    let speed = pins
        .property("slew-rate")
        .and_then(|property| property.as_u32())
        .unwrap_or(0)
        .min(3);
    for cell in cells {
        let number = cell >> 8;
        let (bank, line) = (number / 16, number % 16);
        if bank >= GPIO_BANKS {
            return Err("a pin is in a bank this kernel does not map");
        }
        let (mode, function) = match cell & 0xFF {
            0 => (0, 0),
            alternate @ 1..=16 => (2, alternate - 1),
            17 => (3, 0),
            _ => return Err("a pinmux cell names no mode"),
        };
        out.push(Pin {
            bank,
            line,
            mode,
            function,
            open_drain,
            pull,
            speed,
        });
    }
    Ok(())
}

/// The bridge's `reset-gpios`: bank, line and whether it is active low.
fn reset_line(tree: &Fdt<'_>, bridge: &Node<'_>) -> Result<Option<(u32, u32, bool)>, &'static str> {
    let Some(property) = bridge.property("reset-gpios") else {
        return Ok(None);
    };
    let mut cells = property.cells();
    let (Some(phandle), Some(line), Some(flags)) = (cells.next(), cells.next(), cells.next())
    else {
        return Err("the bridge's reset-gpios is not one GPIO");
    };
    let bank_name = tree
        .node_by_phandle(phandle)
        .and_then(|node| node.property("st,bank-name")?.as_str())
        .ok_or("the bridge's reset GPIO is not a bank's")?;
    let bank = match bank_name.as_bytes() {
        [b'G', b'P', b'I', b'O', letter @ b'A'..=b'K'] => u32::from(letter - b'A'),
        _ => return Err("the bridge's reset GPIO is in a bank this kernel does not map"),
    };
    if line > 15 {
        return Err("the bridge's reset GPIO names no line");
    }
    Ok(Some((bank, line, flags & 1 != 0)))
}

/// Assert the reset line for a millisecond and let it go: the `SiI9022`'s
/// datasheet asks for 100 µs, and Linux gives it 150.
fn pulse_reset(bank: u32, line: u32, active_low: bool) -> Result<(), &'static str> {
    let window = Window::map(GPIO_BASE + u64::from(bank) * PAGE_SIZE, PAGE_SIZE)?;
    let r = window.mmio;
    let high = 1_u32 << line;
    let low = 1_u32 << (line + 16);
    let (asserted, released) = if active_low { (low, high) } else { (high, low) };
    // The level first, then the pin an output: no glitch the other way.
    r.write32(GPIO_BSRR, asserted);
    let two = line * 2;
    let otype = r.read32(GPIO_OTYPER) & !(1 << line);
    r.write32(GPIO_OTYPER, otype);
    let mode = r.read32(GPIO_MODER) & !(0x3 << two);
    r.write32(GPIO_MODER, mode | (1 << two));
    spin(1_000_000);
    r.write32(GPIO_BSRR, released);
    Ok(())
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

/// The TAMP's `compatible`, whose backup registers keep their contents
/// through a reset.
const TAMP_COMPATIBLE: &str = "st,stm32-tamp";

/// `TAMP_BOOT_CONTEXT`, backup register 20: the ROM leaves the boot device in
/// it, and its low byte is the forced boot mode U-Boot reads, acts on and
/// clears at its next start (U-Boot's `arch/arm/mach-stm32mp`, `stm32.h` and
/// `setup_boot_mode`).
const TAMP_BOOT_CONTEXT: u64 = 0x100 + 4 * 20;
/// The forced boot mode's bits.
const FORCED_MASK: u32 = 0xFF;

/// The words `reboot(2)`'s RESTART2 can carry on a DK board, what U-Boot's
/// forced boot mode for each is, and what it does with it: ST's names for
/// its `reboot-mode` node where it has one, and `firmware` for systemd's
/// `reboot --firmware-setup`.
///
/// Recovery runs U-Boot's `altbootcmd` before the autoboot, and the board's
/// environment makes that stop at the prompt (`docs/vendor/st/stm32mp157-dk.md`), which
/// is what `firmware` means.
const BOOT_MODES: [(&str, u32, &str); 5] = [
    (
        "firmware",
        0x02,
        "U-Boot runs altbootcmd, which stops at its prompt",
    ),
    (
        "recovery",
        0x02,
        "U-Boot runs altbootcmd, which stops at its prompt",
    ),
    ("fastboot", 0x01, "U-Boot starts fastboot"),
    ("ums", 0x10, "U-Boot puts the SD card on USB"),
    ("ums_mmc0", 0x10, "U-Boot puts the SD card on USB"),
];

/// Physical address of `TAMP_BOOT_CONTEXT`, when the tree has a TAMP; zero
/// otherwise.
static BOOT_CONTEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Note where the forced boot mode is kept, once at boot: at a reboot the
/// tree may be out of reach.
fn note_boot_context(tree: &Fdt<'_>) {
    let Some(region) = tree
        .compatible_nodes(TAMP_COMPATIBLE)
        .find(Node::is_enabled)
        .and_then(|tamp| tamp.reg().next())
    else {
        return;
    };
    if region.size >= TAMP_BOOT_CONTEXT + 4 {
        BOOT_CONTEXT.store(region.address + TAMP_BOOT_CONTEXT, Ordering::Relaxed);
    }
}

/// Ask the firmware to come back up as `word` says, at the next reset: set
/// U-Boot's forced boot mode and read it back. What U-Boot will do, or why
/// nothing will change: a machine with no TAMP, or a word U-Boot has no mode
/// for, restarts as it would have without it, as Linux restarts with a word
/// no reboot-mode driver knows.
fn request_boot_mode(word: &str) -> Result<&'static str, &'static str> {
    let at = BOOT_CONTEXT.load(Ordering::Relaxed);
    if at == 0 {
        return Err("this machine keeps no boot mode");
    }
    let (_, mode, what) = BOOT_MODES
        .iter()
        .find(|(name, _, _)| *name == word)
        .copied()
        .ok_or("U-Boot has no boot mode of that name")?;
    let page = at - at % PAGE_SIZE;
    let window = Window::map(page, PAGE_SIZE)?;
    let offset = at - page;
    let context = window.mmio.read32(offset);
    window.mmio.write32(offset, (context & !FORCED_MASK) | mode);
    if window.mmio.read32(offset) & FORCED_MASK != mode {
        return Err("the TAMP did not take the boot mode");
    }
    Ok(what)
}
