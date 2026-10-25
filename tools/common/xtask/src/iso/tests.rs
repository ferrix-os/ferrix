use std::io::Cursor;

use ferrix_partition::{ESP, parse_entries, parse_header};

use super::{BLOCK, CATALOG, Extra, lay_out};
use crate::fat::Fat32;

/// A small FAT32 volume holding one file, as the ESP.
fn volume() -> Vec<u8> {
    let mut fat = Fat32::new(40 << 20).expect("a volume");
    fat.add_file("EFI/BOOT/BOOTX64.EFI", b"a loader")
        .expect("a file");
    fat.finish()
}

fn iso() -> (Vec<u8>, super::Layout) {
    let esp = volume();
    let extras = [Extra {
        name: "README.TXT".to_owned(),
        data: b"Ferrix\n".to_vec(),
    }];
    let mut out = Cursor::new(Vec::new());
    let layout = lay_out(
        &mut Cursor::new(esp.clone()),
        esp.len() as u64,
        &extras,
        "FERRIX_LIVE",
        &mut out,
    )
    .expect("an ISO");
    (out.into_inner(), layout)
}

#[test]
fn the_gpt_names_the_esp_where_el_torito_does() {
    let (bytes, layout) = iso();
    assert_eq!(bytes.len() as u64, layout.bytes);
    assert!(layout.bytes.is_multiple_of(1 << 20));
    let sectors = layout.bytes / 512;
    let header = parse_header(&bytes[512..1024], sectors).expect("a GPT header");
    let partitions = parse_entries(&header, &bytes[1024..1024 + 128 * 128]).expect("entries");
    assert_eq!(partitions.len(), 1);
    let (_, esp) = partitions[0];
    assert_eq!(esp.type_guid, ESP);
    assert_eq!(esp.first, layout.esp_block * 4);
    assert_eq!((esp.last - esp.first + 1) * 512, layout.esp_bytes);
    // The protective MBR.
    assert_eq!(bytes[450], 0xEE);
    assert_eq!(&bytes[510..512], &[0x55, 0xAA]);
    // El Torito's default entry is the same extent.
    let catalog = &bytes[(CATALOG * BLOCK) as usize..];
    assert_eq!(catalog[1], 0xEF);
    assert_eq!(catalog[32], 0x88);
    let lba = u32::from_le_bytes(catalog[40..44].try_into().expect("four bytes"));
    assert_eq!(u64::from(lba), layout.esp_block);
    // The ESP's boot sector knows where it starts.
    let boot = (layout.esp_block * BLOCK) as usize;
    let hidden = u32::from_le_bytes(bytes[boot + 28..boot + 32].try_into().expect("four"));
    assert_eq!(u64::from(hidden), esp.first);
    assert_eq!(&bytes[16 * 2048 + 1..16 * 2048 + 6], b"CD001");
}

#[test]
fn a_name_that_is_not_level_2_is_refused() {
    let esp = volume();
    let extras = [Extra {
        name: "third-party.md".to_owned(),
        data: Vec::new(),
    }];
    let refused = lay_out(
        &mut Cursor::new(esp.clone()),
        esp.len() as u64,
        &extras,
        "FERRIX_LIVE",
        &mut Cursor::new(Vec::new()),
    );
    assert!(refused.is_err());
}

/// Where this machine has `xorriso`, it reads the ISO as an ISO: the boot
/// catalog's EFI image and the file in the root.
#[test]
fn xorriso_reads_it() {
    let Ok(found) = std::process::Command::new("xorriso")
        .arg("-version")
        .output()
    else {
        eprintln!("no xorriso here; skipped");
        return;
    };
    if !found.status.success() {
        return;
    }
    let (bytes, layout) = iso();
    let dir = std::env::temp_dir().join(format!("ferrix-iso-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    let path = dir.join("t.iso");
    std::fs::write(&path, &bytes).expect("written");
    let report = std::process::Command::new("xorriso")
        .args(["-indev"])
        .arg(&path)
        .args(["-report_el_torito", "plain", "-ls", "/"])
        .output()
        .expect("xorriso ran");
    let text = String::from_utf8_lossy(&report.stdout).into_owned()
        + &String::from_utf8_lossy(&report.stderr);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(text.contains("README.TXT"), "{text}");
    assert!(text.contains("EFIBOOT.IMG"), "{text}");
    assert!(
        text.lines()
            .any(|line| line.contains("UEFI") && line.contains(&layout.esp_block.to_string())),
        "{text}"
    );
}
