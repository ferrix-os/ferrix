# Backlog detail: Branches that still hold unlanded work

Detail kept out of [`docs/BACKLOG.md`](../BACKLOG.md), which holds a short line per item linking here. Each item's text is as it stood in the backlog when it was split out (2026-10-16); add new findings here, under the item's id, and keep the backlog line short.

### br-001

- `os-35/ipc-wake`, two "WIP:" commits.
  - 87675432, part B, a sync wake onto the waker's processor. Its relay and
    affinity checks and controls fired. The effect is bimodal until the
    ring's spurious `wake_all` is gone (part A removes it). Left: rebase
    onto the ring's parts, re-apply `Wake::Sync` at the new bell and
    completion wakes, the full row, the consultant.
  - dbd39280, part C: an adaptive halt-poll under a hypervisor (at most
    50 us, idle-task time), and targeted SGIs on GICv2 and GICv3. Checks
    and controls fired. Left: the full row, and F-50's line confirmed.
  - MSI affinity is noted only: it needs a native call to set a thread's
    affinity, vector retargeting, and devmgr to choose the processor.

### br-002

- `guest-frame-time`: diverged from its own name on GitHub. Origin carries
  six newer `WIP` commits on terminal and scroll rendering
  (`c1534cfaa`..`3f382700b`); this checkout has one unreviewed commit,
  `d33a86602`, a guest-compositor frame-cost fix (musl's `f32::round` and
  `memcmp` costing 30-40x glibc's in a guest). The two overlap in
  `compositor/render/exact.rs` and `compositor/term/*`, so neither a
  force-push nor a blind merge is safe. Left: whoever owns the current WIP
  decides which stands and reconciles the files that overlap. `d33a86602`
  is on GitHub as `guest-frame-time-local`.

### br-003

- `clipboard-vdagent`: also diverged under its own name — origin had landed
  a `user/vport`-based clipboard agent (`c7d7b93fe`, from the 2026-09-27
  wind-down) while this checkout independently built a `compositor/vdagent`
  crate for the same feature. Rather than merge two different
  implementations of one feature, the `vdagent` attempt is kept as
  `wip/clipboard-vdagent-compositor-approach` (pushed), and the
  `clipboard-vdagent` branch itself now matches origin. Left: decide which
  approach to keep, or whether `vport` already covers what `vdagent` did.

### br-004

- `stage13-timens` (worktree `timens`): most of stage 13's time namespaces
  staged (30 files, `fs/timens_check.rs`, `syscall/timens.rs`, vdso, futex,
  timerfd and procfs integration), blocked on an unresolved merge conflict
  in `src/kernel/src/panic/catalog.rs` — both sides add an adjacent catalog
  entry, FX-0907 and FX-0910. Left: take both entries, then regenerate
  `docs/generated/PANICS.md` and the two certification coverage JSONs that
  depend on it, before this can be committed.

### br-005

- A detached-HEAD worktree (`display-show`, at `d9c5658ed`) has one
  uncommitted line: `compositor/blank/src/modeset.rs`'s `BACKGROUND`
  changed from a dark slate to pure blue, while the adjacent comment still
  reads "a dark slate, so a screen that is merely black is not mistaken for
  success." Reads as a leftover debug probe, not an intended change. Left:
  confirm and discard, or finish it and fix the comment.

### br-006

The second branch sweep of 2026-10-23 (customer: GitHub down to about a
hundred branches; `docs/REPO-FLOW.md`, "Branch clean-up"). Every
`feature/`, `wip/`, `mk/` and `chore/` branch with no commit for seven
days that no running agent uses was archived, after its line below was
written. Each line is what the branch did or wanted, from its unlanded
commits' subjects. Nothing is lost: the tip is the archive ref named, on
origin (`git fetch origin <ref>:<ref>`, or `git push origin
<ref>:refs/heads/<name>` to bring the branch back). A doc that still names
a branch by an older name finds it with `git ls-remote origin
'refs/archive/*<name>'`. Snapshots and measurement or diagnostic branches
that said they were never to land were archived the same way without a
line here; `docs/repo-flow/`'s map lists them.

- **ab**
  - idea: ferrix-slots: the loader's words parse back, and AB-BOOT says what rollback does; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/feature/ab-boot`
  - idea: 3 (+2 more commits); last commit 2026-10-11; archived as `refs/archive/2026-10-23b/feature/ab-sys-wip-backup`
- **ahci**
  - idea: sysml, certification: the requirements say what fbcon, the output fallback and BAR widening do (+1 more commits); last commit 2026-10-16; archived as `refs/archive/2026-10-23b/feature/ahci`
  - idea: sysml, certification: the requirements say what fbcon, the output fallback and BAR widening do (+1 more commits); last commit 2026-10-16; archived as `refs/archive/2026-10-23b/feature/ahci-n150-kernel`
- **bm**
  - idea: Reserve L.quiesce.6 for H4, a USB stick behind the block ring; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/bm-h4-reserve`
  - idea: usb-host: 16-byte SCSI commands, a refused status tag, a caught short read; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/bm-h4-usb-storage`
  - idea: H6: Cargo.lock for ferrix-efifb (+1 more commits); last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/bm-h6-gop`
  - idea: AB-BOOT.md §6.4: the design for ferrix.panic=reset, for review; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/bm-nvme-panic`
  - idea: NVMe namespaces are nvme<C>n1, and their partitions nvme<C>n1pN; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/bm-nvme-parts`
  - idea: Reserve requirement ids for STOP at power-off and ferrix.panic=reset; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/bm-nvme-reserve`
  - idea: NVME.md §2.1: the design for STOP at power-off and reset, for review; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/bm-nvme-stop`
- **bs**
  - idea: Kernel split S3: the code review's conditions (ledger 1777, 1795) (+2 more commits); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/bs-a-s3b-pre-rebase4`
  - idea: S3: IPC record, release GOT figure (+17 more commits); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/bs-a-s3b-wip-1`
  - idea: Kernel split S3: rebased on main 814a95809 (+4 more commits); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/bs-a-s3b-wip-2`
  - idea: Kernel split S3: the code review's conditions (ledger 1777, 1795) (+3 more commits); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/bs-a-s3b-wip-3`
- **bs-a-s3b**
  - idea: S3: rebase onto main 51c8be4a6 in progress (does not build: device/check.rs names discovery::firmware) (+3 more commits); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/bs-a-s3b`
- **btrfs-enospc**
  - idea: SCRATCH: a large host-file volume written through the writable mount; last commit 2026-10-14; archived as `refs/archive/2026-10-23b/btrfs-enospc`
- **clipboard**
  - idea: Give the guest a clipboard agent, carried into the initramfs and started by hyprland; last commit 2026-10-01; archived as `refs/archive/2026-10-23b/wip/clipboard-vdagent-compositor-approach`
- **cm**
  - idea: ferrousli at f70f78d: the C tests' sync wait (4cbaf4b) on top of the loader's DT_RPATH (113c587); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/cm-cd-stack`
- **ef**
  - idea: SCRATCH: a kept Endfield volume boots with the yserver it was made with (+2 more commits); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/ef-night`
  - idea: SCRATCH: a kept Endfield volume boots with the yserver it was made with (+3 more commits); last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/ef-night2`
- **endfield**
  - idea: nvrm: a card with no monitor gets a screen from the EDID run-compositor --edid gives it (+8 more commits); last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/endfield-card`
  - idea: test-unity WIP squashed; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/endfield-s3`
  - idea: trace getaddrinfo and the Qt thread (+9 more commits); last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/endfield-s4`
- **everything**
  - idea: --everything carries the preinstalled Steam library whenever the host has it; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/feature/everything-preinstall`
  - idea: --everything carries the preinstalled Steam library whenever the host has it; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/feature/everything-try`
- **fx1012**
  - idea: Coverage evidence carried over the stack of 2026-10-12's last landings; last commit 2026-10-12; archived as `refs/archive/2026-10-23b/feature/fx1012-stack`
- **g24**
  - idea: yserver pin: b1bfb042, an input handed to X with the time the device stamped it; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/chore/g24-i1-pre-rebase`
  - idea: GATING ONLY: pin yserver i1-input-time b1bfb042, not to land (+2 more commits); last commit 2026-10-14; archived as `refs/archive/2026-10-23b/feature/g24-i1-wip`
- **gap**
  - idea: ; last commit 2026-10-12; archived as `refs/archive/2026-10-23b/feature/gap-gone-checks`
  - idea: ; last commit 2026-10-12; archived as `refs/archive/2026-10-23b/feature/gap-gone-checks-2`
  - idea: Raise SEGV_ACCERR for a fault its region refuses, as Linux does (+1 more commits); last commit 2026-10-12; archived as `refs/archive/2026-10-23b/feature/gap-sigpaths-e2e`
  - idea: test-init: the su stage judges each status as well as what su says; last commit 2026-10-12; archived as `refs/archive/2026-10-23b/feature/gap-zinc-su-status`
- **land**
  - idea: components: ferrousli at f0c1d55, apps at b83b851; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/chore/land-devd0-r`
  - idea: F-46: the port onto main, code as it will land; docs and message follow; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/chore/land-f46-controls`
  - idea: components: ferrousli at f0c1d55, apps at b83b851; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/chore/land-pins-r`
  - idea: bench-pipe: the tests module last, after test_pipewait (+9 more commits); last commit 2026-10-11; archived as `refs/archive/2026-10-23b/chore/land-pipe-p2-replay`
- **mm**
  - idea: Reserve L.object.188-189 for mm-pcache: writable-mount reclaim and the cache totals in /proc/meminfo; last commit 2026-10-14; archived as `refs/archive/2026-10-23b/feature/mm-pcache`
- **n150**
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-16; archived as `refs/archive/2026-10-23b/feature/n150-wc`
- **n3b**
  - idea: chardev self-check: a fence's signal wakes its queue; SYNC_IOC_FILE_INFO through the program's memory with no array and with one entry, flags (EINVAL), a bad pointer (EFAULT); MERGE ENOTTY; no read, no mapping (S6, S9). test-boot x86_64 kvm: 14 sync_file refusals (+11 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/n3b-gbm`
- **n3c-vtest**
  - idea: N3c: vtest moves socket bytes in 256 KiB pieces and names failed transfers; FERRIX_VTEST_PROTOCOL reaches hyprix under --nvidia (+6 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/n3c-vtest`
- **nv**
  - idea: N3c: vtest moves socket bytes in 256 KiB pieces and names failed transfers; FERRIX_VTEST_PROTOCOL reaches hyprix under --nvidia (+6 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-cardvm`
  - idea: FERRIX_DRI3_PROBE: yserver's warnings and errors, and every pixmap line, in the serial log (+9 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-dri3`
  - idea: N3c: vtest moves socket bytes in 256 KiB pieces and names failed transfers; FERRIX_VTEST_PROTOCOL reaches hyprix under --nvidia (+6 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-mapslots`
  - idea: hyprix: the import line says the modifier and whether the buffer is sampled or mapped (+40 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-modifiers`
  - idea: Handover 2026-10-10: Teeworlds reads 75 to 80 in full screen, 160 at most (+43 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-next`
  - idea: nvrm quiesce: three defects a second reading found, and a switch that leaves it out (+8 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-quiesce`
  - idea: Name-only dmabufs: clippy-clean self-check and size test (+7 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-vram-dmabuf`
  - idea: (wallpaper): FFmpeg leaves out two device extensions nvrm cannot serve (+6 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-wallpaper`
  - idea: hyprix says whether NVIDIA draws into nvrm's buffer in place; xtask carries FERRIX_VTEST_SCANOUT (+12 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nv-zerocopy`
- **nvidia**
  - idea: (N3b): the dmabuf self-check gives vmo_write its offset in memory (+2 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/nvidia-n3b`
- **r1**
  - idea: Pin yserver at the Present clock (2966c377), for test-frames: drop before landing; the product owner lands the pin (+3 more commits); last commit 2026-10-14; archived as `refs/archive/2026-10-23b/feature/r1/before-squash`
  - idea: Pin yserver at the Present clock (2966c377), for test-frames: drop before landing; the product owner lands the pin (+3 more commits); last commit 2026-10-14; archived as `refs/archive/2026-10-23b/feature/r1/frames-pinned`
  - idea: test only: pin yserver at the Present clock (2966c377), for test-frames (+2 more commits); last commit 2026-10-14; archived as `refs/archive/2026-10-23b/feature/r1/frames-yclock`
- **rn**
  - idea: run-nazuna: Ferrix installed on a copy of nazuna's SN720, on a machine shaped like nazuna and described by its ACPI tables; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/rn/run-nazuna`
- **run**
  - idea: NAZUNA-VM.md: the board's ACPI tables, how they were checked, and the boot on them (+9 more commits); last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/run-nazuna`
- **s13f-seccomp**
  - idea: S6: dispatch the booted musl tests through one list in sem.rs; last commit 2026-10-14; archived as `refs/archive/2026-10-23b/chore/s13f-seccomp/pre-rebase-4cd46c706`
  - idea: S4: carry the coverage anchors, regenerate the pages; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/chore/s13f-seccomp/pre-rebase-b2090d734`
- **s13f-timens**
  - idea: Time namespaces: the design's where it stands, the controls by INDEX tag, the backlog and the vulnerability rows (+2 more commits); last commit 2026-10-14; archived as `refs/archive/2026-10-23b/feature/s13f-timens/pre-reservation-9be83c667`
- **s13fx**
  - idea: 8: CGROUPS.md §15, the exit test and its controls (INDEX lines owed) (+8 more commits); last commit 2026-10-12; archived as `refs/archive/2026-10-23b/chore/s13fx-pre-rebase`
- **s4port**
  - idea: fixup! S5: record TSYNC, its evidence and controls (+5 more commits); last commit 2026-10-11; archived as `refs/archive/2026-10-23b/feature/s4port-pre-squash`
  - idea: S4: carry the coverage anchors, regenerate the pages; last commit 2026-10-11; archived as `refs/archive/2026-10-23b/feature/s4port-pre-squash2`
- **softgl**
  - idea: N3c: vtest moves socket bytes in 256 KiB pieces and names failed transfers; FERRIX_VTEST_PROTOCOL reaches hyprix under --nvidia (+6 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/softgl-gap`
- **stage13**
  - idea: 8: CGROUPS.md §15, the exit test and its controls (INDEX lines owed) (+8 more commits); last commit 2026-10-12; archived as `refs/archive/2026-10-23b/feature/stage13-exit`
- **steam**
  - idea: (N3b): the kernel's chardev render node and dmabufs, hyprix maps NVIDIA's dmabufs (+1 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/steam-nvdesk`
  - idea: futex: FUTEX_WAKE_OP, and say which futex operation was answered ENOSYS; last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/steam-stall-wakeop`
  - idea: snapshot: Steam window harness into the repo (+1 more commits); last commit 2026-09-29; archived as `refs/archive/2026-10-23b/wip/steam-e2e`
  - idea: snapshot: N2 bind mounts, host tests in progress (+1 more commits); last commit 2026-09-29; archived as `refs/archive/2026-10-23b/wip/steam-userns`
  - idea: snapshot: WM_NORMAL_HINTS, a fixed-size window floats; last commit 2026-09-29; archived as `refs/archive/2026-10-23b/wip/steam-y6`
- **steam-e2e**
  - idea: run-steam, hyprix + yserver + Valve's Steam client from a volume; last commit 2026-09-29; archived as `refs/archive/2026-10-23b/steam-e2e`
- **steam-userns**
  - idea: N2 binds; last commit 2026-09-29; archived as `refs/archive/2026-10-23b/steam-userns`
- **tv**
  - idea: run-compositor --nvidia --everything: Teeworlds in a 1280x720 window at low detail, with hyprix composited on the 3060 (n3c merged) (+7 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/feature/tv-teeworlds`
- **win**
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-16; archived as `refs/archive/2026-10-23b/chore/win/land-ahci-1`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-16; archived as `refs/archive/2026-10-23b/chore/win/land-ahci-2`
  - idea: sysml, certification: the requirements say what fbcon, the output fallback and BAR widening do (+1 more commits); last commit 2026-10-16; archived as `refs/archive/2026-10-23b/chore/win/land-ahci-3`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/ebs-1`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/ebs-2`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/ebs-3`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/ebs-4`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/fbcon-6`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-220620`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-221632`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-223137`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-223234`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-223310`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-223437`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-223801`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-223906`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-224218`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-224436`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-225729`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-230142`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-231152`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-233339`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261015-234710`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-16; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261016-000108`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-16; archived as `refs/archive/2026-10-23b/feature/win/n150-esp-20261016-001009`
  - idea: coverage: carry the evidence's anchors to ahci's tip; last commit 2026-10-15; archived as `refs/archive/2026-10-23b/feature/win/x2apic-10`
- **wine**
  - idea: x86-64: a 64-bit signal frame carries the fault's trapno, err and cr2 (+1 more commits); last commit 2026-10-10; archived as `refs/archive/2026-10-23b/chore/wine-m1-before-rebase`
  - idea: test-wine: a terminal beside notepad, not xev (WIP) (+4 more commits); last commit 2026-10-13; archived as `refs/archive/2026-10-23b/chore/wine-vnc-input-rebase`
  - idea: test-wine: the terminal runs sleep, whose title stays (WIP) (+5 more commits); last commit 2026-10-13; archived as `refs/archive/2026-10-23b/feature/wine-vnc-input`
- **yserver**
  - idea: fetch-yserver: the fork rebased onto upstream's master of 2026-10-12; last commit 2026-10-13; archived as `refs/archive/2026-10-23b/chore/yserver-rebase-pin`
