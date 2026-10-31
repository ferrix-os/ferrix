//! `cargo xtask gate-rows --since <ref>`: which of `docs/BACKLOG.md`'s gate
//! rows a change has to run, from the paths it changes (`docs/TEST-TIME.md`,
//! Phase 3, B1).
//!
//! The table *What a landing runs* says, by what a change touches, which
//! gates land it. Applying it by hand is where time went: a docs edit paid
//! the whole `check`, and a branch rebased onto a moved `main` ran its whole
//! row again whether or not the commits that moved it came near its files.
//! This reads the table's columns as code and prints the commands. It never
//! checks less than the table: a path no row names gets the image row, the
//! widest, and says so, and what the table leaves to judgement ("a stage 7
//! change also runs ...") is printed as a question beside the rows.
//!
//! With `--moved <old main>` it answers the rebase question instead: whether
//! the commits between the old base and `main` touched any file the branch
//! does. None, and the gate stands (`docs/CONVENTIONS.md`, splitting rule 3).

use std::collections::BTreeSet;
use std::process::Command;

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "gate-rows",
    help_at: 1020,
    help: &[
        "gate-rows --since <ref> [--moved <old main>]",
        "              The gate rows docs/BACKLOG.md asks of the changes since <ref>; with",
        "              --moved, whether a rebase from <old main> asks for a gate again",
    ],
    run: |_, args| run(args),
}];

/// What a change's paths ask of its gate.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Rows {
    /// The commands, in the order to run them.
    pub(crate) commands: Vec<String>,
    /// Why each part of the gate is there, one line a reason.
    pub(crate) reasons: Vec<String>,
    /// What the table leaves to whoever knows the change.
    pub(crate) questions: Vec<String>,
}

impl Rows {
    fn add(&mut self, commands: &[&str]) {
        for command in commands {
            if !self.commands.iter().any(|known| known == command) {
                self.commands.push((*command).to_owned());
            }
        }
    }

    fn ask(&mut self, question: &str) {
        if !self.questions.iter().any(|known| known == question) {
            self.questions.push(question.to_owned());
        }
    }
}

/// Stage 13's exit test (`docs/BACKLOG.md`, *What a landing runs*).
const TEST_CONTAINER: &str = "test-container --arch all";

/// The memory-pressure row (`docs/BACKLOG.md`, *What a landing runs*;
/// consultant ledger 2026-10-17 line 2314 (7)): its default form, about
/// 2.5 min. The starved form depends on the host and is never a row.
const TEST_BTRFS_PRESSURE: &str = "test-btrfs-pressure --arch x86_64 --accel kvm \
     --init ~/.local/share/ferrix/busybox/{arch}/bin/busybox.static";

/// What asks for [`TEST_BTRFS_PRESSURE`]: the page cache and its reclaim,
/// the frame allocator and its watermark, btrfs, and the job's memory charge.
const PRESSURED: &[&str] = &[
    "src/kernel/src/fs/pages.rs",
    "src/kernel/src/user/cache.rs",
    "src/kernel/src/user/vmo.rs",
    "src/kernel/src/mm.rs",
    "src/kernel/src/mm/",
    "src/kernel/src/fs/btrfs",
    "src/lib/fs/btrfs/",
    "src/kernel/src/object/quota.rs",
];

/// The Intel GPU's restart row (`docs/BACKLOG.md`, *What a landing runs*;
/// `docs/INTEL-GPU.md` §11, §12.8): its worker killed twice under a running
/// Vulkan program on the QEMU stand-in, which must never know; its keeper
/// killed twice, whose program's device is lost; and Mesa's start-up on the
/// node it serves.
const TEST_GPU_RESTART: &[&str] = &[
    "test-restart --boot intel",
    "test-restart --boot intel-keeper",
    "test-intel-uapi",
];

/// What asks for [`TEST_GPU_RESTART`]: the Intel driver, its fake and its
/// library, devmgr, which starts it again, the chardev core its clients
/// reach it through, and the gate's own program and judge.
const GPU_RESTARTED: &[&str] = &[
    "src/user/system/native/drivers/gpu/intel/",
    "src/user/system/native/drivers/gpu/intel-fake/",
    "src/lib/drivers/gpu/intel-xelp/",
    "src/user/system/native/devmgr/",
    "src/kernel/src/interfaces/chardev/",
    "src/user/system/linux/drivers/nvrm/test/vk-survive.c",
    "tools/common/xtask/src/restart/intel.rs",
    "tools/common/xtask/src/intel_uapi.rs",
];

/// The table's image row: anything the image contains, and the default.
/// Both `x86_64` accelerators are named, so the row means the same thing on
/// every host (`docs/BACKLOG.md`, *Name the accelerator*).
const IMAGE_ROW: &[&str] = &[
    "check",
    "build --arch all --release",
    "test-boot --arch x86_64 --accel tcg",
    "test-boot --arch x86_64 --accel kvm",
    "test-boot --arch aarch64",
    "test-boot --arch armv7a",
    "test-boot --arch armv7a --smp 2",
];

/// The core quarantine row (`docs/CORE-QUARANTINE.md` §7, ledger 4932):
/// lode takes a processor out, the surface says so, and root brings it
/// back, on four processors and on two, under both accelerators; a
/// compatibility-format message refuses (VT-d remapping off); and test-nvrm,
/// whose unremapped boot runs the native check's refusal arm (ledger 4992).
const TEST_QUARANTINE: &[&str] = &[
    "test-quarantine --arch x86_64 --accel kvm",
    "test-quarantine --arch x86_64 --accel tcg",
    "test-quarantine-smp2 --arch x86_64 --accel kvm",
    "test-quarantine-unremapped --arch x86_64 --accel kvm",
    "test-nvrm --arch x86_64",
];

/// Whether `path` reaches core quarantine: the service, the scheduler's
/// placement and park, the messages' aim and retarget, the doors, lode's
/// policy, or the gate itself.
fn reaches_quarantine(path: &str) -> bool {
    [
        "src/kernel/src/smp/service.rs",
        "src/kernel/src/sched/mod.rs",
        "src/kernel/src/sched/queue.rs",
        "src/kernel/src/sched/task.rs",
        "src/kernel/src/arch/x86_64/msi.rs",
        "src/kernel/src/iommu/remapping.rs",
        "src/kernel/src/iommu/vtd.rs",
        "src/kernel/src/iommu/amdvi.rs",
        "src/kernel/src/settings/cpu_online.rs",
        "src/kernel/src/fs/sysfs.rs",
        "src/kernel/src/init.rs",
        "src/lib/log/lode/src/quarantine.rs",
        "src/user/system/native/lode/src/quarantine.rs",
        "tools/common/xtask/src/quarantine.rs",
    ]
    .contains(&path)
        || path.starts_with("src/tests/quarantine/")
}

/// The RAM scrubber's row (`docs/RAM-SCRUBBER.md` §7, ledger 5057): a full
/// sweep under the gate's override and the stand-down without it, under
/// both accelerators on x86-64 and on AArch64, with the facade's pinned
/// disassembly; ARMv7-A's stand-down.
const TEST_SCRUB: &[&str] = &[
    "test-scrub --arch x86_64 --accel kvm",
    "test-scrub --arch x86_64 --accel tcg",
    "test-scrub --arch aarch64",
    "test-scrub --arch armv7a",
];

/// Whether `path` reaches the RAM scrubber: the batch, the facade, the
/// type-16 decode, the frame allocator's block look-up, the token's call,
/// the service, or the gate itself.
fn reaches_scrub(path: &str) -> bool {
    [
        "src/kernel/src/mm/scrub.rs",
        "src/kernel/src/arch/x86_64/scrub.rs",
        "src/kernel/src/arch/aarch64/scrub.rs",
        "src/lib/platform/smbios/src/lib.rs",
        "src/lib/kernel/frame/src/lib.rs",
        "src/user/system/linux/init/units/memscrub.service",
        "tools/common/xtask/src/scrub.rs",
    ]
    .contains(&path)
        || path.starts_with("src/user/system/native/memscrub/")
}

/// The machine check at a system call's copy (`docs/MACHINE-CHECK-COPY.md`
/// §6, ledger 5216): the recovery under both accelerators and at two
/// processors, and every mode that must stop or leave it alone.
const TEST_MCE_COPY: &[&str] = &[
    "test-mce-copy --arch x86_64 --accel kvm",
    "test-mce-copy --arch x86_64 --accel tcg",
    "test-mce-copy --arch x86_64 --accel kvm --smp 2",
    "test-mce-copy-kill --arch x86_64 --accel kvm",
    "test-mce-copy-tcc --arch x86_64 --accel kvm",
    "test-mce-copy-page --arch x86_64 --accel kvm",
    "test-mce-copy-off --arch x86_64 --accel kvm",
    "test-mce-copy-outside --arch x86_64 --accel kvm",
    "test-mce-copy-environ --arch x86_64 --accel kvm",
    "test-mce-copy-intel --arch x86_64 --accel kvm",
    "test-mce-copy-file --arch x86_64 --accel kvm",
];

/// Whether `path` reaches the copy's recovery: the copy, the decision and
/// the handler, the readers of a program's memory, the poison record and
/// its way out, or the gate.
fn reaches_mce_copy(path: &str) -> bool {
    [
        "src/kernel/src/arch/x86_64/copy_mc.rs",
        "src/kernel/src/arch/x86_64/mce.rs",
        "src/kernel/src/arch/x86_64/mce/decode.rs",
        "src/kernel/src/syscall/uaccess.rs",
        "src/kernel/src/sched/task.rs",
        "src/kernel/src/trap.rs",
        "tools/common/xtask/src/mce_copy.rs",
    ]
    .contains(&path)
        || path.starts_with("src/tests/mce-copy/")
}

/// Poison in the page cache (`docs/MACHINE-CHECK-PAGECACHE.md` §6, ledger
/// 5294): a clean page's read and mapping under KVM with AMD's records, the
/// dirty modes, the poll under TCG with Intel's UCNA record, and the copy
/// from a clean file's mapping.
const TEST_MCE_PAGECACHE: &[&str] = &[
    "test-mce-pagecache --arch x86_64 --accel kvm",
    "test-mce-pagecache --arch x86_64 --accel tcg",
    "test-mce-pagecache-mapped --arch x86_64 --accel kvm",
    "test-mce-pagecache-dirty --arch x86_64 --accel kvm",
    "test-mce-pagecache-poll --arch x86_64 --accel tcg",
    "test-mce-copy-file --arch x86_64 --accel kvm",
];

/// Whether `path` reaches the page cache's poison: the drop, the cache read,
/// the fill loop, the ring-3 and poll paths, or the gate.
fn reaches_mce_pagecache(path: &str) -> bool {
    [
        "src/kernel/src/user/cache.rs",
        "src/kernel/src/user/vmo.rs",
        "src/kernel/src/fs/pages.rs",
        "src/kernel/src/arch/x86_64/mce.rs",
        "src/kernel/src/trap.rs",
        "src/kernel/src/fs/btrfs_write_check/poison_check.rs",
        "tools/common/xtask/src/mce_pagecache.rs",
    ]
    .contains(&path)
        || path.starts_with("src/tests/mce-pagecache/")
}

/// A warm restart after a fatal machine check (`docs/MACHINE-CHECK-RESTART.md`
/// §5, ledger 5467): the stop word, the pstore table, the loader's keep-out,
/// the kernel's leave-out.
const TEST_MCE_RESTART: &[&str] = &[
    "test-mce-restart --arch x86_64 --accel tcg",
    "test-mce-restart-retired --arch x86_64 --accel tcg",
];

/// Whether `path` reaches the warm restart: the pstore table, the stop word,
/// the loader's bank read and keep-out, the allocator's leave-out, the
/// machine check's stop, or the gate.
fn reaches_mce_restart(path: &str) -> bool {
    [
        "src/lib/proto/bootinfo/src/pstore.rs",
        "src/kernel/src/console/pstore.rs",
        "src/kernel/src/panic.rs",
        "src/kernel/src/mm.rs",
        "src/kernel/src/arch/x86_64/mce.rs",
        "src/boot/common/uefi/src/badframes.rs",
        "src/boot/common/uefi/src/pstore.rs",
        "tools/common/xtask/src/mce_restart.rs",
    ]
    .contains(&path)
}

/// A ring-3 driver's poison (`docs/MACHINE-CHECK-DRIVER.md` §3, ledger
/// 5444): Own and Kill under both accelerators, Copy under KVM with AMD's
/// records, Bystander under TCG with Intel's.
const TEST_MCE_DRIVER: &[&str] = &[
    "test-mce-driver --arch x86_64 --accel kvm",
    "test-mce-driver --arch x86_64 --accel tcg",
    "test-mce-driver-kill --arch x86_64 --accel kvm",
    "test-mce-driver-kill --arch x86_64 --accel tcg",
    "test-mce-driver-copy --arch x86_64 --accel kvm",
    "test-mce-driver-bystander --arch x86_64 --accel tcg",
];

/// Whether `path` reaches a driver's poison: the pin quarantine, the victim
/// driver, or the gate.
fn reaches_mce_driver(path: &str) -> bool {
    [
        "src/kernel/src/object/pin.rs",
        "tools/common/xtask/src/mce_driver.rs",
    ]
    .contains(&path)
        || path.starts_with("src/user/system/native/mce-victim/")
}

/// CMCI (`docs/MACHINE-CHECK-CMCI.md` §5, ledger 5348): every mode under
/// TCG on QEMU with patch 0008, which no other machine here can run.
const TEST_MCE_CMCI: &[&str] = &[
    "test-mce-cmci --arch x86_64 --accel tcg",
    "test-mce-cmci-ucna --arch x86_64 --accel tcg",
    "test-mce-cmci-storm --arch x86_64 --accel tcg",
    "test-mce-cmci-window --arch x86_64 --accel tcg",
    "test-mce-cmci-off --arch x86_64 --accel tcg",
    "test-mce-cmci-unoffered --arch x86_64 --accel tcg",
    "test-mce-cmci-parked --arch x86_64 --accel tcg",
];

/// Whether `path` reaches CMCI: the claim, the handler and the poll's
/// loop, the local APIC's entry, the return-to-service hook, the QEMU patch,
/// or the gate.
fn reaches_mce_cmci(path: &str) -> bool {
    [
        "src/kernel/src/arch/x86_64/mce.rs",
        "src/kernel/src/arch/x86_64/mce/cmci.rs",
        "src/kernel/src/arch/x86_64/apic.rs",
        "tools/common/data/qemu/0008-target-i386-CMCI-under-TCG.patch",
        "tools/common/xtask/src/mce_cmci.rs",
    ]
    .contains(&path)
}

/// The machine-check row (F-73, `docs/MACHINE-CHECK.md`): a fatal machine
/// check injected into every processor must stop the kernel as FX-9005, and
/// the log's and recovery's modes must hold, under both accelerators. Under
/// KVM on a host whose processor model has no `MCG_SER_P` (nazuna's AMD) the
/// recovery modes require the fail-closed stop instead.
const TEST_MCE: &[&str] = &[
    "test-mce --arch x86_64 --accel kvm",
    "test-mce --arch x86_64 --accel tcg",
    "test-mce-corrected --arch x86_64 --accel kvm",
    "test-mce-corrected --arch x86_64 --accel tcg",
    "test-mce-srar --arch x86_64 --accel kvm",
    "test-mce-srar --arch x86_64 --accel tcg",
    "test-mce-broadcast --arch x86_64 --accel kvm",
    "test-mce-broadcast --arch x86_64 --accel tcg",
    "test-mce-srao --arch x86_64 --accel kvm",
    "test-mce-srao --arch x86_64 --accel tcg",
    "test-mce-left --arch x86_64 --accel kvm",
    "test-mce-left --arch x86_64 --accel tcg",
    "test-mce-recover-off --arch x86_64 --accel kvm",
    "test-mce-recover-off --arch x86_64 --accel tcg",
    "test-mce-kill --arch x86_64 --accel kvm",
    "test-mce-kill --arch x86_64 --accel tcg",
    "test-mce-amd-srar --arch x86_64 --accel kvm",
    "test-mce-amd-srar --arch x86_64 --accel tcg",
    "test-mce-amd-kill --arch x86_64 --accel kvm",
    "test-mce-amd-kill --arch x86_64 --accel tcg",
    "test-mce-amd-tcc --arch x86_64 --accel kvm",
    "test-mce-amd-tcc --arch x86_64 --accel tcg",
    "test-mce-amd-eipv --arch x86_64 --accel kvm",
    "test-mce-amd-eipv --arch x86_64 --accel tcg",
    "test-mce-amd-pcc --arch x86_64 --accel kvm",
    "test-mce-amd-pcc --arch x86_64 --accel tcg",
    "test-mce-amd-nosuccor --arch x86_64 --accel kvm",
    "test-mce-amd-nosuccor --arch x86_64 --accel tcg",
    "test-mce-amd-over --arch x86_64 --accel kvm",
    "test-mce-amd-over --arch x86_64 --accel tcg",
    "test-mce-amd-deferred --arch x86_64 --accel kvm",
    "test-mce-amd-deferred --arch x86_64 --accel tcg",
    "test-mce-amd-ring0 --arch x86_64 --accel kvm",
    "test-mce-amd-ring0 --arch x86_64 --accel tcg",
    "test-mce-amd-corrected --arch x86_64 --accel kvm",
    "test-mce-amd-corrected --arch x86_64 --accel tcg",
    "test-mce-amd-recover-off --arch x86_64 --accel kvm",
    "test-mce-amd-recover-off --arch x86_64 --accel tcg",
];

/// Whether a change to `path` reaches the machine check: x86-64's `CR4`
/// setup, its paranoid entry and their check, the trap gates, the
/// secondaries' start, the machine-check log and recovery, and their gate
/// and guest program (`docs/BACKLOG.md`, *What a landing runs*).
fn reaches_machine_check(path: &str) -> bool {
    [
        "src/kernel/src/arch/x86_64/cpu.rs",
        "src/kernel/src/arch/x86_64/paranoid.rs",
        "src/kernel/src/arch/x86_64/paranoid/check.rs",
        "src/kernel/src/arch/x86_64/trap.rs",
        "src/kernel/src/arch/x86_64/smp.rs",
        "src/kernel/src/arch/x86_64/mce.rs",
        "src/kernel/src/arch/x86_64/mce/check.rs",
        "src/kernel/src/arch/x86_64/mce/decode.rs",
        "tools/common/xtask/src/machine_check.rs",
    ]
    .contains(&path)
        || path.starts_with("src/tests/mce/")
}

/// Whether `path` is documentation only: under `docs/`, or a Markdown file
/// at the top of the tree.
fn is_docs(path: &str) -> bool {
    path.starts_with("docs/") || (!path.contains('/') && path.ends_with(".md"))
}

/// The rows for `changed`, given the directories (relative, with `/`) of the
/// crates the kernel and the loader build.
pub(crate) fn rows(changed: &[String], image_crates: &BTreeSet<String>) -> Rows {
    let mut rows = Rows::default();
    if changed.is_empty() {
        rows.reasons
            .push("nothing committed changes anything (gate-rows reads commits)".to_owned());
        return rows;
    }
    if changed.iter().all(|path| is_docs(path)) {
        rows.add(&["check-docs"]);
        rows.reasons
            .push("only docs/ and top-level Markdown: the steps that read them".to_owned());
        return rows;
    }
    for path in changed.iter().filter(|path| !is_docs(path)) {
        rows_for(path, image_crates, &mut rows);
    }
    rows
}

/// The machine check's and core quarantine's rows `path` reaches.
fn machine_rows(path: &str, rows: &mut Rows) {
    if reaches_machine_check(path) {
        rows.add(TEST_MCE);
        rows.reasons.push(format!(
            "{path}: the machine check's code: test-mce under KVM and TCG"
        ));
    }
    if reaches_quarantine(path) {
        rows.add(TEST_QUARANTINE);
        rows.reasons.push(format!(
            "{path}: core quarantine's code: test-quarantine under KVM and TCG"
        ));
    }
}

/// What one path adds to `rows`.
fn rows_for(path: &str, image_crates: &BTreeSet<String>, rows: &mut Rows) {
    let under = |prefix: &str| path.starts_with(prefix);
    let image_crate = image_crates
        .iter()
        .any(|dir| path.starts_with(&format!("{dir}/")));
    let reason = |rows: &mut Rows, why: &str| rows.reasons.push(format!("{path}: {why}"));
    if under("src/user/system/linux/ferrousli/") {
        rows.add(&[
            "check --ferrousli",
            "busybox",
            "test-shell --arch x86_64 --init ferrousli",
            "test-vfs --arch x86_64 --init ferrousli",
            "uutils",
        ]);
        reason(rows, "ferrousli's row");
        if under("src/user/system/linux/ferrousli/tools/ports/") {
            rows.add(&[
                "ports",
                "test-net --arch x86_64 --init ferrousli",
                "ports --arch aarch64",
                "ports --arch armv7a",
                "test-net --arch all",
            ]);
            reason(rows, "ferrousli's ports");
        }
    } else if under("src/user/system/linux/zinc/") {
        rows.add(&["check --fast --zinc"]);
        reason(rows, "zinc's row");
        rows.ask(
            "zinc: does the change alter what zinc does at boot? then test-boot --arch x86_64",
        );
        rows.ask(
            "zinc: does it change how zinc starts, waits for or signals a process? then \
             test-shell --arch all and test-jobs",
        );
    } else if under("src/user/system/linux/init/") || under("src/lib/init/svc/") {
        rows.add(&["check", "test-init --arch all"]);
        reason(rows, "init's row");
    } else if under("src/user/system/linux/settingsd/")
        || path == "tools/common/xtask/src/settingsd.rs"
    {
        rows.add(&["check", "test-settings"]);
        reason(rows, "the settings writer's row: check, then test-settings");
    } else if under("src/user/system/linux/auth/") || path == "tools/common/xtask/src/auth.rs" {
        rows.add(IMAGE_ROW);
        rows.add(&["test-auth --arch all", "test-settings"]);
        reason(rows, "authd's row: the image row, then test-auth");
        rows.ask(
            "authd: does it change what authd refuses? then test-auth --arch x86_64 \
             --sabotage NAME for each name in src/user/system/linux/auth/authd/src/sabotage.rs",
        );
    } else if under("src/tests/container/") {
        rows.add(&["check", TEST_CONTAINER]);
        reason(rows, "stage 13's exit program: check, then test-container");
    } else if path == "tools/common/xtask/src/container.rs" {
        rows.add(IMAGE_ROW);
        rows.add(&[TEST_CONTAINER]);
        reason(
            rows,
            "test-container's judge: the image row, then test-container",
        );
    } else if under("src/kernel/")
        || under("src/boot/common/uefi/")
        || under("tools/common/xtask/")
        || image_crate
        || path == "Cargo.toml"
        || path == "Cargo.lock"
        || under(".cargo/")
    {
        rows.add(IMAGE_ROW);
        reason(rows, "in the image: the image row");
        machine_rows(path, rows);
        rows.ask(
            "stage 7 or 8? then test-shell --arch x86_64 (zinc, --init ferrousli, the musl and \
             the glibc busybox) and test-vfs --arch x86_64 (ferrousli and musl busybox)",
        );
        rows.ask("stage 7? then test-threads --arch all");
        rows.ask(
            "stage 13 (namespaces, cgroups, reclaim, the OOM kill, seccomp)? then \
             test-container --arch all",
        );
        rows.ask("the loader, exec or ferrousli? then test-shell with Debian's dynamic busybox");
    } else if under("src/lib/") {
        rows.add(&["check", "test-boot --arch armv7a --smp 2"]);
        reason(rows, "a library no image crate uses: check and one boot");
    } else {
        rows.add(IMAGE_ROW);
        reason(
            rows,
            "no row of the table names this path, so the widest one; narrow it by hand if \
             you know better, and say why",
        );
    }
    area_rows(path, rows);
}

/// The rows an area of the tree adds to whatever its kind of path asked:
/// the memory-pressure row, the RAM scrubber's row and the Intel GPU's
/// restart row.
fn area_rows(path: &str, rows: &mut Rows) {
    let under = |prefix: &str| path.starts_with(prefix);
    let reason = |rows: &mut Rows, why: &str| rows.reasons.push(format!("{path}: {why}"));
    if PRESSURED.iter().any(|prefix| under(prefix)) {
        rows.add(&[TEST_BTRFS_PRESSURE]);
        reason(
            rows,
            "the page cache, reclaim, the frame allocator, btrfs or the memory charge: \
             test-btrfs-pressure (docs/BTRFS-PRESSURE.md)",
        );
    }
    if reaches_mce_restart(path) {
        rows.add(TEST_MCE_RESTART);
        reason(
            rows,
            "a warm restart after a machine check: test-mce-restart",
        );
    }
    if reaches_mce_driver(path) {
        rows.add(TEST_MCE_DRIVER);
        reason(rows, "a driver's poison: test-mce-driver and its modes");
    }
    if reaches_mce_cmci(path) {
        rows.add(TEST_MCE_CMCI);
        reason(rows, "CMCI: test-mce-cmci and its modes");
    }
    if reaches_mce_pagecache(path) {
        rows.add(TEST_MCE_PAGECACHE);
        reason(
            rows,
            "the page cache's poison: test-mce-pagecache and its modes",
        );
    }
    if reaches_mce_copy(path) {
        rows.add(TEST_MCE_COPY);
        reason(
            rows,
            "the copy's machine-check recovery: test-mce-copy and its modes",
        );
    }
    if reaches_scrub(path) {
        rows.add(TEST_SCRUB);
        reason(
            rows,
            "the RAM scrubber's code: test-scrub on every architecture",
        );
    }
    if GPU_RESTARTED.iter().any(|prefix| under(prefix)) {
        rows.add(TEST_GPU_RESTART);
        reason(
            rows,
            "the Intel GPU's driver, its restart or its clients' path: test-restart --boot intel \
             and --boot intel-keeper, and test-intel-uapi (docs/INTEL-GPU.md §11, §12)",
        );
    }
}

/// The directories of the crates the kernel and the loader build, relative
/// to the workspace, from `cargo tree` for each architecture's two targets.
fn image_crates() -> Result<BTreeSet<String>> {
    let root = paths::workspace_root();
    let mut found = BTreeSet::new();
    for arch in Arch::ALL {
        for (package, target) in [
            ("ferrix-kernel", arch.kernel_target()),
            ("ferrix-boot", arch.loader_target()),
        ] {
            let output = Command::new(crate::cargo::cargo())
                .current_dir(&root)
                .args(["tree", "-q", "-p", package, "--target", target])
                .args(["-e", "normal,build", "--prefix", "none"])
                .output()
                .map_err(|error| Error::new(format!("could not run cargo tree: {error}")))?;
            crate::cargo::finished(output.status, "cargo tree")?;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Some(path) = line
                    .rsplit_once('(')
                    .and_then(|(_, rest)| rest.strip_suffix(')'))
                else {
                    continue;
                };
                let path = std::path::Path::new(path.trim_end_matches(" (*)"));
                if let Ok(relative) = path.strip_prefix(&root) {
                    let _ = found.insert(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    Ok(found)
}

/// What BACKLOG's landing rule calls cross-cutting: "nothing cross-cutting
/// (locks, the scheduler, the trap or system-call entry, memory
/// management)". A `main` that moved in any of these asks for the gate
/// again, whichever files the branch touches. Each entry is a module of
/// `src/kernel/src` (its `.rs` and its directory) or, ending in `/`, a
/// directory.
const CROSS_CUTTING: &[&str] = &[
    "src/kernel/src/sched",
    "src/kernel/src/trap",
    "src/kernel/src/mm",
    "src/kernel/src/user/",
    "src/kernel/src/smp",
    "src/kernel/src/sync",
    "src/lib/kernel/sync/",
];

/// The parts of an architecture's code that are the trap and system-call
/// entry and the switch: `src/kernel/src/arch/<arch>/<one of these>`.
const CROSS_CUTTING_ARCH: &[&str] = &["trap", "syscall", "entry", "switch"];

/// Whether a change to `path` is cross-cutting in BACKLOG's sense.
fn cross_cutting(path: &str) -> bool {
    let module = |name: &str| path == format!("{name}.rs") || path.starts_with(&format!("{name}/"));
    let listed = CROSS_CUTTING.iter().any(|entry| {
        if entry.ends_with('/') {
            path.starts_with(entry)
        } else {
            module(entry)
        }
    });
    let arch = path
        .strip_prefix("src/kernel/src/arch/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, part)| CROSS_CUTTING_ARCH.iter().any(|name| part.starts_with(name)));
    listed || arch
}

/// The certification item's manifest, whose `core` and `item` rings name
/// the kernel files, and whose `crates` the library crates, a change to
/// which the consultant reviews.
const MANIFEST: &str = "tools/common/data/certification-item.json";

/// The paths of `changed` that need the certification consultant's review
/// before `land.sh take` (`docs/CONVENTIONS.md`, *Changes to the certified
/// item go through review*): the manifest, `docs/certification/`,
/// `docs/sysml/`, `tools/common/data/`, a kernel file in the `core` or
/// `item` ring (`members`, relative to `src/kernel/src`, exact or `dir/**`),
/// any file of a `core` or `item` crate (`crates`, its directory: sources,
/// tests and `Cargo.toml` alike), and every path in `added_unsafe`.
fn needs_review(
    changed: &[String],
    members: &[String],
    crates: &[String],
    added_unsafe: &[String],
) -> Vec<String> {
    let in_ring = |path: &str| {
        path.strip_prefix("src/kernel/src/")
            .is_some_and(|relative| {
                members
                    .iter()
                    .any(|member| match member.strip_suffix("/**") {
                        Some(dir) => relative.starts_with(&format!("{dir}/")),
                        None => relative == member,
                    })
            })
    };
    changed
        .iter()
        .filter(|path| {
            path.as_str() == MANIFEST
                || path.starts_with("docs/certification/")
                || path.starts_with("docs/sysml/")
                || path.starts_with("tools/common/data/")
                || in_ring(path)
                || crates
                    .iter()
                    .any(|dir| path.starts_with(&format!("{}/", dir.trim_end_matches('/'))))
                || added_unsafe.contains(path)
        })
        .cloned()
        .collect()
}

/// The `core` and `item` rings' members, and the directories of the `core`
/// and `item` crates, read from the manifest itself.
fn ring_members() -> Result<(Vec<String>, Vec<String>)> {
    let interpreter = crate::check::python_interpreter()
        .ok_or_else(|| Error::new("no working Python interpreter on PATH"))?;
    let output = Command::new(interpreter)
        .current_dir(paths::workspace_root())
        .args([
            "-c",
            "import json, sys\n\
             manifest = json.load(open(sys.argv[1], encoding='utf-8'))\n\
             rings = manifest['rings']\n\
             crates = manifest.get('crates', {}).get('members', {}).values()\n\
             print('\\n'.join(['file ' + m for r in ('core', 'item') for m in rings[r]['members']]\n\
                 + ['crate ' + c['path'] for c in crates if c['ring'] in ('core', 'item')]))",
            MANIFEST,
        ])
        .output()
        .map_err(|error| Error::new(format!("could not read {MANIFEST}: {error}")))?;
    crate::cargo::finished(output.status, "reading the certification item's rings")?;
    let mut members = Vec::new();
    let mut crates = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(member) = line.strip_prefix("file ") {
            members.push(member.to_owned());
        } else if let Some(dir) = line.strip_prefix("crate ") {
            crates.push(dir.to_owned());
        }
    }
    Ok((members, crates))
}

/// The kernel files to which the commits since `since` add a line holding
/// `unsafe`.
fn added_unsafe(since: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(paths::workspace_root())
        .args([
            "diff",
            "--unified=0",
            &format!("{since}...HEAD"),
            "--",
            "src/kernel",
        ])
        .output()
        .map_err(|error| Error::new(format!("could not run git diff: {error}")))?;
    crate::cargo::finished(output.status, "git diff")?;
    let mut found = Vec::new();
    let mut file = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            file = Some(path.to_owned());
        } else if line.starts_with('+')
            && !line.starts_with("+++")
            && line.contains("unsafe")
            && let Some(path) = file.take()
        {
            found.push(path);
        }
    }
    Ok(found)
}

/// The files changed between `from` and `to`.
fn changed(from: &str, to: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(paths::workspace_root())
        .args(["diff", "--name-only", &format!("{from}...{to}")])
        .output()
        .map_err(|error| Error::new(format!("could not run git diff: {error}")))?;
    crate::cargo::finished(output.status, "git diff --name-only")?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// `gate-rows`: print the rows for the branch's changes since `--since`, or
/// with `--moved`, whether a rebase onto `main` asks for a gate again.
///
/// # Errors
///
/// No `--since`, or git or cargo failing.
pub(crate) fn run(args: &Args) -> Result<()> {
    let since = args
        .since
        .as_deref()
        .ok_or_else(|| Error::new("gate-rows needs --since <ref>, the branch's base (develop)"))?;
    let branch = changed(since, "HEAD")?;
    // First, since it holds whether the gate stands or not.
    let (members, crates) = ring_members()?;
    let review = needs_review(&branch, &members, &crates, &added_unsafe(since)?);
    if !review.is_empty() {
        println!(
            "needs the certification consultant's review before land.sh take \
             (docs/CONVENTIONS.md):"
        );
        for path in &review {
            println!("  {path}");
        }
    }
    if let Some(old) = args.moved.as_deref() {
        let moved = changed(old, since)?;
        let both: Vec<&String> = branch.iter().filter(|path| moved.contains(path)).collect();
        let crossing: Vec<&String> = moved.iter().filter(|path| cross_cutting(path)).collect();
        for path in &crossing {
            println!(
                "re-gate: {since} moved in {path}, cross-cutting (docs/BACKLOG.md, Landing on main)"
            );
        }
        if both.is_empty() && crossing.is_empty() {
            println!(
                "none of the {} files changed between {old} and {since} is one of the branch's {}: \
                 the gate stands (docs/CONVENTIONS.md, rule 3); say so in the report",
                moved.len(),
                branch.len()
            );
            return Ok(());
        }
        if !both.is_empty() {
            println!("{since} moved in files the branch changes; gate again:");
            for path in both {
                println!("  {path}");
            }
        }
    }
    let rows = rows(&branch, &image_crates()?);
    println!(
        "{} paths changed since {since}; docs/BACKLOG.md, What a landing runs:",
        branch.len()
    );
    for reason in &rows.reasons {
        println!("  {reason}");
    }
    println!("\nrun:");
    for command in &rows.commands {
        println!("  cargo xtask {command}");
    }
    if !rows.questions.is_empty() {
        println!("\nand if the change is one of these, add what it says:");
        for question in &rows.questions {
            println!("  {question}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(paths: &[&str]) -> Rows {
        let crates = BTreeSet::from(["src/lib/proto/blkring".to_owned()]);
        rows(
            &paths
                .iter()
                .map(|path| (*path).to_owned())
                .collect::<Vec<_>>(),
            &crates,
        )
    }

    #[test]
    fn a_docs_change_runs_only_the_docs_steps() {
        let rows = rows_of(&[
            "docs/BACKLOG.md",
            "README.md",
            "docs/sysml/10-roadmap.sysml",
        ]);
        assert_eq!(rows.commands, ["check-docs"]);
    }

    #[test]
    fn docs_beside_code_change_nothing_about_the_code_row() {
        let rows = rows_of(&["docs/BACKLOG.md", "src/kernel/src/main.rs"]);
        assert_eq!(rows.commands, IMAGE_ROW);
    }

    #[test]
    fn a_kernel_crate_in_src_lib_is_the_image_row_and_another_lib_one_boot() {
        assert_eq!(
            rows_of(&["src/lib/proto/blkring/src/lib.rs"]).commands,
            IMAGE_ROW
        );
        assert_eq!(
            rows_of(&["src/lib/proto/other/src/lib.rs"]).commands,
            ["check", "test-boot --arch armv7a --smp 2"]
        );
    }

    #[test]
    fn the_page_cache_and_btrfs_add_the_pressure_row_and_other_kernel_code_does_not() {
        for path in [
            "src/kernel/src/fs/pages.rs",
            "src/kernel/src/user/cache.rs",
            "src/kernel/src/mm.rs",
            "src/kernel/src/fs/btrfs_write_check.rs",
            "src/lib/fs/btrfs/vfs/src/rw.rs",
            "src/kernel/src/object/quota.rs",
        ] {
            let rows = rows_of(&[path]);
            assert!(
                rows.commands.iter().any(|c| c == TEST_BTRFS_PRESSURE),
                "{path}"
            );
        }
        let rows = rows_of(&["src/kernel/src/main.rs"]);
        assert!(!rows.commands.iter().any(|c| c == TEST_BTRFS_PRESSURE));
    }

    #[test]
    fn the_copys_recovery_adds_its_row() {
        for path in [
            "src/kernel/src/syscall/uaccess.rs",
            "src/kernel/src/arch/x86_64/copy_mc.rs",
            "src/tests/mce-copy/src/main.rs",
        ] {
            let rows = rows_of(&[path]);
            assert!(
                TEST_MCE_COPY
                    .iter()
                    .all(|row| rows.commands.iter().any(|c| c == row)),
                "{path}"
            );
        }
        let rows = rows_of(&["src/kernel/src/main.rs"]);
        assert!(!rows.commands.iter().any(|c| c == TEST_MCE_COPY[0]));
    }

    #[test]
    fn the_scrubber_adds_its_row_and_other_kernel_code_does_not() {
        for path in [
            "src/kernel/src/mm/scrub.rs",
            "src/kernel/src/arch/aarch64/scrub.rs",
            "src/user/system/native/memscrub/src/main.rs",
        ] {
            let rows = rows_of(&[path]);
            assert!(
                TEST_SCRUB
                    .iter()
                    .all(|row| rows.commands.iter().any(|c| c == row)),
                "{path}"
            );
        }
        let rows = rows_of(&["src/kernel/src/main.rs"]);
        assert!(!rows.commands.iter().any(|c| c == TEST_SCRUB[0]));
    }

    #[test]
    fn the_intel_driver_and_its_restart_path_add_the_gpu_restart_row() {
        for path in [
            "src/user/system/native/drivers/gpu/intel/src/main.rs",
            "src/lib/drivers/gpu/intel-xelp/src/backend.rs",
            "src/user/system/native/devmgr/src/main.rs",
            "src/kernel/src/interfaces/chardev/mod.rs",
            "tools/common/xtask/src/restart/intel.rs",
        ] {
            let rows = rows_of(&[path]);
            for row in TEST_GPU_RESTART {
                assert!(rows.commands.iter().any(|c| c == row), "{path}: {row}");
            }
        }
        let rows = rows_of(&["src/kernel/src/main.rs"]);
        assert!(!rows.commands.iter().any(|c| c == TEST_GPU_RESTART[0]));
    }

    #[test]
    fn a_path_the_table_does_not_name_gets_the_widest_row() {
        let rows = rows_of(&["src/user/apps/no-such-app/build.sh"]);
        assert_eq!(rows.commands, IMAGE_ROW);
        assert!(rows.reasons[0].contains("no row of the table"));
    }

    #[test]
    fn rows_are_joined_without_repeats() {
        let rows = rows_of(&[
            "src/user/system/linux/auth/authd/src/main.rs",
            "src/kernel/src/main.rs",
        ]);
        assert_eq!(rows.commands.iter().filter(|c| *c == "check").count(), 1);
        assert_eq!(
            rows.commands.last().map(String::as_str),
            Some("test-settings")
        );
    }

    #[test]
    fn stage_13s_exit_program_and_its_judge_run_test_container() {
        assert_eq!(
            rows_of(&["src/tests/container/src/main.rs"]).commands,
            ["check", "test-container --arch all"]
        );
        let judge = rows_of(&["tools/common/xtask/src/container.rs"]);
        assert_eq!(
            judge.commands.last().map(String::as_str),
            Some("test-container --arch all")
        );
        assert!(
            judge
                .commands
                .iter()
                .any(|c| c == "build --arch all --release")
        );
    }

    #[test]
    fn ferrousli_ports_add_the_network_rows() {
        let rows = rows_of(&["src/user/system/linux/ferrousli/tools/ports/curl/build.sh"]);
        assert!(rows.commands.iter().any(|c| c == "test-net --arch all"));
        assert_eq!(rows.commands[0], "check --ferrousli");
    }

    #[test]
    fn a_main_moved_in_locks_scheduling_traps_or_memory_is_cross_cutting() {
        for path in [
            "src/kernel/src/sched/queue.rs",
            "src/kernel/src/sched.rs",
            "src/kernel/src/mm.rs",
            "src/kernel/src/mm/space.rs",
            "src/kernel/src/trap.rs",
            "src/kernel/src/sync.rs",
            "src/kernel/src/smp/ipi.rs",
            "src/kernel/src/user/copy.rs",
            "src/kernel/src/arch/x86_64/syscall.rs",
            "src/kernel/src/arch/aarch64/trap/vectors.rs",
            "src/kernel/src/arch/armv7a/switch.rs",
            "src/lib/kernel/sync/src/lib.rs",
        ] {
            assert!(cross_cutting(path), "{path}");
        }
        for path in [
            "src/kernel/src/mmio.rs",
            "src/kernel/src/fs/tmpfs.rs",
            "src/kernel/src/arch/x86_64/cpu.rs",
            "docs/BACKLOG.md",
        ] {
            assert!(!cross_cutting(path), "{path}");
        }
    }

    #[test]
    fn review_is_asked_for_the_rings_the_evidence_and_new_unsafe() {
        let members = vec!["audit.rs".to_owned(), "arch/**".to_owned()];
        let crates = vec!["src/lib/fs/btrfs/format".to_owned()];
        let changed: Vec<String> = [
            "src/lib/fs/btrfs/format/Cargo.toml",
            "src/lib/fs/btrfs/vfs/src/lib.rs",
            "src/kernel/src/audit.rs",
            "src/kernel/src/arch/x86_64/cpu.rs",
            "src/kernel/src/fs/tmpfs.rs",
            "src/kernel/src/net/tcp.rs",
            "docs/certification/ITEM.md",
            "docs/sysml/10-roadmap.sysml",
            "tools/common/data/certification-item.json",
            "docs/BACKLOG.md",
            "tools/common/xtask/src/main.rs",
        ]
        .map(str::to_owned)
        .to_vec();
        let unsafe_added = vec!["src/kernel/src/net/tcp.rs".to_owned()];
        assert_eq!(
            needs_review(&changed, &members, &crates, &unsafe_added),
            [
                "src/lib/fs/btrfs/format/Cargo.toml",
                "src/kernel/src/audit.rs",
                "src/kernel/src/arch/x86_64/cpu.rs",
                "src/kernel/src/net/tcp.rs",
                "docs/certification/ITEM.md",
                "docs/sysml/10-roadmap.sysml",
                "tools/common/data/certification-item.json",
            ]
        );
    }

    #[test]
    fn the_machine_checks_code_adds_test_mce() {
        let rows = rows_of(&["src/kernel/src/arch/x86_64/paranoid/check.rs"]);
        assert!(
            rows.commands.ends_with(
                &TEST_MCE
                    .iter()
                    .map(|row| (*row).to_owned())
                    .collect::<Vec<_>>()
            )
        );
        assert_eq!(
            rows_of(&["src/kernel/src/arch/x86_64/pcid.rs"]).commands,
            IMAGE_ROW
        );
        assert!(
            rows_of(&["src/kernel/src/arch/x86_64/apic.rs"])
                .commands
                .ends_with(
                    &TEST_MCE_CMCI
                        .iter()
                        .map(|row| (*row).to_owned())
                        .collect::<Vec<_>>()
                )
        );
    }

    #[test]
    fn both_x86_accelerators_are_named_in_the_image_row() {
        assert!(IMAGE_ROW.contains(&"test-boot --arch x86_64 --accel tcg"));
        assert!(IMAGE_ROW.contains(&"test-boot --arch x86_64 --accel kvm"));
    }
}
