//! `testseconds`: the driver `cargo xtask test-driver-model` serves a
//! device with that the kernel has never named (`docs/DRIVER-MODEL.md` §8,
//! D1, D20), written against `ferrix-driver`'s driver model.
//!
//! The device is QEMU `virt`'s PL031 real-time clock, its node given the
//! made-up compatible `ferrix,test-seconds` in an edited tree. The kernel
//! publishes it as a generic node; devmgr matches it by that compatible,
//! holds the node to the requirement table `#[derive(Resources)]` embeds in
//! this image and starts it. [`run`] resolves its resources, and
//! [`SecondsCounter::bind`] shows the device working through nothing but
//! its own aperture and vector:
//!
//! 1. the identification registers read as a PL031's;
//! 2. the counter moves;
//! 3. a match two seconds ahead raises the node's interrupt, which arrives
//!    on the port it is bound to and is cleared.
//!
//! It says each on its line, and `served` last, then serves until devmgr
//! ends it.

#![no_std]
#![no_main]

use ferrix_driver::{
    BindContext, Driver, Errno, Event, Flow, Interrupt, ReadOnly, ReadWrite, Registers, Resources,
    Result, WriteOnly, bitfields, register_map, run,
};
use ferrix_platform::ClockClient;

ferrix_driver::entry!();

// The counter's registers. The PL031 technical reference manual's names
// are in the comments.
register_map! {
    SecondsCounterRegisters {
        0x00 => count: ReadOnly<u32>,                            // RTCDR
        0x04 => match_value: ReadWrite<u32>,                     // RTCMR
        0x10 => interrupt_mask: ReadWrite<MatchInterrupt>,       // RTCIMSC
        0x18 => interrupt_status: ReadOnly<MatchInterrupt>,      // RTCMIS
        0x1C => interrupt_clear: WriteOnly<MatchInterrupt>,      // RTCICR
        0xFE0 => peripheral_identification_0: ReadOnly<u32>,     // RTCPeriphID0
        0xFE4 => peripheral_identification_1: ReadOnly<u32>,     // RTCPeriphID1
        0xFE8 => peripheral_identification_2: ReadOnly<u32>,     // RTCPeriphID2
        0xFEC => peripheral_identification_3: ReadOnly<u32>,     // RTCPeriphID3
        0xFF0 => cell_identification_0: ReadOnly<u32>,           // RTCPCellID0
        0xFF4 => cell_identification_1: ReadOnly<u32>,           // RTCPCellID1
        0xFF8 => cell_identification_2: ReadOnly<u32>,           // RTCPCellID2
        0xFFC => cell_identification_3: ReadOnly<u32>,           // RTCPCellID3
    }
}

bitfields! {
    MatchInterrupt {
        match_reached: bit 0,       // RTCINTR
    }
}

/// What the driver needs of its node, which devmgr checks before starting
/// it: the counter's registers, its interrupt, and the bus clock the
/// binding names `apb_pclk`.
#[derive(Resources)]
struct SecondsCounterResources {
    #[registers(index = 0)]
    registers: Registers<SecondsCounterRegisters>,
    #[interrupt(index = 0)]
    interrupt: Interrupt,
    // QEMU's bus clock always runs: held, never enabled, for the requirement.
    #[clock(tree_name = "apb_pclk")]
    bus_clock: ClockClient,
}

/// The driver.
struct SecondsCounter {
    registers: Registers<SecondsCounterRegisters>,
    _interrupt: Interrupt,
    _bus_clock: ClockClient,
}

/// What the identification registers read as on a PL031, the revision
/// masked out of the third: part 0x031, designer ARM, then the
/// `PrimeCell`'s fixed cell identity.
const PL031: [u32; 8] = [0x31, 0x10, 0x04, 0x00, 0x0D, 0xF0, 0x05, 0xB1];

/// A second.
const SECOND: u64 = 1_000_000_000;
/// How long the match interrupt may take: the match is two seconds ahead.
const PATIENCE: u64 = 6 * SECOND;

fn now() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
}

impl SecondsCounter {
    /// The identification registers, the revision masked out.
    fn identification(registers: &SecondsCounterRegisters) -> [u32; 8] {
        [
            registers.peripheral_identification_0.read() & 0xFF,
            registers.peripheral_identification_1.read() & 0xFF,
            registers.peripheral_identification_2.read() & 0x0F,
            registers.peripheral_identification_3.read() & 0xFF,
            registers.cell_identification_0.read() & 0xFF,
            registers.cell_identification_1.read() & 0xFF,
            registers.cell_identification_2.read() & 0xFF,
            registers.cell_identification_3.read() & 0xFF,
        ]
    }
}

impl Driver for SecondsCounter {
    type Resources = SecondsCounterResources;

    fn bind(context: BindContext<SecondsCounterResources>) -> Result<Self> {
        let path = context.info.path();
        let resources = context.resources;
        let registers = resources.registers;
        let identification = SecondsCounter::identification(&registers);
        if identification != PL031 {
            ferrix_rt::err!("testseconds: {path} does not read as a PL031: {identification:x?}");
            return Err(Errno::ENODEV);
        }
        ferrix_rt::info!("testseconds: {path} reads as a PL031 through its own aperture");

        let first = registers.count.read();
        let deadline = now().saturating_add(3 * SECOND);
        let moved = loop {
            let count = registers.count.read();
            if count != first {
                break count;
            }
            if now() > deadline {
                return Err(Errno::ETIMEDOUT);
            }
            core::hint::spin_loop();
        };
        ferrix_rt::info!("testseconds: the counter moved from {first} to {moved}");

        let interrupt = resources.interrupt;
        registers
            .interrupt_clear
            .write(MatchInterrupt::new().match_reached(true));
        registers.match_value.write(moved.wrapping_add(2));
        registers
            .interrupt_mask
            .write(MatchInterrupt::new().match_reached(true));
        let asked = now();
        let occurrence = interrupt.wait_until(asked.saturating_add(PATIENCE))?;
        let raised = registers.interrupt_status.read().match_reached_value();
        registers.interrupt_mask.write(MatchInterrupt::new());
        registers
            .interrupt_clear
            .write(MatchInterrupt::new().match_reached(true));
        interrupt.acknowledge(occurrence);
        if !raised {
            return Err(Errno::EIO);
        }
        ferrix_rt::info!(
            "testseconds: the match interrupt came after {} ms and was cleared",
            now().saturating_sub(asked) / 1_000_000
        );
        ferrix_rt::info!("testseconds: {path} served by a driver the kernel never named");
        Ok(SecondsCounter {
            registers,
            _interrupt: interrupt,
            _bus_clock: resources.bus_clock,
        })
    }

    fn event(&mut self, event: Event) -> Flow {
        match event {
            Event::Stopping => Flow::Stop,
            _ => Flow::Continue,
        }
    }

    fn quiesce(&mut self) {
        // The match interrupt off: nothing more is raised.
        self.registers.interrupt_mask.write(MatchInterrupt::new());
    }
}

fn main() {
    run::<SecondsCounter>()
}
