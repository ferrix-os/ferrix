//! The choices a modeset makes and the pixels it draws, apart from the card,
//! so the host can test them.

use ferrix_linux_abi::drm::{self, ModeInfo};

/// The colour the screen is filled with, as `XRGB8888`: a dark slate, so a
/// screen that is merely black is not mistaken for success.
pub const BACKGROUND: u32 = 0x001E_1E2E;

/// The colour the negative control draws its first pixel in.
pub const NEGATIVE: u32 = 0x00FF_00FF;

/// The mode to set from a connector's list: the preferred one, else the
/// first.
pub fn choose_mode(modes: &[ModeInfo]) -> Option<ModeInfo> {
    modes
        .iter()
        .find(|mode| mode.r#type & drm::MODE_TYPE_PREFERRED != 0)
        .or_else(|| modes.first())
        .copied()
}

/// The CRTC an encoder can drive: the one it is on, else the first its
/// `possible_crtcs` bit mask allows among `crtcs`.
pub fn choose_crtc(current: u32, possible: u32, crtcs: &[u32]) -> Option<u32> {
    if current != 0 {
        return Some(current);
    }
    crtcs
        .iter()
        .enumerate()
        .find(|&(index, _)| index < 32 && possible & (1 << index) != 0)
        .map(|(_, &crtc)| crtc)
}

/// Fill a `width` × `height` `XRGB8888` buffer of `stride` bytes a row with
/// `color`, and with `negative`, draw pixel (0, 0) in [`NEGATIVE`].
pub fn fill(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    color: u32,
    negative: bool,
) {
    let bytes = color.to_le_bytes();
    for row in pixels.chunks_exact_mut(stride).take(height) {
        for pixel in row.chunks_exact_mut(4).take(width) {
            pixel.copy_from_slice(&bytes);
        }
    }
    if negative && let Some(first) = pixels.get_mut(..4) {
        first.copy_from_slice(&NEGATIVE.to_le_bytes());
    }
}

/// A mode's refresh in millihertz: its pixel clock over its total pixels,
/// which is exact where `vrefresh` is rounded (59.94 Hz is 60 there), or
/// `vrefresh` for a mode without timings, and 60 Hz for one with neither.
#[must_use]
pub fn refresh_mhz(mode: &ModeInfo) -> i32 {
    let total = u64::from(mode.htotal) * u64::from(mode.vtotal);
    let mhz = if total > 0 && mode.clock > 0 {
        u64::from(mode.clock) * 1_000_000 / total
    } else {
        u64::from(mode.vrefresh) * 1000
    };
    match i32::try_from(mhz) {
        Ok(0) | Err(_) => 60_000,
        Ok(mhz) => mhz,
    }
}

/// A mode's name, up to its NUL.
pub fn mode_name(mode: &ModeInfo) -> String {
    c_name(&mode.name)
}

/// A NUL-padded name field as text, up to its NUL.
pub fn c_name(field: &[u8]) -> String {
    let end = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(field.get(..end).unwrap_or(&[])).into_owned()
}

/// A plane as the card described it after the modeset: its id, the CRTC and
/// framebuffer it shows, the CRTCs it can be on, and the name its `type`
/// property's value has in that property's enum list, if it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plane {
    /// The plane's object id.
    pub id: u32,
    /// The CRTC it is showing on, or zero.
    pub crtc: u32,
    /// The framebuffer it is showing, or zero.
    pub framebuffer: u32,
    /// A bit per CRTC of the card's list that this plane can be on.
    pub possible_crtcs: u32,
    /// `Primary`, `Overlay` or `Cursor`, when the `type` property named one.
    pub kind: Option<String>,
}

/// The name `value` has among an enum property's `(value, name)` pairs.
pub fn enum_name(value: u64, names: &[(u64, String)]) -> Option<String> {
    names
        .iter()
        .find(|(named, _)| *named == value)
        .map(|(_, name)| name.clone())
}

/// What the marker line says of the planes that can be on the CRTC at index
/// `crtc_index`: the primary plane, as Smithay's legacy path picks one, with
/// the `type` name the card gave it; else the first plane with whatever type
/// it has, so a card that got the type wrong shows it; else that there is
/// none.
///
/// A primary plane that does not show `framebuffer` on `crtc` after the
/// modeset is an error: the card would be telling a compositor something
/// other than what is on the screen.
pub fn describe_planes(
    planes: &[Plane],
    crtc_index: usize,
    crtc: u32,
    framebuffer: u32,
) -> Result<String, String> {
    let usable: Vec<&Plane> = planes
        .iter()
        .filter(|plane| crtc_index < 32 && plane.possible_crtcs & (1 << crtc_index) != 0)
        .collect();
    let primary = usable
        .iter()
        .find(|plane| plane.kind.as_deref() == Some("Primary"));
    match (primary, usable.first()) {
        (Some(plane), _) if plane.crtc != crtc || plane.framebuffer != framebuffer => Err(format!(
            "primary plane {} shows framebuffer {} on CRTC {}, not {framebuffer} on {crtc}",
            plane.id, plane.framebuffer, plane.crtc
        )),
        (Some(plane), _) => Ok(format!("plane {} Primary", plane.id)),
        (None, Some(plane)) => Ok(format!(
            "plane {} {}",
            plane.id,
            plane.kind.as_deref().unwrap_or("untyped")
        )),
        (None, None) => Ok(String::from("plane none")),
    }
}

/// What a connector of `kind` is called, by the names libdrm's
/// `drmModeGetConnectorTypeName` gives -- which is what wlroots and so
/// Hyprland name a monitor after, so `DP-1` here is `DP-1` there.
///
/// A type this list does not have is `Unknown`, as libdrm's is.
#[must_use]
pub fn connector_type_name(kind: u32) -> &'static str {
    match kind {
        1 => "VGA",
        2 => "DVI-I",
        3 => "DVI-D",
        4 => "DVI-A",
        5 => "Composite",
        6 => "SVIDEO",
        7 => "LVDS",
        8 => "Component",
        9 => "DIN",
        10 => "DP",
        11 => "HDMI-A",
        12 => "HDMI-B",
        13 => "TV",
        14 => "eDP",
        15 => "Virtual",
        16 => "DSI",
        17 => "DPI",
        18 => "Writeback",
        19 => "SPI",
        20 => "USB",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use ferrix_linux_abi::drm::{Field, ModeInfo};

    use super::*;

    fn mode(width: u16, height: u16, preferred: bool) -> ModeInfo {
        let mut mode = ModeInfo::ZERO;
        mode.hdisplay = width;
        mode.vdisplay = height;
        if preferred {
            mode.r#type = drm::MODE_TYPE_PREFERRED | drm::MODE_TYPE_DRIVER;
        }
        mode.name[..8].copy_from_slice(b"1280x800");
        mode
    }

    #[test]
    fn the_preferred_mode_wins_and_otherwise_the_first() {
        let modes = [
            mode(1024, 768, false),
            mode(1280, 800, true),
            mode(800, 600, false),
        ];
        assert_eq!(choose_mode(&modes).map(|m| m.hdisplay), Some(1280));
        assert_eq!(choose_mode(&modes[..1]).map(|m| m.hdisplay), Some(1024));
        assert_eq!(choose_mode(&[]), None);
        assert_eq!(mode_name(&modes[1]), "1280x800");
    }

    #[test]
    fn the_crtc_is_the_current_one_or_the_first_possible() {
        assert_eq!(choose_crtc(41, 0b10, &[40, 41]), Some(41));
        assert_eq!(choose_crtc(0, 0b10, &[40, 41]), Some(41));
        assert_eq!(choose_crtc(0, 0b01, &[40, 41]), Some(40));
        assert_eq!(choose_crtc(0, 0b100, &[40, 41]), None);
    }

    fn plane(id: u32, kind: Option<&str>, possible_crtcs: u32) -> Plane {
        Plane {
            id,
            crtc: 1,
            framebuffer: 32,
            possible_crtcs,
            kind: kind.map(String::from),
        }
    }

    #[test]
    fn the_primary_plane_on_the_crtc_is_named() {
        let names = vec![
            (0, String::from("Overlay")),
            (1, String::from("Primary")),
            (2, String::from("Cursor")),
        ];
        assert_eq!(enum_name(1, &names).as_deref(), Some("Primary"));
        assert_eq!(enum_name(3, &names), None);

        let planes = [
            plane(7, Some("Overlay"), 0b1),
            plane(8, Some("Primary"), 0b10),
            plane(9, Some("Primary"), 0b1),
        ];
        assert_eq!(
            describe_planes(&planes, 0, 1, 32).as_deref(),
            Ok("plane 9 Primary")
        );
        // The primary plane for another CRTC does not count.
        assert_eq!(
            describe_planes(&planes[..2], 0, 1, 32).as_deref(),
            Ok("plane 7 Overlay")
        );
        assert_eq!(
            describe_planes(&[plane(4, None, 1)], 0, 1, 32).as_deref(),
            Ok("plane 4 untyped")
        );
        assert_eq!(describe_planes(&[], 0, 1, 32).as_deref(), Ok("plane none"));
    }

    #[test]
    fn a_primary_plane_not_showing_the_modeset_is_an_error() {
        let shown = plane(4, Some("Primary"), 1);
        assert!(describe_planes(core::slice::from_ref(&shown), 0, 1, 33).is_err());
        assert!(describe_planes(core::slice::from_ref(&shown), 0, 2, 32).is_err());
        assert!(describe_planes(&[shown], 0, 1, 32).is_ok());
    }

    /// The names are libdrm's, which is where every compositor's monitor
    /// names come from. Virtual is virtio-gpu's, and so Ferrix's.
    #[test]
    fn a_connector_is_named_as_libdrm_names_it() {
        assert_eq!(connector_type_name(15), "Virtual");
        assert_eq!(connector_type_name(10), "DP");
        assert_eq!(connector_type_name(11), "HDMI-A");
        assert_eq!(connector_type_name(14), "eDP");
        assert_eq!(connector_type_name(0), "Unknown");
        assert_eq!(connector_type_name(99), "Unknown");
    }

    #[test]
    fn fill_covers_every_visible_pixel_and_no_padding() {
        // 3 × 2 pixels in rows of 16 bytes: one pixel of padding a row.
        let mut pixels = vec![0xAAu8; 32];
        fill(&mut pixels, 3, 2, 16, BACKGROUND, false);
        for row in pixels.chunks(16) {
            for pixel in row[..12].chunks(4) {
                assert_eq!(pixel, BACKGROUND.to_le_bytes());
            }
            assert_eq!(row[12..], [0xAA; 4], "padding untouched");
        }

        fill(&mut pixels, 3, 2, 16, BACKGROUND, true);
        assert_eq!(pixels[..4], NEGATIVE.to_le_bytes());
        assert_eq!(pixels[4..8], BACKGROUND.to_le_bytes());
    }
}
