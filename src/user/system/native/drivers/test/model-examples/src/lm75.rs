//! The driver model design page's child-device example, as the page writes
//! it: an LM75 temperature sensor wired to an I2C controller a ring-3 driver
//! runs. Kept compiling as ferrix-driver's test of its API shape; never put
//! in an image. Child devices are step 4's, so its parent bus never resolves
//! and it is never started.
//!
//! What differs from the page: the `entry!` line, `extern crate alloc` for
//! the page's `format!`, and its imports, which the page leaves out.

#![no_std]
#![no_main]

extern crate alloc;

mod no_memory;

use alloc::format;

use ferrix_driver::bus::I2cDevice;
use ferrix_driver::class::{CharacterDevice, File, UserMemoryWriter};
use ferrix_driver::{BindContext, Driver, Event, Flow, Resources, Result, run};
use ferrix_platform::RegulatorClient;

ferrix_driver::entry!();

#[derive(Resources)]
struct Lm75Resources {
    #[parent_bus]
    bus: I2cDevice, // the channel to the I2C controller's driver; the address is fixed already
    // The LM75 datasheet calls its power pin "Vs"; the device-tree binding
    // names its supply property "vs-supply", hence tree_name = "vs".
    #[power_supply(tree_name = "vs", optional)]
    power_supply: Option<RegulatorClient>,
}

struct Lm75 {
    bus: I2cDevice,
}

/// The LM75's temperature register (datasheet: "Temp", pointer value 0x00).
const TEMPERATURE_REGISTER: u8 = 0x00;

impl Driver for Lm75 {
    type Resources = Lm75Resources;

    fn bind(context: BindContext<Lm75Resources>) -> Result<Self> {
        let resources = context.resources;
        if let Some(power_supply) = &resources.power_supply {
            power_supply.enable()?;
        }
        let driver = Lm75 { bus: resources.bus };
        context.classes.character_device_granted(&driver)?; // for example /dev/lm75-0
        Ok(driver)
    }

    fn event(&mut self, event: Event) -> Flow {
        match event {
            Event::CharacterDevice(request) => request.serve(self),
            _ => Flow::Continue,
        }
    }

    fn quiesce(&mut self) {}
}

impl CharacterDevice for Lm75 {
    fn read(
        &mut self,
        _file: &mut File,
        mut output: UserMemoryWriter,
        _offset: u64,
    ) -> Result<usize> {
        let [high_byte, low_byte] = self.bus.write_then_read::<2>(&[TEMPERATURE_REGISTER])?;
        // 9 significant bits, half a degree Celsius per step.
        let half_degrees = i16::from_be_bytes([high_byte, low_byte]) >> 7;
        let millidegrees_celsius = i32::from(half_degrees) * 500;
        output.write(format!("{millidegrees_celsius}\n").as_bytes())
    }
    // open, write and ioctl keep the trait's defaults: EINVAL and ENOTTY.
}

fn main() {
    run::<Lm75>()
}
