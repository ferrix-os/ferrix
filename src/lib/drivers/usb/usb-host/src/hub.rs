//! The hub class of USB 2.0's chapter 11: its descriptor, and its ports'
//! status and features.

use crate::usb::{
    CLEAR_FEATURE, DIRECTION_IN, GET_DESCRIPTOR, GET_STATUS, HUB, RECIPIENT_OTHER, SET_FEATURE,
    Setup, TYPE_CLASS,
};

/// `PORT_RESET`.
pub const PORT_RESET: u16 = 4;
/// `PORT_POWER`.
pub const PORT_POWER: u16 = 8;
/// `C_PORT_CONNECTION`.
pub const C_PORT_CONNECTION: u16 = 16;
/// `C_PORT_ENABLE`.
pub const C_PORT_ENABLE: u16 = 17;
/// `C_PORT_SUSPEND`.
pub const C_PORT_SUSPEND: u16 = 18;
/// `C_PORT_OVER_CURRENT`.
pub const C_PORT_OVER_CURRENT: u16 = 19;
/// `C_PORT_RESET`.
pub const C_PORT_RESET: u16 = 20;

/// `wPortStatus`: a device is there.
pub const STATUS_CONNECTION: u16 = 1 << 0;
/// `wPortStatus`: the port is enabled.
pub const STATUS_ENABLE: u16 = 1 << 1;
/// `wPortStatus`: the port is being reset.
pub const STATUS_RESET: u16 = 1 << 4;
/// `wPortStatus`: the port is powered.
pub const STATUS_POWER: u16 = 1 << 8;
/// `wPortStatus`: the device is low speed.
pub const STATUS_LOW_SPEED: u16 = 1 << 9;
/// `wPortStatus`: the device is high speed.
pub const STATUS_HIGH_SPEED: u16 = 1 << 10;

/// `wPortChange`: the connection changed.
pub const CHANGE_CONNECTION: u16 = 1 << 0;
/// `wPortChange`: the port was disabled.
pub const CHANGE_ENABLE: u16 = 1 << 1;
/// `wPortChange`: a resume finished.
pub const CHANGE_SUSPEND: u16 = 1 << 2;
/// `wPortChange`: the over-current indicator changed.
pub const CHANGE_OVER_CURRENT: u16 = 1 << 3;
/// `wPortChange`: a reset finished.
pub const CHANGE_RESET: u16 = 1 << 4;

/// A port's status and what changed, as `GET_STATUS` answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PortStatus {
    /// `wPortStatus`.
    pub status: u16,
    /// `wPortChange`.
    pub change: u16,
}

impl PortStatus {
    /// Read `GET_STATUS`'s four bytes.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<PortStatus> {
        Some(PortStatus {
            status: u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]),
            change: u16::from_le_bytes([*bytes.get(2)?, *bytes.get(3)?]),
        })
    }

    /// Whether a device is there.
    #[must_use]
    pub const fn connected(&self) -> bool {
        self.status & STATUS_CONNECTION != 0
    }

    /// The speed of the device a reset enabled.
    #[must_use]
    pub const fn speed(&self) -> crate::usb::Speed {
        if self.status & STATUS_LOW_SPEED != 0 {
            crate::usb::Speed::Low
        } else if self.status & STATUS_HIGH_SPEED != 0 {
            crate::usb::Speed::High
        } else {
            crate::usb::Speed::Full
        }
    }

    /// The `C_PORT_*` features each set change bit is cleared with.
    pub fn changes(&self) -> impl Iterator<Item = u16> + '_ {
        [
            (CHANGE_CONNECTION, C_PORT_CONNECTION),
            (CHANGE_ENABLE, C_PORT_ENABLE),
            (CHANGE_SUSPEND, C_PORT_SUSPEND),
            (CHANGE_OVER_CURRENT, C_PORT_OVER_CURRENT),
            (CHANGE_RESET, C_PORT_RESET),
        ]
        .into_iter()
        .filter(|(bit, _)| self.change & bit != 0)
        .map(|(_, feature)| feature)
    }
}

/// What a hub descriptor says that the driver uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HubDescriptor {
    /// `bNbrPorts`.
    pub ports: u8,
    /// `bPwrOn2PwrGood`, in milliseconds rather than its units of two.
    pub power_good_ms: u32,
    /// `wHubCharacteristics`' TT think time, bits 6 and 5: how long its
    /// transaction translator needs between transactions.
    pub think_time: u8,
}

impl HubDescriptor {
    /// Read a hub descriptor's first bytes.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<HubDescriptor> {
        if bytes.get(1) != Some(&HUB) {
            return None;
        }
        Some(HubDescriptor {
            ports: *bytes.get(2)?,
            power_good_ms: u32::from(*bytes.get(5)?) * 2,
            think_time: (*bytes.get(3)? >> 5) & 0x3,
        })
    }
}

/// `GET_DESCRIPTOR` of the hub descriptor.
#[must_use]
pub const fn get_hub_descriptor(length: u16) -> Setup {
    Setup {
        request_type: DIRECTION_IN | TYPE_CLASS,
        request: GET_DESCRIPTOR,
        value: (HUB as u16) << 8,
        index: 0,
        length,
    }
}

/// `GET_STATUS` of a port.
#[must_use]
pub const fn get_port_status(port: u8) -> Setup {
    Setup {
        request_type: DIRECTION_IN | TYPE_CLASS | RECIPIENT_OTHER,
        request: GET_STATUS,
        value: 0,
        index: port as u16,
        length: 4,
    }
}

/// `SET_FEATURE` of a port.
#[must_use]
pub const fn set_port_feature(port: u8, feature: u16) -> Setup {
    Setup {
        request_type: TYPE_CLASS | RECIPIENT_OTHER,
        request: SET_FEATURE,
        value: feature,
        index: port as u16,
        length: 0,
    }
}

/// `CLEAR_FEATURE` of a port.
#[must_use]
pub const fn clear_port_feature(port: u8, feature: u16) -> Setup {
    Setup {
        request_type: TYPE_CLASS | RECIPIENT_OTHER,
        request: CLEAR_FEATURE,
        value: feature,
        index: port as u16,
        length: 0,
    }
}
