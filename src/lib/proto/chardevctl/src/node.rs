//! Which nodes a driver may serve, and what each is called.
//!
//! NVIDIA's static major, 195, alone (`docs/NVIDIA.md` §4.4; the
//! consultant's N3, ledger 294): `nvidia<N>` at minor N for 0 to 253,
//! `nvidia-modeset` at 254 and `nvidiactl` at 255, as NVIDIA's Linux driver
//! names them. No other name can be had, so a driver cannot take a name some
//! other program expects, and every node is mode 0666, root's, with no
//! execute or set-id bit, as NVIDIA's `NVreg_DeviceFileMode` default leaves
//! them.

/// NVIDIA's static character major.
pub const MAJOR: u32 = 195;

/// Every node's permission bits.
pub const MODE: u32 = 0o666;

/// The control device's minor.
pub const CONTROL_MINOR: u16 = 255;

/// NVKMS's minor.
pub const MODESET_MINOR: u16 = 254;

/// The longest name: `nvidia-modeset`.
pub const NAME_MAX: usize = 14;

/// A node's name, in `/dev`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Name {
    bytes: [u8; NAME_MAX],
    len: usize,
}

impl Name {
    /// The name's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// The name minor `minor` of [`MAJOR`] has, if it may be served at all.
#[must_use]
pub fn name(minor: u16) -> Option<Name> {
    let mut bytes = [0_u8; NAME_MAX];
    let mut len = 0;
    let mut put = |text: &[u8]| {
        for byte in text {
            if let Some(slot) = bytes.get_mut(len) {
                *slot = *byte;
                len += 1;
            }
        }
    };
    match minor {
        CONTROL_MINOR => put(b"nvidiactl"),
        MODESET_MINOR => put(b"nvidia-modeset"),
        0..=253 => {
            put(b"nvidia");
            let digits = [minor / 100, minor / 10 % 10, minor % 10];
            let first = digits.iter().position(|digit| *digit != 0).unwrap_or(2);
            for digit in digits.iter().skip(first) {
                put(&[b'0' + u8::try_from(*digit).unwrap_or(0)]);
            }
        }
        _ => return None,
    }
    Some(Name { bytes, len })
}

/// The minor a name in `/dev` is, if it is one [`name`] gives.
#[must_use]
pub fn minor_of(text: &[u8]) -> Option<u16> {
    match text {
        b"nvidiactl" => Some(CONTROL_MINOR),
        b"nvidia-modeset" => Some(MODESET_MINOR),
        _ => {
            let digits = text.strip_prefix(b"nvidia")?;
            if digits.is_empty()
                || digits.len() > 3
                || (digits.len() > 1 && digits.first() == Some(&b'0'))
            {
                return None;
            }
            let mut minor: u16 = 0;
            for digit in digits {
                if !digit.is_ascii_digit() {
                    return None;
                }
                minor = minor * 10 + u16::from(digit - b'0');
            }
            (minor <= 253).then_some(minor)
        }
    }
}
