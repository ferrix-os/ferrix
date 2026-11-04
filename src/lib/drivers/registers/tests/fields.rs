//! `bitfields!`'s setters, on the host: the fallible one refuses what the
//! field cannot hold, and both store what it can, encoded.

extern crate ferrix_registers as ferrix_driver;

use ferrix_registers::OutOfRange;

ferrix_driver_derive::bitfields! {
    Configuration1 {
        // The hardware stores "bits - 1"; the field takes the real size, 4 to 32 bits.
        frame_size: bits 0..=4 as OffsetByOne<4, 32>,
        // A plain four-bit field.
        threshold: bits 5..=8,
        enable: bit 9,
    }
}

#[test]
fn the_fallible_setter_refuses_what_the_field_cannot_hold() {
    for refused in [40, 3, 33, 0] {
        assert_eq!(
            Configuration1::new().try_frame_size(refused),
            Err(OutOfRange { value: refused }),
            "{refused}"
        );
    }
    for accepted in [4, 32, 8] {
        let set = Configuration1::new()
            .try_frame_size(accepted)
            .expect("in range");
        assert_eq!(set.frame_size_value(), accepted);
        assert_eq!(
            set.bits() & 0x1F,
            accepted - 1,
            "stored as the value less one"
        );
    }
    assert_eq!(
        Configuration1::new().try_threshold(16),
        Err(OutOfRange { value: 16 })
    );
    assert_eq!(
        Configuration1::new()
            .try_threshold(15)
            .map(Configuration1::bits),
        Ok(15 << 5)
    );
}

/// A fixed configuration, as a driver builds one: in a `const` item.
const SETUP: Configuration1 = Configuration1::new()
    .frame_size(8)
    .threshold(3)
    .enable(true);

#[test]
fn a_constant_configuration_is_stored_encoded() {
    assert_eq!(SETUP.frame_size_value(), 8);
    assert_eq!(SETUP.threshold_value(), 3);
    assert!(SETUP.enable_value());
    assert_eq!(SETUP.bits(), 7 | (3 << 5) | (1 << 9));
    assert!(!SETUP.enable(false).enable_value());
}
