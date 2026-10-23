// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0,
// drivers/gpu/drm/i915/display/intel_gmbus_regs.h:
// Copyright © 2022 Intel Corporation
// drivers/gpu/drm/i915/display/intel_gmbus.c:
// Copyright (c) 2006 Dave Airlie <airlied@linux.ie>
// Copyright © 2006-2008,2010 Intel Corporation
// drivers/gpu/drm/i915/display/intel_hdmi.c (icl_encoder_to_ddc_pin):
// Copyright © 2006 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! The monitor's own EDID, read over GMBUS, the display engine's I2C
//! controller on the port's DDC pins (`docs/DESKTOP.md` §3.4, step 3g).
//!
//! i915's `do_gmbus_xfer` for the two messages an EDID read is -- write the
//! offset, read the block -- as one index cycle (`gmbus_index_xfer`): pin
//! and rate in `GMBUS0`, the cycle in `GMBUS1`, four bytes at a time out of
//! `GMBUS3` as `GMBUS2` says each word is ready, the wait phase, a STOP
//! cycle, idle, `GMBUS0` cleared. A NAK on the first message is retried
//! once, as i915 retries it for passive adapters; any NAK clears the error
//! with `GMBUS_SW_CLR_INT`. No interrupt: every wait is a poll.

use super::hotplug::Port;
use crate::io::{Clock, Mmio};

/// `PCH_DISPLAY_BASE` + `0x5100`: `GMBUS0`, the pin and the rate.
pub const GMBUS0: u32 = 0xC_5100;
/// `GMBUS1`: the command.
pub const GMBUS1: u32 = 0xC_5104;
/// `GMBUS2`: the status.
pub const GMBUS2: u32 = 0xC_5108;
/// `GMBUS3`: the data, four bytes at a time.
pub const GMBUS3: u32 = 0xC_510C;
/// `GMBUS4`: the interrupt mask, kept 0.
pub const GMBUS4: u32 = 0xC_5110;

/// `GMBUS_RATE_100KHZ`.
pub const GMBUS_RATE_100KHZ: u32 = 0;
/// `GMBUS_SW_CLR_INT`.
pub const GMBUS_SW_CLR_INT: u32 = 1 << 31;
/// `GMBUS_SW_RDY`.
pub const GMBUS_SW_RDY: u32 = 1 << 30;
/// `GMBUS_CYCLE_WAIT`.
pub const GMBUS_CYCLE_WAIT: u32 = 1 << 25;
/// `GMBUS_CYCLE_INDEX`.
pub const GMBUS_CYCLE_INDEX: u32 = 2 << 25;
/// `GMBUS_CYCLE_STOP`.
pub const GMBUS_CYCLE_STOP: u32 = 4 << 25;
/// `GMBUS_BYTE_COUNT_SHIFT`.
pub const GMBUS_BYTE_COUNT_SHIFT: u32 = 16;
/// `GMBUS_SLAVE_INDEX_SHIFT`.
pub const GMBUS_SLAVE_INDEX_SHIFT: u32 = 8;
/// `GMBUS_SLAVE_ADDR_SHIFT`.
pub const GMBUS_SLAVE_ADDR_SHIFT: u32 = 1;
/// `GMBUS_SLAVE_READ`.
pub const GMBUS_SLAVE_READ: u32 = 1;
/// `GMBUS_HW_WAIT_PHASE`.
pub const GMBUS_HW_WAIT_PHASE: u32 = 1 << 14;
/// `GMBUS_HW_RDY`.
pub const GMBUS_HW_RDY: u32 = 1 << 11;
/// `GMBUS_SATOER`: the target did not acknowledge.
pub const GMBUS_SATOER: u32 = 1 << 10;
/// `GMBUS_ACTIVE`.
pub const GMBUS_ACTIVE: u32 = 1 << 9;

/// `GMBUS_PIN_1_BXT`: the first combo port's DDC pins; `GMBUS_PIN_9_TC1_ICP`
/// the first Type-C port's.
const PIN_COMBO: u32 = 1;
const PIN_TYPE_C: u32 = 9;

/// The DDC address of a monitor's EDID (`DDC_ADDR`).
pub const DDC_ADDR: u8 = 0x50;
/// One EDID block.
pub const EDID_BLOCK: usize = 128;

/// `gmbus_wait`'s patience: 50 ms.
const WAIT_US: u64 = 50_000;
/// `gmbus_wait_idle`'s: 10 ms.
const IDLE_US: u64 = 10_000;

/// Why a GMBUS transfer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failed {
    /// The target did not acknowledge: nothing on the pins, or no EDID.
    Nak,
    /// The controller did not answer in time.
    Timeout,
}

/// The DDC pin pair a port's monitor is read on: `icl_encoder_to_ddc_pin`
/// as `adlp_ddc_pin_map` leaves it, `GMBUS_PIN_1_BXT` plus the combo port,
/// `GMBUS_PIN_9_TC1_ICP` plus the Type-C one.
#[must_use]
pub const fn pin_of(port: Port) -> u32 {
    match port {
        Port::Combo(index) => PIN_COMBO + index as u32,
        Port::TypeC(number) => PIN_TYPE_C + (number as u32).saturating_sub(1),
    }
}

/// Wait for any of `status` in `GMBUS2`, or a NAK: `gmbus_wait`.
fn wait<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    status: u32,
) -> Result<(), Failed> {
    let start = clock.now_us();
    loop {
        let read = mmio.read32(GMBUS2);
        if read & GMBUS_SATOER != 0 {
            return Err(Failed::Nak);
        }
        if read & status != 0 {
            return Ok(());
        }
        if clock.now_us().saturating_sub(start) >= WAIT_US {
            return Err(Failed::Timeout);
        }
        clock.delay_us(2);
    }
}

/// Wait for the controller to go idle: `gmbus_wait_idle`.
fn wait_idle<M: Mmio + ?Sized, C: Clock + ?Sized>(mmio: &mut M, clock: &mut C) -> bool {
    crate::io::wait_for_register(mmio, clock, GMBUS2, GMBUS_ACTIVE, 0, IDLE_US).is_ok()
}

/// Read `out.len()` bytes (at most 256) from target `addr` starting at its
/// register `index`, on `pin`: one index transfer, retried once on a NAK.
///
/// # Errors
///
/// [`Failed::Nak`] when nothing answers, [`Failed::Timeout`] when the
/// controller does not.
pub fn read_indexed<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    pin: u32,
    addr: u8,
    index: u8,
    out: &mut [u8],
) -> Result<(), Failed> {
    let mut tried = false;
    loop {
        match transfer(mmio, clock, pin, addr, index, out) {
            Err(Failed::Nak) if !tried => tried = true,
            other => return other,
        }
    }
}

/// One `do_gmbus_xfer` of an index read.
fn transfer<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    pin: u32,
    addr: u8,
    index: u8,
    out: &mut [u8],
) -> Result<(), Failed> {
    let size = u32::try_from(out.len().min(256)).unwrap_or(256);
    mmio.write32(GMBUS4, 0);
    mmio.write32(GMBUS0, pin | GMBUS_RATE_100KHZ);
    mmio.write32(
        GMBUS1,
        GMBUS_CYCLE_INDEX
            | (u32::from(index) << GMBUS_SLAVE_INDEX_SHIFT)
            | GMBUS_CYCLE_WAIT
            | (size << GMBUS_BYTE_COUNT_SHIFT)
            | (u32::from(addr) << GMBUS_SLAVE_ADDR_SHIFT)
            | GMBUS_SLAVE_READ
            | GMBUS_SW_RDY,
    );
    let read = (|| {
        let mut chunks = out.chunks_mut(4);
        for chunk in chunks.by_ref() {
            wait(mmio, clock, GMBUS_HW_RDY)?;
            let word = mmio.read32(GMBUS3).to_le_bytes();
            for (byte, value) in chunk.iter_mut().zip(word) {
                *byte = value;
            }
        }
        wait(mmio, clock, GMBUS_HW_WAIT_PHASE)
    })();
    match read {
        Ok(()) => {
            mmio.write32(GMBUS1, GMBUS_CYCLE_STOP | GMBUS_SW_RDY);
            let idle = wait_idle(mmio, clock);
            mmio.write32(GMBUS0, 0);
            if idle { Ok(()) } else { Err(Failed::Timeout) }
        }
        Err(Failed::Nak) => {
            // Wait for idle before clearing the NAK, then toggle the
            // software clear, which resets the controller.
            let _ = wait_idle(mmio, clock);
            mmio.write32(GMBUS1, GMBUS_SW_CLR_INT);
            mmio.write32(GMBUS1, 0);
            mmio.write32(GMBUS0, 0);
            Err(Failed::Nak)
        }
        Err(Failed::Timeout) => {
            mmio.write32(GMBUS0, 0);
            Err(Failed::Timeout)
        }
    }
}

/// The monitor's EDID on `pin`: the base block and, where it says it has
/// one, the first extension (the CTA block with the HDMI modes). Gives how
/// many bytes of `out` are the EDID.
///
/// # Errors
///
/// As [`read_indexed`]; and [`Failed::Nak`] for a first block that is not
/// an EDID (its header is not `00 FF FF FF FF FF FF 00`).
pub fn read_edid<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    pin: u32,
    out: &mut [u8; 2 * EDID_BLOCK],
) -> Result<usize, Failed> {
    let (base, extension) = out.split_at_mut(EDID_BLOCK);
    read_indexed(mmio, clock, pin, DDC_ADDR, 0, base)?;
    if base.get(..8) != Some(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0][..]) {
        return Err(Failed::Nak);
    }
    if base.get(126).copied().unwrap_or(0) == 0 {
        return Ok(EDID_BLOCK);
    }
    match read_indexed(mmio, clock, pin, DDC_ADDR, 128, extension) {
        Ok(()) => Ok(2 * EDID_BLOCK),
        // The base block alone still names the monitor.
        Err(_) => Ok(EDID_BLOCK),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::{
        Failed, GMBUS_ACTIVE, GMBUS_HW_RDY, GMBUS_HW_WAIT_PHASE, GMBUS_SATOER, GMBUS_SW_RDY,
        GMBUS0, GMBUS1, GMBUS2, GMBUS3, pin_of, read_edid,
    };
    use crate::display::hotplug::Port;
    use crate::io::{Clock, Mmio};

    /// A GMBUS with a monitor on pin 1 holding `edid`, or nothing.
    struct Bus {
        edid: Vec<u8>,
        pin: u32,
        cursor: usize,
        left: usize,
        active: bool,
        nak: bool,
        naks_left: u32,
        writes: Vec<(u32, u32)>,
    }

    impl Bus {
        fn new(edid: Vec<u8>) -> Self {
            Self {
                edid,
                pin: 0,
                cursor: 0,
                left: 0,
                active: false,
                nak: false,
                naks_left: 0,
                writes: Vec::new(),
            }
        }
    }

    impl Mmio for Bus {
        fn read32(&mut self, offset: u32) -> u32 {
            match offset {
                GMBUS2 => {
                    if self.nak {
                        return GMBUS_SATOER | GMBUS_ACTIVE;
                    }
                    let mut status = 0;
                    if self.active {
                        status |= GMBUS_ACTIVE;
                    }
                    if self.left > 0 {
                        status |= GMBUS_HW_RDY;
                    } else if self.active {
                        status |= GMBUS_HW_WAIT_PHASE;
                    }
                    status
                }
                GMBUS3 => {
                    let taken = self.left.min(4);
                    let mut word = [0u8; 4];
                    for (byte, at) in word.iter_mut().zip(self.cursor..self.cursor + taken) {
                        *byte = self.edid.get(at).copied().unwrap_or(0);
                    }
                    self.cursor += taken;
                    self.left -= taken;
                    u32::from_le_bytes(word)
                }
                _ => 0,
            }
        }

        fn write32(&mut self, offset: u32, value: u32) {
            self.writes.push((offset, value));
            match offset {
                GMBUS0 => self.pin = value & 0x1F,
                GMBUS1 if value & GMBUS_SW_RDY != 0 && value & 1 == 1 => {
                    if self.pin != 1 || self.edid.is_empty() || self.naks_left > 0 {
                        self.naks_left = self.naks_left.saturating_sub(1);
                        self.nak = true;
                        return;
                    }
                    self.cursor = ((value >> 8) & 0xFF) as usize;
                    self.left = ((value >> 16) & 0x1FF) as usize;
                    self.active = true;
                }
                GMBUS1 if value & (4 << 25) != 0 => self.active = false,
                GMBUS1 if value == 1 << 31 => {
                    self.nak = false;
                    self.active = false;
                }
                _ => {}
            }
        }
    }

    struct Ticks(u64);

    impl Clock for Ticks {
        fn now_us(&mut self) -> u64 {
            self.0
        }
        fn delay_us(&mut self, us: u32) {
            self.0 += u64::from(us);
        }
    }

    fn edid(blocks: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; 128 * (usize::from(blocks) + 1)];
        bytes[..8].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
        bytes[126] = blocks;
        bytes[128..]
            .iter_mut()
            .enumerate()
            .for_each(|(at, byte)| *byte = at as u8);
        bytes
    }

    /// The N150's lit port is pin 1, and a monitor's two blocks come back
    /// whole, the controller left idle and unselected.
    #[test]
    fn an_edid_is_read_block_by_block() {
        assert_eq!(pin_of(Port::Combo(0)), 1);
        assert_eq!(pin_of(Port::Combo(1)), 2);
        assert_eq!(pin_of(Port::TypeC(1)), 9);
        let wanted = edid(1);
        let mut bus = Bus::new(wanted.clone());
        let mut out = [0u8; 256];
        assert_eq!(read_edid(&mut bus, &mut Ticks(0), 1, &mut out), Ok(256));
        assert_eq!(&out[..], &wanted[..]);
        assert_eq!(bus.writes.last(), Some(&(GMBUS0, 0)));
    }

    /// No monitor: a NAK, retried once, then said; the error is cleared.
    #[test]
    fn nothing_on_the_pins_is_a_nak() {
        let mut bus = Bus::new(Vec::new());
        let mut out = [0u8; 256];
        assert_eq!(
            read_edid(&mut bus, &mut Ticks(0), 1, &mut out),
            Err(Failed::Nak)
        );
        let clears = bus
            .writes
            .iter()
            .filter(|(at, value)| *at == GMBUS1 && *value == 1 << 31);
        assert_eq!(clears.count(), 2, "one try and one retry");
        // One NAK only: the retry reads it.
        let mut bus = Bus::new(edid(0));
        bus.naks_left = 1;
        assert_eq!(read_edid(&mut bus, &mut Ticks(0), 1, &mut out), Ok(128));
    }
}
