//! What a network interface's files say that is not a plain number.

/// `ARPHRD_ETHER`, an Ethernet interface's `type`.
pub const ARPHRD_ETHER: u16 = 1;
/// `ARPHRD_LOOPBACK`, the loopback interface's `type`.
pub const ARPHRD_LOOPBACK: u16 = 772;

/// `IFF_UP`.
pub const IFF_UP: u32 = 0x1;
/// `IFF_LOOPBACK`.
pub const IFF_LOOPBACK: u32 = 0x8;
/// `IFF_LOWER_UP`: the carrier is there.
pub const IFF_LOWER_UP: u32 = 0x1_0000;

/// An interface's `operstate`, from its flags.
///
/// The loopback interface never sets an operational state, so Linux reports
/// it `unknown`. Any other is `up` when it is up and has its carrier, and
/// `down` otherwise: Ferrix has no dormant or testing states to report.
#[must_use]
pub const fn operstate(flags: u32) -> &'static [u8] {
    if flags & IFF_LOOPBACK != 0 {
        b"unknown\n"
    } else if flags & IFF_UP != 0 && flags & IFF_LOWER_UP != 0 {
        b"up\n"
    } else {
        b"down\n"
    }
}

/// An interface's `carrier`: whether it has one, or `None` for an interface
/// that is down, whose `carrier` Linux refuses to read with `EINVAL`.
#[must_use]
pub const fn carrier(flags: u32) -> Option<bool> {
    if flags & IFF_UP == 0 {
        None
    } else {
        Some(flags & IFF_LOWER_UP != 0)
    }
}
