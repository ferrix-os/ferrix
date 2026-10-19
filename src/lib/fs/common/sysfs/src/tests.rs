//! The formats, pinned against what Linux prints.

use alloc::vec::Vec;

use crate::input::{self, Map};
use crate::name::{self, Slot};
use crate::{attr, drm, net, order, path, pci, uevent};

fn text(fill: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    fill(&mut out);
    out
}

/// QEMU's `virtio-gpu-pci` at 00:02.0, as a Linux guest's sysfs shows it.
const GPU: pci::Identity = pci::Identity {
    vendor: 0x1af4,
    device: 0x1050,
    subsystem_vendor: 0x1af4,
    subsystem_device: 0x1100,
    class: 0x03_80_00,
};

#[test]
fn pci_attributes_are_what_linux_printed() {
    assert_eq!(text(|out| attr::hex16(out, GPU.vendor)), b"0x1af4\n");
    assert_eq!(text(|out| attr::hex16(out, 0)), b"0x0000\n");
    assert_eq!(text(|out| attr::class(out, GPU.class)), b"0x038000\n");
    assert_eq!(text(|out| attr::class(out, 0x06_00_00)), b"0x060000\n");
    assert_eq!(text(|out| attr::hex8(out, 1)), b"0x01\n");
}

#[test]
fn a_pci_uevent_is_pci_uevent() {
    let mut slot = Vec::new();
    Slot::from_word(0x0010).name(&mut slot);
    assert_eq!(slot, b"0000:00:02.0");
    assert_eq!(
        text(|out| pci::uevent(out, &GPU, &slot, Some(b"gpu"))),
        b"DRIVER=gpu\n\
          PCI_CLASS=38000\n\
          PCI_ID=1AF4:1050\n\
          PCI_SUBSYS_ID=1AF4:1100\n\
          PCI_SLOT_NAME=0000:00:02.0\n\
          MODALIAS=pci:v00001AF4d00001050sv00001AF4sd00001100bc03sc80i00\n"
    );
    // The host bridge of a q35: class 0x060000, which %04X prints as five
    // digits and not six.
    let bridge = pci::Identity {
        vendor: 0x8086,
        device: 0x29c0,
        subsystem_vendor: 0x1af4,
        subsystem_device: 0x1100,
        class: 0x06_00_00,
    };
    let lines = text(|out| pci::uevent(out, &bridge, b"0000:00:00.0", None));
    assert!(lines.starts_with(b"PCI_CLASS=60000\nPCI_ID=8086:29C0\n"));
    assert_eq!(
        text(|out| pci::modalias(out, &bridge)),
        b"pci:v00008086d000029C0sv00001AF4sd00001100bc06sc00i00\n"
    );
}

#[test]
fn slots_are_named_and_read_as_pci_name() {
    let slot = Slot::from_word((1 << 16) | (0x3a << 8) | (0x1f << 3) | 7);
    assert_eq!(
        slot,
        Slot {
            segment: 1,
            bus: 0x3a,
            device: 0x1f,
            function: 7
        }
    );
    let named = text(|out| slot.name(out));
    assert_eq!(named, b"0001:3a:1f.7");
    assert_eq!(Slot::parse(&named), Some(slot));
    for wrong in [
        &b"0000:00:02.8"[..],
        b"0000:00:20.0",
        b"0000:00:2.0",
        b"0000:00:02.0\n",
        b"0000:0A:02.0",
        b"00000:00:02.0",
        b"",
    ] {
        assert_eq!(Slot::parse(wrong), None, "{wrong:?} read as a slot");
    }
    assert_eq!(text(|out| name::pci_root(out, 0, 0)), b"pci0000:00");
    assert_eq!(
        text(|out| name::platform(out, 0x5a00_1000, "display-controller")),
        b"5a001000.display-controller"
    );
}

#[test]
fn device_numbers_are_major_colon_minor() {
    assert_eq!(text(|out| attr::dev(out, 226, 128)), b"226:128\n");
    assert_eq!(text(|out| name::dev_number(out, 1, 3)), b"1:3");
    assert_eq!(name::parse_dev_number(b"226:0"), Some((226, 0)));
    assert_eq!(name::parse_dev_number(b"13:64"), Some((13, 64)));
    for wrong in [
        &b"226:00"[..],
        b"226",
        b":0",
        b"226:",
        b"+1:3",
        b"1:3:4",
        b"01:3",
    ] {
        assert_eq!(name::parse_dev_number(wrong), None, "{wrong:?}");
    }
}

#[test]
fn numbered_names_are_spelt_once() {
    assert_eq!(name::numbered(b"card", b"card0"), Some(0));
    assert_eq!(name::numbered(b"cpu", b"cpu12"), Some(12));
    assert_eq!(name::numbered(b"renderD", b"renderD128"), Some(128));
    for wrong in [
        &b"card00"[..],
        b"card",
        b"card-1",
        b"card+1",
        b"cardx",
        b"cpu4294967296",
    ] {
        assert_eq!(name::numbered(b"card", wrong), None, "{wrong:?}");
        assert_eq!(name::numbered(b"cpu", wrong), None, "{wrong:?}");
    }
}

#[test]
fn a_bind_write_names_what_echo_wrote() {
    assert_eq!(name::written(b"0000:00:02.0\n"), Some(&b"0000:00:02.0"[..]));
    assert_eq!(name::written(b"0000:00:02.0"), Some(&b"0000:00:02.0"[..]));
    // One newline is dropped, not two, and nothing past a NUL is read.
    assert_eq!(name::written(b"x\n\n"), Some(&b"x\n"[..]));
    assert_eq!(name::written(b"x\0y"), Some(&b"x"[..]));
    assert_eq!(name::written(b"\n"), None);
    assert_eq!(name::written(b""), None);
    assert_eq!(name::written(b"\0x"), None);
}

#[test]
fn processor_lists_are_ranges() {
    assert_eq!(text(|out| attr::cpu_list(out, &[0, 1, 2, 3])), b"0-3\n");
    assert_eq!(text(|out| attr::cpu_list(out, &[0])), b"0\n");
    assert_eq!(text(|out| attr::cpu_list(out, &[0, 2, 3, 5])), b"0,2-3,5\n");
    assert_eq!(text(|out| attr::cpu_list(out, &[])), b"\n");
    assert_eq!(
        text(|out| attr::cpu_list(out, &[u32::MAX - 1, u32::MAX])),
        b"4294967294-4294967295\n"
    );
}

#[test]
fn interface_files_are_what_linux_printed() {
    assert_eq!(
        text(|out| attr::hardware_address(out, &[0x52, 0x54, 0, 0x12, 0x34, 0x56])),
        b"52:54:00:12:34:56\n"
    );
    assert_eq!(
        text(|out| attr::hardware_address(out, &[0; 6])),
        b"00:00:00:00:00:00\n"
    );
    // `ip link`'s eth0: up, broadcast, running, multicast, lower up.
    assert_eq!(text(|out| attr::alternate_hex(out, 0x1003)), b"0x1003\n");
    assert_eq!(text(|out| attr::alternate_hex(out, 9)), b"0x9\n");
    assert_eq!(text(|out| attr::alternate_hex(out, 0)), b"0\n");
    assert_eq!(net::operstate(0x1_1043), b"up\n");
    assert_eq!(net::operstate(0x1003), b"down\n");
    assert_eq!(net::operstate(0x1002), b"down\n");
    assert_eq!(net::operstate(0x1_0049), b"unknown\n");
    assert_eq!(net::carrier(0x1_1043), Some(true));
    assert_eq!(net::carrier(0x1003), Some(false));
    assert_eq!(net::carrier(0x1002), None);
    assert_eq!(
        text(|out| uevent::interface(out, b"eth0", 2)),
        b"INTERFACE=eth0\nIFINDEX=2\n"
    );
}

#[test]
fn node_uevents_are_dev_uevent() {
    let card = uevent::Node {
        major: 226,
        minor: 0,
        name: b"dri/card0",
        mode: None,
        kind: Some(b"drm_minor"),
    };
    assert_eq!(
        text(|out| uevent::node(out, &card)),
        b"MAJOR=226\nMINOR=0\nDEVNAME=dri/card0\nDEVTYPE=drm_minor\n"
    );
    let null = uevent::Node {
        major: 1,
        minor: 3,
        name: b"null",
        mode: Some(0o666),
        kind: None,
    };
    assert_eq!(
        text(|out| uevent::node(out, &null)),
        b"MAJOR=1\nMINOR=3\nDEVNAME=null\nDEVMODE=0666\n"
    );
    assert_eq!(
        text(|out| input::event_uevent(out, 13, 64, b"input/event0")),
        b"MAJOR=13\nMINOR=64\nDEVNAME=input/event0\n"
    );
}

/// QEMU's virtio keyboard: `EV_SYN`, `EV_KEY`, `EV_MSC`, `EV_LED` and
/// `EV_REP`.
const KEYBOARD_TYPES: [u8; 4] = [0x13, 0x00, 0x12, 0x00];

#[test]
fn bitmaps_are_words_of_the_kernels_long() {
    // What `cat capabilities/ev` prints for it on any Linux.
    assert_eq!(
        text(|out| input::bitmap(out, &KEYBOARD_TYPES, input::EV_MAX, 64)),
        b"120013\n"
    );
    assert_eq!(
        text(|out| input::bitmap(out, &KEYBOARD_TYPES, input::EV_MAX, 32)),
        b"120013\n"
    );
    // A key in the second word: bit 64 and bit 1, in 64-bit words, then the
    // same bits in 32-bit ones, where a zero word between them is printed.
    let mut keys = [0_u8; 96];
    keys[0] = 0b10;
    keys[8] = 1;
    assert_eq!(
        text(|out| input::bitmap(out, &keys, input::KEY_MAX, 64)),
        b"1 2\n"
    );
    assert_eq!(
        text(|out| input::bitmap(out, &keys, input::KEY_MAX, 32)),
        b"1 0 2\n"
    );
    // Nothing is a lone zero.
    assert_eq!(
        text(|out| input::bitmap(out, &[0; 96], input::KEY_MAX, 64)),
        b"0\n"
    );
    // A short array is zeros past its end, not a refusal.
    assert_eq!(
        text(|out| input::bitmap(out, &[], input::FF_MAX, 64)),
        b"0\n"
    );
    // BITS_TO_LONGS(max), not of the count: bit 64 of a map whose max is 64
    // is past the words printed, as Linux leaves it.
    let mut edge = [0_u8; 9];
    edge[8] = 1;
    assert_eq!(text(|out| input::bitmap(out, &edge, 64, 64)), b"0\n");
}

#[test]
fn an_input_uevent_is_input_dev_uevent() {
    let mut keys = [0_u8; 96];
    keys[0] = 0xfe;
    let leds = [0x07_u8, 0];
    let maps = [
        (
            1,
            Map {
                key: "KEY",
                bits: &keys,
                max: input::KEY_MAX,
            },
        ),
        (
            2,
            Map {
                key: "REL",
                bits: &[0xff],
                max: input::REL_MAX,
            },
        ),
        (
            17,
            Map {
                key: "LED",
                bits: &leds,
                max: input::LED_MAX,
            },
        ),
    ];
    let device = input::Device {
        id: [6, 0x0627, 1, 1],
        name: b"QEMU Virtio Keyboard",
        uniq: b"",
        properties: &[0; 4],
        types: &KEYBOARD_TYPES,
        maps: &maps,
    };
    // No REL: the keyboard does not have the type, whatever its map holds.
    assert_eq!(
        text(|out| input::uevent(out, &device, 64)),
        b"PRODUCT=6/627/1/1\n\
          NAME=\"QEMU Virtio Keyboard\"\n\
          PROP=0\n\
          EV=120013\n\
          KEY=fe\n\
          LED=7\n"
    );
    assert_eq!(text(|out| input::id(out, 6)), b"0006\n");
    assert_eq!(text(|out| input::id(out, 0x0627)), b"0627\n");
}

#[test]
fn connectors_are_drm_sysfs() {
    assert_eq!(
        text(|out| drm::connector_name(out, 0, drm::CONNECTOR_VIRTUAL, 1)),
        b"card0-Virtual-1"
    );
    assert_eq!(
        text(|out| drm::connector_name(out, 1, drm::CONNECTOR_HDMIA, 1)),
        b"card1-HDMI-A-1"
    );
    assert_eq!(text(|out| drm::status(out, true)), b"connected\n");
    assert_eq!(text(|out| drm::enabled(out, false)), b"disabled\n");
    assert_eq!(
        text(|out| drm::modes(out, &[(1280, 800), (1920, 1080)])),
        b"1280x800\n1920x1080\n"
    );
}

#[test]
fn disk_sizes_count_512_byte_sectors() {
    assert_eq!(attr::size_in_512_byte_sectors(2048, 512), 2048);
    assert_eq!(attr::size_in_512_byte_sectors(2048, 4096), 16384);
    assert_eq!(
        attr::size_in_512_byte_sectors(u64::MAX, 4096),
        u64::MAX / 512
    );
}

fn relative(from: &[&[u8]], to: &[&[u8]]) -> Vec<u8> {
    text(|out| path::relative(out, from, to))
}

#[test]
fn links_are_relative_as_kernfs_spells_them() {
    let slot: [&[u8]; 3] = [b"devices", b"pci0000:00", b"0000:00:02.0"];
    // /sys/block/vda
    assert_eq!(
        relative(
            &[b"block"],
            &[b"devices", b"pci0000:00", b"0000:00:04.0", b"block", b"vda"]
        ),
        b"../devices/pci0000:00/0000:00:04.0/block/vda"
    );
    // /sys/bus/pci/drivers/gpu/0000:00:02.0
    assert_eq!(
        relative(&[b"bus", b"pci", b"drivers", b"gpu"], &slot),
        b"../../../../devices/pci0000:00/0000:00:02.0"
    );
    // /sys/devices/pci0000:00/0000:00:02.0/driver
    assert_eq!(
        relative(&slot, &[b"bus", b"pci", b"drivers", b"gpu"]),
        b"../../../bus/pci/drivers/gpu"
    );
    // /sys/devices/pci0000:00/0000:00:02.0/drm/card0/device
    assert_eq!(
        relative(
            &[b"devices", b"pci0000:00", b"0000:00:02.0", b"drm", b"card0"],
            &slot
        ),
        b"../../../0000:00:02.0"
    );
    // /sys/dev/char/1:3
    assert_eq!(
        relative(
            &[b"dev", b"char"],
            &[b"devices", b"virtual", b"mem", b"null"]
        ),
        b"../../devices/virtual/mem/null"
    );
    // A link to an ancestor names it, after climbing past it.
    assert_eq!(relative(&[b"a"], &[b"a"]), b"../a");
    assert_eq!(relative(&[b"a", b"b"], &[b"a"]), b"../../a");
    assert_eq!(relative(&[], &[]), b".");
}

#[test]
fn numbers_and_cursors_come_from_names() {
    assert_eq!(order::inode(&[]), 1);
    let card: [&[u8]; 2] = [b"class", b"drm"];
    let first = order::inode(&card);
    assert_eq!(first, order::inode(&card), "one path, one number");
    assert!((2..(1 << 63)).contains(&first), "{first} is out of range");
    assert_ne!(first, order::inode(&[b"class", b"net"]));
    // Components are separated, so a name split differently is another node.
    assert_ne!(order::inode(&[b"ab", b"c"]), order::inode(&[b"a", b"bc"]));
    let cursor = order::cursor(b"card0");
    assert!(
        (order::FIRST_CURSOR..(1 << 62) + order::FIRST_CURSOR).contains(&cursor),
        "{cursor} is out of range"
    );
    assert_ne!(cursor, order::cursor(b"card1"));
}
