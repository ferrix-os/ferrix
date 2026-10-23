//! `devmgr`'s pin budgets (`docs/NVIDIA.md` §12.2, check P6): the GPU's four
//! outcomes and their lines, and that no launch path hands a driver the
//! right to set one.

extern crate std;

use std::format;
use std::string::String;
use std::vec::Vec;

use ferrix_native_abi::rights::Rights;

use crate::budget::{GIB_PAGES, GpuBudget, NVRM_MIN_PIN_PAGES, PAGE_BYTES};

/// Pages in `mib` MiB.
const fn mib(mib: u64) -> u64 {
    mib * 1024 * 1024 / PAGE_BYTES
}

/// The ceiling on a machine of `ram_gib` GiB: a quarter of it.
const fn ceiling(ram_gib: u64) -> u64 {
    ram_gib * GIB_PAGES / 4
}

/// The PCI address word of 01:00.0.
const CARD: u32 = 0x0000_0100;

#[test]
fn sixteen_gib_gives_an_eighth_of_ram() {
    let budget = GpuBudget::plan(ceiling(16), ceiling(16));
    assert_eq!(budget, GpuBudget::Whole { pages: mib(2048) });
    assert_eq!(budget.to_set(), Some(mib(2048)));
    assert_eq!(
        format!("{}", budget.line(CARD)),
        "devmgr   gpu 01:00.0: pin budget 2048 MiB (an eighth of RAM, at least 1 GiB)"
    );
}

#[test]
fn eight_gib_gives_the_floor_whole() {
    let budget = GpuBudget::plan(ceiling(8), ceiling(8));
    assert_eq!(budget, GpuBudget::Whole { pages: GIB_PAGES });
    assert_eq!(
        format!("{}", budget.line(CARD)),
        "devmgr   gpu 01:00.0: pin budget 1024 MiB (an eighth of RAM, at least 1 GiB)"
    );
}

#[test]
fn four_gib_cuts_the_floor_to_the_ceiling() {
    let budget = GpuBudget::plan(ceiling(4), ceiling(4));
    assert_eq!(
        budget,
        GpuBudget::Cut {
            pages: mib(512),
            wanted: GIB_PAGES,
            ram: 4 * GIB_PAGES,
        },
        "the floor yields to the ceiling"
    );
    assert_eq!(budget.to_set(), Some(mib(512)));
    assert_eq!(
        format!("{}", budget.line(CARD)),
        "devmgr   gpu 01:00.0: pin budget 512 MiB, cut from 1024 MiB: the 1 GiB floor \
         yields to the kernel's ceiling (4096 MiB of RAM)"
    );
}

#[test]
fn one_gib_is_too_small_to_start() {
    let budget = GpuBudget::plan(ceiling(1), ceiling(1));
    assert_eq!(budget, GpuBudget::TooSmall { pages: mib(128) });
    assert_eq!(budget.to_set(), None, "not started");
    assert_eq!(
        format!("{}", budget.line(CARD)),
        "devmgr   gpu 01:00.0 not started: its pin budget of 128 MiB is under the 256 MiB \
         nvrm needs"
    );
}

#[test]
fn a_ceiling_partly_taken_cuts_what_is_left() {
    // 16 GiB, of whose 4 GiB ceiling another raised device holds 3584 MiB.
    let room = ceiling(16) - mib(3584);
    let budget = GpuBudget::plan(ceiling(16), room);
    assert_eq!(
        budget,
        GpuBudget::Cut {
            pages: mib(256),
            wanted: mib(2048),
            ram: 16 * GIB_PAGES,
        }
    );
    // And one more MiB taken leaves less than nvrm needs.
    let budget = GpuBudget::plan(ceiling(16), room - mib(2));
    assert_eq!(budget, GpuBudget::TooSmall { pages: mib(255) });
    assert!(mib(255) < NVRM_MIN_PIN_PAGES);
    assert_eq!(
        format!("{}", budget.line(CARD)),
        "devmgr   gpu 01:00.0 not started: its pin budget of 255 MiB is under the 256 MiB \
         nvrm needs"
    );
}

#[test]
fn a_budget_the_kernel_refuses_does_not_start_the_gpu() {
    let budget = GpuBudget::plan(ceiling(4), ceiling(4)).refused();
    assert_eq!(budget, GpuBudget::Refused { pages: mib(512) });
    assert_eq!(budget.to_set(), None, "no silent fallback to the default");
    assert_eq!(
        format!("{}", budget.line(0x0001_0100)),
        "devmgr   gpu 0001:01:00.0 not started: the kernel refused its pin budget of 512 MiB \
         (past the ceiling)"
    );
}

/// devmgr's source, for the launch paths' rights.
const DEVMGR: &str = include_str!("../../../../user/system/native/devmgr/src/main.rs");

/// The rights a `Requested::Exactly` in devmgr may name: a driver's device
/// and its control channel, for either ring; and the Intel GPU keeper's job
/// and its worker's image (`docs/INTEL-GPU.md` §12.3), neither with
/// `SET_LIMIT` (checked below).
const NARROWED: [&str; 6] = [
    "Requested::Exactly(DEVICE_RIGHTS)",
    "Requested::Exactly(NET_DEVICE_RIGHTS)",
    "Requested::Exactly(CONTROL_RIGHTS)",
    "Requested::Exactly(NET_CONTROL_RIGHTS)",
    "Requested::Exactly(KEEPER_JOB_RIGHTS)",
    "Requested::Exactly(WORKER_IMAGE_RIGHTS)",
];

/// Each `fn` in `source`, by name, with its body up to the next `fn`.
fn functions(source: &str) -> Vec<(String, &str)> {
    let mut out = Vec::new();
    let starts: Vec<usize> = source
        .match_indices("\nfn ")
        .map(|(at, _)| at + 1)
        .collect();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(source.len());
        let body = source.get(start..end).unwrap_or_default();
        let name: String = body
            .get(3..)
            .unwrap_or_default()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        out.push((name, body));
    }
    out
}

/// P6: the right to set a budget never reaches a driver. The kernel hands
/// devmgr each device with `SET_LIMIT`; every path that starts a driver
/// narrows the device handle to exactly a driver's rights, which hold no
/// `SET_LIMIT`, and nothing in devmgr asks for the same rights it holds.
#[test]
fn every_launch_path_narrows_the_device_to_a_drivers_rights() {
    assert!(!ferrix_blkring::control::DEVICE_RIGHTS.contains(Rights::SET_LIMIT));
    assert!(!ferrix_netring::control::DEVICE_RIGHTS.contains(Rights::SET_LIMIT));
    assert!(
        !DEVMGR.contains("SAME_RIGHTS") && !DEVMGR.contains("Requested::Same"),
        "a handle is passed on with the rights devmgr holds"
    );
    for (at, _) in DEVMGR.match_indices("Requested::") {
        let rest = DEVMGR.get(at..).unwrap_or_default();
        assert!(
            NARROWED.iter().any(|narrowed| rest.starts_with(narrowed)),
            "a handle is narrowed to something else: {}",
            rest.get(..rest.find(')').map_or(rest.len(), |end| end + 1))
                .unwrap_or_default()
        );
    }
    for keeper in ["const KEEPER_JOB_RIGHTS", "const WORKER_IMAGE_RIGHTS"] {
        let at = DEVMGR
            .find(keeper)
            .expect("the keeper's rights are defined");
        let line = DEVMGR.get(at..).unwrap_or_default();
        let line = line
            .get(..line.find(';').unwrap_or(line.len()))
            .unwrap_or_default();
        assert!(!line.contains("SET_LIMIT"), "{keeper} names SET_LIMIT");
    }
    let mut paths = 0;
    for (name, body) in functions(DEVMGR) {
        let starts_a_driver = name.starts_with("start_") || name == "launch_again";
        if !starts_a_driver || name == "start_name" {
            continue;
        }
        paths += 1;
        let narrowed = [
            ".replace(Requested::Exactly(DEVICE_RIGHTS))",
            ".replace(Requested::Exactly(NET_DEVICE_RIGHTS))",
            ".duplicate(Requested::Exactly(DEVICE_RIGHTS))",
        ]
        .iter()
        .any(|call| body.contains(call));
        assert!(narrowed, "{name} hands a device on without narrowing it");
    }
    // blk, net, display, input, sound, plain, the Intel keeper, and a
    // restart.
    assert_eq!(paths, 8, "a launch path was added or lost; check it here");
}
