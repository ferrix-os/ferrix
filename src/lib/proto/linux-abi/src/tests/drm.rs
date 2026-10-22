//! `drm`: every number and layout against the probe's output, at both widths,
//! and each structure read and written back byte for byte.

use super::std::borrow::ToOwned;
use super::std::collections::BTreeMap;
use super::std::format;
use super::std::string::String;
use super::std::vec;
use super::std::vec::Vec;

use crate::drm::{self, Field, Layout, Version};
use crate::socket::Width;

const PROBE_64: &str = include_str!("../../probe/drm-64.txt");
const PROBE_32: &str = include_str!("../../probe/drm-32.txt");

/// The probe's lines as `name → value`.
fn probe(text: &str) -> BTreeMap<String, u64> {
    let mut lines = BTreeMap::new();
    for line in text.lines() {
        let (name, value) = line.split_once(' ').expect("each line is `name value`");
        let value = value.parse().expect("each value is decimal");
        assert!(
            lines.insert(name.to_owned(), value).is_none(),
            "{name} printed twice"
        );
    }
    lines
}

/// Every constant this module defines, by the name the headers give it.
fn constants(width: Width) -> Vec<(&'static str, u64)> {
    let mut all = ioctls(width);
    all.extend(values());
    all.extend(object_values());
    all
}

/// The ioctl numbers.
fn ioctls(width: Width) -> Vec<(&'static str, u64)> {
    vec![
        ("DRM_IOCTL_VERSION", u64::from(drm::ioctl_version(width))),
        ("DRM_IOCTL_GET_CAP", drm::IOCTL_GET_CAP.into()),
        ("DRM_IOCTL_SET_CLIENT_CAP", drm::IOCTL_SET_CLIENT_CAP.into()),
        ("DRM_IOCTL_SET_MASTER", drm::IOCTL_SET_MASTER.into()),
        ("DRM_IOCTL_DROP_MASTER", drm::IOCTL_DROP_MASTER.into()),
        (
            "DRM_IOCTL_MODE_GETRESOURCES",
            drm::IOCTL_MODE_GETRESOURCES.into(),
        ),
        ("DRM_IOCTL_MODE_GETCRTC", drm::IOCTL_MODE_GETCRTC.into()),
        ("DRM_IOCTL_MODE_SETCRTC", drm::IOCTL_MODE_SETCRTC.into()),
        (
            "DRM_IOCTL_MODE_GETENCODER",
            drm::IOCTL_MODE_GETENCODER.into(),
        ),
        (
            "DRM_IOCTL_MODE_GETCONNECTOR",
            drm::IOCTL_MODE_GETCONNECTOR.into(),
        ),
        ("DRM_IOCTL_MODE_ADDFB", drm::IOCTL_MODE_ADDFB.into()),
        ("DRM_IOCTL_MODE_ADDFB2", drm::IOCTL_MODE_ADDFB2.into()),
        ("DRM_IOCTL_MODE_RMFB", drm::IOCTL_MODE_RMFB.into()),
        ("DRM_IOCTL_MODE_PAGE_FLIP", drm::IOCTL_MODE_PAGE_FLIP.into()),
        ("DRM_IOCTL_MODE_DIRTYFB", drm::IOCTL_MODE_DIRTYFB.into()),
        ("DRM_IOCTL_MODE_CURSOR", drm::IOCTL_MODE_CURSOR.into()),
        ("DRM_IOCTL_MODE_CURSOR2", drm::IOCTL_MODE_CURSOR2.into()),
        (
            "DRM_IOCTL_MODE_CREATE_DUMB",
            drm::IOCTL_MODE_CREATE_DUMB.into(),
        ),
        ("DRM_IOCTL_MODE_MAP_DUMB", drm::IOCTL_MODE_MAP_DUMB.into()),
        (
            "DRM_IOCTL_MODE_DESTROY_DUMB",
            drm::IOCTL_MODE_DESTROY_DUMB.into(),
        ),
        (
            "DRM_IOCTL_MODE_GETPLANERESOURCES",
            drm::IOCTL_MODE_GETPLANERESOURCES.into(),
        ),
        ("DRM_IOCTL_MODE_GETPLANE", drm::IOCTL_MODE_GETPLANE.into()),
        ("DRM_IOCTL_GEM_CLOSE", drm::IOCTL_GEM_CLOSE.into()),
        (
            "DRM_IOCTL_PRIME_HANDLE_TO_FD",
            drm::IOCTL_PRIME_HANDLE_TO_FD.into(),
        ),
        (
            "DRM_IOCTL_PRIME_FD_TO_HANDLE",
            drm::IOCTL_PRIME_FD_TO_HANDLE.into(),
        ),
        (
            "DRM_IOCTL_MODE_OBJ_GETPROPERTIES",
            drm::IOCTL_MODE_OBJ_GETPROPERTIES.into(),
        ),
        (
            "DRM_IOCTL_MODE_GETPROPERTY",
            drm::IOCTL_MODE_GETPROPERTY.into(),
        ),
        (
            "DRM_IOCTL_MODE_GETPROPBLOB",
            drm::IOCTL_MODE_GETPROPBLOB.into(),
        ),
    ]
}

/// Every other constant.
fn values() -> Vec<(&'static str, u64)> {
    vec![
        ("DRM_CAP_DUMB_BUFFER", drm::CAP_DUMB_BUFFER),
        ("DRM_CAP_VBLANK_HIGH_CRTC", drm::CAP_VBLANK_HIGH_CRTC),
        (
            "DRM_CAP_DUMB_PREFERRED_DEPTH",
            drm::CAP_DUMB_PREFERRED_DEPTH,
        ),
        ("DRM_CAP_DUMB_PREFER_SHADOW", drm::CAP_DUMB_PREFER_SHADOW),
        ("DRM_CAP_PRIME", drm::CAP_PRIME),
        ("DRM_PRIME_CAP_IMPORT", drm::PRIME_CAP_IMPORT),
        ("DRM_PRIME_CAP_EXPORT", drm::PRIME_CAP_EXPORT),
        ("DRM_CAP_TIMESTAMP_MONOTONIC", drm::CAP_TIMESTAMP_MONOTONIC),
        ("DRM_CAP_ASYNC_PAGE_FLIP", drm::CAP_ASYNC_PAGE_FLIP),
        ("DRM_CAP_CURSOR_WIDTH", drm::CAP_CURSOR_WIDTH),
        ("DRM_CAP_CURSOR_HEIGHT", drm::CAP_CURSOR_HEIGHT),
        ("DRM_CAP_ADDFB2_MODIFIERS", drm::CAP_ADDFB2_MODIFIERS),
        ("DRM_CAP_PAGE_FLIP_TARGET", drm::CAP_PAGE_FLIP_TARGET),
        (
            "DRM_CAP_CRTC_IN_VBLANK_EVENT",
            drm::CAP_CRTC_IN_VBLANK_EVENT,
        ),
        ("DRM_CAP_SYNCOBJ", drm::CAP_SYNCOBJ),
        ("DRM_CAP_SYNCOBJ_TIMELINE", drm::CAP_SYNCOBJ_TIMELINE),
        (
            "DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP",
            drm::CAP_ATOMIC_ASYNC_PAGE_FLIP,
        ),
        ("DRM_CLIENT_CAP_STEREO_3D", drm::CLIENT_CAP_STEREO_3D),
        (
            "DRM_CLIENT_CAP_UNIVERSAL_PLANES",
            drm::CLIENT_CAP_UNIVERSAL_PLANES,
        ),
        ("DRM_CLIENT_CAP_ATOMIC", drm::CLIENT_CAP_ATOMIC),
        ("DRM_CLIENT_CAP_ASPECT_RATIO", drm::CLIENT_CAP_ASPECT_RATIO),
        (
            "DRM_CLIENT_CAP_WRITEBACK_CONNECTORS",
            drm::CLIENT_CAP_WRITEBACK_CONNECTORS,
        ),
        (
            "DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT",
            drm::CLIENT_CAP_CURSOR_PLANE_HOTSPOT,
        ),
        ("DRM_EVENT_VBLANK", drm::EVENT_VBLANK.into()),
        ("DRM_EVENT_FLIP_COMPLETE", drm::EVENT_FLIP_COMPLETE.into()),
        ("DRM_EVENT_CRTC_SEQUENCE", drm::EVENT_CRTC_SEQUENCE.into()),
        ("DRM_DISPLAY_MODE_LEN", drm::DISPLAY_MODE_LEN as u64),
        ("DRM_MODE_TYPE_PREFERRED", drm::MODE_TYPE_PREFERRED.into()),
        ("DRM_MODE_TYPE_USERDEF", drm::MODE_TYPE_USERDEF.into()),
        ("DRM_MODE_TYPE_DRIVER", drm::MODE_TYPE_DRIVER.into()),
        ("DRM_MODE_FLAG_PHSYNC", drm::MODE_FLAG_PHSYNC.into()),
        ("DRM_MODE_FLAG_NHSYNC", drm::MODE_FLAG_NHSYNC.into()),
        ("DRM_MODE_FLAG_PVSYNC", drm::MODE_FLAG_PVSYNC.into()),
        ("DRM_MODE_FLAG_NVSYNC", drm::MODE_FLAG_NVSYNC.into()),
        ("DRM_MODE_CONNECTOR_Unknown", drm::CONNECTOR_UNKNOWN.into()),
        ("DRM_MODE_CONNECTOR_VIRTUAL", drm::CONNECTOR_VIRTUAL.into()),
        ("DRM_MODE_ENCODER_NONE", drm::ENCODER_NONE.into()),
        ("DRM_MODE_ENCODER_VIRTUAL", drm::ENCODER_VIRTUAL.into()),
        ("DRM_MODE_FB_INTERLACED", drm::FB_INTERLACED.into()),
        ("DRM_MODE_FB_MODIFIERS", drm::FB_MODIFIERS.into()),
        ("DRM_MODE_PAGE_FLIP_EVENT", drm::PAGE_FLIP_EVENT.into()),
        ("DRM_MODE_PAGE_FLIP_ASYNC", drm::PAGE_FLIP_ASYNC.into()),
        (
            "DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE",
            drm::PAGE_FLIP_TARGET_ABSOLUTE.into(),
        ),
        (
            "DRM_MODE_PAGE_FLIP_TARGET_RELATIVE",
            drm::PAGE_FLIP_TARGET_RELATIVE.into(),
        ),
        (
            "DRM_MODE_FB_DIRTY_MAX_CLIPS",
            drm::FB_DIRTY_MAX_CLIPS.into(),
        ),
        ("DRM_MODE_CURSOR_BO", drm::MODE_CURSOR_BO.into()),
        ("DRM_MODE_CURSOR_MOVE", drm::MODE_CURSOR_MOVE.into()),
        ("DRM_MODE_CURSOR_FLAGS", drm::MODE_CURSOR_FLAGS.into()),
        ("DRM_FORMAT_XRGB8888", drm::FORMAT_XRGB8888.into()),
        ("DRM_FORMAT_ARGB8888", drm::FORMAT_ARGB8888.into()),
        ("DRM_FORMAT_XBGR8888", drm::FORMAT_XBGR8888.into()),
        ("DRM_FORMAT_ABGR8888", drm::FORMAT_ABGR8888.into()),
    ]
}

/// The mode object types and property flags.
fn object_values() -> Vec<(&'static str, u64)> {
    vec![
        ("DRM_MODE_OBJECT_CRTC", drm::MODE_OBJECT_CRTC.into()),
        (
            "DRM_MODE_OBJECT_CONNECTOR",
            drm::MODE_OBJECT_CONNECTOR.into(),
        ),
        ("DRM_MODE_OBJECT_ENCODER", drm::MODE_OBJECT_ENCODER.into()),
        ("DRM_MODE_OBJECT_MODE", drm::MODE_OBJECT_MODE.into()),
        ("DRM_MODE_OBJECT_PROPERTY", drm::MODE_OBJECT_PROPERTY.into()),
        ("DRM_MODE_OBJECT_FB", drm::MODE_OBJECT_FB.into()),
        ("DRM_MODE_OBJECT_BLOB", drm::MODE_OBJECT_BLOB.into()),
        ("DRM_MODE_OBJECT_PLANE", drm::MODE_OBJECT_PLANE.into()),
        ("DRM_MODE_OBJECT_ANY", drm::MODE_OBJECT_ANY.into()),
        ("DRM_PROP_NAME_LEN", drm::PROP_NAME_LEN as u64),
        ("DRM_MODE_PROP_PENDING", drm::MODE_PROP_PENDING.into()),
        ("DRM_MODE_PROP_RANGE", drm::MODE_PROP_RANGE.into()),
        ("DRM_MODE_PROP_IMMUTABLE", drm::MODE_PROP_IMMUTABLE.into()),
        ("DRM_MODE_PROP_ENUM", drm::MODE_PROP_ENUM.into()),
        ("DRM_MODE_PROP_BLOB", drm::MODE_PROP_BLOB.into()),
        ("DRM_MODE_PROP_BITMASK", drm::MODE_PROP_BITMASK.into()),
        (
            "DRM_MODE_PROP_LEGACY_TYPE",
            drm::MODE_PROP_LEGACY_TYPE.into(),
        ),
        (
            "DRM_MODE_PROP_EXTENDED_TYPE",
            drm::MODE_PROP_EXTENDED_TYPE.into(),
        ),
        ("DRM_MODE_PROP_OBJECT", drm::MODE_PROP_OBJECT.into()),
        (
            "DRM_MODE_PROP_SIGNED_RANGE",
            drm::MODE_PROP_SIGNED_RANGE.into(),
        ),
        ("DRM_MODE_PROP_ATOMIC", drm::MODE_PROP_ATOMIC.into()),
    ]
}

/// A layout's `sizeof` and `offsetof` lines as the probe prints them.
fn layout_lines(c_name: &str, size: usize, fields: &[(&str, usize)]) -> Vec<(String, u64)> {
    let mut lines = vec![(format!("sizeof.{c_name}"), size as u64)];
    for (field, offset) in fields {
        lines.push((format!("offsetof.{c_name}.{field}"), *offset as u64));
    }
    lines
}

fn layouts<L: Layout>() -> Vec<(String, u64)> {
    layout_lines(L::C_NAME, L::SIZE, L::FIELDS)
}

/// Every line this module says the probe prints at `width`.
fn expected(width: Width) -> BTreeMap<String, u64> {
    let mut lines: Vec<(String, u64)> = constants(width)
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
    lines.extend(layout_lines(
        Version::C_NAME,
        Version::size(width),
        &Version::fields(width),
    ));
    lines.extend(layouts::<drm::GetCap>());
    lines.extend(layouts::<drm::SetClientCap>());
    lines.extend(layouts::<drm::ModeInfo>());
    lines.extend(layouts::<drm::CardRes>());
    lines.extend(layouts::<drm::Crtc>());
    lines.extend(layouts::<drm::GetEncoder>());
    lines.extend(layouts::<drm::GetConnector>());
    lines.extend(layouts::<drm::FbCmd>());
    lines.extend(layouts::<drm::FbCmd2>());
    lines.extend(layouts::<drm::CrtcPageFlip>());
    lines.extend(layouts::<drm::FbDirtyCmd>());
    lines.extend(layouts::<drm::ModeCursor>());
    lines.extend(layouts::<drm::ModeCursor2>());
    lines.extend(layouts::<drm::ClipRect>());
    lines.extend(layouts::<drm::CreateDumb>());
    lines.extend(layouts::<drm::MapDumb>());
    lines.extend(layouts::<drm::DestroyDumb>());
    lines.extend(layouts::<drm::GetPlaneRes>());
    lines.extend(layouts::<drm::GetPlane>());
    lines.extend(layouts::<drm::ObjGetProperties>());
    lines.extend(layouts::<drm::GetProperty>());
    lines.extend(layouts::<drm::GetBlob>());
    lines.extend(layouts::<drm::PropertyEnum>());
    lines.extend(layouts::<drm::GemClose>());
    lines.extend(layouts::<drm::PrimeHandle>());
    lines.extend(layouts::<drm::Event>());
    lines.extend(layouts::<drm::EventVblank>());
    let count = lines.len();
    let map: BTreeMap<String, u64> = lines.into_iter().collect();
    assert_eq!(map.len(), count, "a line is defined twice");
    map
}

/// Every line on which this module and the probe disagree, one per line.
fn disagreements(width: Width, text: &str) -> Vec<String> {
    let ours = expected(width);
    let theirs = probe(text);
    let mut names: Vec<&String> = ours.keys().chain(theirs.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|name| ours.get(*name) != theirs.get(*name))
        .map(|name| {
            format!(
                "{name}: this module says {:?}, the probe printed {:?}",
                ours.get(name),
                theirs.get(name)
            )
        })
        .collect()
}

#[test]
fn every_number_and_layout_matches_the_probe_at_64_bits() {
    assert_eq!(disagreements(Width::Bits64, PROBE_64), Vec::<String>::new());
}

#[test]
fn every_number_and_layout_matches_the_probe_at_32_bits() {
    assert_eq!(disagreements(Width::Bits32, PROBE_32), Vec::<String>::new());
}

#[test]
fn only_drm_version_differs_between_the_widths() {
    let wide = probe(PROBE_64);
    let narrow = probe(PROBE_32);
    let differing: Vec<&str> = wide
        .iter()
        .filter(|(name, value)| narrow.get(*name) != Some(value))
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(!differing.is_empty());
    for name in differing {
        assert!(
            name == "DRM_IOCTL_VERSION" || name.contains(".drm_version"),
            "{name} differs between widths"
        );
    }
}

/// Bytes counting up from `seed`, so every field reads a different value.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

fn round_trip<L: Layout + core::fmt::Debug + PartialEq>() {
    let bytes = pattern(L::SIZE, 7);
    let value = L::read(&bytes).expect("a whole structure reads");
    let mut out = vec![0u8; L::SIZE];
    value.write(&mut out).expect("it fits");

    // Every byte comes back as it was, or as zero where it is padding no field
    // covers; zero is what the kernel must hand back to userspace there. The
    // pattern has no zero byte in a structure this small, so a field that
    // lost a byte shows.
    for (index, (&got, &was)) in out.iter().zip(&bytes).enumerate() {
        assert!(
            got == was || got == 0,
            "{} byte {index} came back as {got}, not {was}",
            L::C_NAME
        );
    }
    let padding = out.iter().filter(|&&byte| byte == 0).count();
    assert!(padding <= 4, "{} has {padding} bytes of padding", L::C_NAME);
    assert_eq!(L::read(&out), Some(value));
    assert_eq!(L::read(&bytes[..L::SIZE - 1]), None, "{} short", L::C_NAME);
    assert_eq!(value.write(&mut out[..L::SIZE - 1]), None);
    assert_eq!(<L as Field>::get(&bytes, 0), Some(value));
    assert_eq!(<L as Field>::get(&bytes, 1), None);
}

#[test]
fn every_structure_reads_and_writes_back() {
    round_trip::<drm::GetCap>();
    round_trip::<drm::SetClientCap>();
    round_trip::<drm::ModeInfo>();
    round_trip::<drm::CardRes>();
    round_trip::<drm::Crtc>();
    round_trip::<drm::GetEncoder>();
    round_trip::<drm::GetConnector>();
    round_trip::<drm::FbCmd>();
    round_trip::<drm::FbCmd2>();
    round_trip::<drm::CrtcPageFlip>();
    round_trip::<drm::FbDirtyCmd>();
    round_trip::<drm::ModeCursor>();
    round_trip::<drm::ModeCursor2>();
    round_trip::<drm::ClipRect>();
    round_trip::<drm::CreateDumb>();
    round_trip::<drm::MapDumb>();
    round_trip::<drm::DestroyDumb>();
    round_trip::<drm::GetPlaneRes>();
    round_trip::<drm::GetPlane>();
    round_trip::<drm::ObjGetProperties>();
    round_trip::<drm::GetProperty>();
    round_trip::<drm::GetBlob>();
    round_trip::<drm::PropertyEnum>();
    round_trip::<drm::Event>();
    round_trip::<drm::EventVblank>();
}

#[test]
fn fields_land_where_the_headers_put_them() {
    let mut bytes = vec![0u8; drm::CreateDumb::SIZE];
    drm::CreateDumb {
        height: 800,
        width: 1280,
        bpp: 32,
        flags: 0,
        handle: 1,
        pitch: 5120,
        size: 4_096_000,
    }
    .write(&mut bytes)
    .expect("it fits");
    assert_eq!(bytes[0..4], 800u32.to_le_bytes());
    assert_eq!(bytes[4..8], 1280u32.to_le_bytes());
    assert_eq!(bytes[20..24], 5120u32.to_le_bytes());
    assert_eq!(bytes[24..32], 4_096_000u64.to_le_bytes());

    let mut event = [0u8; 32];
    drm::EventVblank {
        base: drm::Event {
            r#type: drm::EVENT_FLIP_COMPLETE,
            length: 32,
        },
        user_data: 0xDEAD_BEEF,
        tv_sec: 1,
        tv_usec: 2,
        sequence: 3,
        crtc_id: 4,
    }
    .write(&mut event)
    .expect("it fits");
    assert_eq!(event[0..8], [2, 0, 0, 0, 32, 0, 0, 0]);
    assert_eq!(event[8..16], 0xDEAD_BEEFu64.to_le_bytes());
    assert_eq!(event[28..32], 4u32.to_le_bytes());

    let mut modes = [0u8; 104];
    let mut mode = drm::ModeInfo::ZERO;
    mode.hdisplay = 1280;
    mode.vdisplay = 800;
    mode.name[..8].copy_from_slice(b"1280x800");
    drm::Crtc {
        mode,
        mode_valid: 1,
        ..drm::Crtc::ZERO
    }
    .write(&mut modes)
    .expect("it fits");
    assert_eq!(modes[36 + 4..36 + 6], 1280u16.to_le_bytes());
    assert_eq!(modes[36 + 14..36 + 16], 800u16.to_le_bytes());
    assert_eq!(&modes[36 + 36..36 + 44], b"1280x800");
}

#[test]
fn drm_version_at_both_widths() {
    for width in [Width::Bits32, Width::Bits64] {
        let version = Version {
            version_major: 0,
            version_minor: 1,
            version_patchlevel: -2,
            name_len: 10,
            name: 0x1000,
            date_len: 8,
            date: 0x2000,
            desc_len: 20,
            desc: 0x3000,
        };
        let mut bytes = vec![0u8; Version::size(width)];
        version.write(width, &mut bytes).expect("it fits");
        assert_eq!(Version::read(width, &bytes), Some(version));
        let [.., (_, desc)] = Version::fields(width);
        assert_eq!(width.word(&bytes, desc), Some(0x3000));
        assert_eq!(Version::read(width, &bytes[1..]), None);
    }
    let too_wide = Version {
        name: 1 << 40,
        ..Version::read(Width::Bits64, &[0; 64]).expect("zeroes read")
    };
    assert_eq!(too_wide.write(Width::Bits32, &mut [0; 36]), None);
    assert_eq!(too_wide.write(Width::Bits64, &mut [0; 64]), Some(()));
}

#[test]
fn a_property_and_its_enum_land_where_the_headers_put_them() {
    let mut name = [0u8; drm::PROP_NAME_LEN];
    name[..4].copy_from_slice(b"type");
    let mut bytes = vec![0u8; drm::GetProperty::SIZE];
    drm::GetProperty {
        values_ptr: 0x1000,
        enum_blob_ptr: 0x2000,
        prop_id: 5,
        flags: drm::MODE_PROP_ENUM | drm::MODE_PROP_IMMUTABLE,
        name,
        count_values: 3,
        count_enum_blobs: 3,
    }
    .write(&mut bytes)
    .expect("it fits");
    assert_eq!(bytes[16..20], 5u32.to_le_bytes());
    assert_eq!(bytes[20..24], 12u32.to_le_bytes());
    assert_eq!(&bytes[24..28], b"type");
    assert_eq!(bytes[56..64], [3, 0, 0, 0, 3, 0, 0, 0]);

    let mut record = [0u8; 40];
    let mut primary = [0u8; drm::PROP_NAME_LEN];
    primary[..7].copy_from_slice(b"Primary");
    drm::PropertyEnum {
        value: drm::PLANE_TYPE_PRIMARY,
        name: primary,
    }
    .write(&mut record)
    .expect("it fits");
    assert_eq!(record[0..8], 1u64.to_le_bytes());
    assert_eq!(&record[8..15], b"Primary");

    let mut request = [0u8; 32];
    drm::ObjGetProperties {
        obj_id: 4,
        obj_type: drm::MODE_OBJECT_PLANE,
        ..drm::ObjGetProperties::ZERO
    }
    .write(&mut request)
    .expect("it fits");
    assert_eq!(request[20..24], 4u32.to_le_bytes());
    assert_eq!(request[24..28], [0xEE; 4]);
    assert_eq!(request[28..32], [0; 4], "tail padding");
}

#[test]
fn fourcc_is_little_endian_ascii() {
    assert_eq!(drm::FORMAT_XRGB8888, 0x3432_5258);
    assert_eq!(drm::FORMAT_XRGB8888.to_le_bytes(), *b"XR24");
}
