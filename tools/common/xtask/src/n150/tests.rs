use std::io::Cursor;

use super::*;

fn lines(text: &[&str]) -> Vec<String> {
    text.iter().map(|line| (*line).to_owned()).collect()
}

#[test]
fn a_512_gb_disk_has_its_esp_at_one_mib_and_its_root_to_the_last_whole_mib() {
    let layout = layout(DEFAULT_DISK_BYTES, 512 << 20).unwrap();
    assert_eq!(layout.sectors, 1_000_215_216);
    assert_eq!(layout.esp, (2048, 2048 + 1_048_576 - 1));
    assert_eq!(layout.root.0, 2048 + 1_048_576);
    assert_eq!((layout.root.1 + 1) % MIB_SECTORS, 0);
    // The backup table's 33 sectors stay clear of the root.
    assert!(layout.root.1 < layout.sectors - 33);
    let (at, len) = layout.root_bytes();
    assert_eq!(at, 513 << 20);
    assert!(len > 476 << 30, "{len}");
}

#[test]
fn an_esp_of_an_odd_size_still_puts_the_root_on_a_mib() {
    let layout = layout(TEST_DISK_BYTES, 512 * 3).unwrap();
    assert_eq!(layout.esp, (2048, 2050));
    assert_eq!(layout.root.0, 4096);
}

#[test]
fn a_disk_too_small_for_a_root_of_a_gib_is_refused() {
    assert!(layout(1 << 30, 512 << 20).is_err());
    assert!(layout(TEST_DISK_BYTES + 1, 512 << 20).is_err());
    assert!(layout(TEST_DISK_BYTES, 0).is_err());
}

#[test]
fn total_bytes_is_the_superblocks_and_not_the_device_items() {
    let said = "superblock: bytenr=65536, device=root\n\
                dev_item.total_bytes\t\t999\n\
                total_bytes\t\t3757047808\n\
                bytes_used\t\t131072\n";
    assert_eq!(parse_total_bytes(said), Some(3_757_047_808));
    assert_eq!(parse_total_bytes("dev_item.total_bytes 5\n"), None);
}

#[test]
fn the_image_is_never_a_device() {
    assert!(refuse_devices(Path::new("/dev/nvme0n1")).is_err());
    assert!(refuse_devices(Path::new("/dev/disk/by-id/nvme-WD")).is_err());
    assert!(refuse_devices(Path::new("/")).is_err(), "a directory");
    assert!(refuse_devices(Path::new("/nonexistent/n150-disk.img")).is_ok());
}

#[test]
fn a_sparse_copy_leaves_zeros_unwritten_and_places_the_rest() {
    let mut source = vec![0_u8; 3 << 20];
    source[(1 << 20) + 5] = 7;
    let mut to = Cursor::new(vec![0xEE_u8; 5 << 20]);
    copy_sparse(&mut Cursor::new(&source), &mut to, 1 << 20).unwrap();
    let to = to.into_inner();
    // The first and last MiB of the source were zeros: not written.
    assert_eq!(to[1 << 20], 0xEE);
    assert_eq!(to[(3 << 20) + 1], 0xEE);
    // The middle MiB was written whole, its one byte in place.
    assert_eq!(to[(2 << 20) + 5], 7);
    assert_eq!(to[2 << 20], 0);
}

#[test]
fn the_root_line_names_the_disk() {
    let said = lines(&[
        "  1.234 |   root     / is btrfs on vde2; the system was installed on it (4096 KiB), \
         committed every 30 s",
    ]);
    assert_eq!(root_disk(&said).as_deref(), Some("vde2"));
    assert!(root_disk(&lines(&["  root     / stays the tmpfs"])).is_none());
}

/// The first boot's lines, as the guest printed them (abridged).
const FIRST: &[&str] = &[
    "  0.900 |   nvme     0000:03:00.0: NVMe 1.4 controller, QEMU NVMe Ctrl",
    "  1.234 |   root     / is btrfs on vde2; the system was installed on it (4096 KiB), \
     committed every 30 s",
    "  2.000 | n150: marker ferrix-n150-fixture",
    "  2.001 | n150: nothing kept yet",
    "  2.050 | n150: kept written and synced",
];

#[test]
fn the_first_boot_passes_with_every_line_and_fails_naming_the_missing_one() {
    check_first(&lines(FIRST), false).unwrap();
    let mut on_tmpfs = lines(FIRST);
    on_tmpfs.retain(|line| !line.contains("/ is btrfs"));
    let error = check_first(&on_tmpfs, false).unwrap_err().to_string();
    assert!(error.contains("/ is not on its root partition"), "{error}");
    let mut no_marker = lines(FIRST);
    no_marker.retain(|line| !line.contains("marker"));
    let error = check_first(&no_marker, false).unwrap_err().to_string();
    assert!(error.contains("marker was not read from /data"), "{error}");
    let mut wrong = lines(FIRST);
    wrong[1] = wrong[1].replace("vde2", "vdd");
    assert!(check_first(&wrong, false).is_err());
}

#[test]
fn the_sata_boot_asks_for_the_ahci_drive_not_the_nvme_one() {
    // The NVMe boot's lines do not pass the SATA check: no AHCI drive line.
    let error = check_first(&lines(FIRST), true).unwrap_err().to_string();
    assert!(error.contains("AHCI driver did not bring the drive up"), "{error}");
    let mut sata = lines(FIRST);
    sata[0] = "  0.900 |   ahci     8086:2922 port 0: QEMU HARDDISK serial ferrix-n150: \
               2097152 sectors of 512 (1024 MiB)"
        .to_owned();
    check_first(&sata, true).unwrap();
    // And the SATA lines do not pass the NVMe check.
    assert!(check_first(&sata, false).is_err());
}

#[test]
fn the_second_boot_must_find_what_the_first_wrote() {
    let second = lines(&[
        "  1.1 |   root     / is btrfs on vde2, as the last boot left it; committed every 30 s",
        "  2.0 | n150: found n150-kept-by-the-first-boot",
    ]);
    check_second(&second).unwrap();
    let fresh = lines(&[
        "  1.1 |   root     / is btrfs on vde2; the system was installed on it (4096 KiB)",
        "  2.0 | n150: nothing kept yet",
    ]);
    let error = check_second(&fresh).unwrap_err().to_string();
    assert!(error.contains("the root the first left"), "{error}");
}

const DESKTOP: [&str; 11] = [
    "  nvme     0000:00:04.0 NVMe 1.4 controller up",
    "  root     / is btrfs on vdd2; the system was installed on it",
    "  display  firmware framebuffer 1024x768 lent to a driver",
    "efifb: 1024x768 firmware framebuffer",
    "hyprix: card0 Virtual-1 1024x768 1024x768 /tmp/wayland-1",
    "hyprix: seat 2 devices [event0 QEMU QEMU USB Keyboard, event1 QEMU QEMU USB Mouse], 5 binds",
    "n150-desktop: the launcher's video host gl-utils-public.hg-cdn.com resolves nowhere",
    "n150-desktop: gryphlink is on /data",
    "n150-desktop: a lease for eth0",
    "n150-desktop: yserver is up on :0 after 2s",
    "n150-desktop: ready",
];

#[test]
fn the_desktop_boot_passes_with_every_line_and_fails_naming_the_missing_one() {
    check_desktop(&lines(&DESKTOP)).unwrap();
    for (missing, said) in [
        (2, "did not lend the firmware framebuffer"),
        (5, "no USB keyboard"),
        (6, "video host is not blocked"),
        (8, "got no lease"),
        (9, "yserver did not start"),
    ] {
        let mut without = DESKTOP.to_vec();
        let _ = without.remove(missing);
        let error = check_desktop(&lines(&without)).unwrap_err().to_string();
        assert!(error.contains(said), "{error}");
    }
    let mut unblocked = DESKTOP.to_vec();
    unblocked[6] = "n150-desktop: /etc/hosts does not block gl-utils-public.hg-cdn.com";
    assert!(check_desktop(&lines(&unblocked)).is_err());
}

#[test]
fn a_cleared_screen_has_one_colour_and_a_desktop_many() {
    assert_eq!(distinct_colours(&[0; 3 * 64]), 1);
    let ramp: Vec<u8> = (0..=255_u8).flat_map(|v| [v, v, 0]).collect();
    assert_eq!(distinct_colours(&ramp), 256);
}
