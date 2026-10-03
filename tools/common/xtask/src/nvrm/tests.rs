use std::collections::BTreeMap;

use ferrix_native_abi::nr;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types;

use super::{TOO_SMALL, UNISOLATED, judge_handed, judge_refused, place_after, report};

// Recorded 2026-10-03 with nvrm loading its core from the volume
// (x86-64, KVM, the patched ferrix-cfi QEMU): devmgr's lines, nvrm's and
// nvos's, and the kernel's report, as `run` returns them. nvrm waits for the
// volume, which mounts after devmgr's report, so the report comes before the
// core is loaded.
const HANDED_KVM: &str = "\
devmgr   gpu 00:04.0: marked for isolated interrupts, device_isolation 0x2 (interrupts isolated); handing it to nvrm\n\
devmgr   gpu 00:04.0: pin budget 488 MiB, cut from 1024 MiB: the 1 GiB floor yields to the kernel's ceiling (3904 MiB of RAM)\n\
nvrm: started on 00:04.0, 1b36:0005 class 00ff00, 2 apertures, 0 vectors; its device over bootstrap\n\
nvrm: device_isolation 0x3: interrupts isolated (bit 1), DMA translated\n\
nvrm: pin budget 124928 pages (488 MiB), isolated-interrupts mark 1\n\
nvrm: waiting up to 60 s for the NVIDIA volume (/data/usr/lib/ferrix/nvrm-core)\n\
  devmgr   14 devices, 12 drivers, 5 started, 0 failed\n\
nvos: core loaded: sha256 be83e087cb7787b7..., 13445824 bytes, base 0x40000000, 17 exports, build-id cfe4ba34e3a74eba6216b6bb82cfa0bd754aba81\n\
nvrm: aperture 0: BAR 0, 0x81415000, 4 KiB\n\
nvrm: aperture 1: BAR 2, 0xc000000000, 8388608 KiB, 64-bit, prefetchable\n\
nvrm: configuration window: vendor 1b36 device 0005\n\
nvrm: BAR0 mapped, 4 KiB; its register 0x0 reads 0x00000000 (the test device: no NV_PMC_BOOT_0)\n\
nvrm: a thread ran and was joined\n\
nvrm: skeleton up on 00:04.0; idle\n\
";

// The same boot under TCG.
const HANDED_TCG: &str = "\
devmgr   gpu 00:04.0: marked for isolated interrupts, device_isolation 0x2 (interrupts isolated); handing it to nvrm\n\
devmgr   gpu 00:04.0: pin budget 488 MiB, cut from 1024 MiB: the 1 GiB floor yields to the kernel's ceiling (3904 MiB of RAM)\n\
nvrm: started on 00:04.0, 1b36:0005 class 00ff00, 2 apertures, 0 vectors; its device over bootstrap\n\
nvrm: device_isolation 0x3: interrupts isolated (bit 1), DMA translated\n\
nvrm: pin budget 124928 pages (488 MiB), isolated-interrupts mark 1\n\
nvrm: waiting up to 60 s for the NVIDIA volume (/data/usr/lib/ferrix/nvrm-core)\n\
  devmgr   14 devices, 12 drivers, 5 started, 0 failed\n\
nvos: core loaded: sha256 be83e087cb7787b7..., 13445824 bytes, base 0x40000000, 17 exports, build-id cfe4ba34e3a74eba6216b6bb82cfa0bd754aba81\n\
nvrm: aperture 0: BAR 0, 0x81415000, 4 KiB\n\
nvrm: aperture 1: BAR 2, 0xc000000000, 8388608 KiB, 64-bit, prefetchable\n\
nvrm: configuration window: vendor 1b36 device 0005\n\
nvrm: BAR0 mapped, 4 KiB; its register 0x0 reads 0x00000000 (the test device: no NV_PMC_BOOT_0)\n\
nvrm: a thread ran and was joined\n\
nvrm: skeleton up on 00:04.0; idle\n\
";

// The default 512 MiB machine, KVM.
const TOO_SMALL_KVM: &str = "\
  7.65 | devmgr   gpu 00:04.0 not started: its pin budget of 42 MiB is under the 256 MiB nvrm needs\n\
  7.66 |   devmgr   13 devices, 12 drivers, 3 started, 1 failed\n\
  8.34 |   devmgr   bind of blk and the device at pci 0000:00:05.0, asked through sysfs: EBUSY\n\
";

// The VT-d unit with intremap=off, KVM.
const UNISOLATED_KVM: &str = "\
  6.33 |   iommu    vt-d unit 0xfed90000: translating, queue on, remapping off\n\
  8.04 | devmgr   gpu 00:04.0 not started: its interrupts are not isolated (device_isolation 0x0)\n\
  8.05 |   devmgr   13 devices, 12 drivers, 3 started, 1 failed\n\
";

fn lines(transcript: &str) -> Vec<String> {
    transcript.lines().map(str::to_owned).collect()
}

#[test]
fn the_recorded_hand_overs_pass() {
    assert_eq!(judge_handed(&lines(HANDED_KVM)), None);
    assert_eq!(judge_handed(&lines(HANDED_TCG)), None);
}

#[test]
fn the_recorded_refusals_pass() {
    assert_eq!(judge_refused(&lines(TOO_SMALL_KVM), TOO_SMALL), None);
    assert_eq!(judge_refused(&lines(UNISOLATED_KVM), UNISOLATED), None);
}

#[test]
fn a_hand_over_is_not_a_refusal_nor_the_other_way() {
    assert!(judge_refused(&lines(HANDED_KVM), UNISOLATED).is_some());
    assert!(judge_refused(&lines(HANDED_KVM), TOO_SMALL).is_some());
    assert!(judge_handed(&lines(UNISOLATED_KVM)).is_some());
    assert!(judge_handed(&lines(TOO_SMALL_KVM)).is_some());
    assert!(judge_refused(&lines(TOO_SMALL_KVM), UNISOLATED).is_some());
}

#[test]
fn nvrm_running_after_a_refusal_fails_it() {
    // devmgr refusing, and nvrm started anyway -- refusing the device
    // itself, as its second guard does -- is a failed hand-over check.
    let run = format!(
        "{UNISOLATED_KVM}  8.06 | nvrm: device_isolation 0: interrupts NOT isolated; refusing the device\n"
    );
    let why = judge_refused(&lines(&run), UNISOLATED).unwrap();
    assert!(why.contains("nvrm ran although devmgr refused"), "{why}");
    let run = UNISOLATED_KVM.replace("3 started, 1 failed", "4 started, 0 failed");
    assert!(judge_refused(&lines(&run), UNISOLATED).is_some());
}

#[test]
fn a_hand_over_without_bit_one_or_its_budget_fails() {
    let run = HANDED_KVM.replace(
        "interrupts isolated (bit 1)",
        "interrupts NOT isolated; refusing the device",
    );
    assert!(judge_handed(&lines(&run)).is_some());
    let run = HANDED_KVM.replace("124928 pages (488 MiB)", "66560 pages (260 MiB)");
    let why = judge_handed(&lines(&run)).unwrap();
    assert!(why.contains("not the 488 MiB devmgr set"), "{why}");
    let run = HANDED_KVM.replace("pin budget 488 MiB, cut", "pin budget 128 MiB, cut");
    assert!(judge_handed(&lines(&run)).is_some());
    let run = HANDED_KVM.replace("device_isolation 0x2 (", "device_isolation 0x1 (");
    assert!(judge_handed(&lines(&run)).is_some());
}

#[test]
fn a_hand_over_missing_a_step_or_out_of_order_fails() {
    let run = HANDED_KVM.replace("nvrm: a thread ran and was joined\n", "");
    let why = judge_handed(&lines(&run)).unwrap();
    assert!(why.contains("a thread ran"), "{why}");
    let run = HANDED_KVM.replace("started on 00:04.0", "started on 00:05.0");
    assert!(judge_handed(&lines(&run)).is_some());
    let run = format!("{HANDED_KVM}nvrm: stopped: io_mapping_map refused BAR0 (status -13)\n");
    assert!(judge_handed(&lines(&run)).is_some());
}

#[test]
fn a_core_refused_or_not_at_its_base_fails_the_hand_over() {
    let run = format!("{HANDED_KVM}nvos: core refused: the core's sha256 is not nvrm's pin\n");
    let why = judge_handed(&lines(&run)).unwrap();
    assert!(why.contains("core refused"), "{why}");
    let loaded = lines(HANDED_KVM)
        .into_iter()
        .find(|line| line.starts_with("nvos: core loaded: "))
        .unwrap();
    let run = HANDED_KVM.replace(&format!("{loaded}\n"), "");
    let why = judge_handed(&lines(&run)).unwrap();
    assert!(why.contains("core loaded"), "{why}");
    let run = HANDED_KVM.replace("base 0x40000000", "base 0x50000000");
    let why = judge_handed(&lines(&run)).unwrap();
    assert!(why.contains("not at its base"), "{why}");
}

#[test]
fn the_report_and_places_are_read() {
    assert_eq!(report(&lines(HANDED_KVM)), Some((5, 0)));
    assert_eq!(report(&lines(UNISOLATED_KVM)), Some((3, 1)));
    assert_eq!(
        place_after("devmgr   gpu 01:00.0: pin", "devmgr   gpu "),
        Some("01:00.0")
    );
    assert_eq!(
        place_after("nvrm: started on 00:04.0, 1b36", "started on "),
        Some("00:04.0")
    );
    assert_eq!(
        place_after("nvrm: skeleton up on 00:04.0; idle", "up on "),
        Some("00:04.0")
    );
}

/// `nvrm`'s C header, which names the native calls by number.
const NATIVE_H: &str =
    include_str!("../../../../../src/user/system/linux/drivers/nvrm/src/native.h");

/// Every `#define NV_<NAME> <number>` in the header.
fn defines() -> BTreeMap<&'static str, u64> {
    NATIVE_H
        .lines()
        .filter_map(|line| {
            let mut words = line.strip_prefix("#define NV_")?.split_whitespace();
            let name = words.next()?;
            let value = words.next()?;
            let value = match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok()?,
                None => value.parse().ok()?,
            };
            Some((name, value))
        })
        .collect()
}

#[test]
fn nvrms_header_names_the_abis_numbers() {
    let defines = defines();
    let expected: [(&str, u64); 21] = [
        ("HANDLE_CLOSE", nr::HANDLE_CLOSE as u64),
        ("OBJECT_WAIT_ONE", nr::OBJECT_WAIT_ONE as u64),
        ("CHANNEL_READ", nr::CHANNEL_READ as u64),
        ("IO_MAPPING_CREATE", nr::IO_MAPPING_CREATE as u64),
        ("IO_MAPPING_MAP", nr::IO_MAPPING_MAP as u64),
        ("DEVICE_INFO", nr::DEVICE_INFO as u64),
        ("DEVICE_APERTURE", nr::DEVICE_APERTURE as u64),
        ("DEVICE_CONFIG_READ", nr::DEVICE_CONFIG_READ as u64),
        ("DEVICE_GET_LIMIT", nr::DEVICE_GET_LIMIT as u64),
        ("DEVICE_ISOLATION", nr::DEVICE_ISOLATION as u64),
        ("SIGNAL_READABLE", u64::from(Signals::READABLE.0)),
        ("SIGNAL_PEER_CLOSED", u64::from(Signals::PEER_CLOSED.0)),
        ("CHANNEL_MAX_HANDLES", types::CHANNEL_MAX_HANDLES as u64),
        ("DEVICE_LIMIT_PIN_PAGES", types::DEVICE_LIMIT_PIN_PAGES),
        ("DEVICE_LIMIT_PIN_CEILING", types::DEVICE_LIMIT_PIN_CEILING),
        ("DEVICE_LIMIT_PIN_ROOM", types::DEVICE_LIMIT_PIN_ROOM),
        (
            "DEVICE_LIMIT_ISOLATED_INTERRUPTS",
            types::DEVICE_LIMIT_ISOLATED_INTERRUPTS,
        ),
        (
            "DEVICE_ISOLATION_DMA_TRANSLATED",
            types::DEVICE_ISOLATION_DMA_TRANSLATED,
        ),
        (
            "DEVICE_ISOLATION_INTERRUPTS",
            types::DEVICE_ISOLATION_INTERRUPTS,
        ),
        (
            "APERTURE_PREFETCHABLE",
            u64::from(types::APERTURE_PREFETCHABLE),
        ),
        (
            "APERTURE_WHOLE_PAGES",
            u64::from(types::APERTURE_WHOLE_PAGES),
        ),
    ];
    for (name, value) in expected {
        assert_eq!(defines.get(name), Some(&value), "NV_{name}");
    }
    assert_eq!(
        defines.get("APERTURE_BAR_64"),
        Some(&u64::from(types::APERTURE_BAR_64))
    );
    assert_eq!(
        defines.len(),
        expected.len() + 1,
        "a number in native.h is not checked here"
    );
    assert!(NATIVE_H.contains("== 96, \"DEVICE_INFO_BYTES\""));
    assert_eq!(types::DEVICE_INFO_BYTES, 96);
    assert!(NATIVE_H.contains("== 32, \"APERTURE_INFO_BYTES\""));
    assert_eq!(types::APERTURE_INFO_BYTES, 32);
}
