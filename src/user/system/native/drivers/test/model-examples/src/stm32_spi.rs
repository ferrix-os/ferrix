//! The driver model design page's SPI example, as the page writes it: an
//! STM32 SPI controller serving Linux's spidev interface, every resource
//! through its typed `SpiResources`. Kept compiling as ferrix-driver's test
//! of its API shape (`cargo xtask check` lints it on every architecture); it
//! is never put in an image, and its providers and class are step 2's and
//! `CHARDEV.md`'s, so at run time those calls answer `ENOSYS`.
//!
//! What differs from the page: the `entry!` line that starts a native
//! program, `extern crate alloc` for the page's `vec!`, the stand-in for
//! `linux_uapi::spi` below, which no crate of the tree carries yet, and the
//! configuration built in a `const` item (`SPI_SETUP`), where an
//! out-of-range field fails the build.

#![no_std]
#![no_main]
#![expect(
    clippy::indexing_slicing,
    reason = "AUDIT: the design page's example, kept as it is written; never in an image"
)]

extern crate alloc;

mod no_memory;

use alloc::vec;

use ferrix_driver::class::{
    CharacterDevice, CommandTable, Direction, File, Readiness, UserMemory, UserMemoryReader,
    UserMemoryWriter,
};
use ferrix_driver::{
    BindContext, Driver, Errno, Event, Flow, Interrupt, PinControl, PinStates, ReadOnly, ReadWrite,
    Registers, Resources, Result, WriteOnly, bitfields, register_map, run,
};
use ferrix_platform::{ClockClient, ResetClient};
use linux_uapi::spi::{SPI_IOC_MESSAGE_BASE, SPI_IOC_WR_MAX_SPEED_HZ, SpiTransfer};

ferrix_driver::entry!();

/// A stand-in for the Linux UAPI crate the page names: `spi_ioc_transfer`
/// and the two commands the example answers.
mod linux_uapi {
    pub(crate) mod spi {
        use ferrix_driver::class::{Plain, PointerField};

        /// `SPI_IOC_WR_MAX_SPEED_HZ`.
        pub(crate) const SPI_IOC_WR_MAX_SPEED_HZ: u32 = 0x4004_6B04;
        /// `SPI_IOC_MESSAGE(0)`, its size bits cleared.
        pub(crate) const SPI_IOC_MESSAGE_BASE: u32 = 0x4000_6B00;

        /// `struct spi_ioc_transfer`, the fields the example reads.
        #[derive(Clone, Copy, Debug)]
        pub(crate) struct SpiTransfer {
            /// `len`.
            pub(crate) length: u32,
        }

        impl SpiTransfer {
            /// Bytes one record takes.
            pub(crate) const SIZE: usize = 32;
            /// `tx_buf` and `len`.
            pub(crate) const TRANSMIT_BUFFER_FIELD: PointerField = PointerField {
                pointer: 0,
                length: 16,
            };
            /// `rx_buf` and `len`.
            pub(crate) const RECEIVE_BUFFER_FIELD: PointerField = PointerField {
                pointer: 8,
                length: 16,
            };
        }

        impl Plain for SpiTransfer {
            fn from_bytes(bytes: &[u8]) -> Option<Self> {
                let length = u32::from_bytes(bytes.get(16..20)?)?;
                Some(SpiTransfer { length })
            }
        }
    }
}

// The controller's registers, one type each: offset, access mode and named
// bit fields. A real driver generates this from ST's SVD description of the
// STM32MP157; written by hand here. The datasheet's names are in the
// comments (reference manual, SPI chapter).
register_map! {
    SpiRegisters {
        0x00 => control_1: ReadWrite<Control1>,                 // SPI_CR1
        0x08 => configuration_1: ReadWrite<Configuration1>,     // SPI_CFG1
        0x10 => interrupt_enable: ReadWrite<InterruptEnable>,   // SPI_IER
        0x20 => transmit_data: WriteOnly<u8>,                   // SPI_TXDR
        0x30 => receive_data: ReadOnly<u8>,                     // SPI_RXDR
    }
}

bitfields! {
    Control1 {
        enable: bit 0,                           // SPE
        start_master_transfer: bit 9,            // CSTART
    }
    Configuration1 {
        // The hardware stores "bits - 1"; the field takes the real size, 4 to 32 bits.
        frame_size: bits 0..=4 as OffsetByOne<4, 32>,      // DSIZE
        // The hardware stores "bytes - 1"; the field takes the real threshold, 1 to 16.
        fifo_threshold: bits 5..=8 as OffsetByOne<1, 16>,  // FTHLV
    }
    InterruptEnable {
        receive_packet_available: bit 0,         // RXPIE
    }
}

/// The controller's fixed configuration: 8-bit frames, an 8-byte threshold.
/// Built in a `const` item, so a value a field cannot hold fails the build:
/// `.frame_size(40)` here does not compile. A value known only at run time
/// goes through the fallible setter, `try_frame_size`, which answers
/// `OutOfRange` instead.
const SPI_SETUP: Configuration1 = Configuration1::new().frame_size(8).fifo_threshold(8);

/// What this driver needs from its device-tree node. The derive resolves
/// each field and embeds a requirement table that devmgr checks before it
/// starts the driver.
#[derive(Resources)]
struct SpiResources {
    #[registers(index = 0)]
    registers: Registers<SpiRegisters>,
    #[interrupt(index = 0)]
    interrupt: Interrupt,
    #[clock(index = 0)]
    kernel_clock: ClockClient, // this binding has one clock and gives it no name
    #[reset(index = 0)]
    reset: ResetClient,
    #[pin_control]
    pins: PinControl<PinState>,
}

/// The pin states this binding defines. The tree names them "default" and
/// "sleep"; the derive maps each variant to its lower-case name. "sleep" may
/// be absent.
#[derive(Clone, Copy, PinStates)]
enum PinState {
    Default,
    #[optional]
    Sleep,
}

struct Stm32Spi {
    registers: Registers<SpiRegisters>,
    interrupt: Interrupt,
    kernel_clock: ClockClient,
    pins: PinControl<PinState>,
    speed_hertz: u32,
}

impl Driver for Stm32Spi {
    type Resources = SpiResources;

    fn bind(context: BindContext<SpiResources>) -> Result<Self> {
        // Already resolved and type-checked: a misspelt field is a compile
        // error, and a node without a clock never reaches this function.
        let resources = context.resources;
        resources.pins.apply(PinState::Default)?;
        resources.kernel_clock.enable()?;
        resources.reset.pulse()?;
        resources.registers.configuration_1.write(SPI_SETUP);
        let driver = Stm32Spi {
            registers: resources.registers,
            interrupt: resources.interrupt,
            kernel_clock: resources.kernel_clock,
            pins: resources.pins,
            speed_hertz: 1_000_000,
        };
        // devmgr's grant names the node /dev/spidev0.0. The driver picks no name.
        context.classes.character_device_granted(&driver)?;
        Ok(driver)
    }

    fn event(&mut self, event: Event) -> Flow {
        match event {
            Event::CharacterDevice(request) => request.serve(self), // dispatches to CharacterDevice below
            Event::ProviderLost(_) => Flow::Quiesce, // the clock provider is restarting; devmgr rebinds us
            _ => Flow::Continue,
        }
    }

    fn quiesce(&mut self) {
        self.registers.control_1.write(Control1::new()); // everything off
        let _ = self.kernel_clock.disable();
        let _ = self.pins.apply(PinState::Sleep); // does nothing if the tree has no "sleep" state
    }
}

impl CharacterDevice for Stm32Spi {
    /// `SPI_IOC_MESSAGE` encodes its size, so no command needs a table entry.
    fn commands(&self) -> CommandTable {
        CommandTable::empty()
    }

    fn open(&mut self, _file: &mut File) -> Result<Readiness> {
        Ok(Readiness::READABLE | Readiness::WRITABLE)
    }

    fn write(&mut self, _file: &mut File, input: UserMemoryReader, _offset: u64) -> Result<usize> {
        let mut buffer = [0u8; 4096];
        let count = input.read(&mut buffer)?; // exactly the caller's buffer, nothing else (tier 0)
        self.shift(&buffer[..count], None)?;
        Ok(count)
    }

    fn read(
        &mut self,
        _file: &mut File,
        mut output: UserMemoryWriter,
        _offset: u64,
    ) -> Result<usize> {
        let count = output.len().min(4096);
        let zeros = [0u8; 4096];
        let mut received = [0u8; 4096];
        self.shift(&zeros[..count], Some(&mut received[..count]))?;
        output.write(&received[..count])
    }

    fn ioctl(&mut self, _file: &mut File, command: u32, argument: UserMemory) -> Result<i64> {
        if command == SPI_IOC_WR_MAX_SPEED_HZ {
            self.speed_hertz = argument.read_value::<u32>(0)?;
            let _ = self
                .kernel_clock
                .set_rate(u64::from(self.speed_hertz) * 2)?;
            return Ok(0);
        }
        if command & !0x3FFF_0000 == SPI_IOC_MESSAGE_BASE {
            // The argument is the caller's array of spi_ioc_transfer records,
            // sized by the command's own encoding.
            for index in 0..argument.len() / SpiTransfer::SIZE {
                let at = index * SpiTransfer::SIZE;
                let transfer: SpiTransfer = argument.read_value(at)?;
                // Tier 1: the kernel itself reads each buffer's address and
                // length from the caller's record, so the driver reaches only
                // what the caller pointed to.
                let transmit_buffer = argument.follow_pointer(
                    SpiTransfer::TRANSMIT_BUFFER_FIELD,
                    at,
                    Direction::FromCaller,
                )?;
                let receive_buffer = argument.follow_pointer(
                    SpiTransfer::RECEIVE_BUFFER_FIELD,
                    at,
                    Direction::ToCaller,
                )?;
                let mut transmitted = vec![0u8; transfer.length as usize];
                let mut received = vec![0u8; transfer.length as usize];
                transmit_buffer.read_at(0, &mut transmitted)?;
                self.shift(&transmitted, Some(&mut received))?;
                receive_buffer.write_at(0, &received)?;
            }
            return Ok(0);
        }
        Err(Errno::ENOTTY)
    }
}

impl Stm32Spi {
    /// Shift bytes out, and optionally in, waiting on the controller's
    /// interrupt for each byte.
    fn shift(&mut self, transmit: &[u8], mut receive: Option<&mut [u8]>) -> Result<()> {
        self.registers.control_1.modify(|value| value.enable(true));
        for (index, byte) in transmit.iter().enumerate() {
            self.registers
                .interrupt_enable
                .write(InterruptEnable::new().receive_packet_available(true));
            self.registers.transmit_data.write(*byte);
            let occurrence = self.interrupt.wait()?; // can be killed; ends if the peer dies
            let arrived = self.registers.receive_data.read(); // receive_data.write(..) would not compile: ReadOnly
            if let Some(receive) = receive.as_deref_mut() {
                receive[index] = arrived;
            }
            self.interrupt.acknowledge(occurrence);
        }
        self.registers.control_1.modify(|value| value.enable(false));
        Ok(())
    }
}

fn main() {
    run::<Stm32Spi>() // bootstrap, the event loop, and the restart hooks
}
