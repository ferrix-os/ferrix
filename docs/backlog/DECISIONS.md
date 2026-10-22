# Backlog: decisions

Moved out of [`docs/BACKLOG.md`](../BACKLOG.md) on 2026-10-16. A decision is recorded here when it is taken, so a session that was not there does not revert it.

## Decisions

Dated, newest first. A decision here is final until the customer says
otherwise; one a later decision replaced is deleted, and the history keeps it.

* **2026-10-23 (customer, through the coordinator)** **The power core
  (`docs/POWERCTL.md`) is a standing interface, with a 48 A / 23 A row.**
  Asked "Power limits (consultant OK IF): accept about +500 kernel lines?
  also retire msrlog (−321 lines)? your dated words for a standing
  interface and for 48 A.", the customer answered "power limits are ok":
  `ferrix.powerctl` and its driver stay in the tree, and the N150's model
  row allows 48 A and at most 23 A above firmware's (the consultant's
  ledger 3130 condition 1 (a) and (c), ledger 3180 condition 1). No answer
  on `ferrix.msrlog`, which stays.

* **2026-10-21 (customer, through the coordinator os-22)** **ptrace is out
  of the certified `present` profile** (docs/DEBUG.md S2; the consultant's
  design review, ledger 2937, condition D2). The facility is compiled out of
  `present` by a build flag rather than refused at run time, its
  requirements cover the debug builds alone, and the N150 box is given an
  image built with the flag on when gdbserver or strace are needed there.

* **2026-10-13 (customer)** **Ferrix becomes a microkernel whose ring 0
  is the certified item whole** (`docs/MICROKERNEL.md`, route C with
  restricted mode). The customer asked to "plan out the switch of ferrix to
  become fully certifiable microkernels", and answered the plan's questions
  in these words: "Linux =use space => ok"; "performance: maximum
  performance"; "btrfs server: ok"; "cert level asil d etc so max
  certification"; and, by choosing among offered answers: a build switch for
  speculation inside a Linux partition; ARMv7-A a native-only safety target;
  feature work in the load allowed during the move, its growth reported; the
  reference boot's disk path proven by user-space checks; both builds gated;
  the new targets for the new kernel from M7; IEC 61508 at SIL 4; timing
  claims on AArch64 safety chips; later the same day, "which other
  components to certify; which one would the cert agent recommend?" and,
  offered the consultant's set, "Accept as recommended"; on the `core` ring,
  "ask the cert agent"; on the first standard, "the SIL D / ASIL D", then
  "SIL 4 first"; "find a proofer"; on the measurement host, "i don't have
  one, so use amd instead"; on the stop rule, "Use the same methods seL4
  uses, it must be possible to reach the same level, they did it, so can
  we"; and on the safety processor, "find one"; then "3 builds sounds
  good, so a good start would then be to define the build time flags?",
  "yes desktop build is a hybrid kernel, while max and fast are true
  microkernels", and, asked whether the IPC fast path becomes a build flag,
  "Build-time flag"; asked whether its 2026-10-02 rule binds `fast` too,
  "Only max"; and on the barrier, the contacts and the board, "let the cert
  agent decide. ideally it shall be almost not realizable on desktop, maybe
  even deactivateable, as it is not certified anyways, and max security for
  max" (the consultant's decisions: ledger line 926, MICROKERNEL.md §11). Then: "can you optimze max as well with a 1.5x seL4
  difference max", so `max`'s restricted row is at most 660 ns (MICROKERNEL.md
  §7). **The plan reads these as** (MICROKERNEL.md
  §11): the Linux ABI served from user space by `linuxd`, which replaces the
  2026-09-16 decision (a monolithic core) and the 2026-09-27 shelving of the
  opaque-kernel plan, both deleted here, and reopens `docs/ARCHITECTURE.md`
  §2; the certified build (barrier on) gated within 2x today's in-kernel
  figures, failing if missed, and the performance build at 1x, failing
  above 1.1 times; a certified
  `btrfsd`; and **an amendment of the 2026-10-01 targets for the new kernel
  from M7**: ISO 26262 ASIL D, IEC 61508 SIL 4, EN 50716 SIL 4, DO-178C DAL A
  objectives, IEC 62304 Class C, Common Criteria EAL6+ (MICROKERNEL.md
  §6.5). Until the flip, today's item keeps the 2026-10-01 targets (EAL5+,
  DAL C, Class C, SIL 2); `README.md`, `CLAIM.md` and the manifest's ring
  targets change in M7. Further readings (MICROKERNEL.md §5, §6.3, §6.5,
  §7): a certified partition manager, loader library and `rt`, devmgr
  uncertified with safety devices static; `core` kept as a layer with one
  set of targets; IEC 61508 SIL 4 first, ISO 26262 ASIL D alongside, and the
  claim's wording; F1 in Isabelle/HOL, F2 in Verus, Proofcraft and the Verus
  groups or CertiK as providers; nazuna as the measurement host, SVM ASIDs
  rejected, the bare-call targets kept and reached by seL4's methods;
  Cortex-A78AE in Lock-mode as the timing class, AMD's VEK385 as the board.
  Three build profiles from one source (MICROKERNEL.md §12,
  `docs/BUILD-FLAGS.md`): `max` (SC 4 / ASIL D) and `fast` (SC 2 / ASIL B)
  microkernels carrying claims, and `desktop`, a hybrid kernel with the
  Linux layer and btrfs in ring 0 and no claim; the IPC fast path a build
  flag, off in `max` until the 2026-10-02 rule is met. The plan's and the flag design's reviews by the certification consultant
  **passed with every condition folded in** (ledger lines 812 to 941, and
  950 for landing); each kernel mechanism, the `ipc_fastpath` move and the
  Zenbleed/GDS change still get their own reviews before code. The customer
  then said "ok go with m0". The barrier, the contacts and
  the board were decided by the consultant under the customer's delegation;
  sending the contact messages and buying the board stay the customer's.
  Each kernel mechanism gets its own design review before code; nothing is
  built.
* **2026-10-11 (customer)** **Game mode: a mark first, then the
  protections, behind administrator privilege** (`docs/SETTINGS.md` §8).
  Three answers. That a mark with no effect is acceptable as Settings'
  first version: "yes". That the apps branch be pushed so it can land:
  "yes". And what game mode should do first once something privileged can
  act on it: "disable speculative protections and basically everything that
  brings down latency and brings up fps that might not be acceptable in
  normal high assurance apps, but this path should require admin privileges
  on the system, as otherwise normal apps could disable some protections".
  So the first privileged effect is the speculation protections and the
  other latency and frame-rate trades, not G0's weights first (per
  application is the reading of the question asked and the aim;
  `docs/SETTINGS.md` §7.3 says what a grant reaches); the customer's intent
  is that it be designed and built, which it can be only if the certified
  item takes such a mechanism, a question still open; and it is granted
  only with administrator privilege, never by the user's own
  `game-mode.conf`. Not
  decided by this: whether the administrator is `wheel` with a password or
  root alone, how the kernel learns the grant, and whether and how the
  certified item carries it (*Waiting on the customer*). The consultant's
  order stands: the one-way barrier and a bare-metal figure, then a design
  round, then code (ledger lines 690 and 698). Nothing of it is built.
* **2026-10-10 (customer)** **The game path is on the roadmap, as stage
  24** ("add GAME-PATH.md to main and to the roadmap"), which decides
  question 1 of `docs/GAME-PATH.md` §10 as that document recommended: a
  stage of its own after stage 21's zero-copy landings, the measuring rig
  as its first step, M1 and M4 against a stock Linux desktop as its exit,
  about 68 points before the game domain, and the game domain (52) stage
  14's ground. Questions 2 to 10 of §10 stay open (*Waiting on the
  customer*). Nothing is built or measured, and the points are not in the
  burndown's scope.
* **2026-10-10 (customer)** **Arknights: Endfield is on the roadmap, as
  stage 23**, the road after Steam ("update the roadmap to Endfield (add new
  points after the road to Steam)"): the Windows game through Wine with DXVK
  on NVIDIA's Vulkan, to its world past the login and playable at 1080p60 on
  the RTX 3060. Eight steps, about 205 points as a first guess, from a
  feasibility study of the same day; step 0, the game on plain Linux in the
  same VM shape, is first and can end the stage, because whether the
  anti-cheat accepts a virtual machine is not known. Stage 22's third exit
  step, a Windows game through Steam's own Proton, stays stage 22's; what
  Wine needs of the kernel is sized in stage 23. Nothing is built, and the
  points are not in the burndown's scope.
* **2026-10-04 (customer)** **A docs-only change runs `check-docs`, not
  `check`.** A change that touches only `docs/`, top-level Markdown, the
  skills and agent definitions under `.claude/`, or the generators that write documents alone (`gen-roadmap-charts.py`,
  `gen-arch-doc.py`, `split-roadmap.py`, `build-roadmap-book.sh`) owes
  `cargo xtask check-docs` and nothing more, to save the 3.5 to 9 minutes of
  a full `check` (*What a landing runs*). It needs no gate host. Owed:
  `cargo xtask gate-rows` (`tools/common/xtask/src/gate_rows.rs`, `is_docs`)
  still counts those four generators as code; teaching it is an xtask change
  with its own row.
* **2026-10-04 (customer)** **The roadmap's queue is built first, then
  shortest.** The Gantt's forecast queue orders the sized rows that are built
  and only need landing first, smallest first, then every other sized row,
  smallest points first; ties keep the table's order. The rule is the sort
  key in `tools/common/gen/gen-roadmap-charts.py`, and the status table's
  sized rows follow it. The date does not move at the same rate; more items
  finish sooner and the large ones (Chrome on the DK1, CUDA) last.
* **2026-10-04 (customer)** **The Gantt splits wait from work.** Each
  in-progress row shows, after today, a faint bar for its wait in the queue
  and a light dashed bar for its work, with "N pts left, ~MM-DD" at the end,
  so a row's bar length is its size.
* **2026-10-04 (customer)** **Each session gets its own certification
  consultant.** A session that needs a review briefs its own consultant
  subagent with `AGENTS.md`'s *The certification consultant* and the ledger on
  the gate host, instead of sending to one standing consultant session.
* **2026-10-04 (customer)** **The product owner lands batch stacks.** A
  stack `batch.sh` PASSED is landed by the product owner, under `land.sh
  take` with `git merge --ff-only <tip>`; the sessions whose branches are in
  it join, wait for the verdict and leave `main` alone.
* **2026-10-04 (customer)** **Full runs are batched.** Branches ready to
  land share one run of the image row instead of paying it each:
  `~/.local/share/ferrix/fleet/batch.sh` stacks them on `main`, runs the
  union of their gates once across the gate pool's slots, finds a failing
  entry by running its failed gates on the stack's prefixes at once, and the
  stack lands whole. How the product owner runs it is in `AGENTS.md`, *The
  product owner*, *Batching full runs*.
* **2026-10-03 (customer)** **The `--everything` desktop runs as the user
  `ferrix`, and the machine keeps the users' files apart from the system.**
  The session is started the way `docs/AUTH.md` §6 plans it, not as an
  interim: `sessiond` owns seat0 and hands hyprix its devices, and hyprix
  and every client it starts run as `ferrix` (P2.4, P2.5), the device nodes
  staying `0660 root`. The users' files are on a disk of their own,
  `build/home.img` labelled `ferrix-home` and mounted at `/home`.
  `--reset-root` starts only the system's root over, keeping `/home` and
  `--persistent`'s `/data`; the new `--reset-flash` starts all three over.
  The host's `hyprland.conf`, waybar and fuzzel settings are copied into
  `/home/ferrix` once, when absent, so edits made in Ferrix are kept;
  `--reset-flash` copies them again. With the consultant's seat empty, an
  interim reviewer subagent of the session doing the work reviews its
  kernel changes, and its verdicts go in the consultant's ledger.
* **2026-10-02 (customer)** **GPU support is NVIDIA's own driver.**
  NVIDIA's open-gpu-kernel-modules are ported so that their OS-agnostic core
  (RM, NVKMS, GSP boot for Ampere) runs as a Ferrix driver. NVIDIA's
  unmodified userspace (`libnvidia-*`, the Vulkan and GL ICD, later CUDA)
  talks to `/dev/nvidia*` as on Linux. Not nouveau and not NVK. The
  hardware is nazuna's RTX 3060, through libvirt, and only while the
  customer's domains sharing it are shut off. The order follows yserver's:
  a feasibility pass and a written design (`docs/NVIDIA.md`), then the
  customer's decisions, then small landings. The customer answered the
  design's questions the same day (`docs/NVIDIA.md` §9). The core runs in
  `nvrm`, a ring-3 Linux-personality program. GL for X clients is DRI3 and
  Present in yserver. CUDA is wanted now, alongside the graphics, with
  `nvidia-uvm` designed by a separate session. A monitor on the 3060 comes
  later, and N6 waits for it. The sources are fetched at 580.173.02 and
  never committed. The copy layer comes before dmabuf. Each platform change
  inside the item goes through the consultant on its own. Agents may start
  `ferrix-3060` whenever the customer's domains sharing the card are shut
  off; the 3090 is never touched.
* **2026-10-02 (customer)** **Build System V shared memory for Steam's web
  helper's MIT-SHM.** Asked by the session making Steam smooth: the web
  helper's Chromium presents its software-composited frames to yserver with
  MIT-SHM, whose classic path is `shmget`/`shmat`/`shmctl(IPC_RMID)` while
  both sides are attached; with those calls `ENOSYS` every 8 MiB frame went
  through the socket as a core `PutImage`. Built on branch `steam-sysv-shm`
  (`syscall/shm.rs`, the boot's `shm` line, `test-shm`), modelled on the
  semaphores: per IPC namespace, `ipcperms`, the per-job bound, and i386's
  `ipc` and direct numbers. Message queues stay `ENOSYS`.
* **2026-10-01 (customer)** **The speculation domain lands ahead of its full
  gate.** os-c7 (os-86 after the restart) asked whether to wait for the last
  controls and rows. The customer answered "speculatively merge what you
  have", and in the same session gave its word for the push to `origin`.
  It landed as bf9efba95. The consultant (os-bd) recorded it in its ledger as
  landed ahead of its evidence. What was still running is a row under
  *Verification audit*.
* **2026-10-01 (customer)** **The barrier between programs is skipped only
  inside a speculation domain, and a domain is a job marked at its
  creation.** Asked by os-c7, whose native round trip is 6.5 us with every
  barrier and 2.6 us without, about 4.4 us of it the `IBPB` and return-stack
  refill at each switch between programs. The certification consultant
  recommended it, and the customer chose it over keeping the barrier at every
  switch and over Linux's opt-in mode. The rules:
  - Only the holder of the parent job's MANAGE right may mark a job as one
    speculation domain, and only when creating it. The mark can't be added
    later, a job that already has processes can't be marked, child jobs don't
    inherit it, and it is off by default. Marking a job writes an audit
    record.
  - A switch between programs of the same marked job skips the predictor
    barrier. Every other switch keeps it, on every architecture.
  - The claim narrows to "no program can read one outside its speculation
    domain" (`docs/certification/SPECULATION.md` §3, the Security Target's
    O.ISOLATE). A new assumption of use says the integrator places in one
    domain only programs that may read each other's memory.
  - What it takes to land, set out in the consultant's conditions to os-c7:
    a design note first; the requirements; a check that counts barriers on
    each processor; negative controls (the domain compare always true, the
    MANAGE test removed); and the vulnerability-analysis row.
  Option (b), the barrier by credentials, was rejected: credentials belong to
  the Linux personality, and the job is the item's isolation unit.
* **2026-10-01 (customer)** **The certification targets are a must, and the
  goal is to reach them.** That is all four in
  `docs/certification/README.md`: Common Criteria EAL5+, DO-178C DAL C, IEC
  62304 Class C and EN 50716 SIL 2. The work that produces their evidence is
  required, not optional: traceability, coverage, checks with negative
  controls, the findings register and the vulnerability analysis. Changes to
  the certified item go to the certification consultant before they land
  (`docs/CONVENTIONS.md`). `docs/certification/CLAIM.md`'s two questions,
  the claim's wording and which standard is pursued first, stay open until
  the customer settles them.
* **2026-09-28 (customer)** **A live installer, as Linux distributions
  have one** (`docs/INSTALLER.md`). The customer chose: real PCs as well as
  virtual machines (so NVMe, AHCI, xHCI, USB mass storage, i8042 and a
  firmware-framebuffer display join the plan); a graphical installer on
  hyprix from the start; installing beside another operating system, not
  only on a wiped disk; and a raw `.img` and a hybrid `.iso`, both attached
  to GitHub releases. The design (21 slices, 162 points) was **approved the
  same day**, with §10's recommendations taken for decisions 2 and 5: an
  auto-login `ferrix` live user and a text-mode fallback. **Secure Boot is
  supported** through Ubuntu's Microsoft-signed shim and a Ferrix key the
  owner enrolls once in MokManager (§5.7, I13, 13 points). For decision 1 the customer wants **the full setup for
  shrinking the other system's partition**: the installer shrinks NTFS,
  ext4 and btrfs itself (§4.6, S1–S6, 76 points; 251 in all). Order: the
  VM path I1–I12 first, S1 beside it, then the PC slices.
* **2026-09-28 (customer)** The yserver Wayland backend's design
  (`docs/YSERVER.md`, 7 slices, 36 points) is approved. Its code lives on a
  **fork of yserver on the customer's GitHub account**, pinned by commit in
  `tools/common/fetch/fetch-yserver.sh`, not as a patch series here and not
  upstream first. The backend speaks Wayland through **Ferrix's own**
  `compositor-wire`, `compositor-protocol` and `compositor-shm`, not the
  `wayland-client` crate.
* **2026-09-27 (customer, relayed by ferrix-f6)** **Steam's X server is
  yserver** (github.com/joske/yserver, Rust, MIT), not C Xwayland with
  xwayland-satellite. Ferrix adds the rootless Wayland backend yserver
  lacks, behind its backend trait: each top-level X window an
  `xdg_toplevel` on hyprix, input and the clipboard bridged.
  xwayland-satellite (MPL-2.0) is read as a reference, not copied. Order:
  a feasibility pass on a pinned release (headless backend on Ferrix, an X
  client against it), then a written design sent to the coordinator, then
  the build in small landings (`docs/roadmap/stage-22-steam.md`). Owner:
  ferrix-c3 (was ferrix-41).
* **2026-09-27 (customer, ferrix-55)** `src/kernel/src` is grouped by what each
  file is (`docs/LAYOUT.md`): `arch/<isa>/`, `arch/arm_common/`,
  `platform/<vendor>/<soc>/`. Every file kept its ring in the move.
  `#[cfg(target_arch)]` stays under `arch/` alone -- the certification
  consultant declined an exception for driver selector files -- so
  architecture-bound drivers stay under `arch/`.
* **2026-09-26 (customer)** **Authentication is a ring-3 service,
  `authd`, as `docs/AUTH.md` proposes, with all eleven of its decisions as
  recommended.** Among them: Argon2id written in the tree, root locked and
  `wheel` members becoming root with their own password, a throttle and no
  permanent lockout, programs that read `/etc/shadow` refused and PAM
  programs given a shim later, and a lock screen that will not lock an
  account with no password. Phase 1 locks the desktop with root's
  password until phase 2 moves the desktop off root.

* **2026-09-26 (customer)** **Doom is room4doom, fetched at build time,**
  never committed. It is labelled MIT but calls itself a transliteration of
  id's GPL-2.0 C source, so Ferrix treats it as GPL: cargo fetches it as a
  git dependency pinned to one revision, the way Chrome is fetched, and the
  repository holds only Ferrix's MIT backend for it (`docs/MEDIA.md` §1).
* **2026-09-26 (customer)** The fleet has a coordinator, ferrix-2c, which
  keeps the landing order under the landing lock (standing rules above); the
  customer stays product owner for scope. No new scope starts without the
  customer.
* **2026-09-26 (customer)** Audio moved forward to current work:
  `/dev/snd` over a ring-3 virtio-snd driver, then a sound server
  (`docs/AUDIO.md`).
* **2026-09-26 (customer)** The desktop's clients are Rust programs that
  read the real dotfiles unchanged: waybar, fuzzel, hyprlock and hypridle
  over shared clients-base crates (`docs/DESKTOP-CLIENTS.md`).
* **2026-09-26 (customer)** The 32-bit x86 ABI is Steam's next step
  (`docs/I386.md`).
* **2026-09-26 (customer)** The kernel takes an EDID override,
  `drm.edid_firmware=`, so `run-compositor`'s screen can be the host's
  monitor (55aca9d1).
* **2026-09-26 (customer)** Certification engineering: F-23 at every site,
  F-31 behind an on/off switch, F-35's quotas built into Jobs, and coverage at
  100 % or justified line by line (`docs/certification/`).
* **2026-09-26 (customer)** The repository is laid out again
  (`docs/LAYOUT.md`), in one short freeze.
* **2026-09-26 (customer)** A USB CDC-ACM device driver for the Pixel 7
  (`docs/vendor/google/pixel7/USB-HANDOVER.md`).

* **2026-09-24 (customer)** **Gears, as the 3D demo: real vkgears through
  Venus, and GLES2 gears on the DK1's own GPU.** vkgears runs in QEMU on the
  Linux host, where Venus carries Vulkan to the host's GPU. Mesa's Venus
  driver is built as a static archive against ferrousli and presents over
  `wl_shm` until dmabuf lands. The DK1's Vivante GC400T is an OpenGL ES 2.0
  core with no Vulkan in any driver, so the board gets gears drawn by that
  core through a ring-3 driver of Ferrix's own, rather than vkgears on a CPU
  Vulkan that would test no GPU. `docs/GPU.md` §6 has the reasons and the
  steps: 39 points for Venus, 32 for the GC400.

* **2026-09-23 (customer)** **Stage 13's cgroups come before init.** The
  real init that is the rest of stage 15 is planned as if cgroup v2 exists,
  and stage 13's cgroup half is built first to make that true. Its namespaces
  and seccomp follow and are not init's prerequisites. `docs/INIT.md` is the
  design, §0.1 what it needs from stage 13. **And stage 13 builds every
  cgroup over a `Job`** (C8): one container object, seen as a cgroup by the
  Linux ABI and as a job by the native one, so that a microkernel can keep
  the jobs if it drops cgroupfs. The rest of `docs/INIT.md` §14 is still
  open.
* **2026-09-18 (customer)** **Steam is on the roadmap, as stage 22**, the
  step after the GPU decision below: the client starts and logs in, a
  native game installs to btrfs and plays with sound through the GPU path,
  and a Windows game runs through Proton. It is a guest's stage first and
  does not wait for bare metal. What it stands on that the roadmap did not
  stage until now -- the 32-bit x86 ABI, XWayland, sound, Vulkan through
  Venus -- is written into the stage as a list of what has to be true, with
  first guesses that sum past 300 points; the rest is stages 12, 13 and
  dynamic linking, which were already there.
* **2026-09-18 (customer)** The GPU: **Path A first, Path B in a later
  stage.** Path A is GPU acceleration for Ferrix as a guest through
  virtio-gpu's 3D commands, which uses the host's NVIDIA driver without
  porting it: the ring-3 driver's 3D commands, a render node with the
  `virtgpu` ioctls and 3D scanout, the host half in xtask, and a Rust virgl
  encoder as the compositor's renderer behind a renderer trait -- 52 points,
  in that order, `docs/GPU.md` §3. Path B is a driver for an NVIDIA card
  under Ferrix itself, for the day Ferrix runs on bare metal: stage 21,
  unsized, `docs/GPU.md` §4. This settles the third of the three choices
  left open on 2026-09-13 below: neither Mesa on ferrousli nor Vulkan for
  the compositor, but a Rust encoder of virgl's command stream, with Mesa
  kept for the day clients render on the GPU themselves. The Windows QEMU
  already offers `virtio-gpu-gl`; the gate host's has to be rebuilt for it.
* **2026-09-17 (customer, delegated to the GUI session) The compositor
  server is written from scratch, not on Smithay.** The customer asked for
  the work to go on without stopping for questions, which settles the open
  choice of 2026-09-13 below. Smithay's value is its backends -- udev,
  libinput, libseat, GBM, EGL and its DRM session handling -- and every one
  of those is C, which `src/user/system/linux/compositor/README.md`'s no-C-device-stack rule
  already forbids and which this tree has already replaced: `blank` drives
  `/dev/dri/card0` itself, `src/lib/drivers/input/virtio-input` is the input driver, and
  `src/user/system/linux/compositor/render` is the renderer Smithay's own pixman would have been.
  What would be left of Smithay is `wayland-server`'s marshalling and its
  protocol handlers, and taking those means every Ferrix system call they
  make is a dependency's choice rather than this tree's -- on a kernel whose
  Linux surface is still being filled in, that turns a compositor bug into a
  hunt through someone else's crate. Writing the wire protocol is about 20
  points more before the first client, and buys a `no_std`-shaped,
  host-tested, fuzzed crate in the same shape as `src/lib/network/netwire`, `src/lib/fs/common/cpio`
  and `src/lib/proto/inputctl`. The protocol XML this is written from is on this
  machine (`/usr/share/wayland/wayland.xml`,
  `/usr/share/wayland-protocols/`), and so is Hyprland 0.56.2's own source,
  the behaviour reference. `xkbcommon` stays the one C library allowed at
  stage 18 and is unaffected; the stage 19 GPU choice was settled on 2026-09-18.

* **2026-09-15 (customer)** The customer holds the product owner seat:
  there is no product-owner session. A session lands when the gate its
  change's row names has passed on the commit being landed, and commits to
  `main` directly rather than through `develop`, which is retired. What does
  not change: the gate table, judging a gate by its output, and that a
  landing carries its own documentation.

* **2026-09-14** The busybox built against ferrousli is the primary busybox:
  the userland Ferrix is measured with, first in every `test-shell` and
  `test-vfs` the gates run. The musl and glibc busyboxes stay required as
  compatibility checks. A `src/user/system/linux/ferrousli/` landing rebuilds it and runs both with
  it. This carries out the customer's 2026-09-13 order below once the binary
  passed both, at 5e9b0b6 with no stub reached.

* **2026-09-13 (customer)** The goal after `rustc` is a Hyprland-shaped
  Wayland compositor, written in Rust, running on Ferrix. Roadmap stages 17
  (display and input), 18 (the compositor) and 19 (Hyprland fidelity and the
  GPU) carry it; self-hosting moves to stage 20. Pulled onto the path by it:
  `AF_UNIX` with `SCM_RIGHTS` (from networking), `memfd_create` with sealing
  and `MAP_SHARED` file mappings (stage 8), and the `epoll`, `eventfd`,
  `timerfd` and `signalfd` families. `xkbcommon` is the one C library
  allowed at stage 18; the other two choices named that day were settled on
  2026-09-17 (from scratch, not Smithay) and 2026-09-18 (the GPU's Path A).
* **2026-09-13 (customer)** POSIX.1-2024 compatibility is a goal, on the
  condition that it never breaks Linux compatibility. Ferrix takes POSIX
  through its libc over the Linux ABI (ARCHITECTURE §2), so the goal costs
  the kernel nothing new in kind: every mandatory POSIX.1-2024 interface is a
  Linux system call the kernel must answer as Linux does, and the libc side is
  ferrousli's. Where POSIX and Linux differ, Linux wins. Rows: threads (P0),
  the POSIX interface sweep and `AF_UNIX` sockets (P1), libc-test as the
  measure (P2). `AF_INET` stays with the networking stage.
* **2026-09-13 (customer)** Ferrousli is to replace the musl and glibc
  busyboxes as the userland Ferrix is measured with, as fast as it can be
  done: once its busybox passes `test-shell` it becomes the primary binary of
  that gate, with musl and glibc kept as the compatibility checks. Static musl
  stays the goal path to `rustc`, whose `std` targets it.
* **2026-09-13 (PO)** `vmo_map` refuses executable mappings in its first
  landing, and the roadmap records that under stage 9's "Left for later
  stages", not as done: an EXECUTE right on the VMO handle comes with
  `process_create`'s native loader, its first consumer.
* **2026-09-13 (PO)** The DK1 reset follow-ups, as specified: the loader reads
  `bootargs` from a `CMDLINE.TXT` on the ESP so `ferrix.onexit=reset` survives
  a reset without U-Boot's `saveenv`; `test-boot --reset` boots with that option
  and requires QEMU to show a reset, not a power-off. Both go into
  `docs/vendor/st/stm32mp157-dk.md` with the landing.
* **2026-09-13 (customer)** Order of everything: first a working `main`, second
  ferrousli on the build with busybox rebuilt against it, third being surer
  that `main` is stable before it moves. Ferrousli is on the roadmap, and
  its busybox is a `test-shell` target.

* **2026-09-13** Stage 10's exit criterion requires the out-of-domain DMA fault
  on x86-64 and AArch64 only; ARMv7-A's virtio-pci runs in degraded trusted
  mode because U-Boot forces the SMMU bypass. A ring-3 driver reading sectors
  through an untranslated domain may land so stage 11 can proceed, but stage
  10 is not done until domains translate and the fault is shown.
* **2026-09-13** Stage 11's read stage refuses a volume with a log tree, with a
  clear message; log replay is stage 12's. The default subvolume, data
  checksums and a node cache are required before stage 11 is done.
* **2026-09-13** The page-cache interface, agreed between stages 8 and 11: a
  `PageSource` in `src/lib/fs/common/vfs` with `fill_range(first, pages)` filling at least
  one page, stopping at an extent boundary, zeros past the file's size, called
  under no lock and allowed to block; a checksum failure is `EIO`, never a
  zeroed page; the kernel VMO allocates first, fills with no lock held and
  inserts only if the page is still absent; `read` reports `EIO` and a fault
  `SIGBUS`. Eviction and writeback are stage 12's. Order: the VMO reverse
  map, then `PageSource` with tmpfs over it, then file-backed `mmap`.
* **2026-09-13** A ring-3 driver serving a disk must never fault on a file
  mapping of that disk, or it waits on its own completion: its image and data
  come from initramfs, tmpfs, anonymous or ring VMOs, or are committed before
  any pivot onto btrfs. Stage 10's `devmgr` enforces it.
* **2026-09-13** The page cache is the inode's VMO, on tmpfs and on btrfs
  alike, and file-backed `mmap` and `read` share its pages. One interface,
  agreed between the stage 8 and stage 11 owners before either writes it.
* **2026-09-13** No doc gate compares quoted boot lines with a live log; they
  are illustrations and the roadmap says the numbers move. The host-test table
  becomes a generated document with a gate instead.
* **2026-09-13** CI's 120 s boot timeout stays. A quiet boot takes about 10 s;
  local runs on a loaded host use `--timeout 600`.
* **2026-09-13** (customer, relayed) Drivers stay in ring 3; no interim
  kernel-side disk path, even as a test harness.

---
