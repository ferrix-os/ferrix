//! The SMBIOS identity every boot gives the machine, and what a test boot
//! requires the kernel to say of it (`docs/DMI.md` §4).
//!
//! QEMU builds the tables from `-smbios` and firmware installs them; the
//! loader copies them and the kernel shows them in sysfs. A test boot
//! requires the kernel's `smbios` line to name exactly the vendor and the
//! product given here, and requires the serial numbers and the UUID, which
//! are markers no other line could hold by chance, to appear nowhere in the
//! serial log: they identify a machine, and only root reads them, in sysfs.

use crate::paths::Arch;

/// The system's manufacturer, as `sys_vendor` shows it: with a byte past
/// ASCII (UTF-8 `é`), which the kernel's boot line must escape.
pub(crate) const VENDOR: &str = "Ferrix QEMU \u{e9}";

/// [`VENDOR`] as the kernel's boot line shows it, escaped.
const VENDOR_SHOWN: &str = "Ferrix QEMU \\xc3\\xa9";

/// The system's product, as `product_name` shows it.
pub(crate) const PRODUCT: &str = "Ferrix test machine";

/// The system's serial: a marker the log must never hold.
const SYSTEM_SERIAL: &str = "FXSER-SYS-71c3e9";

/// The system's UUID: a marker the log must never hold, as written here and
/// as the kernel would format it (SMBIOS 2.6 and later: the first three
/// fields byte-swapped by QEMU into the table and back by the reader, so the
/// same text).
const UUID: &str = "5f0d9a7e-3b21-4c8e-9d4a-0b6e2f8c1d33";

/// The board's serial: a marker.
const BOARD_SERIAL: &str = "FXSER-BRD-2a90d4";

/// The chassis's serial: a marker.
const CHASSIS_SERIAL: &str = "FXSER-CHS-e5b017";

/// Every marker the log must not hold.
const MARKERS: [&str; 4] = [SYSTEM_SERIAL, UUID, BOARD_SERIAL, CHASSIS_SERIAL];

/// QEMU's arguments for them.
pub(crate) fn arguments() -> [String; 6] {
    [
        "-smbios".to_owned(),
        format!(
            "type=1,manufacturer={VENDOR},product={PRODUCT},serial={SYSTEM_SERIAL},uuid={UUID}"
        ),
        "-smbios".to_owned(),
        format!("type=2,serial={BOARD_SERIAL}"),
        "-smbios".to_owned(),
        format!("type=3,serial={CHASSIS_SERIAL}"),
    ]
}

/// What the kernel's line begins with when it accepted a table.
const ACCEPTED: &str = "  smbios   ";

/// Why a boot's log does not show what it should of the SMBIOS tables, if
/// it does not.
///
/// Pinned from the gated boots of 2026-10-15 (d1-boot-kvm-1, d1-boot-a64-1,
/// d1-boot-a32-1; `docs/DMI.md` §3.4): on every architecture the firmware
/// xtask boots installs QEMU's table with QEMU's strings -- OVMF its 2.8
/// 32-bit entry point unless `FERRIX_X86_MACHINE` asks for another, AAVMF and
/// U-Boot a 3.0 64-bit one -- so the loader must say it copied that kind,
/// and the kernel's line must name exactly [`VENDOR`] (escaped) and
/// [`PRODUCT`]. No line, the loader's included, may hold a serial or the
/// UUID.
///
/// Verifies: H.BOOT.16
pub(crate) fn problem(arch: Arch, lines: &[String]) -> Option<String> {
    if let Some(line) = lines.iter().find(|line| holds_marker(line)) {
        return Some(format!(
            "a serial number or the UUID of the machine's SMBIOS table is in the log: `{}`",
            line.trim()
        ));
    }
    let Some(line) = lines.iter().find(|line| {
        line.contains(ACCEPTED) && line.contains(" structures in ") && line.contains(" bytes: ")
    }) else {
        let said = lines
            .iter()
            .find(|line| line.contains(ACCEPTED))
            .map_or("nothing", |line| line.trim());
        return Some(format!(
            "the kernel did not accept the firmware's SMBIOS table: `{said}`"
        ));
    };
    let kind = entry_point_kind(arch, std::env::var("FERRIX_X86_MACHINE").ok().as_deref());
    if !lines
        .iter()
        .any(|line| line.contains(LOADER_COPIED) && line.contains(kind))
    {
        return Some(format!(
            "the loader never said it copied a {kind} SMBIOS entry point's tables, so its \
             part of the log was not seen as this firmware writes it"
        ));
    }
    if !line
        .trim_end()
        .ends_with(&format!(": {VENDOR_SHOWN} {PRODUCT}"))
    {
        return Some(format!(
            "the kernel's SMBIOS line does not name `{VENDOR_SHOWN}` and `{PRODUCT}`: `{}`",
            line.trim()
        ));
    }
    None
}

/// The entry point the firmware xtask boots on `arch` installs, as the
/// loader's line names it: `(32-bit)` or `(64-bit)`. On x86-64 QEMU's
/// `smbios-entry-point-type` in `FERRIX_X86_MACHINE` decides; OVMF's
/// default under `auto` was the 32-bit one.
fn entry_point_kind(arch: Arch, x86_machine: Option<&str>) -> &'static str {
    match arch {
        Arch::X86_64
            if x86_machine.is_some_and(|extra| extra.contains("smbios-entry-point-type=64")) =>
        {
            "(64-bit)"
        }
        Arch::X86_64 => "(32-bit)",
        Arch::AArch64 | Arch::Armv7a => "(64-bit)",
    }
}

/// What the loader's line holds when it copied the tables.
const LOADER_COPIED: &str = " bytes copied";

/// Whether `line` holds a marker: as written, and the UUID also in upper
/// case and without its hyphens.
fn holds_marker(line: &str) -> bool {
    let flat = line.to_ascii_lowercase().replace('-', "");
    MARKERS.iter().any(|marker| {
        line.contains(marker) || flat.contains(&marker.to_ascii_lowercase().replace('-', ""))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn the_named_table_passes() {
        let boot = lines(&[
            "  smbios   3.0 entry point (64-bit), table of 400 bytes copied",
            "  smbios   3.0, 9 structures in 380 bytes: Ferrix QEMU \\xc3\\xa9 Ferrix test machine",
        ]);
        assert_eq!(problem(Arch::AArch64, &boot), None);
        assert_eq!(problem(Arch::Armv7a, &boot), None);
        let x86 = lines(&[
            "  smbios   2.8 entry point (32-bit), table of 508 bytes copied",
            "  smbios   2.8, 10 structures in 508 bytes: Ferrix QEMU \\xc3\\xa9 Ferrix test machine",
        ]);
        assert_eq!(entry_point_kind(Arch::X86_64, None), "(32-bit)");
        assert_eq!(
            entry_point_kind(Arch::X86_64, Some("smbios-entry-point-type=64")),
            "(64-bit)"
        );
        if std::env::var_os("FERRIX_X86_MACHINE").is_none() {
            assert_eq!(problem(Arch::X86_64, &x86), None);
            assert!(
                problem(Arch::X86_64, &boot).is_some(),
                "64-bit on x86 by default"
            );
        }
    }

    #[test]
    fn another_name_fails() {
        let boot = lines(&[
            "  smbios   3.0 entry point (64-bit), table of 400 bytes copied",
            "  smbios   3.0, 9 structures in 380 bytes: QEMU Standard PC",
        ]);
        assert!(problem(Arch::AArch64, &boot).is_some());
        assert!(problem(Arch::Armv7a, &boot).is_some());
    }

    #[test]
    fn a_refusal_or_silence_fails() {
        let refused = lines(&["  smbios   refused: bad checksum"]);
        assert!(problem(Arch::X86_64, &refused).is_some());
        let none = lines(&["  smbios   none handed over"]);
        assert!(problem(Arch::AArch64, &none).is_some());
        assert!(problem(Arch::X86_64, &[]).is_some());
    }

    #[test]
    fn a_marker_anywhere_fails() {
        for marker in MARKERS {
            let boot = lines(&[
                "  smbios   3.0 entry point (64-bit), table of 400 bytes copied",
                "  smbios   3.0, 9 structures in 380 bytes: Ferrix QEMU \\xc3\\xa9 Ferrix test machine",
                &format!("FERRIX-BOOT-OK {marker}"),
            ]);
            assert!(problem(Arch::AArch64, &boot).is_some(), "{marker}");
        }
    }

    #[test]
    fn an_unescaped_vendor_or_no_loader_line_fails() {
        let raw = lines(&[
            "  smbios   3.0 entry point (64-bit), table of 400 bytes copied",
            "  smbios   3.0, 9 structures in 380 bytes: Ferrix QEMU \u{e9} Ferrix test machine",
        ]);
        assert!(problem(Arch::AArch64, &raw).is_some());
        let no_loader = lines(&[
            "  smbios   3.0, 9 structures in 380 bytes: Ferrix QEMU \\xc3\\xa9 Ferrix test machine",
        ]);
        assert!(problem(Arch::AArch64, &no_loader).is_some());
    }

    #[test]
    fn the_uuid_in_upper_case_or_without_hyphens_is_a_marker() {
        assert!(holds_marker(&UUID.to_ascii_uppercase()));
        assert!(holds_marker(&UUID.replace('-', "")));
        assert!(!holds_marker("5f0d9a7e"));
    }

    #[test]
    fn the_arguments_carry_every_marker() {
        let joined = arguments().join(" ");
        for marker in MARKERS {
            assert!(joined.contains(marker), "{marker}");
        }
    }
}
