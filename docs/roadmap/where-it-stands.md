# Where it stands, in full

*reviewed 2026-11-02, to `main` e51fc67fd: the first section holds the
landings of 2026-10-21 to -31 and the second those since 2026-10-09 (reviewed
to `main` 2ba25d12a on 2026-10-21); the older sections are corrected where
`main` has overtaken them. Dates are the day a commit reached `main`.* The short
version is on the [overview](README.md).

## Landed 2026-10-21 to 2026-10-31 (`main` e51fc67fd)

About 650 commits. `develop` and `main` are the same commit, e51fc67fd (the
Promote workflow fast-forwards `main`; 700373fb5 on 2026-10-27 made `main`
an ancestor of `develop` again after the eight promotion merge commits of
before 2026-10-24). What is still on a branch is at the end of this section.

* **The repository flow** (`docs/REPO-FLOW.md`, the customer, 2026-10-23).
  Landings go to `develop`; `main` moves only by promotion: the Promote
  workflow cuts `promote/<sha>` from `develop`, posts `flow/lane` and the
  certification consultant's `certification/verdict`, dispatches CI, and
  fast-forwards `main` when `CI result` and both statuses are green. No
  pull request is opened for it since 2026-10-24 (f76dc4d31, 217fa0ce9).
  `tools/common/release/cert-verdict.py develop..<branch>` shows what a
  promotion would wait on; a commit that touches the item needs its review
  named in the message or a `Certification:` line. The second branch sweep of
  2026-10-23 deleted or archived 102 idle branches
  (`docs/backlog/branches.md`).
* **v0.2.0, the first live ISO** (tagged, notes dated 2026-10-29,
  `docs/RELEASES.md`, `docs/TRY-FERRIX.md`). `cargo xtask iso` writes a
  hybrid ISO in Rust (dc95c66f8, 2026-10-25): Yoru from memory as uid 1000,
  the graphical installer as the one window at login, the licence inventory
  in it. A PC shows the desktop on the firmware's framebuffer
  (`ferrix.efifb`, `ferrix.fbcon`, ac9eae331); the live session drives a
  machine with no IOMMU and the installed one never does (a4afbfd74); ACPI
  S5 switches a PC off (04d72384b) and the seat's person may restart and
  power off the machine (4a17b1512); the ISO refuses a developer desktop's
  host programs (93b0f3950). Checked on QEMU machines shaped like a PC; not
  booted on a real PC, VirtualBox or VMware (the notes say so, with what does
  not work: installing from a CD or a USB stick, USB as the only input, three
  Ethernet chips).
* **lode, the log manager** (`docs/LOGGING.md`, the customer's go on
  2026-10-23). The native service `/sbin/lode` in a sealed job with
  `ferrix-lode-core` (`src/lib/log/lode`), its store (CRC-checked segments,
  zstd when sealed), `journalctl`'s reader and options, the filter language,
  retention, floods held and said, session ids, the Linux front (`/dev/log`,
  the journal's sockets, stdout streams), crash records that name their
  process, crash screenshots as blobs, tombstones (steps 1 to 3: the blob
  container, hang snapshots, crash storms cut), `lode top` and alerts,
  store-and-forward with TLS to a collector (`lode-receive`, `cargo xtask
  box-log`) and a reload for retention and capture (a95b55d1d to bac0955e0,
  2026-10-23 to -24). The kernel log is Linux's records: times through
  `syslog(2)`, `/dev/kmsg`, `log_buf_len`, and `dmesg` is Ferrix's own
  (4c3087f43, 815604fa7). Not built: backtraces (the stack capture), `-o
  short-delta` and the rest §20's table lists as missing.
  The log viewer is an app in `ferrix-os/apps` (89e1b3075).
* **tole, the UI framework** (`docs/UI-FRAMEWORK.md`; a repository of its own,
  `ferrix-os/tole`, since 2026-10-23, pinned in `components.toml`). A
  QML-shaped tree compiled into Rust at build time: U1 to U7 (bindings, the
  `.tole` compiler, signals and the pointer, damage and focus, layouts and
  models, states and animations, controls with the theme, popups, canvas
  painters) landed on 2026-10-23, and U8 (the lode viewer) and U9 (Settings)
  on 2026-10-24, a GPU renderer with a fallback on 2026-10-24 and its pacing
  with it. Settings, the file manager `shelf`, the clients of Yoru and the
  lode viewer are drawn with it.
* **Settings and the settings writer** (`docs/SETTINGS-WRITER.md`,
  `docs/RUNTIME-SETTINGS.md`). `settingsd` is root and socket-activated: a
  closed enum of typed requests, an `authd` conversation for the area, one
  audit line (80f9ef39c, B1: `set-hostname`, `service-enable-persistent`;
  1113d698b, B3 and B6: `set-timezone`, and service-enable-persistent's `now`). The
  trusted prompt is hyprix's: every screen is the compositor's alone while it
  is up, only the seat's devices answer, and no capture path shows it
  (21ba98dce). init sets the host name from `/etc/hostname` and makes
  `/etc/machine-id` (72cbc95ed, 30e2afb76); `runtime_settings` and the
  removable crate `ferrix-kernel-settings` are in (ceb8d678c).
* **The GNOME-like desktop on hyprix** (`docs/DESKTOP.md`, all four parts):
  floating mode with snapping, the activities overview and minimize,
  the Ferrix light theme and windows that draw their own frame; screens that
  come and go (hotplug, output management, an arrangement kept per set of
  monitors); `notifyd` and a D-Bus (`compositor-dbus`), with drivers' deaths
  told as banners through `devmgr-events` (1f92468ad). On the Intel
  display, hotplug on the lit port, the monitor's EDID over GMBUS and a
  modeset of the second HDMI port (3h h1 and h2, under
  `ferrix.intelgpu.modeset=on`) with a second scanout on `card0`; the box
  run of 3h is the coordinator's and is not recorded. Every display driver
  lists every mode, the core sets only a listed one, and hyprix changes mode
  while it runs (`docs/DISPLAY.md` §9, 9475d13ea).
* **Stage 25, Yoru** is as the stage's page says (reconciled 2026-10-25),
  and the live ISO's and the N150's desktop are Yoru's (e4ee1da17); on
  2026-10-28 hyprix learned a maximized window without a shadow, a dragged
  window that keeps its snap distance, and a fullscreen window that covers
  the top layer.
* **hyprix draws on the GPU in a helper process** (`docs/GPU-COMPOSITOR.md`).
  `compositor-gpu-protocol` (the records hyprix and `hyprix-gpu` exchange,
  with `MODIFIER_MEMFD`) and `hyprix-gpu`, a glibc program that draws the
  compositor's frames with Vulkan on the device tole picks (5c253d3cc,
  2026-10-28), a client's dmabuf drawn once its writes are done
  (c91bc141d) and a boot of it in `test-compositor`. hyprix's own option to
  start the helper is on a branch (below).
* **Suspending one program** (`docs/APP-SUSPEND.md`, S1, 2026-10-30):
  `hyprctl dispatch suspend [window|pid:<n>]` freezes the program's
  processes with `cgroup.freeze`, `resume` thaws them and puts them back in
  the cgroup above (7e1dd7a88, df789b033); both shipped themes bind
  SUPER+Z and SUPER+SHIFT+Z. Measured on Endfield on the N150 (12c879584).
* **The N150 box and its Intel GPU** (stage 21). Large pages: `vmo_pin`
  takes a `PIN_BLOCKS` option and a pin's absent pages come in aligned
  blocks (95560643f), the GEM objects are mapped with 2 MiB and 64 KiB GPU
  pages (6230ce313, `intel-xelp` 56586c895), and the IOMMU takes 2 MiB
  VT-d entries for a pin's aligned runs, taken down whole (c735f308e,
  `docs/INTEL-GPU.md` §14); with them Endfield's frame rate on the box is
  unchanged at about 27 (`docs/GAME-MODE.md` §14), so turning the IOMMU
  off is not recommended. `card0` copies on BCS0 (459853668), evicts a
  GPU-written buffer's lines before copying (eb555dc28), blends the pointer
  into the plane (f26fb6da6) and copies a frame in pipe A's vertical blank
  (da674893d). The driver is started again when it dies, its keeper
  (`intel-xelp`'s journal) outlives a worker it replaces, it releases
  forcewake a dead driver held (515069070), and `devmgr` backs a crash
  loop off, reinitialises, then quarantines the device (07f59338a).
* **NVIDIA's card** (stage 21, `docs/NVIDIA.md`). N3c landed on 2026-10-25:
  hyprix draws on the RTX 3060 through virglrenderer's test server on
  NVIDIA's EGL (83cb9cc88 to 6db4623db), and `run-compositor --nvidia` keeps
  the software compositor unless `FERRIX_NVIDIA_VTEST=1`, because Chrome's
  WebGL page is faster under it. `nvrm` follows a monitor plugged, unplugged
  or swapped (ccfad5570), quiesces the adapter when `/run/nvrm-quiesce`
  appears (b5a59302d; on the card it shut down in 1.45 s with no Xid) and
  imports sync files on its dmabufs (3059a21c4); `devmgr` lets `nvrm` drive
  one GPU, never two (3c529c58b).
* **Drivers from ring 3, and USB serial.** FTDI's UARTs and the USB-Blaster
  from a class driver process of their own (97de20f7a to 23158676a,
  2026-10-25; `docs/USB-SERIAL.md`), with the three kernel items its review
  asked for (numeric pty speeds, back-pressure and flow control, hang-up on
  the master's close: 7829e7fa7, 2eba5cddc, eedc89b6a); a ring-3 driver for
  nazuna's onboard RTL8111H (`rtl8169`, cb3d2ba93); a disk that stops
  answering is taken offline (1171f63af). `docs/CHARDEV.md` and
  `docs/DRIVER-MODEL.md` ("a driver never needs a kernel change", the
  customer's rule of 2026-10-25) are designs, reviewed and not built.
* **Processor power** (`docs/POWERCTL.md`). `ferrix.vrcurrent` is replaced
  by a power core of typed calls on a claim with per-model bounds
  (3465911c2) and a ring-3 power driver with its policy library that `devmgr`
  starts (abf758e42): a plan `ferrix.power.vr=A1,...,An@S`, a back-off on
  temperature, and `auto:LO-HI`, a plan that finds what the cooling holds
  (1af3292dc, d5cf8796b); `ferrix.msrlog` logs the power-limit MSRs
  read-only (f452e8b67).
* **Machine checks, and memory that fails** (customer, 2026-10-30; one design
  document each, every one reviewed by the consultant before its code).
  `docs/MACHINE-CHECK.md`: banks read and enabled on every processor, a log of
  hardware errors, `/proc/ferrix-mce`, and a bad user page that kills the
  program that read it instead of the kernel (83dec24a1, under
  `ferrix.mce.recover`); on AMD the vendor's own rules (Linux 6.18's
  `mce_severity_amd`), SMCA's banks and `McaX` (55b750b00), where Intel's
  rules had stopped the kernel on every uncorrected error of a Ryzen 9 9900X;
  CMCI
  interrupts a processor at once on a corrected record (b12045c1a); a system
  call's copy that consumes poison fails the call and the program gets
  `SIGBUS` (1308245de); poison in a clean cached page costs a disk read
  (2464379ed); a ring-3 driver that consumes poison ends alone and `devmgr`
  starts it again (7352a897a); a bad frame stays out of the next boot after
  a warm reset (368e7f5e9, pstore header version 3) and across a power cycle
  (fa6e08d0d, in the boot disk's event log), switchable as `off`, `reset` or
  `persist`, fixed in the `max` profile and chosen by a boot word in
  `desktop` (e51fc67fd). All eight of the customer's follow-ups of 2026-10-30 are on
  `main`. Intel's recovery has run under TCG only, since no Intel processor at
  hand reports `MCG_SER_P` (`docs/MACHINE-CHECK.md` §7), and the AMD path's
  run on the 9900X is a BACKLOG row. The RAM scrubber tests free frames while the
  machine idles and retires a frame that fails twice (37b8a6701,
  `docs/RAM-SCRUBBER.md`); an unstable core is taken out of service and
  brought back by `echo 1 > cpuN/online` (805b289b1, `docs/CORE-QUARANTINE.md`).
* **Memory under a game's load** (`docs/RAM-RELEASE.md`, `docs/HEAP-CACHES.md`,
  `docs/PCID.md`, from Endfield on the N150). Per-processor heap caches in
  front of the heap's lock (e4d44f24a); R1, the initramfs and the tmpfs's
  copy given back once the root is final, about 195 MB on the box's image
  (7d25922f3); `/proc/<pid>/status` splits the resident set by kind
  (ea54ed162); the machine-wide OOM kill, and a pin that reclaims as a fault
  does (86b79a87d); a buffer of many pages that a full heap refuses with an
  error instead of a panic (062fa1664); the large-block check retries when
  another processor moved the count (b433cedcd); PCIDs, on where every
  processor has them (94d0c9df0, FX-0410 if one does not); a task woken
  from a call is pulled onto its waker's processor (48d0efdc5, L.sched.90-91)
  and onto an idle one when its own is busy (6a919687e, L.sched.85).
* **Game mode** (`docs/GAME-MODE.md`, `docs/GAME-MODE-SCHED.md`, 2026-10-30
  and -31): a design in tiers with its measurements on the N150 (at the
  settings Endfield is played at the GPU is the limit; freezing, nice values
  and the protections each buy about 1% of a core and no frame; the GPU's
  4 KiB pages cost 15 to 25% of its shader bandwidth, which the large pages
  above went after). Built so far: T0h's card0 copy on BCS0 (459853668), and
  layer B's second piece, `sched_setaffinity` that pins (3e77c0df6, F3) and
  a fair task's own slice request (2e717f409, F7), both measured on 2026-10-31
  with no effect beyond the noise (`docs/GAME-MODE-SCHED.md` §7.1). The frame
  mark, lends, the idle-gap class, deadlines and the session service are
  designed, reviewed and have no code.
* **Certification and signing.** The write path's clean-node cache is
  dropped between operations only, F-99 closed (d0b034524, 60559327d);
  `/proc/<pid>/maps` in page-sized blocks charged before each is taken
  (F-97, 7a0a357d3); sealed jobs let no process leave a driver's job but by
  ending (F-92, 1b2040006); the register stands at 30 open and 68 closed, of
  98 (`docs/certification/FINDINGS.md`, which owns the count). Product signing
  (`docs/SIGNING.md`): S1, `ferrix-sigverify`, `ferrix-sha256` and the soft
  signer (546080e4b), S2, the verifier's time measured on the DK1
  (99db9e544), and S3, the anchors and the family registry that keep
  development keys and the verifier out of a release (2b04af0cf), S4, the UEFI
  loader verifying a slot's signed manifest against anchors it carries
  (3c0875b4a, 2026-10-25), and the CI development key (56bc97bbb); `present`
  keeps slot signing off until a release key exists, and S5 (the TPM path) and
  S6 (the release key and its runbook) are not built.
* **The Linux ABI behind a build flag** (`docs/LINUX-ABI-SEPARATION.md`
  §11): `linux_abi = on | off`, `off` an ad-hoc build at level none with no
  claim (f5307143b, 2026-10-23). The kernel is four crates, `ferrix-kernel-core`,
  `-item`, `-load` and the binary (ffd75a08e, `docs/KERNEL-CRATES.md` S3).
* **Stage 13's follow-ons** (`docs/roadmap/follow-ons.md`, 2026-10-26): time
  namespaces (2daef95d2), unprivileged mounting and `test-bwrap` as uid 1000
  (3a65627b9), the dentry and inode caches' reclaim (6baabc374), Linux's
  `seccomp_bpf` selftest (69f52ecec), and `memory.reclaim`, `cpu.max.burst`,
  `cpu.idle`, `io.weight`, `io.latency` and `memory.stat`'s `anon` and
  `pagetables` (L.object.182 to 187). The gate host's runs each commit named
  as owed are not recorded in the tree, and the landing left `develop` red
  until eb2ddbfd6.
* **Smaller landings.** `ETXTBSY` (66eb95da0); `/proc/cpuinfo` as Linux
  writes it (4b521bd25); a program's `int3` and `into` through Linux's gates
  (a2623409d); AArch64's branch-history plan under a hypervisor covers every
  core a vCPU may run on (66cb8ee90); `AF_UNIX` streams queue a
  small send whole (ef47ac330); AHCI reads ahead (59b86eeb0); `port_wait`
  trusts its wakes, so idle drivers no longer wake 200 times a second
  (e7977992d); the dead are freed while no
  processor is idle (F-95, landing 2, 7fdc1c99d).
* **Still on branches, so not landed.** The roadmap branch
  `claude/ferrix-roadmap-completion-hkxlpp` carries stage 14 work past the
  stage 13 follow-ons (61 commits ahead of `develop`, 2026-10-28, its
  scheduler's `SoftRt` domain among them) and `feature/sched-softrt-domain`
  is the same line; `feature/hyprix-gpu-backend` (hyprix's `hyprix:gpu`
  option and the helper's environment lines), `land/hyprix-gpu-backend`
  (`test-compositor --boot helper-desktop`), `feature/csd-decorations`,
  `feature/mk/*` (the microkernel's M1, unchanged since 2026-10-21; its rebase and the rest of the 2026-11-03 run's work are on local `feature/mk-*` branches, unlanded),
  `park/f95-option-a` (F-95's landing 2 conditions), `wip/stability-try`
  (the nightly stability workflow) and `docs/intel-display` (a design paused
  by 3h). `docs/roadmap/open-branches.md` has the 2026-11-02 count.


## Landed 2026-10-09 to 2026-10-21 (`main` 2ba25d12a)

* **The switch to a microkernel begins** (`docs/MICROKERNEL.md`, the
  customer's go for M0 on 2026-10-13; [M0's page](microkernel-m0.md)). The
  plan went through fifteen revisions under the certification consultant's
  reviews. On `main`: the build-flag system with its four kernel profiles
  (`present`, `desktop`, `fast`, `max`; the last two are validated, not built,
  until M5), the resolver, the boot line the gate compares with the profile,
  `checks=off` and the load ratchet (2026-10-17, d98ab2314, b91a85311); a
  type-check of the item with upstream Rust 1.95.0, the release Ferrocene 26.05
  is built from (e56d1a75c; Ferrocene's own compiler is owed by the customer's
  subscription); F-73 (every x86-64 processor sets `CR4.MCE`, d9c93c7c9) and
  F-64 (505c5aa1b), both on 2026-10-21, which `smt=off` and
  `program_barrier=session` waited on; and `bench-rows`, the tooling for the
  performance baselines (40996829f), whose baselines are not taken yet. M1's
  designs K5, K6, K7, K10 and K12 are on `main` with the consultant's OK IF and
  their ids reserved (2026-10-19); the code is on `mk/*` branches, none
  gated, about 33 of about 70 points spent (`docs/BACKLOG.md`). The ids of M2's
  K2, K3 and K4 are reserved (2026-10-21, 2426e7038). The overnight run of
  2026-11-03 landed B5's orphan step (b4a6b3ff9, `docs/microkernel/B5.md`): a step
  of `xtask check` fails when a kernel requirement is verified only by checks
  that leave with the load; it lists 135 ids and holds 299 more on raised check
  files by a ratchet. The id blocks of the run's agents are reserved
  (bfc8a5f00, 709c13e91). Landed since: K6 (kernel-attested sender, 671ad76a8), `smt=off` (b8a57534b) and F-91 (an ended process gives its memory back, 95e5e0e1c), each with its derived files; K17's high-level requirement and K9's scheduler ids are reserved (7818d5b82); B5's class A re-homing (3e6bc2f0e, 27ed2c039, 4fb6e5fa3, bd64fc419, 2026-11-04): the orphan list falls from 135 to 70 ids, 65 re-homed to core or item checks and 18 of the 135 moved to class B, with a BACKLOG row for the `linux_abi=off` rows B5's checks owe; mk-ci's cut M3 (05347c1a3, 68b54b00c: the kernel's lint directories without incremental state, 6,885 to 511 MB per check, control fired) and `gate.sh` keeping xtask's incremental cache in its trim (276f2fde8); **K10 (9f99d9cd5), K4 (landings 1 and 2, 694ab07a8 to 50487ef7c), K5 (4472c8239), K2 (slices A and B, 4d0a661ee, 7908891c0) and K3 slice A (pager-backed VMOs, 46fafb730)**, each with its design text and derived files, up to df6a06e6f (2026-11-04, the consultant's rulings at ledger 6355 to 6365); B5's list at 69 ids (`drivers.slice`, 1fcd9afb2); `gate.sh`'s requeue, and why the pool went back to five slots (27f314070); **K8 and K14 §2 to §4** (device authority by handle, sealed and measured images; 28ae06a61 to 40ad1851b, with the consultant's J.3, J.5 and J.6 of ledger 6426), the second id block of mk-authority (K16, M8's kernel half; 1231194d7), the tests' GPU work pinned to the RTX 3090 and a pin that selects nothing failing (6467a26fc, 9f15630d7), and a kept incremental cache going after an hour unwritten (d8928fdc4), up to 40ad1851b; mk-ci's cuts to the check and the gate hosts (clippy settings at once, side workspaces without incremental state, `gate.sh` trimming a slot's caches; e1725720e, a532f7d6e, f7034b37a, 2026-11-04) are in. `develop` is b7338d734. Everything else of that run is on the local run branches, unlanded and ungated (the agents' notes of 2026-11-04 10:15, not `develop`): `feature/mk-land2` (develop plus K7, the compare command and K3B, joined as `mkb2-k7` and `mkb2-k3b`; K12a/b, DF6 and BF6 are to be rebased on top; BF6 is NOT YET W1 to W9); `feature/mk-m2` (K15 and K13 joined as batch `mk-m2-kk-b1` at 31a6d3bcf on the consultant's OK IF, ledger 6438; being rebased onto the K8/K14 stack with a measured `process_make`; K3b on `feature/mk-m2-k3b`); `feature/mk-restricted-land` (953a72373, K1, K11 and K17 on `develop` without K7, checks and 73 controls queued; K9 §1 on `feature/mk-restricted-land-k7`; ledger 6282 held K1 and K11 at NOT YET R1 to R8); `feature/mk-lift` (`linuxd`: `test-shell`, `test-btrfs`, `test-net`, `test-vfs`, `test-threads` through `gate.sh`, and by hand `test-chrome`, `test-yserver`, `test-settings`, `test-apps` and `test-steamcmd`; Wine 32 is blocked by K11's layout; `test-selfhost` needs a longer limit); `feature/mk-boot` (boot authority, the partition manager, `btrfsd`, M6 cuts and `netd`, current with `mk-lift`, landing inside `mk-lift`'s as its own commit, ledger 6343; `PROCESS_CREATE` and `PROCESS_START` are out of the micro kernel there) and `feature/mk-k8b-devel` (K8b, the consultant's K.1 to K.6 met, `k8cd-check` running); and `feature/mk-perf` (the restricted-exit levers: CPUID, wake under the lock, unmap skip, a window unmap that names its leaves; ledger 6421 OK IF U1 to U6, U4 to U6 owed; its paired Linux-program figures, five boots on a loaded host (load 22 to 44), minimum ratios against the hybrid kernel: `getppid` 630 against 120 ns (5.25x), `mmap`+`munmap` 2,030 against 1,250 ns (1.62x), a touch 2.36x, a signal 2.28x, `futex` 3.07x, fork+exit 1.35x, fork+exec 1.38x, a miss 1.19x; **branch-only and noisy, a busy SMT sibling makes `getppid` bimodal at 630 and 830 ns, and none is a baseline**; the lazy fault exit is parked on `feature/mk-perf-fault-lazy`, ledger 6458, and removing the TS flag work is NOT YET, ledger 6464). Landed since, 2026-11-04 (776b56a6e, rows through gate.sh): K15 and K13 (d89c2b59f), K7 (5cfea606c), `cargo xtask compare` (e450fe51a, no clean full run yet), K3 slice B (9a5745c01), hyprix's busy cgroup, test-init on ARMv7-A, FX-0916's ended case (fl-087), a native process built in its `Arc` and `fire_timers` in place (F-101, mitigated). Then, on the night of 2026-11-04 to 05, each with its rows and controls through gate.sh: K1, K11, K17 and the ASIDs (cb2672199), K3b and K13's ticket case (f487fa8ff), SUD (fb568b5ba), the copy miss (1744cbb53), K12a and K12b (d121827e1), DF6 (68e6d48ad), BF6 (feb49e695, `desktop` on the fast path), M5's lift with boot authority (14ccf6a64: linuxd built from the load's one source; accepted dark at level `none` with no flip, ledger 6923: dark but for `.route` in `copy_to_user_through`'s present path, in `present` and `desktop` too), K16's audit (153b65ce4), K14 section 5 (cf83b1791), btrfsd and netd (c4d607c44), further partitions (bfb1026a0), and Bare's harness, B5's check host (1aa9bffbb: 23 ids home in the item, for `present` and `desktop` only until a caller is built in a user image). M6's cuts and K16's seal are still being landed; the flip of `fast` and `max` is not on this list and owes BACKLOG's rows.
* **Stage 13's exit is met** (`docs/CGROUPS.md` §15). `cargo xtask
  test-container` (f231cdf09, 2026-10-14) runs the criterion as one program,
  an unprivileged user namespace with a pid 1 under a memory limit with
  scoped reclaim and a scoped OOM kill, behind a seccomp filter, and passed
  on all three architectures with six negative controls fired. Seccomp's
  `TRAP`, `TSYNC` and `test-seccomp` (S4 to S6) reached `main` on 2026-10-18,
  and Syscall User Dispatch (`prctl` 59, for Proton's Wine, `test-sud`,
  `docs/SECCOMP.md` §13) with them. A writable mount's clean pages are
  reclaimed, `/proc/meminfo` counts the cache truthfully and a commit writes
  back in slices (f0bc9ab2d, 2026-10-15, L.object.188-190: the work of the
  branch `mm-pcache`, landed under another hash), which the Endfield download's
  ENOSPC asked for. Time namespaces, unprivileged mounting (N5) and a few
  controllers' files stayed on branches or unwritten then; all were built and
  landed on 2026-10-26 (below).
* **Ferrix on the customer's N150 box** (stage 21, `docs/N150.md`).
  Built from the branch `ahci`, it booted on the box on 2026-10-15 to a
  desktop on the firmware framebuffer, `/` on the SATA drive and the network
  up, with `gryphlink` showing the launcher. On `main`: the AHCI driver, the
  YT6801's network driver (`ferrix-dwmac`, `ferrix-yt6801`), `efifb` with
  hyprix on the firmware's framebuffer and the framebuffer write-combining
  (H6), the boot console on that framebuffer, a PC's xHCI controller behind
  `usbhid` with bulk-only disks (2026-10-11), an NVMe driver (2026-10-11),
  x2APIC mode and bounded PIT and HPET waits with a CPUID fallback for the
  clock, Intel HWP at bring-up and `ferrix.epp`, `CR0.NE` and `CR4.MCE` on
  every processor, the loader's SMBIOS copy shown in sysfs as Linux does (DMI
  D1), a kernel log kept across a warm reset (PM3) and an event-log partition
  the loader appends panic reports to (PM4), `ferrix.panic=reset`, the
  defences for RFDS and BHI (F-84, F-85), and VT-d driven on the box's two
  units (2026-10-20) with K2's kernel-held identity domain for the iGPU's
  RMRR. The kernel bring-up is evidenced on QEMU only and has not run on the
  box with checks on; the NIC and USB still poll, since the YT6801's one MSI
  arrives rarely and xHCI raises none (BACKLOG). On 2026-10-21: `/` is on the
  box's drive again, after the image's unpack stopped on a directory the drive
  held where the archive has a link and kept the system in memory; the unpack
  now moves such a directory aside to `<name>.ferrix-old` (94d6c39fa). The
  pstore header is version 2 with a stop word that says how the last boot
  ended, and `reboot(2)`'s restart writes the region back (S1, a0025e505).
  `docs/POSTMORTEM.md` §6 gains S4, a design for review: a lockup detector
  and the PCH TCO watchdog, so that the unattended box resets after a hard
  hang (68cc4a702).
* **An Intel GPU driver for the box** (`docs/INTEL-GPU.md`, design OK IF,
  ledger 2352): ring 3, following i915 file by file. On `main`: the kernel's
  render-only chardev node, the screen lent to the driver by identity,
  write-combining mappings and sync-file fences on a dmabuf's slot (K3 to K6),
  and `intel-xelp`, the i915 uAPI front end over a back end with the GT's
  bring-up, host-tested. On the box on 2026-10-20: M1 (the device answers) and
  M2 (workarounds, the engines resumed, the first GPU commands, a blit exact by
  hash); M3, the render node served over the GT back end, is wired and not
  shown on the box. The box hung hard twice under `ferrix.intelgpu` (the first
  cause was found on 2026-10-20), the GT's frequency and RC6 are set by the
  host, and `ferrix.intelgpu.uapitrace` finds a render-node call that never
  completes. The GT ran at 350 MHz with the request pinned at RP0 on the
  box, so 2026-10-21 brought i915's frequency policy for Gen12 without SLPC,
  the ring frequency table and forcewake per access (2ba25d12a), and the
  status page read past the processor's cache (6627e499f). Endfield's first run on it is prepared: the launcher on
  lavapipe, only the game on anv.
* **NVIDIA's dmabufs** (stage 21, N3b's kernel half, 2026-10-11): a chardev
  render node, VMO dmabufs, `sync_file` fences and name-only dmabufs of video
  memory that `nvrm` exports. hyprix drawing on the card (N3c) and an X
  window drawn by the card (`nv-next`) stayed on branches then; N3c landed on
  2026-10-25 (below).
* **Installing and releasing.** I3: `ferrix-install` makes the root volume at
  its partition's size with `ferrix-btrfs-mkfs` (2026-10-19). A/B system slots
  (`docs/AB-BOOT.md`, 2026-10-11): the loader boots a trial slot once and
  falls back to the good one, `bootctl` writes the idle slot, `cargo xtask
  test-ab-boot` judges it. The first numbered release, v0.1.0 (tagged
  2026-10-20), built none of its assets, because a host answered the release
  workflow's runners 418; each pinned download now names a mirror, and v0.1.1
  (2026-10-21, `docs/RELEASES.md`) carries the x86-64 desktop as a zip that
  plain QEMU boots, a licence inventory and the sources of its GPL programs.
  `/bin/dhcpc`, a Rust DHCP client, replaces busybox's `udhcpc` (2026-10-19).
  `cargo xtask test-install` fails on `main` in the live medium's boot (a
  BACKLOG row).
* **Developing Ferrix on Ferrix** (`docs/DEV-ON-FERRIX.md`, 2026-10-11): the
  loop walked by `cargo xtask test-devloop`, helix and rust-analyzer on the
  volume, `pkg install --replace` and `install-app`, and QEMU measured running
  on Ferrix (it does not end on its guest's power-off).
* **The desktop and its apps.** A `settings` app and the `compositor-desktop`
  crate (2026-10-11); the Claude desktop app shows its sign-in screen on the
  `--everything` desktop, natively on ferrousli (C1 to C4, 2026-10-16);
  `hyprctl clients` lists every mapped window; an opaque fullscreen window is
  drawn alone, with no shadow and nothing under it (2026-10-21, f0d338fcf; a fullscreen window drawn alone is copied straight to the screen, e4a418554, and a window over a fullscreen one is undecorated, e63185611,
  which saved 21 of a 78 ms software frame on the box).
* **Stages 22 to 24.** Steam has a way to start a native library game around
  the stalled in-guest install (`--steam-preinstall` and `ferrix_direct`,
  2026-10-10). Wine's notepad, DXVK on lavapipe, 32-bit Windows programs, a
  Unity game, fsync through `futex_waitv` (449) and Endfield's launcher in
  Debian's Wine and in GE-Proton are on `main` (stage 23's page). The
  download's two stops, the page cache (above) and a virtio-net ring whose
  shared indices tore (every one is a single aligned access since 2026-10-15,
  36a1bea84, 11d006073, bdccab15d), are fixed, and no download has run on
  `main` since. Stage 24's R1 to R3 and I1 stand as its page says.
* **The Linux surface.** An `epoll` ready list the files' wakes fill, per-thread
  kernel time and switch counts, a nice value that is a thread's, `getrusage`
  and `times` reporting processor time, thread names as `PR_SET_NAME` and
  `/proc/.../comm`, `sendmmsg` and `recvmmsg`, `memfd_create`'s exec seal,
  `arch_prctl`'s GS base, and a `connect` to `0.0.0.0` refused by the loopback.
  `/proc/<pid>/task/<tid>/syscall`, `wchan`, `stack` and `environ` tell what a
  blocked thread waits in (2026-10-21, 2022ed449, S1 of `docs/DEBUG.md`).
  `ptrace` is compiled out of the certified present profile by the customer's
  decision, its requirements cover debug builds only (H.DEBUG.1 and L.debug.1-14
  are reserved, 62d2b09da); the box gets an image with it on when `gdbserver` or
  `strace` is needed.
* **The channel round trip.** The direct switch folds its tasks' job load
  (L.sched.64-66), a Linux call lends the caller's thread, and pipe waits trust
  their queues (all 2026-10-11); `bench-pipe` times pipe, Unix-socket and futex
  round trips. No figure has been taken for the whole of `main` since F-69
  ([the page](ipc-round-trip.md)).
* **Building and testing.** A step cache for the gate's checks and test
  binaries, host tests side by side while `check` goes on, and a dev profile
  without LTO (`docs/TEST-TIME.md`); the kernel is split along its rings in
  the tools (`docs/KERNEL-CRATES.md`: S1, S2 and S3a are on `main`, S3 and S4
  are not started); xtask's commands register themselves.
* **The certification register** stands at 28 open and 62 closed, of 90
  (`docs/certification/FINDINGS.md`, 2026-10-21). F-70 to F-90 are new, most of
  them from the N150's first boots and from btrfs under memory pressure; F-86 to
  F-89 and F-72 (`TSYNC`'s lock holds) are open.

## Where it stands now (2026-09-30)

Since the 2026-09-27 wind-down, everything finished is on `main` and pushed:

* **Claude Code runs on Ferrix** (2026-09-30, `docs/CLAUDE-CODE.md`,
  `cargo xtask test-claude-code`): Anthropic's linux-x64 release takes a
  prompt, runs the model's Bash command on Ferrix and sends back its
  output, and `claude` is in the `--everything` desktop's terminals, and so
  in `remote-desktop`'s. It needed AVX: the kernel saves it with `XSAVE`
  now, and every x86-64 guest's model has x86-64-v3. On that desktop it
  runs on ferrousli, as every glibc program there does, since ferrousli got
  the POSIX timers on 2026-10-01. On 2026-10-15 (72ab80693) the linux-arm64
  release runs too, under QEMU TCG on AArch64 (`cargo xtask test-claude-code
  --arch aarch64`), with no kernel change. The Claude desktop app was assessed
  on 2026-10-14 (`docs/CLAUDE-CODE.md` §7). Anthropic now publishes a
  Linux build, and running it natively is 12–18 points as first guesses,
  ordered by the customer the same day (C1 to C4; C5 to C7 are not ordered,
  and C6's Secret Service will be served by `authd`). C1 to C4 were built
  on 2026-10-15: the app shows its sign-in screen on the `--everything`
  desktop (`cargo xtask test-claude-desktop`, §7.8).

* **The trip to ring 3 is being cut** (2026-09-30, `docs/OPAQUE-KERNEL.md`
  § 8). The customer's direction is Linux software beside safety functions
  on a kernel that can be assured. Moving btrfs's parser out of ring 0 is a
  step of that, and it needs a cheap trip first. The `seam` check now
  traces where a 4 KiB read's 230 us go under KVM (`seam-trip`,
  `seam-count`): no missed wake-ups, but 13 switches, 3 IPIs and 60 to 85%
  of wakes landing on another processor per read. Two fixes are in, both
  reviewed by the certification consultant (2026-10-01):
  - the block ring wakes a reader once, with one copy (b7cab053). At
    depth 32 the mean fell from 10.3 ms to 2.1 ms;
  - an MSI-X vector is no longer masked per interrupt (1dcc433f). The
    depth-1 p50 fell from 291 us to 206 us.

  Lazy TLB, the ring task off the data path, and same-processor wakes are
  on `os-35/ipc-*` branches; `docs/BACKLOG.md` says what each has left.
  The claim they serve is drafted as a proposal in
  `docs/certification/CLAIM.md`.

* **Speculation domains** (2026-10-01, bf9efba95, `docs/OPAQUE-KERNEL.md`
  §9). A job marked at its creation, by the holder of MANAGE on its parent,
  is one speculation domain. A switch between two programs born in it skips
  `IBPB`, and every other switch keeps the barrier. F-60, a leave that could
  miss an installing processor, was found and closed on 2026-10-03.

* **The channel round trip, toward seL4** (2026-10-03, `docs/OPAQUE-KERNEL.md`
  §9.5 to §9.11). The customer's target is seL4's own figure, 440 ns a round
  trip on nazuna, measured with protections matched, and since 2026-10-06
  under 400 ns. Step 1, F-60's fix, step 2 (2a to 2f), step 3 (3a and 3b) and
  step 4's direct switch and fast path are on `main`, each reviewed by the
  certification consultant, as are step 5's ERAPS and its vector reset
  (2026-10-06, 445d09420 to 98fdf69cc). A send of at most 24 bytes to a reader
  parked on the other end of a channel is handed over directly, with the
  processor; every other call goes the general way. It runs behind
  `ipc_fastpath=on`, on x86-64 only, and is off in the certified
  configuration for the first release (the customer, §9.7). `domain-call`
  p50, every mitigation on, was 2,556 ns at the 2026-10-05 wind-down (37 us
  before step 1); step 4 read 1,548 to 1,558 ns against 2,576 ns without it.
  On 2026-10-07 the round's cuts went in: the DS and ES skip (690754979), the
  object side's two cuts (69cf2754c, then 523eeccc6 and 7b06cef25, the task
  keeping its core process as an `Arc`), the entry and T2 called by
  link-time hooks (4066f41dd) and a task's slots as atomic cells (f6627521c,
  L.sched.63). On 4066f41dd the p50 is about 1,048 ns in the faster of the
  two modes a boot falls in (about 1,250 and 1,550 ns, §9.11); the slots
  (about -20 to -30 ns) and cut 2 (about -30 ns) are preliminary and there is
  cut 2 (7b06cef25) read 1,028 against 1,078 in one boot each. Landed
  since: 3c, FS and GS left unloaded 0 over 0 (27d3e23c8, 2026-10-07: 873 ns
  in the faster mode in a quiet window against 998); the user-side inlining of
  `write_read` and `decode` (7fd6c7a17, about -55 ns); the object side's cut 3
  (ee58d3912, 2026-10-08), in which seccomp's `quiet` predicate, asked twice a
  direction, reads `FILTERED_THREADS`, a live count of filtered threads, in
  place of a flag every boot set (about -40 to -50 ns on a busy host,
  preliminary); Q4 (dc5a3c495, each processor's GDT and RSP0 noted at the table
  loads, not at every switch; preliminary, about -1.8% cycles) and Q8
  (d3cbb0d55, the x86-64 context switch called directly, not through the GOT;
  no figure claimed, inside the spread); and the fix of F-65 (9ac307691,
  FX-0535): the fast path let the caller's endpoint go with interrupts masked,
  and now lets it go open (about +40 to +50 instructions a round trip, no p50
  change resolved); and, on the night of 2026-10-08, `cpu.stat`'s charge kept
  back per run queue (df7ee418c, about 237 instructions fewer a round trip for
  the same job, preliminary, measured on a loaded host so no ns is quoted: the
  cgroup controllers had put a walk up the job tree into every direction of
  the fast path) and F-69's fix (d884add58, a wait's last look holds
  preemption off: about 60 ns dearer on the general path, a cost to be won
  back by a cheaper hold). Each was measured on its own base and host load, so
  the figures are not summed: [the round trip's page](ipc-round-trip.md) lists
  them. Redox, measured the same way, takes 1,965 ns
  with no speculative defence. Left: step 5's rest (112a12b63 holds the ids
  of the user-space stream, and global pages have no session), PCIDs where
  hardware has them and step 4b; the handover is
  `docs/handover/2026-10-03-ipc.md`.

* **Two boot stops closed on 2026-10-07.** FX-0902, the memory sweep's "kept
  frames", was the wait for the reaper not counting a program thread
  part-way through its end (f3299dc04); and x86-64 TCG boots that stopped
  with no panic before the skipped-arm report were the boot self-check's one-shot waits
  sleeping on after the timer had fired (10d6dbc60). The second keeps a P1
  flake row in `docs/BACKLOG.md` until 20 TCG boots of `main` under load show
  no hang.

* **Landed on the evening of 2026-10-07 and on 2026-10-08** (4466212c3 to
  9ed9e8428), besides the round trip's cuts above:
  - **The cgroup controllers** (stage 13, 2026-10-07, batch
    08984c2db..8ec24ef0e, `po6/cgctl` rebased by po10-cgctl): reclaim of a
    read-only mount's clean pages and `memory.high`, `cgroup.freeze`, `cpu.max`
    with `cpu.stat`, and the `io` controller with `io.max`, 23 negative controls
    fired. The fast path's exits are held to `cpu.max` as the general way out is,
    and a `cpu.max` cut is armed for the task a switch picks. `cgroup.controllers`
    lists `cpu io memory pids`.
  - **Init's L13c** (2026-10-08, aae4c0eb1): `SystemCallFilter=`,
    `SystemCallErrorNumber=` and `SystemCallArchitectures=`, compiled by init to
    a seccomp filter per ABI and installed as the child's last step. Every key
    of L13 is carried out.
  - **Live driver updates** (D1, D2 and D4 for the display kind, aae4c0eb1):
    `devmgr` replaces a running driver with a new image and goes back to the
    old one if the new one does not publish in 15 s (`docs/DEVMGR.md` §4.1).
    The helper `drvupdated` is in test images only, until the customer answers
    how an image is to be signed; D3 is owed.
  - **Chromium on the DK1's architecture** (aae4c0eb1): Debian's armhf
    Chromium runs headless on ARMv7-A under QEMU in 512 MiB, after ARM's `send`
    and `recv`, `cacheflush` and `mmap` of a read-only btrfs were fixed. And the
    **DK1's SD card** (03fce4f81): `sdmmc`, a ring-3 driver for SDMMC1 behind the
    block ring, FIFO only, writing only inside `ferrix-*` partitions
    (`docs/CHROME.md` §10.1). Nothing has run on the board; the card has host
    tests against a model of the controller and a boot check, because no
    emulator has the controller.
  - **GPU step 4** (03fce4f81, `docs/GPU.md` §3.13): the render node takes
    `PRIME_FD_TO_HANDLE`, `renderctl` has `ATTACH_OBJ`, and hyprix speaks
    `zwp_linux_dmabuf_v1` version 3 with a GBM-shaped allocator, so a client's
    GPU buffer is sampled where it lies.
  - **Stage 19's small remainder** (2026-10-07, bab9eed1c): `xray`,
    `no_screen_share`, `blur_popups` and the tiled drag's drop with
    `dwindle:precise_mouse_move`. **The installer's I6a** (64f01d596): a GPT
    live medium. **Windows' QEMU** takes patch 0002 too (7f2659bdc), so
    x86-64 boots pass stage 10 there.
  - **Boot flakes**: FX-0001 under a loaded host (90108e87a, stage 20's S-1): a
    late shootdown answer is waited for and counted, and the machine stops only
    past 10 s; stage 7's hand-off flake was the check misreading a right
    outcome (88279f54b); FX-0309's check owed a `DMA_FAULT` record whenever a
    unit translates and now owes one only for a fault read (c2c7a6e6d), and the
    skipped entropy interrupt behind that boot is still an open row; a stage 3
    tick-rate flake (6 of 37 loaded TCG boots) has a row (8c4edef98).

* **Landed on the night of 2026-10-08** (9ed9e8428 to d884add58), besides the
  round trip's two landings above:
  - **A lost wake-up in a wait's last look** (d884add58, F-69, L.sched.71-72,
    `docs/roadmap/stage-05-tasks-scheduler.md`). `bench-ipc` stalled on ARMv7-A
    under TCG at one processor, 2 runs of 5 on `main` and 7 of 8 on another
    tree, with every task blocked and none to be woken. Every wait lists the
    task, sets it BLOCKED, makes its last look at the condition, and only then
    blocks or goes on, and a switch inside that span counted as the task's
    block: a wake that had drained the entry while the task still ran did
    nothing, and the task was off its run queue and off every list. The defect
    is older than the fast path (5d5b4f960 for a trusting wait, 0edd644b2 for
    the parked block's END). A wait now raises the preemption count before
    BLOCKED and lowers it when the look is over, so no task is ever blocked and
    still queued. Stage 5's and stage 9's cases require it, with negative
    controls that fired on x86-64 under KVM and on ARMv7-A. The boot gates
    passed on x86-64 (KVM and TCG, the fast path on too), AArch64 and ARMv7-A
    at one and two processors. The 0 stalls in 10 runs after the fix are
    counts of finished runs under TCG, not a hardware timing.
  - **ARMv7-A lazy VFP** (4e2f9024c, f42c1ede0, `docs/OPAQUE-KERNEL.md` §9.15,
    L.armv7a.21-26, F-68): a task runs with `FPEXC.EN` clear until its first VFP
    instruction, so a switch between two programs that never used VFP moves no
    VFP register. F-68 (Minor) came with it: a Thumb program's undefined
    instruction named the halfword before itself. Thirteen negative controls
    fired. The DK1 run of stage 9's `lazy` line, and any timing, are owed.
  - **`cpu.stat`'s charge** (df7ee418c, L.object.180): kept in a run queue's
    own record and moved into the jobs' slots once, `cpu.max`'s walk made only
    while a `cpu.max` is set, `floor_div` inline (above, under the round trip).
  - **Windows** (c8d5bbad3 to 09086c4db, os-win): four processors under WHPX
    with the patched QEMU that says `ferrix: whpx-gva`, the GPU by default in a
    window, QEMU above normal priority, a stopped clone of a component cloned
    again, ferrousli's busybox booted where Alpine's static one is missing, and
    ferrousli pinned at 93063b1, whose uutils install where xtask looks.

* **Seven verification-audit rows closed by runs on `main`** (2026-10-07,
  f8b44e04c, the certification's C1, the debt landings leave behind). The
  audit table in `docs/BACKLOG.md` listed landings whose gates had run only
  before a rebase, or never. Each owed run was made on `main` 4066f41dd, which
  holds every one of those landings, and each row names its log: N4's four
  negative controls each fired on their own message, and `test-xwindow`, the
  musl and glibc busybox `test-shell`, the musl `test-vfs`, `test-badapple`,
  `test-apps` and ferrousli's tests passed, as did the Finder's boots in an
  earlier batch on the same commit. Two sentences the rows named as wrong are
  corrected with them (stage 10's credit for the ACPI-or-device-tree decision
  is fa4e88a6's, and the Finder series was reviewed after the fact, by os-ad;
  the namespaces paragraph now reaches N4 and NP). The audit's other rows stay
  open: 37 code landings that name no gate, and the untriaged mutants.

* **ARMv7-A user mode reads the virtual counter** (L.armv7a.4, reserved
  4466212c3, landed 2026-10-07 in batch d44e7d1c3 as b17462efe): every
  processor sets `CNTKCTL.PL0VCTEN` and closes the physical counter and both
  timers to PL0 before it runs a program, and a stage 4 check reads the register
  live on each processor. `bench-ipc --board-log <file>` (404d59fbf) files a
  board's own serial log as a record. So `bench-ipc` can take the native channel
  round trip on the STM32MP157D-DK1; nobody has run it there yet. AArch64's
  counter mask is written the same way now (its check is a BACKLOG P2 row). The
  user state followed on 2026-10-08 (a1e6324c2, `docs/OPAQUE-KERNEL.md` §9.14,
  L.armv7a.5-12): `TPIDRURO` is kept in the task and `TPIDRURW`, which USR mode
  writes itself and nothing used to move, is read at every switch out and
  written at every switch in, carried by `fork` and zeroed by `execve`. That
  closes F-66 (Major), where one program's `TPIDRURW` reached the next program
  on the core; 15 negative controls fired. And 3a's reset: a task blocked in
  `channel_write_read` (and two other native calls) keeps only FPSCR and
  d8-d15, and the rest of the VFP registers are zeroed on resume, as on
  x86-64. The ASID ids are reserved on `main`, not written
  (`tools/common/data/requirement-reservations.json`): L.mm.69-70,
  L.armv7a.13-20 and L.user.125, with F-67's break-before-make order; the lazy
  VFP rows, L.armv7a.21-26, were reserved in 4b9f04043 and written on the night
  of 2026-10-08 (4e2f9024c, below).

* **Steam signs in and shows its store** on the `--everything` desktop,
  its 64-bit side on ferrousli (2026-09-30, `docs/STEAM.md` §5), and
  `cargo xtask test-steam-store` gates it with a test account, signing in
  with no one there. `run-compositor --everything` fetches Steam's volume
  when it is missing, and fuzzel lists Steam to start it again
  (2026-10-01). It still runs with launch-side workarounds, each owned by a
  fix (`docs/STEAM.md` §3): namespaces and seccomp for pressure-vessel and
  Chromium's sandbox, the `/proc` gaps behind the runtime's logger, the
  `SIGBUS` that needs a 16 GiB guest, and the web helper's GPU process
  off. F-55, which the GPU route through Venus met first, is closed; the
  rest of that route is designed (`docs/STEAM.md` §6) and not started.
  The second exit step, a game from the library, has its gate,
  `test-steam-game` (2026-10-01, not passing then; the customer reports a game running by 2026-10-21): Teeworlds is claimed for
  the test account, installed on request and downloaded in about 90 s
  since the guest's network took Steam's dozen connections (3b1b1de7,
  020dc9b2), and then Steam's staging of the files stands still, for a
  reason not yet known (`docs/STEAM.md` §7, BACKLOG).
* **`--everything` is everything** (the customer's rule, 2026-10-01): the
  desktop carries every feature, volume and app, and nothing is left out
  with a line saying how it could have been added. The volume half is in:
  a volume not fetched yet (rustc, Chrome, steamcmd, Claude Code, the
  Steam window) is fetched before the merge, and a failed fetch stops the
  run (`everything.rs`, f7777c17). Left: every app on that desktop, not
  just the `default` ones, and btop built through WSL on a Windows host
  (os-3c, `docs/APPS.md`). vkgears is listed wherever the host gives Venus,
  which a Windows host cannot (`docs/GPU.md` §3).
* **yserver is done**, all 36 of its points (`docs/YSERVER.md`, Y1 to Y7):
  X windows show on hyprix with their input, sizes, close, dialogs, menus
  and clipboard, and a window of a fixed size floats at that size. A
  floating window that draws its own title bar, such as Steam's sign-in
  window, can be dragged by it (2026-10-03: `_NET_WM_MOVERESIZE` becomes
  `xdg_toplevel.move`, which hyprix now carries out). It is the X server
  stage 19 counted 40 for.
* **Namespaces**: N1, per-mount flags (2026-09-28), N2, binds and
  detach, and N3, mount namespaces with `pivot_root` and `openat2`
  (2026-09-30), are in, each reviewed by the certification consultant:
  Debian's bubblewrap runs as root (`cargo xtask test-bwrap`). Every
  namespace stands on an empty bottom mount, as a booted Linux machine's
  `/` does, so `pivot_root` works with `/` in memory too, and Steam's own
  requirements check exits 0 as root in `test-steam-bootstrap`. N4, user
  namespaces, and the rest of stage 13 are os-7c's, which the customer
  asked to take the stage to done (`docs/NAMESPACES.md` §12).
* **An installer MVP** (2026-09-28, `docs/INSTALLER.md` §11):
  `ferrix-install` partitions a VM's disk and installs the system, and
  `cargo xtask test-install` boots the result through OVMF. The customer
  approved the full design the same day.
* **Netboot for a bare-metal PC** (2026-10-15, da37eb541, `docs/NETBOOT.md`):
  `cargo xtask netboot-stick --server HOST` writes a USB stick holding iPXE
  2.0.0's `snp.efi`, which takes the PC's firmware network driver and an
  address from the network's DHCP server and chains `boot.ipxe` from `cargo
  xtask netboot`; that serves the image `build` makes, so the unchanged
  loader reads `\FERRIX\KERNEL.ELF` from the file system iPXE gives it, and
  the PC's disk is not written. `cargo xtask test-netboot` boots the stick
  under OVMF with `--net` to `FERRIX-BOOT-OK`, with a negative control that
  serves under a wrong path. It has not run on a real PC (the first is an
  Intel N150, whose Motorcomm YT6801 card iPXE has no driver for), and a
  netbooted run leaves no post-mortem record (NETBOOT.md §6, owed).
* **The roadmap index is checked** (2026-10-15, 42d2dca8a): the generator
  lists `ipc-round-trip.md` in `SUMMARY.md`, which every `index` run had
  dropped, and `check-docs` runs `split-roadmap.py check` as the audit
  "roadmap index and links".
* **Test run time**, cut 2 (2026-09-28, `docs/TEST-TIME.md`): a finished
  guest is stopped rather than given its grace, and `test-compositor --arch
  x86_64` went from 900 s to 512-616 s. Cut 3 (2026-09-30): `--arch all` of
  `test-boot`, `test-init` and `test-audio` runs the architectures at once,
  `test-boot` from 136-139 s to 46 s and `test-init` from 171-182 s to
  72-86 s on a loaded host; `test-compositor` stays in turn, since its
  x86-64 frame budget failed when three suites emulated side by side.
* **The one red gate is fixed** (2026-09-30): `test-compositor`'s submap
  boot left a window's slide undrawn about one run in two, because hyprix's
  loop owed no frame after one that started an animation. A compositor boot
  whose kernel panics now also asks QEMU where every processor is
  (`panic-registers.txt`), for FX-0001, seen twice there.
* **NVIDIA's own driver drives the RTX 3060's monitor** (2026-10-03 and
  -04, stage 21, `docs/NVIDIA.md`; on `main` since 2026-10-05, `land-n6`,
  a84992dc5):
  * Chrome's WebGL renders on the card, through ANGLE on NVIDIA's Vulkan.
  * The desktop runs on the customer's TV on the 3060's HDMI port, with
    NVKMS inside `nvrm` and `nvrm` as the display core's copying driver.
  * The customer drives it with a dedicated keyboard and mouse.

  It runs at 44–53 fps, with software compositing. The conditions the
  landing left are BACKLOG rows O1 to O6 (consultant ledger 318). Next, in
  the customer's order:
  1. dmabufs for Chrome's GPU compositing (N3b, half built on
     `nvidia-n2`);
  2. a hardware cursor;
  3. measuring page loads.

  Also on `main` (2026-10-05): the chardev core's queue to `nvrm` is
  held to its room (F-63, closed), and stage 10's chardev self-check, with a
  fake driver, requires it and the HELLO rules, copies and drains (N10's
  code half, N12, N13; FX-1013). N5's switch, D1 to D10 and
  `test-nvidia-smi` stay owed.
* **Sound** is done: alsa-lib (U1) and `pulsed`, a PulseAudio-protocol
  server Chrome plays through on the desktop (U2a to U2d, 2026-09-27).
* **Windows**: the desktop runs under WHPX with the TSC as its clock
  (2026-09-29), where it had fallen back to TCG. Since 2026-10-08 it has four
  processors there (06253d284: QEMU 11.1's own x86 emulator could not walk a
  guest's page tables with more than one, so a patch has it ask Hyper-V), draws
  on the GPU in a window by default, and gets QEMU above normal priority
  (the slowest frame of a window slide fell from 112-116 ms to 25-32 ms; a
  busy PC held 60 frames a second at the 10th percentile, where normal priority
  gave 10 to 45).
* **The tree moved** into `src/`, `tools/` and `docs/` (2026-09-29,
  `docs/LAYOUT.md`), tests into `src/tests/`, and the kernel's interface
  cores and discovery code into `interfaces/` and `discovery/`.
* **Discovery and drivers** (2026-09-30, stage 10's last section): the
  kernel finds devices through one `Finder` trait, run once in order, and
  ACPI-or-device-tree is decided in one host-tested place. Ring-3 drivers
  share `ferrix-driver`, where a DMA buffer cannot be freed before its
  device has reset; virtio-input and virtio-blk are on it, seven drivers
  are not yet, and no driver's graceful STOP is reachable yet.

**Red on `main`** (`docs/BACKLOG.md`, *Red on `main`*): on 2026-10-05
`main` had two stage 10 panics, both fixed. FX-1012, one boot in 27 to 160
(`iommu/check.rs` G3 racing the console's pump thread, which had ended 340 of
a night's 9,234 boots as "QEMU killed"): G3 and R5 take their check byte in
the receive path, out of the pump's reach (a6116e822). The seam panic: the block ring's
index was read byte by byte on the driver side, so a reader could see half a
`u32`; the accessors are single accesses now and the virtio notify has a fence
(1e113bb31, 3f0e56b2b). The net ring has the same flaw and is unowned, 3 points.
The Windows gateway test lost a guest's frame when a timed receive raced a
datagram; the gateway waits by peek now (e321befc4). Fixed and on `main`
(0ece8826e's batch):
`test-init --arch all` failing on the revoke reader, on prompts typed too
early and on `su`'s half-read password, and stage 5's moving lock check
panicking because an idle processor could take another idle one's last
waiting task (`sched::steal_from` now leaves it that task); `xtask`'s serial
reader, which sent the "QEMU signal 15" after a stage 10 panic, decodes
lossily. H.SCHED.1's idle-kick rule has no requirement yet. As of 2026-09-30
the submap boot's row is done. Four flake rows gained sightings or were filed
on 2026-09-30, each with its log: FX-0001 (twice, at the submap boot's
`L`), the compositor's frame budget under load (x86-64 and ARMv7-A), the
audit self-check's FX-0309, and the semaphore check's "a waiter returned
without waiting".

**Parked on branches**, pushed, each with a `docs/BACKLOG.md` row:

| Branch | What | Left |
|---|---|---|
| `boot-21b` | W-8 boot requirements, part b (root, init, power, random, F-51) | the consultant's diff review, a rebase with carry, then 21c (`devmgr`) |
| `w8-armv7a` | W-8 file 24, ARMv7-A and `arch/arm_common` (drafts) | write the SysML file against the code as F-48 and F-50 left it |

**Next**, in the order the customer last gave: test and gate run time
(the rest of cut 2, then KVM by default on x86-64 where the host has it,
`docs/TEST-TIME.md` *Next*); Steam, retiring its workarounds, with stage
13's namespaces and seccomp under them (os-7c); W-8's 21b, 21c and file 24;
fuzzel's second-press toggle; the rest of authentication's phase 2 (below).

**Waiting on the customer** (`docs/BACKLOG.md`, *Waiting on the customer*):
F-43 (W^X for programs, or a narrower claim), the Common Criteria version
(F-52), FMT_SMF.1 and FMT_MTD.1, whether the customer's Python desktop
scripts are rewritten for Ferrix, and the installer's reference PC
(`docs/INSTALLER.md` §10, decision 3). Hardware confirmations (F-44 on the
Pixel 7, F-48 and F-50 on the DK1) wait for the product owner's word in
daytime.

## Stage by stage

Stages 0–12 and 16 are done, and so are networking, 17 and 18.

Stages 0–12 are in the boot test on all three architectures, and the boot
marker reads `FERRIX-BOOT-OK stages 1-12`.

ARMv7-A joined after stage 3 — see *ARMv7-A* after stage 4 — and has run on
hardware: an STM32MP157D-DK1 at two cores reached `FERRIX-BOOT-OK stages 1-9`
and ran stage 7's script at `fd4442e`.

Stage 7's exit is somebody else's static musl busybox running a script on
every architecture, checked by `cargo xtask test-shell` rather than the boot
test because it needs a binary the repository does not carry. Since the exit,
a program can `fork`, `execve` and `wait4`; its section lists what the Linux
surface still owes.

Stage 8's self-checks are in the boot test — the root filesystem unpacked from
an initramfs, every process's descriptor table, the calls that take a path,
pipes, `/dev` and `/proc` — and its exit, `cargo xtask test-vfs` running `ls
-R /proc`, `cat /proc/self/maps` and one shell script whose applets are forked
programs, passes on all three architectures; it is a test of its own for the
reason stage 7's is.

Stage 9's exit runs in the boot test itself: two programs in user mode
exchange messages and a handle over a channel, and a job kill takes down a
process tree, with ports, interrupts delivered to them and device memory a
driver can map built on the same objects.

Stage 10's exit runs in the boot test itself: a virtio-blk driver in ring 3,
started by `devmgr`, reads sectors through the block ring with VT-d on x86-64
and the `SMMUv3` on AArch64 translating, and a deliberate out-of-domain write
faulted on both; ARMv7-A runs it in degraded trusted mode, as decided. What
the stage still owes — trusting decoding-off BARs, AMD-Vi, and one unexplained
flake — is after the exit in its section.

Stage 12's exit is met: Ferrix writes btrfs. Every boot mounts a blank volume
writable on a third disk, builds a tree on it, unmounts, mounts it again and
reads it all back, and `cargo xtask test-btrfs` then has host `btrfs check
--check-data-csum` judge that same image, clean on all three architectures.
`fsync` writes a log tree the next mount replays, and `cargo xtask
test-powerfail` kills QEMU in the middle of writing, replays at the next boot
and has `btrfs check` judge the volume before and after: 249 seeds across the
three architectures, 226 of them leaving a log to replay, every one clean.
`cargo xtask run` now boots with `/` on a persistent btrfs volume. Pages
written through `MAP_SHARED` are written back too (2026-09-23): a linker's
output, which `lld` writes through a mapping, lost its pages when the file
left the cache.

Stage 16's exit, the goal, is met: `cargo xtask test-rustc` compiles
`hello.rs` on Ferrix with the rust-lang.org `rustc`, linked through `cc` and
`rust-lld`, from a btrfs volume, and runs what it made, on x86-64 and in CI
since 2026-09-22.

Stage 20, self-hosting, has its first step: Ferrix builds its own x86-64
image. `cargo xtask test-selfhost` runs the same `cargo xtask build` a person
runs on a Linux host inside Ferrix, with Cargo, from the tree and its vendored
crates on a btrfs volume, and the image it made passes the boot test.
Since 2026-10-03 it does so on Arm hardware too: on a Pixel 7, the phone's own
desktop image boots in crosvm with pid 1 a script on the toolchain's volume,
Ferrix builds its AArch64 image there in about nine minutes, and
`cargo xtask test-selfhost --arch aarch64 --volume` finds the volume a clean
btrfs and boots the image to the end of its self-checks
(`tools/vendor/google/pixel7/selfhost.sh`).
The whole matrix built by Ferrix is not done: on 2026-10-04 its 57 rows were
recorded (271 builds, 188 once a test's init went into the initramfs and one
kernel served every test), and that change, with the record-without-booting
mode a weekly CI job needs, landed on `main` the same day (branch
`selfhost-matrix`, ending at 77783565a). CI's self-hosting job was red
from the components split (2263225e1, 2026-10-03), because the guest
tried to clone the component repositories with no network; the volume now
carries the components and the guest clones none (`selfhost-components`,
cb872a732); CI's job has passed on `main` since, from 77783565a (run
37229234877) on. The plan is complete since 2026-10-05 (S-2, ce133294c and
a3b240c6b): the tests that made builds after their first boot, which plan mode
refuses, now build every variant before it, and a plan run records 162
distinct builds against 139.

Dynamic linking is done: Debian's glibc busybox runs on its own `ld-linux`,
and on ferrousli's loader and `libc.so.6` in glibc's place, on all three
architectures, since ferrousli itself was ported to AArch64 and ARMv7-A on
2026-09-23.

Stage 15 has job control and, since 2026-09-26, a real init: `/sbin/init` runs
services in cgroups of their own over `src/lib/init/svc`'s manager, with `svc` to
drive it, readiness, socket activation and resource limits, gives the console
a getty, and powers the machine off, on all three architectures (`cargo xtask
test-init`). `run` and the desktop boot it, and the compositor is its service.
Since 2026-10-04 a unit can be sandboxed with L13a's `NoNewPrivileges=`,
`PrivateTmp=` and `ProtectSystem=` (1e1f2543a, `docs/INIT.md` §4.5);
L13b's `PrivateNetwork=` landed on 2026-10-05 (3349682db); L13c's
`SystemCallFilter=`, with `SystemCallErrorNumber=` and `SystemCallArchitectures=`,
landed on 2026-10-08 (aae4c0eb1), which finishes L13. `test-init` types each password once, when its prompt is
on the console (`ferrix-auth-client` flushes before it shows the prompt), and
judges the revoke reader on one look (2026-10-05).

Stage 15's authentication (`docs/AUTH.md`, approved by the customer on
2026-09-26) has phase 1 done and most of phase 2. Phase 1: its kernel fix,
P0 (a native process runs as the one that made it), Argon2id, `authd`,
`passwd` and `authctl`, gated by `cargo xtask test-auth` on all three
architectures (2026-09-27), and hyprlock over `authd` (P1.5, 2026-10-03).
Phase 2, all landed on 2026-10-03: `--everything`'s desktop runs as `ferrix`
under `sessiond` (P2.4), hyprix unlocks only on `authd`'s grant (P2.5),
`login` on the console (P2.3), a session that ends with its compositor
(P2.7), `su` for `wheel` (P2.6), and `/dev/tty` as the caller's own
terminal. On 2026-10-04 getty began revoking the console before every login
(0f94a6d1a, landed in batch 22384874f), so a program left by one login
reads nothing of the next one's password. K-B (P2.1), NP, landed the same night
(7e9a2806f): `/proc`'s private links are decided by `ptrace_may_access`,
dumpability included, and `/proc/<pid>/fdinfo` exists (`docs/handover/2026-10-04-np.md`).
Left: K-C (P2.2) is not started;
the other desktop images still run as root; and ending a user's processes
at logout is the customer's call.

Stage 13 is done on its exit criterion (2026-10-14; its scope is what `main` holds, and the rest is in the follow-ons page), built cgroups first because init needs them: cgroup2 with
`pids`, `memory` and its scoped OOM kill, and `cpu.weight` (2026-09-26);
mount, user, UTS, IPC, cgroup and pid namespaces, seccomp's checker and
hook (S1, S2), and network namespaces (2026-10-04, 22384874f) are in.
Seccomp filters (S3) landed on 2026-10-04 (248799bdd: `seccomp(2)`, `prctl(PR_SET_SECCOMP)`, chains per thread);
the cgroup controllers (reclaim of a read-only mount's clean pages,
`memory.high`, freezing, `cpu.max`, `io`; 23 negative controls) landed on
2026-10-07, and on 2026-10-08 `cpu.stat`'s charge moved off the fast path's
every direction (df7ee418c); seccomp's `TRAP`, `TSYNC` and `cargo xtask
test-seccomp` (S4 to S6) were ported on 2026-10-11 and are on `main` since
2026-10-18 (`docs/SECCOMP.md` §12), with Syscall User Dispatch; `cargo xtask
test-container`, the exit criterion as one program, is on `main` since
2026-10-14 and passes on all three architectures, so the exit is met; time
namespaces are on a branch (`stage-13-handover.md`).

Chrome runs on Ferrix (2026-09-24): Google's prebuilt Chrome for Testing,
headless and in a window on the compositor, on x86-64, and both on ferrousli's
loader and C library in glibc's place (2026-09-26). Since 2026-09-26 it runs
with its zygote, idles at 13% of a processor where it took 443%, turns a box
at 60 frames a second where it managed 1.5, and plays sound (*Chrome*, after
sysfs, and `docs/CHROME.md`). On ARMv7-A, Debian's armhf Chromium runs headless
in 512 MiB under QEMU (2026-10-08) and the DK1's SD card has a driver; neither
has run on the board.

Sound is a ring-3 virtio-snd driver, an audio core in the kernel and
`/dev/snd`, gated by `cargo xtask test-audio` on x86-64 and AArch64
(`docs/AUDIO.md`, 2026-09-26), with alsa-lib on ferrousli and `pulsed`, a
PulseAudio-protocol server that mixes any number of streams, which Chrome
plays through on the desktop (2026-09-27).

An installer MVP puts Ferrix on a VM's own disk (`docs/INSTALLER.md`,
2026-09-28): `cargo xtask build --installer` carries `ferrix-install`, which
writes a GPT with an ESP and the btrfs root, and `cargo xtask test-install`
boots the installed disk alone through OVMF.
A netboot (2026-10-15, `docs/NETBOOT.md`) boots a bare-metal PC from this
host through an iPXE stick (`cargo xtask netboot-stick`, `netboot`,
`test-netboot`) without writing its disk; it has run under QEMU only.

Networking is done: sockets, a net core and a ring-3 virtio-net driver, with
`curl` fetching over HTTPS and `git` cloning inside the guest.

sysfs is done (2026-09-24): `/sys` is a view of the devices as enumeration,
the ring-3 drivers' cores and `devmgr` describe them, and a driver unbound and
bound through it goes and comes back (`docs/SYSFS.md`).

Stages 17 and 18 are met, and the compositor runs: `cargo xtask
test-compositor` boots it as init on Ferrix, and two Wayland clients tile on
the card, pixel for pixel as the renderer draws them, on x86-64, AArch64 and,
since 2026-09-23, ARMv7-A.

Stage 19 is under way, and the GPU path chosen on 2026-09-18 is built: the
desktop composites on the GPU through `/dev/dri/renderD128`, and the screen is
shown the very texture the compositor drew into, which takes a 1920x1080 frame
of a video wallpaper behind a blurred translucent terminal from 39 ms in
software to 12 (`docs/GPU.md` §3.7 and §3.8; 60 fps is 16.7).

Stage 19's X server is yserver (`docs/YSERVER.md`, 36 points against the
stage's 40 for XWayland, done 2026-09-29). What the stage still owes is
client pages as texture backing. `dwindle:precise_mouse_move`, `xray` and the
second-pass effects landed on 2026-10-07 (bab9eed1c), and `zwp_linux_dmabuf_v1`
with a GBM-shaped allocator on 2026-10-08 (`docs/GPU.md` §3.13); Mesa on
ferrousli, for clients that draw on the GPU themselves, is priced beside it.

The desktop's own clients -- waybar, fuzzel, hyprlock and hypridle, written in
Rust -- are all on `main`. waybar and fuzzel are on `main` (2026-09-27):
`cargo xtask run-compositor --everything` boots the customer's own
`hyprland.conf`, dotfiles, fonts and monitor EDID, waybar draws their bar,
SUPER+R runs their launcher script into fuzzel, and the clipboard is shared
with the host's through xtask. hypridle is on `main` too (2026-09-26), its
`idle` boots passing on x86-64 and AArch64. hyprlock is on `main` too
(2026-10-03), its lock over `authd`. Since 2026-10-04 they, and term, are
apps in `ferrix-os/apps`, built by xtask from their folders (def906ba2), and
foot takes keys: every compositor image carries libxkbcommon's
`usr/share/X11/xkb`. `test-foot`'s check that a typed line reaches foot's
program runs and passes since f55e8ab28 (2026-10-04), which gives `test-foot` zinc as `/bin/sh`
for its keyboard check; before that the check never ran.

Stage 21 is bare metal with a card of Ferrix's own. NVIDIA's driver
already draws the desktop and Chrome's WebGL on the RTX 3060's own
monitor, through libvirt (on `main` since 2026-10-05; see its page). Stage 22 is
Steam,
whose 32-bit x86 ABI is under way (`docs/I386.md`): I1 to I4 are on `main`
(32-bit programs, their threads, signals and fork, Alpine's and Debian's i386
busybox), and I5a: Valve's `steamcmd` logs in to Steam, from `test-steamcmd`
and from the `--everything` desktop's terminal (2026-09-27). The Steam client
itself draws through yserver, a Rust X11 server, with a rootless Wayland
backend of Ferrix's own: its sign-in window is on hyprix since 2026-09-29,
and on the `--everything` desktop, its 64-bit side on ferrousli, it signs
in and shows its store (2026-09-30, gated by `test-steam-store`), with
launch-side workarounds listed in `docs/STEAM.md`. Stage 23 is the Windows
game *Arknights: Endfield* through Wine with DXVK on NVIDIA's Vulkan, on the
roadmap since 2026-10-10 at the customer's word: a feasibility study is
written, and its eight steps are sized at about 205 points as a first
guess. Step 0, the game on plain Linux in the same VM shape, answered on
2026-10-13: the game downloaded on the Linux guest, and its anti-cheat
(ACE) refused to run in a virtual machine (13-65544-2). By the stage's own
rule, steps 4 to 6 wait for Ferrix on the machine rather than in it, the
way the customer chose; whether ACE accepts Wine on bare-metal Linux is not
known. The machine is the customer's N150 box, and on 2026-10-15 Ferrix,
built from branch `ahci` (4a0db8a97), booted on it: hyprix's desktop on the firmware framebuffer, the USB
keyboard, `/` on its SATA drive through a new AHCI driver, the YT6801's
network, and `gryphlink` showing the launcher with its online notices; not
logged in, nothing downloaded, and slow (an uncached framebuffer, and USB
and NIC polling because MSI never arrives; `docs/N150.md` §13). The
driver is on `main`, and the kernel bring-up since the n150k landing (ledger 1884), evidenced on QEMU only and not yet run on the box with checks on (BACKLOG). Before that, Endfield's own launcher, a 64-bit Qt program with
QtWebEngine, drew its first screen on Ferrix in Wine, fetching its news
over the network. On Ferrix, `arch_prctl` answers
`ARCH_SET_GS` since 2026-10-10, which 64-bit Wine could not start without
(`docs/WINE.md`); its notepad is a window of hyprix's at its tile's size
that takes typed text and the pointer since 2026-10-12, through yserver
with no workaround (`test-wine`; `run-compositor --wine` to watch it);
Direct3D 11 draws through DXVK on lavapipe in it with no
card since 2026-10-12, offscreen and presented in a window on hyprix
(`test-dxvk`), Wine runs 32-bit Windows programs inside its 64-bit
process since the same day (its 32-bit `cmd` answers, and since 2026-10-15
a 32-bit program draws a window); GE-Proton's Wine waits through fsync on
the kernel's new `futex_waitv` since 2026-10-15, about eighteen times
faster a turn than through wineserver (`test-wine-sync`); a real Unity game,
Unity's Spaceship Demo on Direct3D 11 with its High Definition Render
Pipeline, starts in it and draws its walkthrough in a window of hyprix's
through DXVK on lavapipe since 2026-10-13, at about 1.3 frames a second
with no card (`test-unity`); Endfield's launcher reaches its server in
Debian's Wine and, since 2026-10-14, in GE-Proton11-7's, whose install
dialog it draws whole (`test-endfield-launcher`, `test-endfield-proton`;
GE-Proton needed `mprotect` to keep a mapping shared and a write-only
region to read, f0c812f01). Its download's certificate roots are in
(GnuTLS and the CA bundle where Wine's `crypt32` reads them), and Wine,
Proton and the launcher are on the `--everything` desktop as `gryphlink`
(2026-10-14); the game's download is not done: it stopped on ENOSPC from
Ferrix's btrfs after about 5.8 GB with about 150 GB unallocated, because
the 8 GiB guest ran out of frames and a writable mount's page cache is not
reclaimed. The fix, the work of the branch `mm-pcache` under another hash
(f0bc9ab2d, L.object.188-190), is on `main` since 2026-10-15. On 2026-10-15
a download on a scratch image reached about 9.5 GB
of 56.9 GB at 2.5 MB/s and died at 00:22: the guest's virtio-net driver
stopped sending after two `devmgr` restarts of it, because the net ring's
shared indices were composed from bytes and tore; every one is a single
aligned access since 2026-10-15 (36a1bea84, 11d006073, bdccab15d, the work of
`net-restart-stall`). No download has run on `main` since. What took the CPU was
the launcher's looping background video, not the download; a click on its
STATIC switch pauses it. The next download (8 vCPUs, a fresh image, from
a scratch branch that is not for `main`) runs at about 3.7 MB/s. A restart
starts a download over, because the launcher keeps its state in a Wine
prefix on tmpfs; the scratch branch keeps the prefix on the volume, which
is not yet shown to make the launcher resume. A window of an X
program drawn by the card exists on unlanded branches only: on 2026-10-10
Teeworlds drew through NVIDIA's GLX over yserver's DRI3 at 85 to 94 swaps a
second between stalls of about 5 s, whose cause is found and whose fix is
not landed ([its page](stage-23-arknights-endfield.md)). Stage 24 is the
game path: where a fullscreen game can be faster on Ferrix than on a stock
Linux desktop on the same hardware, which is the time from an input to the
picture and the evenness of frames under a background load. It is a design,
`docs/GAME-PATH.md`, that the customer made a stage on 2026-10-10, and its rig is
begun: R1, the frame recorder, R2, the input-to-scan-out probe, and R3, the
load, the timed sleep and the launch timer with both guests pinned on an
invariant TSC, landed on 2026-10-14 (`test-frames`, `test-xlatency`,
`test-game-rig`), and the same binaries ran on
Ferrix and on a Linux guest with no card; they stand on yserver's Present
clock (pin 2966c377, eb7ad7411, which also fixed a lavapipe
`PresentPixmap` crash); since 2026-10-15 the clock keeps one refresh (pin
d2bf56e2: every msc 13.334 ms, where an msc had lasted 12.2 to 19.4 ms),
and `test-frames` fails a clock that steps. Of the first cut, I1 followed on 2026-10-15: hyprix reads
input on a thread of its own, and an input's time travels from the input
core to the X client, so the mouse's lag is a number per hop. Nothing of what its exit is judged by is measured on
the card yet. Its first step is the rig that measures it (R1 to R5, 19 to
21 points; R1 to R3 are built, R4 and R5 need the card), then a first cut of 47
points; the game domain (52) is stage 14's ground and waits on the
customer, as eight more decisions do. Its card work stands on displayctl
v9, yserver's dmabuf hand-over and a hardware cursor, none landed
([its page](stage-24-game-path.md)). Ferrix also boots on the customer's Pixel
7: natively on all eight cores to `FERRIX-BOOT-OK stages 1-12`, and as a guest
of the phone's own crosvm from a launcher app, which shows a desktop in that
VM with Chromium on it (2026-09-27), and each release carries that desktop,
full and minimal (2026-09-28); during a native boot a USB serial port
streams the kernel log (2026-09-26). The kernel's certification
set (`docs/certification/`) closed F-23, F-31 and F-35 on 2026-09-26: fallible
allocation, side-channel defences with KASLR, and job quotas. On 2026-09-27
it closed F-21b with an audit record of the TSF's own decisions, claimed in the
Security Target; measured the certified item's statement coverage at 90.1%,
89.9% and 84.5% on x86-64, AArch64 and ARMv7-A; and traced 602 low-level
requirements, 396 of them verified by a named check. On 2026-10-05 the
register filed and closed F-62, a carry of the coverage evidence that nobody made: the gate now
fails on it, and the anchors two landings had skipped were carried again; it filed and
closed F-63 too, the queue from the chardev core to `nvrm` able to grow
without bound, which abandoned requests now leave and admission counts. The
register stood at 16 open and 48 closed, of 64; on 2026-10-08 it was 17 open and
52 closed, of 69 (the register's own count), F-65, F-66, F-68 and F-69 among the closed;
on 2026-10-21 it is 28 open and 62 closed, of 90. CI on `main` is green again (run
37331100176, after a rerun of a flaky job), after the Windows gateway test
`a_lost_segment_is_sent_again_alone` stopped racing the gateway's timer
(39e520e31); another gateway test, `resets_a_connection_to_a_port_nothing_listens_on`,
flaked once on Windows and has a row in `docs/BACKLOG.md`. Stage 25 is Yoru, the main UI: the customer's design "Yoru WM" as the desktop's look and shell on hyprix (`docs/DESKTOP.md` §6, 2026-10-24). The tokens, the dock with its stash, the launcher's look, the widget sidebars and the wallpaper picker are built, each with a `test-compositor` boot, and so are Nocturne for GTK, `portald` and Chrome's GTK volumes; the palette, snapping, the helpers, the lock, notifications and switcher in the tokens, and a Files app are not ([its page](stage-25-yoru.md)).

Stage 17's display iteration is done: `/dev/dri/card0` served by a ring-3
virtio-gpu driver, with `cargo xtask test-display` requiring a compositor's
colour pixel for pixel on all three architectures. Its input iteration is done
too, to the same standard: `/dev/input/eventN` served by a ring-3 virtio-input
driver and a kernel input core, with `cargo xtask test-input` sending a key
and a touch in at QEMU's far end over QMP and requiring them back out of the
nodes on all three architectures, and a negative control that must fail.
`epoll`, `eventfd` and `ioctl(FIONBIO)` are in the boot test, so the kernel
side of iteration 2's prerequisites is done, and `card0` has the primary plane
and `type` property Smithay's legacy path needs (E4). The compositor reads
those nodes now, so stage 17 is met: `cargo xtask test-seat` types into a
window on Ferrix from QEMU's far end. Each stage's section below says what
exists. The marker will not move until a stage meets its exit criterion.
