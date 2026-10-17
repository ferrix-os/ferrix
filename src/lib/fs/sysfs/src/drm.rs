//! A card's connectors, as `drm_sysfs.c` shows them.
//!
//! A connector is a directory beside its card, `card0-Virtual-1`, named by
//! the card, the connector type's name and the type's own count from one.
//! Its `status` and `enabled` are words, and `modes` is one `WxH` a line,
//! the preferred mode first.

use alloc::vec::Vec;

use crate::text::put;

/// What `/sys/class/drm/version` says: the DRM core's version and date,
/// which have not changed since 2006.
pub const VERSION: &[u8] = b"drm 1.1.0 20060810\n";

/// `DRM_MODE_CONNECTOR_VGA`.
pub const CONNECTOR_VGA: u32 = 1;
/// `DRM_MODE_CONNECTOR_DVII`.
pub const CONNECTOR_DVII: u32 = 2;
/// `DRM_MODE_CONNECTOR_DVID`.
pub const CONNECTOR_DVID: u32 = 3;
/// `DRM_MODE_CONNECTOR_LVDS`.
pub const CONNECTOR_LVDS: u32 = 7;
/// `DRM_MODE_CONNECTOR_DisplayPort`.
pub const CONNECTOR_DISPLAYPORT: u32 = 10;
/// `DRM_MODE_CONNECTOR_HDMIA`.
pub const CONNECTOR_HDMIA: u32 = 11;
/// `DRM_MODE_CONNECTOR_eDP`.
pub const CONNECTOR_EDP: u32 = 14;
/// `DRM_MODE_CONNECTOR_VIRTUAL`.
pub const CONNECTOR_VIRTUAL: u32 = 15;
/// `DRM_MODE_CONNECTOR_DSI`.
pub const CONNECTOR_DSI: u32 = 16;
/// `DRM_MODE_CONNECTOR_USB`.
pub const CONNECTOR_USB: u32 = 20;

/// A connector type's name, as `drm_connector_enum_list` spells it, for the
/// types Ferrix's cards have.
#[must_use]
pub const fn connector_type_name(kind: u32) -> &'static str {
    match kind {
        CONNECTOR_VGA => "VGA",
        CONNECTOR_DVII => "DVI-I",
        CONNECTOR_DVID => "DVI-D",
        CONNECTOR_LVDS => "LVDS",
        CONNECTOR_DISPLAYPORT => "DP",
        CONNECTOR_HDMIA => "HDMI-A",
        CONNECTOR_EDP => "eDP",
        CONNECTOR_VIRTUAL => "Virtual",
        CONNECTOR_DSI => "DSI",
        CONNECTOR_USB => "USB",
        _ => "Unknown",
    }
}

/// A connector's directory name: `card0-Virtual-1`.
pub fn connector_name(out: &mut Vec<u8>, card: u32, kind: u32, kind_index: u32) {
    put(
        out,
        format_args!("card{card}-{}-{kind_index}", connector_type_name(kind)),
    );
}

/// `status`.
pub fn status(out: &mut Vec<u8>, connected: bool) {
    out.extend_from_slice(if connected {
        b"connected\n"
    } else {
        b"disconnected\n"
    });
}

/// `enabled`: whether a mode is set on the connector's CRTC.
pub fn enabled(out: &mut Vec<u8>, enabled: bool) {
    out.extend_from_slice(if enabled { b"enabled\n" } else { b"disabled\n" });
}

/// `modes`: each mode's name, `1920x1080`, a line.
pub fn modes(out: &mut Vec<u8>, modes: &[(u32, u32)]) {
    for (width, height) in modes {
        put(out, format_args!("{width}x{height}\n"));
    }
}
