use super::{CASES, PASSED, changed_cores, judge, loads, script};

/// A core's headers as nvrm-core.ld lays them out: the ELF header, three
/// `PT_LOAD`s and `PT_GNU_STACK`, then the read-only segment's bytes at 0x1000.
fn core() -> Vec<u8> {
    let mut bytes = vec![0_u8; 0x2000];
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[56..58].copy_from_slice(&4_u16.to_le_bytes());
    for (index, (kind, flags, offset)) in [
        (1_u32, 4_u32, 0x1000_u64),
        (1, 5, 0x1800),
        (1, 6, 0x1c00),
        (0x6474_e551, 6, 0),
    ]
    .into_iter()
    .enumerate()
    {
        let at = 64 + index * 56;
        bytes[at..at + 4].copy_from_slice(&kind.to_le_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
        bytes[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
    }
    bytes[0x1000] = 0x4e;
    bytes
}

#[test]
fn every_change_is_one_change() {
    let original = core();
    let changed = changed_cores(&original).expect("a core with three segments");
    let names: Vec<&str> = changed.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        [
            "flipped-core",
            "past-2gib-core",
            "rwx-segment-core",
            "bad-magic-core"
        ]
    );
    let loads = loads(&original).expect("three");
    let [_, text, data] = loads[..] else {
        panic!("three")
    };
    let differ = |bytes: &[u8]| -> Vec<usize> {
        bytes
            .iter()
            .zip(&original)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(at, _)| at)
            .collect()
    };
    assert_eq!(differ(&changed[0].1).len(), 1);
    assert!(
        differ(&changed[1].1)
            .iter()
            .all(|&at| (data + 16..data + 32).contains(&at))
    );
    assert_eq!(differ(&changed[2].1), [text + 4]);
    assert_eq!(differ(&changed[3].1), [0x1000]);
}

#[test]
fn a_core_without_three_segments_is_refused() {
    let mut bytes = core();
    bytes[56..58].copy_from_slice(&2_u16.to_le_bytes());
    assert!(loads(&bytes).is_err());
}

#[test]
fn a_case_is_judged_by_its_status_and_its_line() {
    let good = &CASES[0];
    assert_eq!(judge(good, Some(0), PASSED), None);
    assert!(judge(good, Some(0), "nvrm-link-test: RM initialised").is_some());
    assert!(judge(good, Some(1), PASSED).is_some());
    assert!(judge(good, Some(0), &format!("nvos: core refused: x\n{PASSED}")).is_some());
    let flipped = &CASES[1];
    assert_eq!(judge(flipped, Some(25), flipped.says), None);
    assert!(judge(flipped, Some(35), flipped.says).is_some());
    assert!(judge(flipped, None, flipped.says).is_some());
}

#[test]
fn the_script_runs_every_case_and_says_its_status() {
    let script = script();
    for case in CASES {
        assert!(
            script.contains(&format!("/data/{} ", case.program)),
            "{}",
            case.name
        );
        assert!(
            script.contains(&format!("nvrm-link {}: exit $status", case.name)),
            "{}",
            case.name
        );
    }
    assert!(script.contains("/data/nvrm-link-test --control-rwx"));
    assert!(script.ends_with("exit 17\n"));
}
