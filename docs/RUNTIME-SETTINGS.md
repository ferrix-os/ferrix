# Runtime settings

An inventory, taken from the code on `develop` at 9369e0f40 (apps at
876868f, tole at 121d509) on 2026-10-23, of every setting Ferrix honours at
run time: kernel boot words (§1), boot words ring-3 programs read from
`/proc/cmdline` (§2), configuration files (§3), live controls (§4) and,
for contrast, the build-time flags that are **not** run-time settings (§5).
Where a document says one thing and the code another, the code is what is
listed and the difference is a finding (§6). It was written for the
designer of the Settings app; §7 says which settings would come first.

**Classes**: **user-facing** = a sensible Settings page; **admin** =
administrator or developer, belongs in an advanced page or nowhere;
**test-only** = used by gates, never in Settings.

**In numbers**: 27 kernel and loader boot words, plus the four slot words
the loader writes and nobody sets (§1); 18 boot words read by ring-3
programs, plus any word through `ConditionKernelCommandLine=` (§2); 32
configuration files or file families (§3); 18 live-control tools and 11
writable kernel files or calls, among them 17 writable cgroup files (§4.1,
§4.2); 22 rows of environment variables, about 45 names (§4.3); 14 build
flags (§5); 18 findings (§6).

**What the designer should know first**

* **Almost nothing is live.** Kernel boot words are read once, at boot.
  No Ferrix program watches a configuration file (no inotify, no SIGHUP
  reload) except authd, which re-reads its service policy on every
  conversation, and the launchers, which re-read `game-mode.conf` at every
  launch. hyprix re-reads `hyprland.conf` only on `hyprctl reload`, and
  takes `hyprctl keyword` and `hyprctl setting` at once (§3.3). A
  Settings app must either restart the program that owns a setting or
  drive its live control (§4).
* **Boot words persist in `\FERRIX\CMDLINE.TXT`** on the EFI system
  partition (owner's, first word wins) and `DEFAULTS.TXT` (the image's);
  on an A/B install, in the slot's header (`bootctl try --cmdline`). There
  is no tool today that edits `CMDLINE.TXT` on a running system.
* **The certified configuration** is the `present` profile. A Settings
  page that writes a boot word must not offer a value `present` does not
  permit without saying the machine leaves the reference configuration
  (§1.2's "Cert. config" column).
* **Settings exists with one page, Game mode**, which writes
  `~/.config/ferrix/game-mode.conf` (§3.5).

## 1. Kernel boot words

### 1.1 How a boot word reaches the kernel

The kernel reads one command line, whitespace-separated, **the first word
with a key wins** (`ferrix_bootinfo::option_in`, `flag_in`,
`src/lib/proto/bootinfo/src/lib.rs`). Where it comes from depends on the
loader:

| Loader | Where the words come from | Persistent? | Code |
|---|---|---|---|
| UEFI loader (x86-64, AArch64, ARMv7-A, DK1 via U-Boot's EFI, the N150) | `\FERRIX\CMDLINE.TXT` (the owner's), then `\FERRIX\DEFAULTS.TXT` (the image's, written by `cargo xtask` / `flash`), joined owner first, trimmed, at most the room between `BootInfo` and the region array | yes, files on the ESP; `flash` rewrites only `DEFAULTS.TXT` | `src/boot/common/uefi/src/main.rs` `CMDLINE_PATH`, `DEFAULTS_PATH`, `load_cmdline` |
| UEFI loader booting an A/B slot | the loader's own `ferrix.slot=… ferrix.slot.boot=… ferrix.slot.seq=… ferrix.slot.disk=…` first, then the slot header's command line (`bootctl try --cmdline`) | yes, in the slot | `src/boot/common/uefi/src/slots.rs`, `src/lib/proto/slots/src/plan.rs` |
| Device-tree boards (U-Boot) | `/chosen/bootargs` as a **fallback**, read only by the options marked "DT" below | U-Boot environment | `discovery::fdt` `bootargs()` |
| Pixel 7 loader | `console=ramoops,0xfd3ff000,0x200000 ferrix.fbcon` + `FERRIX_PIXEL7_CMDLINE_EXTRA` (build time) + every `ferrix.*` word of ABL's `bootargs`; in a crosvm guest `console=uart8250,mmio,0x3f8` + crosvm's `ferrix.*` words | build time / boot image header | `src/boot/vendor/google/pixel7/src/board.rs` `CMDLINE`, `GUEST_CMDLINE`; `main.rs` `command_line` |

The kernel keeps the line for `/proc/cmdline` (`fs::procfs::remember_command_line`),
which is how every ring-3 reader in §2 sees the same words.

The kernel-side words are declared in a registry, `tools/common/data/build-flags.toml`
`[boot_options.*]`, and a read of an undeclared option fails the gate
(`check-build-flags.py`). Each profile in `tools/common/data/profiles/*.toml`
lists the values it permits; **`present` is the certified (reference)
configuration** (`certification-item.json` `reference_configuration`).

Column key for §1.2: **Ring** is where the reader lives in
`certification-item.json` (`core`/`item` = inside the certified item,
`load` = above it, `loader` = `src/boot`). **Cert. config** is what the
`present` profile permits (— = not permitted, so using it leaves the
reference configuration). **DT** = also read from `/chosen/bootargs`.

### 1.2 Every boot word the kernel and loaders read

| Word | Values | Default | Effect | Arch | Ring | Cert. config (`present`) | Requirement | Class |
|---|---|---|---|---|---|---|---|---|
| `ferrix.checks` | `run`, `skip` | `run` | `skip` brings stages 2–12 up without their self-checks; the boot ends `FERRIX-BOOT-UNCHECKED`. Other values said and ignored. DT. | all | item (`checks.rs`) | `run` only | H.BOOT.2, H.AUD.12 | admin (a "fast boot" candidate; desktop images ship `skip` in `DEFAULTS.TXT`) |
| `ferrix.root` | `btrfs`, `tmpfs` | `btrfs` (switch to a volume labelled `ferrix-root` if one exists) | `tmpfs` keeps `/` in memory (the live installer medium). Other values said; btrfs tried. | all | load (`fs/root_disk.rs`) | any | — | admin (installer writes it) |
| `ferrix.init` | absolute path | the built-in program, else `/sbin/init` | the file pid 1 is started from; a relative value is said and ignored. DT. | all | item (`init.rs`) | any | H.BOOT.15, L.init.1 | admin |
| `ferrix.onexit` | `reset`, `panic` (anything else = power off) | power off | what the machine does when init exits. DT. | all | item (`power.rs`) | any | H.BOOT.6, H.BOOT.7, H.FAIL.1 | test-only in practice (gates) / admin |
| `ferrix.panic` | `halt`, `reset` | `halt` | `reset` restarts the machine 10 s after a panic report (stop-gap for the N150). Anything else = halt. | all | core (`panic/reset.rs`) | `halt` only | H.FAIL.5, L.panic.1, L.panic.4 | admin |
| `ferrix.pstore` | `on`, `off` | `on` | whether the UEFI loader reserves the RAM region the kernel log survives a reset in (`lode -b -1`, `LASTKERNEL`). Anything else = off. | UEFI loader | loader (`src/boot/common/uefi/src/pstore.rs`, `ferrix_bootinfo::pstore::switch`) | `on` only | L.console.42–44, 46–48 (per `certification-item.json`) | admin |
| `log_buf_len` | memparse size (`1M`, `0x…`, K/M/G) | 128 KiB | grows the kernel log to the next power of two, max 4 MiB; smaller keeps 128 KiB | all | core (`console/log.rs`) | not listed in `present.toml` (registry only) | L.console.57 | admin |
| `ferrix.fbcon` | flag | off | draw the kernel's console on the boot framebuffer until a display driver takes it | all with a boot framebuffer | core (`console/screen.rs`) | permitted | L.console.36, L.device.41 | user-facing ("show boot messages") |
| `ferrix.fbcon.small` | flag | off | single-size glyphs whatever the width | same | core | permitted | L.console.37 | admin |
| `ferrix.efifb` | flag | off | lend the firmware framebuffer to the ring-3 `efifb` driver (GPU-less PC, firmware-screen desktop); the PCI function holding it is left undriven. Arm: recorded, not honoured | x86-64 | item (`discovery/firmware.rs`) | — (AoU-31) | L.device.38–43, L.device.46 | admin |
| `ferrix.intelgpu` | flag | off | let the Intel integrated GPU (00:02.0) be driven by the ring-3 `intel` driver | x86-64 | core (`device.rs`, read in `main.rs`) | — | H.DMA.10, L.device.44, L.device.46 | admin (hardware enablement) |
| `ferrix.devmgr` | `kernel`, `init` | `kernel` | `init`: pid 1 starts `devmgr` and `/` waits for its disk (every image that boots `/sbin/init` uses it). DT. | all | item (`discovery/devmgr.rs`) | `kernel` only | L.device.54 | admin (retires at M5) |
| `ferrix.fastpath` | `on`, `off` | `off` | the `channel_write_read` IPC fast path; `on` only takes effect on x86-64. Others said and ignored. DT. | x86-64 (recorded elsewhere) | item (`fastpath.rs`) | `off` only | L.x86_64.159, L.x86_64.161, L.sched.81 | developer (becomes build flag `ipc_fastpath`) |
| `ferrix.vtd` | `on`, `off` | `on` | `off` leaves every VT-d unit alone (no DMA translation, no interrupt remapping): recovery only. Since the DMA gate devmgr then starts no driver, so a recovery boot needs `ferrix.unisolated` as well | x86-64 | core (`iommu.rs`) | `on` only | L.iommu.71 | admin (recovery) |
| `ferrix.amdvi` | `on`, `off` | `on` | `off` leaves every AMD-Vi unit alone (no DMA translation, no interrupt remapping): recovery only. Since the DMA gate devmgr then starts no driver, so a recovery boot needs `ferrix.unisolated` as well | x86-64 | core (`iommu/ivrs.rs`) | `on` only | L.iommu.98 | admin (recovery) |
| `ferrix.unisolated` | flag | off | devmgr may start the drivers of bus masters whose DMA is not translated or whose interrupts are not isolated, each said on an `UNISOLATED` line; the kernel says it at stage 10 and audits it (`Config::Unisolated`). Every ARMv7-A image carries it (outside the claim, G4). No certified profile may permit it (`claimless`) | all | item (`discovery/devmgr.rs`) and ring 3 (`devmgr-proto::admission`) | — (AoU-8) | L.device.57–59 | developer (development images only) |
| `ferrix.apic` | `x2apic`, `xapic` | x2APIC only where firmware locked it or left it on | local APIC mode | x86-64 | core (`arch/x86_64/apic.rs`) | both | L.x86_64.131 | admin |
| `ferrix.epp` | `performance` (0x00), `balance_performance` (0x80), `balance_power` (0xc0), `power` (0xff), or decimal 0–255 | `performance` (0) | HWP energy/performance preference every CPU asks for; boot-time only, nothing changes it later | x86-64 with HWP | core (`arch/x86_64/cpu.rs`) | the four names | L.x86_64.182 | **user-facing** ("power mode") |
| `ferrix.msrlog` | flag | off | read-only `msr:` log of RAPL limits, limit reasons, thermal MSRs | x86-64 | core (`arch/x86_64/msrlog.rs`) | — (no profile permits it) | L.x86_64.187–189 | developer |
| `ferrix.msrlog.period` | seconds 0–3600 (0 = once) | 5 | period of the above | x86-64 | core | — | L.x86_64.187 | developer |
| `ferrix.powerctl` | flag | off | publish the power node: the ring-3 `power` driver may raise the VR current limit (Alder Lake-N / N150 only) | x86-64 N150 | core (`arch/x86_64/powerctl.rs`) | — (no profile permits it) | L.x86_64.194–209, L.device.51–53 | admin (hardware tuning) |
| `ferrix.powerctl.fake` | flag | off | the same over a fake register file | x86-64 | core | — | L.x86_64.207 | test-only |
| `ferrix.btrfs` | `churn`, `replay` | the ordinary stage-12 check | btrfs power-fail test modes | all | load (`fs/btrfs_powerfail.rs`, a check file) | — | — | test-only |
| `ferrix.btrfs.seed` | u64 | 1 | seed for `churn` | all | load | — | — | test-only |
| `console` | AArch64: `uart8250,mmio,0x…` or `ramoops,0x…,0x…`; ARMv7-A: `pl011`, `stm32` (DT bootargs) | PL011 / the tree's `stdout-path` | which port is the kernel console | AArch64, ARMv7-A | core (`arch/*/console.rs`) | any | — | admin |
| `drm.edid_firmware` | Linux's grammar: `[connector:]edid/file.bin[,…]` | none | EDID file from `/lib/firmware` per connector (`Virtual-1`, …) | all | load (`interfaces/display/edid.rs`) | any | — | admin |
| `nokaslr` | flag | KASLR on | loader keeps the kernel where it was linked (xtask `--gdb` adds it) | UEFI, Pixel 7 loaders | loader (`kaslr.rs`) | not listed | H.BOOT.8 (KASLR) | developer |
| `nosmp` | flag | all CPUs | start the boot processor alone | AArch64, ARMv7-A | core (`arch/*/smp.rs`) | not listed | L.aarch64.36 | developer |
| `noactlr` | flag | read ACTLR | do not read ACTLR (firmware traps it). DT. | ARMv7-A | core | not listed | (L.armv7a.3 names ACTLR) | developer |
| `ferrix.slot`, `ferrix.slot.boot`, `ferrix.slot.seq`, `ferrix.slot.disk` | `a`/`b`; `trial`/`good`/`fallback`; n; GPT GUID | absent (not a slot boot) | **written by the loader, not set by anyone**: which A/B slot booted. Read by `bootctl` from `/proc/cmdline` | x86-64 (UEFI A/B) | loader | — | — | read-only information |

Not boot words although they look like it: `ferrix.devmgr.events`,
`ferrix.devmgr.update`, `ferrix.vport`, `ferrix.auth.seat`, `ferrix.log`,
`ferrix.clipboard`, `ferrix.vfs` (socket and directory names),
`ferrix.port0..2`, `ferrix.nvme.*`, `ferrix.nonet`, `ferrix.n150.port`
(QEMU device ids in `tools/common/xtask/src/qemu.rs`), `ferrix.img`
(the image file), `ferrix.test` (xtask's net test name).

### 1.3 What xtask and the installer put on the line

| Source | Words | Code |
|---|---|---|
| Every image that boots `/sbin/init` (`CMDLINE.TXT`) | `ferrix.init=/sbin/init ferrix.devmgr=init` | `tools/common/xtask/src/init.rs` `command_line` |
| Desktop images (`run-compositor`, `--everything`, `DEFAULTS.TXT`) | `ferrix.checks=skip`, plus `drm.edid_firmware=…` with `--edid` | `compositor/run.rs` `DESKTOP_DEFAULTS` |
| Board desktop (`flash --compositor`) | `ferrix.checks=skip ferrix.init=/sbin/init` | `compositor/run.rs` `DESKTOP_BOARD_DEFAULTS` |
| N150 box | `ferrix.checks=skip ferrix.efifb ferrix.fbcon` | `compositor/n150_box.rs` `BOX_OPTIONS` |
| `--gdb`, `--reset`, `--init-path P`, `--kernel-option W` | `nokaslr`, `ferrix.onexit=reset`, `ferrix.init=P`, `W` | `main.rs` `image_cmdline` |
| `lode` gates | `log_buf_len=1M` | `lode.rs`, `lode_budget.rs` |
| Installer: live medium / installed system | `ferrix.root=tmpfs` → rewritten in place to `ferrix.root=btrfs` | `src/user/system/linux/installer/src/main.rs` `LIVE_ROOT`, `INSTALLED_ROOT` |
| `bootctl try --cmdline "words"` | replaces a slot's command line | `installer/src/bootctl.rs` |

## 2. Boot words read by ring-3 programs

Read from `/proc/cmdline`, once, when the program starts. None of these is
in the kernel's registry: a library names no boot option, the program that
reads it does (`check-build-flags.py`). No program in the compositor
stack, auth, lode, tole or the apps reads `/proc/cmdline`; `devmgr` and
the net, input, disk, sound and display drivers read no boot word either;
the USB host driver reads one.

| Word | Reader | Values | Default | Effect | Class |
|---|---|---|---|---|---|
| `ferrix.target` | init (`src/user/system/linux/init/init/src/main.rs:244`) | a unit name | `default.target` (→ `multi-user.target`; desktop images link it to `graphical.target` in `/etc/ferrix/units`) | the target init boots; a bad name or a failed transaction isolates `rescue.target` | admin ("boot to console / desktop / rescue" is a user-facing candidate) |
| any word | init's `ConditionKernelCommandLine=` (`init/src/probe.rs`) | `word` or `key=value` | — | unit conditions; no shipped unit uses it | admin |
| `console` | `getty-generator` (`init/getty/src/bin/getty-generator.rs`) | `NAME[,speed]`, `/dev/` stripped; `uart…` and address forms mean `console` | `console` | one `getty@NAME.service` per console | admin |
| `ferrix.statd.interval` | the statd app (its `src/main.rs:221`, apps component) | ms | 500, at least 100 | sampling interval; `--interval` wins | test-only (Pixel 7 stat runs) |
| `ferrix.statd.seconds` | statd (`:225`) | seconds | 0 = forever | run length; `--seconds` wins | test-only |
| `ferrix.slot`, `.boot`, `.seq`, `.disk` | `bootctl` (`installer/src/bootctl.rs`, `ferrix_slots::plan::parse_words`) | as §1.2 | — | which slot this system runs; refuses to act when record and line disagree | read-only |
| `ferrix.usbhid.reports` | usbhid (`native/drivers/usb/usbhid/src/main.rs`, parsed in `src/lib/drivers/usb/usb-host/src/watch.rs`) | alone or `all`; `off`; `vvvv:pppp`; a kind (`keyboard`, `mouse`, `media`, `system`, `input`); `vvvv:pppp/kind` | off | say every interrupt report of the functions named, raw, stamped, at debug; `/run/usbhid-reports` with the same value overrides it live while it exists (`docs/XHCI.md` §13) | developer |
| `ferrix.intelgpu.modeset` | intel driver (`native/drivers/gpu/intel/src/display/modeset.rs`) | `dry`, `on` | nothing | `dry` prints the second scanout's modeset plan, `on` carries it out | test-only (N150 bring-up) |
| `ferrix.intelgpu.stop` | intel driver (`steps.rs`, parsed in `src/lib/drivers/gpu/intel-xelp/src/options.rs`) | step number ≥ 1 (non-number = 1) | none | stop before step N and touch nothing more | test-only |
| `ferrix.intelgpu.trace` | intel driver | flag | off | say every register access | developer |
| `ferrix.intelgpu.reset` | intel driver | seconds 1–3600 (anything else refused) | none | restart the machine N s after boot | test-only |
| `ferrix.intelgpu.m2` | intel driver | flag | off | run M2's self-checks (commands, blits, a hang) before serving | test-only |
| `ferrix.intelgpu.freq` | intel driver | MHz 1–9999, clamped to RPn..RP0 | the driver's policy | fixed GT frequency request | developer (a "GPU clock" knob, not for users) |
| `ferrix.intelgpu.freqlog` | intel driver | flag | off | per-second frequency / RC6 / package power log | developer |
| `ferrix.intelgpu.freqtest` | intel driver | flag | off | the box's frequency checkpoint | test-only |
| `ferrix.intelgpu.uapitrace` | intel driver | flag | off | trace held waits and failed ioctls on the render node | developer |
| `ferrix.intelgpu.fwhold` | intel driver | flag | off | hold forcewake for the driver's life | developer |
| `ferrix.power.vr` | power driver (`native/drivers/power/x86/src/main.rs:53`, parsed in `src/lib/drivers/power/x86-power/src/options.rs`) | `A1,…,An@S`: 1–8 ascending whole amperes, S ≤ 3600 | none: the driver writes nothing, only reports | the VR current-limit step plan (needs kernel `ferrix.powerctl`) | admin |
| `ferrix.power.hot` | power driver | °C 50–99 | 85 | soft thermal limit the driver backs off at | admin |
| `ferrix.power.test` | power driver | `hang@S`, `exit@S` | none | lease and holder-death tests | test-only |

## 3. Configuration files

Unless the row says otherwise, a file is read **when its program starts**
and a change needs a restart of that program (`svc restart UNIT`).

### 3.1 init and units

| Path | Keys | Reader | When | Class |
|---|---|---|---|---|
| `/lib/ferrix/units` (image) < `/etc/ferrix/units` (admin, `svc enable`) < `/run/ferrix/units` (generators, runtime drop-ins); `UNIT.d/*.conf` drop-ins, `.wants/`, `.requires/`, a link to `/dev/null` masks | systemd's unit grammar, the subset in `src/lib/init/svc/src` (§3.1.1) | init (`src/lib/init/svc/src/source.rs:65`) | at start and on `svc daemon-reload` | admin |
| `/lib/ferrix/generators/*` | programs, 5 s each | init (`init/src/main.rs:73`) | start, `daemon-reload` | admin |
| `/etc/passwd`, `/etc/group` | `User=`, `Group=`, `SupplementaryGroups=` → HOME, USER, LOGNAME, SHELL | init (`spawn.rs`) | each spawn | admin |
| `/etc/ferrix/units/default.target` | link | init | start | admin (the default boot target) |

**3.1.1 Unit keys honoured.** `[Unit]`: Description, Documentation,
DefaultDependencies, StartLimitIntervalSec/Burst (10 s / 5), AllowIsolate,
IgnoreOnIsolate, RefuseManualStart/Stop, StopWhenUnneeded, Requires,
Requisite, Wants, BindsTo, PartOf, Conflicts, Before, After, OnFailure,
`Condition*=`/`Assert*=` (17 tests). `[Install]`: WantedBy, RequiredBy,
Alias, Also, DefaultInstance. `[Service]`: Type (simple, idle, exec,
forking, oneshot, notify, notify-reload, native), Exec{StartPre,Start,
StartPost,Stop,StopPost,Reload}, RemainAfterExit, PIDFile, NotifyFd,
Restart (no … always), RestartSec (100 ms, doubling), Timeout{Start,Stop}Sec
(90 s), Slice, OOMPolicy, Delegate, User, Group, SupplementaryGroups,
WorkingDirectory, Environment, EnvironmentFile, Standard{Input,Output,
Error}, TTYPath, TTYReset, TTYVHangup, Offers, Uses, KillMode, KillSignal,
FinalKillSignal, SendSIGHUP; limits MemoryMax, MemoryHigh, TasksMax,
CPUWeight, CPUQuota, IOWeight; sandbox NoNewPrivileges, PrivateTmp,
ProtectSystem, PrivateNetwork, SystemCallFilter, SystemCallErrorNumber,
SystemCallArchitectures. `[Socket]`, `[Mount]`, `[Slice]`, `[Scope]` as
`kind/*.rs`. Accepted with a warning and **not built**: ProtectHome,
PrivateDevices, PrivateUsers, ProtectKernelTunables, ProtectKernelModules,
ProtectControlGroups, RestrictNamespaces, CapabilityBoundingSet,
AmbientCapabilities, ReadOnlyPaths, InaccessiblePaths. Parsed but always
false: ConditionPathExistsGlob, PathIsMountPoint, Host, Virtualization,
User, Group, FirstBoot.

**3.1.2 Units shipped** (`src/user/system/linux/init/units`,
`src/user/system/linux/auth/units`): the targets (basic, sysinit, sockets,
multi-user, graphical, rescue, poweroff, reboot, shutdown), `devmgr`,
`getty@`, `lode`, `lode-upload` (not enabled by default), `rescue`,
`auth.socket`, `auth.service`. xtask generates `hyprix.service`
(through `sessiond` with a session user), `pulsed`, `sshd`, `net`
(`dhcpc`), `data-home`, `data-nosuid` and the `getty@` login drop-in
(`tools/common/xtask/src/init.rs`, `session.rs`, `auth.rs`). Which of them
run is a user-facing choice only for `sshd` ("remote login") and
`lode-upload` ("send logs to …").

### 3.2 lode, the log service

| Path | Keys and values | Default | Reader | When | Class |
|---|---|---|---|---|---|
| `/etc/ferrix/lode.d/*.rules` | one rule a line: `drop`, `sample 1/N`, `level P`, `fold DURATION`, `limit BURST/DURATION`, each `where FILTER` (`src/lib/log/lode/src/limit.rs`); a file that fails to parse is ignored whole | none; fold 30 s; limiter 10000/2000/1000 per 30 s for err/warn/chatter | lode (`native/lode/src/serve.rs:235`) | start, `lode reload` | admin |
| `/etc/ferrix/lode.d/retention.conf` | `class NAME [where FILTER] [keep AGE] [max SIZE] [ram-only]` (`retention.rs`) | `errors` (priority ≤ err, 1 y, 256 MiB), `system` (90 d, 1 GiB) | lode | start, `lode reload` (the segments being written are sealed first) | admin ("keep logs for …" is a user-facing candidate) |
| `/etc/ferrix/lode.d/capture.conf` | `Capture=yes\|true\|on\|1`; `capture where EXPR` triggers; `SameProgramEvery=` one picture per program per span (`0`/`always`: every crash) | off; 10 min | lode (`capture.rs:30`) | start; `lode reload` for the switch, triggers and span, but turning it on when lode started with it off takes a restart (the socket is made as root) | admin / user-facing ("capture a screenshot with error reports") |
| `/etc/ferrix/lode.d/events.conf`, then `~/.config/ferrix/lode/events.conf` | `KIND on\|off [LEVEL [COUNT]]` for `window-opened`, `app-error`, `session-streams`, `crash-names`, `process-names`, `snapshot`, `tombstone`, `capture` (`src/lib/log/lode/src/events.rs`); `lode events` lists them | window-opened and session-streams off, the rest on; tombstone 5 a signature | lode at start and `lode reload`; hyprix at each event | admin; the user's file for their session's kinds |
| `/etc/ferrix/lode-upload.conf` (or `--config`) | `NAME URL FORMAT [auth] [ca=F [cert=F key=F]] [where EXPR]`; FORMAT `export`/`json`; plain http to localhost only; https needs `ca=` | none: the program exits 0 | lode-upload (`lode-net/src/upload.rs`, `src/lib/log/lode/src/upload.rs`) | start | admin |
| `/etc/TZ`, else `/etc/localtime`'s POSIX footer | POSIX TZ | UTC | lode, both the service and the reader | start | **user-facing** (time zone; no other Ferrix program reads it) |
| `/etc/machine-id` | 32 hex | — | lode, lode-upload | start | read-only |
| Not configurable | sync 5 s; segment 16 MiB / 1 day; persistent store 1 GiB; volatile 64 MiB | — | `serve.rs:53` | — | — |

### 3.3 hyprix, the compositor

Started as `hyprix --config /etc/hyprland.conf --instance ferrix` (behind
`sessiond --user U …` with a session user); on `--everything` images
`/etc/hyprland.conf` says `source = ~/.config/hypr/hyprland.conf`. Command
line (`compositor/hyprix/src/options.rs`): `--config`, `--display`
(`wayland-1`), `--headless WxH`, `--exec` (repeatable), `--instance`,
`--renderer auto|software|gpu|vtest`; test-only `--frames`, `--dump`,
`--deadline`.

**Grammar** (`compositor/config/src`): `key = value`, `cat { }` and
`cat:key`, `$var`, `source =` (`~` from `$HOME`), `submap`. Refused with a
diagnostic, the rest still applying: `# hyprlang` directives, `{{ }}`,
globs in `source`, keyed categories such as `device[name] { }` (so no
per-device input settings). Any of Hyprland 0.56's ~330 option names is
accepted, typed and returned by `hyprctl getoption`; about 95 of them do
something.

**When each part is read.** Live on `hyprctl reload`, `hyprctl keyword`
and `hyprctl setting` (since 2026-10-24): the options in the table below
marked live, `monitor`, `env` (reload only), `animations:*`, `animation`
and `bezier`, `bind*`/`unbind` and the keymap (`input:kb_layout`,
`kb_variant`: the windows already open are sent the new keymap), key
repeat (`input:repeat_rate`, `repeat_delay`, sent to every keyboard at
once), `input:follow_mouse`, `misc:focus_on_activate` and
`misc:lock_grace`. `exec =` lines run at start and again on every
reload, `exec-shutdown =` lines as hyprix ends (it waits up to 5 s for
them). **Start only**: `windowrule`, `layerrule`, `workspace`, `plugin`,
`exec-once`, `cursor:no_hardware_cursors`. There is no autoreload on a
file change, unlike Hyprland.

**The Settings app's file** (`hyprix/src/settings.rs`, 2026-10-24):
`$XDG_CONFIG_HOME/hypr/settings.conf`, else `~/.config/hypr/settings.conf`
(`--settings PATH` names another). hyprix reads it after `hyprland.conf`,
as if a `source =` line ended that file, so a choice made in Settings wins;
a `source = ~/.config/hypr/settings.conf` line in `hyprland.conf` reads it
there instead, and it is not read twice. A missing file is no error. The
app never writes it itself: `hyprctl setting KEY VALUE` checks the value
as `keyword` reads it, writes `KEY = VALUE` in place of the key's earlier
line (written whole and renamed, comments and other lines kept), and puts
it in force at once; any answer but `ok` names what was wrong, and nothing
was written or changed. `hyprctl setting reset KEY` takes the line out and
reloads, so the key is back to `hyprland.conf`'s value or the default.
`hyprctl settings` (`-j`: `[{"key","name","value"}]`) lists the file. A
key is any option of the table below, or a `wallpaper` line, one for each
output (key `wallpaper` for every screen, `wallpaper DP-1` for one);
monitors stay in `monitors.conf`. The Settings pages' keys:

| Page | Keys (`hyprctl setting …`) |
|---|---|
| Appearance | `general:gaps_in`, `general:gaps_out`, `general:border_size`, `general:col.active_border`, `general:col.inactive_border`, `decoration:rounding`, `decoration:inactive_opacity`, `decoration:blur:enabled`, `decoration:shadow:enabled`, `decoration:shadow:color`, `animations:enabled` |
| Windows | `general:layout`, `hyprix:titlebars`, `input:follow_mouse` |
| Keyboard | `input:kb_layout` (comma list, the first the default), `input:kb_variant`, `input:repeat_rate` (keys a second), `input:repeat_delay` (ms) |
| Idle and lock | `misc:lock_grace` (s); the lock and screen-off timeouts stay `hypridle.conf`'s listeners (§3.4) |
| Wallpaper | `wallpaper` lines and `hyprix:wallpaper_lock` (below) |

**The wallpaper** (`compositor/config/src/wallpaper.rs`,
`hyprix/src/wallpaper.rs`, `compositor/wallpaper`; 2026-10-24) is a Ferrix
keyword, one line for each output, the last written winning and `off`
taking one away:

```
wallpaper = , picture /usr/share/ferrix/wallpapers/ferrix.png, fit fill, colour 1d2027
wallpaper = DP-1, slideshow ~/Pictures/Wallpapers, every 30m, shuffle 1
wallpaper = HDMI-A-1, colour rgb(203040)
wallpaper = , video ~/Videos/rain.ivf
wallpaper = DP-1, off
```

The first field is the output (empty: every screen without a line of its
own); then fields, each a word and a value (a path may hold spaces, not a
comma): one of `picture PATH` (PNG, JPEG, SVG), `slideshow DIR` (its
PNG/JPEG/SVG files by name), `video PATH` or a `colour` alone; `fit`
`fill` (default, cover), `fit`, `stretch`, `tile`, `centre`; `colour`
(`rgb()`, `rgba()`, `0xAARRGGBB`, `rrggbb`, and `#rrggbb` through hyprctl
only, `#` being a comment in a file; default `1d2027`), shown alone and
around a picture that does not cover; `every` (seconds, or `30m`, `1h`,
`1d`; default 30 min) and `shuffle` (default 1) for a slideshow; `span 1`
(the empty output's line only): one picture across every screen as they
are placed; `pause_covered`, `pause_battery` (default 1): a slideshow does
not move on while a fullscreen window covers every screen it is on
(`hyprctl monitors`/`workspaces`), or while a `Battery` power supply says
`Discharging`. hyprix starts `wallpaper --line LINE…` (the program beside
`/bin/hyprix`) with the lines in force at start, and again whenever a
keyword, a setting or a reload changes them, ending the one before 2 s
later so no bare background shows; with no line it starts none, so an
`exec-once` wallpaper keeps working. Desktop images now carry the shipped
picture as `wallpaper = , picture …` in `/etc/hyprland.conf`, which a
Settings choice replaces. **Video, not yet in full**: the client hands an
AV1-in-IVF file (what `cargo xtask wallpapers` makes) to `pattern --video`,
which plays it on one screen, ignoring `fit` and both pauses; any other
file shows the colour and says so. `hyprix:wallpaper_lock` (Ferrix's,
default 1): hyprix draws that client's background surfaces -- and no other
client's -- under the lock screen's surfaces, where the lock leaves them
see-through; 0 leaves it dark. hyprlock's own `background` widget, when
`hyprlock.conf` has an opaque one, still covers it: a lock screen that
shows the wallpaper is one with no background or a transparent
one (`color = rgba(0, 0, 0, 0)`).

| Option or keyword | Values | Default | Live? | Class |
|---|---|---|---|---|
| `monitor` | `name, preferred\|WxH[@Hz], auto\|XxY, auto\|scale[, transform, N]`, `disable`, `addreserved`; `mirror`, `bitdepth`, `vrr` reported not done | preferred | live; `keyword monitor` is also saved | **user-facing** (Displays) |
| `input:kb_layout`, `input:kb_variant` | comma lists; shipped layouts us, gb, fr, de, de(nodeadkeys) | `us`, "" | live (a new keymap to every window); `hyprctl switchxkblayout` cycles live | **user-facing** (Keyboard) |
| `input:kb_rules`, `kb_model`, `kb_options` | — | evdev, pc105, "" | reported by `hyprctl devices`, **no effect** | — |
| `input:repeat_rate`, `repeat_delay` | int | 25, 600 | live | **user-facing** (Keyboard) |
| `input:follow_mouse` | 0, non-zero | 1 | live | user-facing |
| `general:layout` | dwindle, master, monocle, scrolling, floating | dwindle | live | **user-facing** (Windows) |
| `general:gaps_in`, `gaps_out`, `float_gaps`, `border_size`, `col.active_border`, `col.inactive_border`, `resize_on_border`, `extend_border_grab_area`, `no_focus_fallback`, `snap:*` | int / colour | 5, 20, 0, 1, ffffffff, ff444444, 0, 15, 0, … | live | **user-facing** (Appearance) |
| `decoration:rounding`, `rounding_power`, `active_opacity`, `inactive_opacity`, `fullscreen_opacity`, `dim_inactive`, `dim_strength`, `dim_around`, `shadow:{enabled,range,render_power,color,offset}`, `blur:{enabled,size,passes,noise,contrast,brightness,vibrancy,vibrancy_darkness,xray,popups}` | — | Hyprland's | live | **user-facing** (Appearance) |
| `animations:enabled`, `animation`, `bezier` | — | 1 | live | user-facing ("reduce motion") |
| `misc:background_color`, `misc:close_special_on_empty` | — | ff111111, 1 | live | user-facing / admin |
| `binds:workspace_back_and_forth`, `hide_special_on_workspace_change`, `allow_pin_fullscreen`, `movefocus_cycles_fullscreen`, `window_direction_monitor_fallback` | bool | 0,0,0,0,1 | live | admin |
| `dwindle:*` (10 keys), `master:*` (11), `scrolling:*` (7) | Hyprland's | Hyprland's | live | admin |
| `cursor:no_hardware_cursors` | 1 disables the plane | 2 | start | admin |
| `misc:focus_on_activate` | bool | 0 | live | admin |
| `misc:lock_grace` (Ferrix's) | seconds, capped at 10 | 0 | live | user-facing (lock screen) |
| `hyprix:titlebars`, `hyprix:own_frames` (Ferrix's) | bool | 0 | live | user-facing (title bars) / developer |
| `debug:overlay` | 1 | 0 | live | developer |
| `bind[flags]`, `unbind`, `submap` | flags `l r o e n m t i s d p c g u`, mods SHIFT CAPS CTRL ALT MOD2 MOD3 SUPER MOD5 | the config's | live (a keyword adds or removes one; a reload reads the file's again and leaves any submap) | user-facing (Shortcuts) |
| `windowrule`, `layerrule`, `workspace` | 0.56 `match:` grammar | — | start | admin |
| `exec-once`, `plugin` | commands | — | start | admin (autostart) |
| `env = NAME, value` | — | — | reload | admin |
| `exec`, `exec-shutdown` | commands | — | `exec` at start and every reload; `exec-shutdown` at the end, waited for up to 5 s | admin |
| Inert (accepted, no effect) | all other `cursor:*`, `general:allow_tearing`, `render:*`, `misc:vrr`, `misc:font_family`, `misc:disable_autoreload`, `input:sensitivity`, `accel_profile`, `natural_scroll`, `touchpad:*`, `group:*`, `gestures:*`, `xwayland:*`, … | — | — | not to be offered |

Other hyprix files: `$XDG_STATE_HOME/hyprix/monitors.conf`
(`~/.local/state/hyprix/monitors.conf`), the monitor arrangement per set
of monitors, written by `keyword monitor` and output management, applied
after the config's `monitor` lines (`hyprix/src/layouts.rs`) — the natural
store for a Displays page.

### 3.4 The rest of the desktop

| Path | Keys | Default | Reader | When | Class |
|---|---|---|---|---|---|
| `~/.config/waybar/config(.jsonc)`, `style.css` (search: `-c`, `$WAYBAR_CONFIG_DIR`, `$XDG_CONFIG_HOME/waybar`, `~/.config/waybar`, `~/waybar`, `/etc/xdg/waybar`) | upstream waybar; modules clock, custom, hyprland, network, pulseaudio, system, tray | seeded from `/etc/skel` | waybar (the waybar app's `src/config.rs:60` (apps component)) | start; SIGUSR1 shows/hides; **SIGUSR2 reload not built** | user-facing (Bar) |
| `~/.config/hypr/hypridle.conf` (XDG chain, `/etc/xdg`) | `general:` lock_cmd, unlock_cmd, on_lock_cmd, on_unlock_cmd, before/after_sleep_cmd, ignore_*_inhibit, inhibit_sleep; `listener:` timeout, on-timeout, on-resume, ignore_inhibit, condition_cmd, condition_retry | — | hypridle | start | **user-facing** (Idle, lock after …) |
| `~/.config/hypr/hyprlock.conf` (XDG chain, `/etc/xdg/hypr`) | general, auth, animations, background, input-field, label, image, shape | — | hyprlock | start | user-facing (Lock screen look) |
| `~/.config/fuzzel/fuzzel.ini` (XDG chain, `/etc/xdg`) | upstream fuzzel `[main] [colors] [border] [dmenu] [key-bindings]`; parsed but not carried out: gamma-correct-blending, scaling-filter other than box, list-executables-in-path, message-mode=expand | — | fuzzel | start | user-facing (Launcher) |
| `~/.config/ferrix/game-mode.conf` | one desktop-file ID a line, `#` comments | none | Settings writes it; fuzzel reads it at every launch | live | **user-facing** (built) |
| `.desktop` files in `$XDG_DATA_HOME`, `$XDG_DATA_DIRS` (`/usr/local/share:/usr/share`) | — | — | launchers, Settings | each listing | — |
| Fonts: `$FERRIX_FONT_DIRS`, `~/.local/share/fonts`, `~/.fonts`, `/usr/share/fonts`, `/usr/local/share/fonts`, `/usr/share/ferrix/fonts` | — | — | every compositor-text client | start | user-facing (Fonts) |
| Icons: `~/.icons`, `<datadir>/icons`, pixmaps | — | — | launchers | start | — |
| Cursor theme | none: the cursor is drawn in code | — | — | — | — |
| `/etc/skel`, `/usr/share/ferrix/home` | copied into the home if missing / over `/home` at every session start | — | sessiond | session start | admin |
| `/etc/ferrix/session-bus.conf` | dbus-daemon's | — | the session bus sessiond starts with `--session-bus` | session start | admin |

`sessiond` itself has no configuration file: `--user`, `--headless WxH`,
`--display-wait S` (60), `--session-bus DAEMON CONFIG`, `--session-env
N=V`. notifyd has none (5000 ms default timeout, colours in code).
`pulsed` has none (socket path argument, plays to `/dev/snd/pcmC0D0p`).

### 3.5 Settings and the apps' own files

| Path | Keys | Default | Reader | When | Class |
|---|---|---|---|---|---|
| `~/.config/ferrix/game-mode.conf` | as §3.4; written atomically, 0644 | — | the Settings app (its `src/store.rs`, apps component) | read at window open and before each switch | user-facing |
| `~/.config/ferrix/lode/machines.conf` | `[machine NAME]`, `kind = ssh\|tls`, `host`, `user`, `port` (22), `key` | — | lode viewer (`remote.rs`) | start; written by its form | user-facing |
| `~/.config/ferrix/lode/viewer-queries.conf` | `name = filter` | built-in `triage`, `gpu` | lode viewer (`query.rs`) | start; written on save | user-facing |
| `~/.config/ferrix/lode/keys/NAME` | a private key, 0600 | — | lode viewer | form submit | user-facing |
| `~/.local/share/ferrix/lode-receive/` | per-machine stores, `.pki/` | — | lode-receive, the viewer | — | user-facing |

The Settings app keeps no state of its own (page and filter are not
saved). The lode viewer's theme is not persisted (`--theme`).

### 3.6 Authentication, network, other system files

| Path | Keys | Default | Reader | When | Class |
|---|---|---|---|---|---|
| `{/lib,/etc,/run}/ferrix/auth/services/SVC` + `SVC.d/*.conf` | `Account` self/caller/any; `Callers` any/root; `Purpose` authenticate/change-password; `Methods` password; `FailDelaySec` 0–30 (2); `Grant` seat; `FirstPassword` local/no; `TargetGroup`; `Description` | shipped: hyprlock, login, passwd, su | authd (`auth/authd/src/policy.rs`) | **every conversation** (live) | admin |
| `/etc/passwd`, `/etc/group`, `/var/lib/ferrix/auth/{users,state}` | accounts, password store | — | authd, login, su, init, sessiond | each use | user-facing (Users; through `passwd`/authd, never by editing) |
| `/etc/resolv.conf` | written by `dhcpc` | — | ferrousli's resolver | each lookup | admin |
| `/etc/ferrix/units/sshd.service`, `net.service` | generated | — | init | — | user-facing (remote login on/off) |
| `/lib/firmware/edid/*.bin` | named by `drm.edid_firmware` | — | kernel | when a card is taken | admin |

`dhcpc` (`-i IFACE` eth0, `-t`, `-T`, `-A`), `adbd` (`--port` 5555,
`--usb`), `pkg` (`--root`) and `pulsed` take only command-line options,
fixed in their units. Nothing in the tree reads `/etc/hostname` or
`/etc/os-release`: the image carries `/etc/hostname` (`ferrix`,
`tools/common/xtask/src/initramfs.rs:692`), but the hostname is the
kernel's (§4.2) and no Ferrix program sets it at boot (finding 18).

### 3.7 yserver (external, `~/.local/share/ferrix/yserver`)

Started on `--everything` desktops by `exec-once = /bin/busybox sh
/etc/yserver.sh` (`tools/common/xtask/src/yserver.rs`), which sets
`YSERVER_BACKEND=wayland`, `YSERVER_ALLOW_SOFTWARE_VULKAN=1`,
`VK_ICD_FILENAMES`, `RUST_LOG` and runs `yserver :0 -nolisten tcp`. Its
own variables (`YSERVER_*`, `XKB_DEFAULT_*`) and options (`:N`,
`-listen`/`-nolisten`, `-reset`, `-terminate`) are yserver's repository's
and were not audited here. Class: admin.

## 4. Live controls

### 4.1 Tools

| Tool | What it changes | Code | Class |
|---|---|---|---|
| `hyprctl keyword K V` | any live option of §3.3 at once; `keyword monitor` also saved to `monitors.conf` | `compositor/hyprix/src/state.rs:4108` | **the Settings app's main lever for the desktop** |
| `hyprctl reload` | re-reads options, style, animations, monitors, `env`, binds, the keymap, repeat, `settings.conf`; runs `exec`; drops earlier keywords | `state.rs` `Compositor::reload` | user-facing |
| `hyprctl setting K V`, `setting reset K`, `settings` | as `keyword`, and kept in `settings.conf` (§3.3) | `hyprix/src/settings.rs` | **the Settings app's lever for what must survive a restart** |
| `hyprctl switchxkblayout <dev\|all\|main\|current> next\|prev\|N` | keyboard layout group | `compositor/ipc/src/reply.rs` | user-facing |
| `hyprctl dispatch NAME ARG` | windows, workspaces, `dpms`, `exit`, `exec`, `overview`, `submap`, … (`hyprix/src/act.rs:166`, `layout/src/dispatch.rs:513`); `toggleswallow`, `releaseinputcapture` only log | | user-facing (as actions, not settings) |
| `hyprctl output create headless [NAME]` / `remove` | virtual outputs | | developer |
| `svc start\|stop\|restart\|reload\|enable\|disable\|mask\|unmask\|isolate\|daemon-reload\|reset-failed` | services; `enable` links in `/etc/ferrix/units` | `init/svc/src/main.rs`, `init/init/src/admin.rs` | admin (on/off for sshd, lode-upload are user-facing) |
| `svc set-property UNIT Key=V… [--persistent]` | MemoryMax, MemoryHigh, TasksMax, CPUWeight, CPUQuota, IOWeight as a drop-in in `/run` or `/etc` | `admin.rs:150` | admin |
| `svc poweroff\|reboot`, `poweroff`, `reboot`, `shutdown` | power | | user-facing |
| `bootctl status\|try IMAGE [--tries N] [--cmdline "…"]\|verify\|good\|rollback\|init` | A/B slots and a slot's command line | `installer/src/bootctl.rs` | admin (updates) |
| `lode mark\|rotate\|delete --where\|session\|reload\|events` | journal; `lode reload` (root's) rereads the rules, alerts, events, retention and capture and prints what changed | `native/lode/src/cli.rs` | admin |
| `lode-receive pair\|run\|check\|install-service\|show`, lode viewer `--pair` | log collection from other machines | `lode-net/src/bin/lode-receive.rs` | user-facing (Logs) |
| `authctl status\|try\|reset ACCOUNT\|unlock-seat` | login throttling, the seat lock | `auth/authctl` | admin |
| `passwd` | a password, through authd | `auth/passwd` | **user-facing** (Users) |
| `notifyd --toggle-list`, `notify-send` | notification list | `compositor/notifyd` | user-facing |
| `wallpaper FILE`, `wallpaper --line LINE…` | the background, as `wallpaper =` lines say (§3.3) | `compositor/wallpaper` | started by hyprix from `wallpaper =` lines; **the Settings app sets `hyprctl setting wallpaper '<line>'`**, which is kept |
| `drvupdate DRIVER IMAGE` | hot-swaps a driver through devmgr | `init/drvupdate` | admin |
| `dmesg -C\|-c` | clears the kernel log's read start; `-n`, `-D`, `-E` succeed and change nothing | `dmesg/src/args.rs`, `kernel syscall/system.rs` | admin |
| `settings --list` | prints the game-mode list; no switch from the command line | `apps/settings/src/cli.rs` | — |

There is no CLI for game mode other than the Settings window, no `lode
reload` or `lode rule`, no `powerctl` command (the power driver's words
are boot words, §2), no brightness or volume control in Ferrix's own
programs (volume is the PulseAudio client's, through `pulsed`; the ALSA
control node has no mixer elements, `interfaces/audio/pcm.rs`).

### 4.2 Writable kernel files and calls

| Path or call | Values | Effect | Code | Class |
|---|---|---|---|---|
| `/proc/sys/kernel/hostname`, `domainname` (and `sethostname`, `setdomainname`) | ≤ 64 bytes | the UTS namespace's names; no Ferrix program sets them at boot | `fs/procfs.rs` `SYS_KERNEL`, `syscall/system.rs:318` | **user-facing** (Computer name; not persisted anywhere) |
| `/proc/sys/vm/memfd_noexec` | Linux's 0–2 | whether `memfd_create` makes executable files | `procfs.rs` `SYS_VM` | admin |
| `/proc/sysrq-trigger` | `c` panics; anything else ignored | | `procfs.rs:323` | developer |
| `/proc/<pid>/oom_score_adj` | −1000..1000 | OOM preference | `procfs/render.rs:689` | admin |
| `/proc/<pid>/comm`, `/proc/<pid>/task/<tid>/comm` | name | | | developer |
| `/proc/<pid>/{uid_map,gid_map,setgroups}` | Linux's | user namespaces | | internal |
| `/sys/bus/*/drivers/*/{bind,unbind}` | device address | a request to devmgr to bind or unbind | `fs/sysfs.rs:508` | admin |
| `/sys/devices/system/cpu/cpuN/online` (x86-64, N > 0) | `0`, `1` | takes a processor out of service or back (core quarantine, `docs/CORE-QUARANTINE.md` §4); root in the first user namespace; through the setters seam | `settings/cpu_online.rs`, `smp/service.rs` | admin |
| `CPU_SERVICE(token, cpu, flag)` | `0`, `1` | the same, lode's door, through the processor service token init grants it | `syscall/native.rs` `cpu_service` | internal (lode's policy, `/etc/ferrix/lode.d/quarantine.conf`) |
| `/sys/fs/cgroup/**` writable: `cgroup.type`, `.procs`, `.threads`, `.subtree_control`, `.max.descendants`, `.max.depth`, `.freeze`, `.kill`, `cpu.weight`, `cpu.weight.nice`, `cpu.max`, `memory.{min,low,high,max}`, `io.max`, `pids.max` | Linux's | resource control (`svc set-property` writes these through init) | `src/lib/fs/common/cgroupfs/src/files.rs:91` | admin |
| `clock_settime(CLOCK_REALTIME)`, `settimeofday` | | sets the clock (not persisted; `adjtimex` reports no discipline) | `syscall/time.rs:721` | **user-facing** (Date and time; no NTP client in the tree) |
| `reboot(RESTART2, word)` | on the STM32MP1: `fastboot`, `ums`, `ums_mmc0` | U-Boot's boot mode at the next reset | `platform/st/stm32mp1.rs:778` | admin |
| `setpriority`, `ioprio_set`, `sched_setaffinity` | Linux's | per process | `syscall/attributes.rs`, `limits.rs` | — |

Read-only for settings purposes: everything else in `/proc` and `/sys`
(sysfs has no `/sys/power`, no backlight, no `/sys/class/*/brightness`).

### 4.3 Environment variables

Read when the program starts. "Set" = the program sets it for its children.

| Variable | Program | Effect | Default | Class |
|---|---|---|---|---|
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_DIRS`, `XDG_CONFIG_DIRS`, `HOME` | every desktop program | where §3's user files are | `~/.config`, `~/.local/share`, `~/.local/state`, `~/.cache`, `/usr/local/share:/usr/share`, `/etc/xdg` | — |
| `XDG_RUNTIME_DIR` | hyprix, hyprctl, notifyd, pulsed, fuzzel, hypridle, hyprlock | sockets | `/tmp` (hyprix), `/run` (pulsed); sessiond makes `/run/user/<uid>` | — |
| `LANG`, `LC_ALL`, `LC_MESSAGES` | `.desktop` lookup (locale names); hyprix sets `LANG=C.UTF-8` for children if unset | | `C.UTF-8` | user-facing (Language: only names in `.desktop` files follow it today) |
| `TZ` | lode's reader | display time zone | `/etc/TZ`, `/etc/localtime`, UTC | user-facing |
| `XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE` | set by hyprix if unset | | `Hyprland`, `wayland` | — |
| `HYPRLAND_INSTANCE_SIGNATURE` | set by hyprix; read by hyprctl, waybar | which compositor | the only one | — |
| `WAYLAND_DISPLAY`, `DISPLAY` | every client | | | — |
| `DBUS_SESSION_BUS_ADDRESS` | notifyd, lode viewer's file chooser | | none | — |
| `PULSE_SERVER`, `PULSE_RUNTIME_PATH` | waybar's pulseaudio module, clients; xtask sets `unix:/run/pulse/native` in hyprix's unit | | | — |
| `FERRIX_FONT_DIRS` | compositor-text | font directories first | none | admin |
| `FERRIX_GAME_MODE`, `FERRIX_GAME_MODE_APP` | set by launchers for a game-mode app; **read by nothing yet** | | | (output) |
| `FERRIX_SEAT_FD`, `FERRIX_LOCK_FD`, `FERRIX_SESSION_TOKEN` | set by sessiond for hyprix | | | internal |
| `HYPRIX_DIRECT_SCANOUT=0`, `HYPRIX_INPUT_THREAD=0`, `HYPRIX_SYNC_FLIP=1` | hyprix | turn off direct scanout / the input thread / the flipping thread (flips waited for on the loop) | on | developer |
| `HYPRIX_INPUT_TRACE`, `HYPRIX_INPUT_AGES`, `HYPRIX_HOLD_FRAME_MS` | hyprix | traces, frame hold | off | test-only |
| `SHELL` | term (`/bin/zinc`), getty (`/bin/sh`) | the shell started | | user-facing (Default shell) |
| `TERM` | init (`vt220` for TTY services), login, su | | | — |
| `TERMINAL` | fuzzel | terminal for `Terminal=true` entries | | user-facing |
| `WAYBAR_CONFIG_DIR` | waybar | | | — |
| `LODE_RECEIVE_DIR` | lode viewer only (lode-receive ignores it) | received stores | `~/.local/share/ferrix/lode-receive` | admin |
| `LODE_RECEIVE_BIN`, `LODE_VIEWER_KNOWN_HOSTS`, `LODE_VIEWER_FONTS` | lode viewer | | | developer |
| `LISTEN_PID`, `LISTEN_FDS` | authd | socket activation | | internal |
| `COMPOSITOR_RENDER_PPM`, `COMPOSITOR_RENDER_BLESS`, `LODE_E2E_*`, `TOLE_FONTS`, `SETTINGS_FRAMES` | tests | | | test-only |

## 5. Build-time flags: not run-time settings

These choose what a kernel contains. They are fixed when it is built
(`cargo xtask build --kernel-profile P`, `--set flag=value` for an ad-hoc
build with no claim), printed on the `build` boot line, and **cannot be
changed by a running system or a Settings app**. Source:
`tools/common/data/build-flags.toml`, `docs/BUILD-FLAGS.md` §3.

| Flag | Values | `present` | `desktop` | Built today |
|---|---|---|---|---|
| `personality` | user, kernel, none | kernel | kernel | kernel, none |
| `fs` | server, kernel, none | kernel | kernel | kernel, none |
| `mitigations` | on, off | on | on | both (off: no claim; also turns KASLR off) |
| `partition_speculation` | exit, partition, not_applicable | n/a | n/a | n/a |
| `ipc_fastpath` | off, on | off | on (stand-in for off) | off; the run-time switch today is `ferrix.fastpath` |
| `restricted_fastpath` | off, on, not_applicable | n/a | n/a | n/a |
| `kernel_memory` | retyped, heap | heap | heap | heap |
| `schedule` | windows, edf, fair | fair | fair | fair |
| `checks` | on, off | on | on | both (`ferrix.checks` still chooses at boot with on) |
| `program_barrier` | switch, session | switch | session (stand-in) | switch |
| `ssbd` | always, on_request | always | on_request (stand-in) | always |
| `smt` | off, stibp | stibp | stibp | stibp |
| `runtime_trades` | none, game_mode | none | game_mode (stand-in) | none: game mode's privileged trades do not exist, so the Settings switch only marks |
| `linux_abi` | on, off | on | on | both (off: build-only, boots nothing useful) |

Other build-time inputs, also not settings: `FERRIX_PIXEL7_CMDLINE_EXTRA`,
`FERRIX_PIXEL7_KERNEL`, `FERRIX_PIXEL7_INITRD` (the Pixel 7 loader),
`FERRIX_INIT` / `--init` (the built-in pid 1), `FERRIX_AUTH_SABOTAGE`
(cfg `ferrix_auth_sabotage`, test-only), `FERRIX_NET_FAULT`,
`FERRIX_DRIVER_VERSION`, `PIPE_BENCH_*` (`option_env!`).

## 6. Findings: documents and code that disagree

| # | Where the doc says | What the code does |
|---|---|---|
| 1 | `docs/LOGGING.md` §13.2 (l. 1072–1080): init writes unit lines to the console at or above `MaxLevelConsole=`, set on gate images with `ferrix.log.console=info` | neither `MaxLevelConsole=` nor `ferrix.log.console` is read anywhere in `src` |
| 2 | `docs/LOGGING.md` l. 489: `*.rules` are read at start "and on `lode reload`"; l. 33 lists `lode status, top, level, reload, …, config, save, upload, receive` | no `reload` (or `status`, `config`, …) subcommand; rules are read at start only (`native/lode/src/cli.rs`, `serve.rs`). The doc's own as-built list (l. 1379) agrees with the code |
| 3 | `docs/LOGGING.md` l. 701–708, 420, 761, 495: `/etc/ferrix/lode.conf` (SystemMaxUse, SyncIntervalSec, …), file sources in `lode.d/*.conf`, `formats.conf`, a `redact` rule | lode reads only `lode.d/*.rules`, `retention.conf` and `capture.conf`; store sizes are constants (`serve.rs:53`) |
| 4 | `docs/LOGGING.md` l. 658–663: a `retention.conf` example with `unit in [...]` and a `default … keep 30d max 4G` class | the filter language has no `in`; the default classes are `errors` and `system` (90 d, 1 GiB). The as-built text (l. 1397) matches the code |
| 5 | `docs/LOGGING.md` l. 1633: upload "TLS is not built yet"; same in `src/lib/log/lode/src/upload.rs:26` and the lode viewer's `remote.rs:18` | `https://` with `ca=`/`cert=`/`key=` is built (l. 1751 of the same doc says so) |
| 6 | `docs/INIT.md` l. 667: `svc set-property` writes a drop-in only with `--persistent` | it always writes one: `/run/ferrix/units` without, `/etc/ferrix/units` with (`admin.rs:199`); INIT.md l. 1536 agrees with the code |
| 7 | `docs/INIT.md` §10 (l. 1035): "There is no journal", svc verbs without `audit` | `svc audit` exists; lode is the journal |
| 8 | `compositor/config/src/lib.rs:84`: `exec =` runs "at startup and on every reload", `exec-shutdown =` "on exit" | **fixed 2026-10-24**: both are run (`exec_runs_at_start_and_reload_and_exec_shutdown_at_the_end`) |
| 9 | `hyprix/src/state.rs:1680` comment: a keyword rebuilds "the animations and the curves"; `seat.rs:309`: "a reload may have changed the keymap" | **fixed 2026-10-24**: `keyword animation`/`bezier` rebuild the curves, and a reload or a keyword sets the binds and the keymap again (`input_keywords_take_at_once`, `an_animation_keyword_takes_at_once`) |
| 10 | Hyprland's documented behaviour, which a user's `hyprland.conf` assumes | no autoreload on file change; `misc:disable_autoreload` and about 235 other option names are accepted and inert (§3.3) |
| 11 | `src/user/system/linux/adbd/src/main.rs:24` doc: `--usb PATH` | `--usb` takes no value; the socket is fixed at `/tmp/adbd-usb` |
| 12 | `build-flags.toml` `[boot_options.nosmp]`: "start the boot processor alone" (no architecture) | only AArch64 and ARMv7-A read it; x86-64 ignores it |
| 13 | `build-flags.toml` `[boot_options."ferrix.btrfs"]` `values = ["*"]` | the code understands `churn` and `replay`; anything else is said and the ordinary check runs |
| 14 | `docs/POSTMORTEM.md` §6.2–6.3 (`ferrix.lockup`, `ferrix.watchdog`, `ferrix.check.lockup`), `docs/DEBUG.md` (`ferrix.kgdb`), `docs/OPAQUE-KERNEL.md` (`ferrix.fsgsbase`), `docs/SCHED-WAKE-AFFINE.md` (`ferrix.coalesce`), `docs/POWERCTL.md` (`ferrix.vrcurrent`) | designs, prototypes on other branches, or retired: no code on `develop` reads them. Each doc frames them so, except that POSTMORTEM's "Off unless asked" reads like built behaviour |
| 15 | `docs/SETTINGS.md` §1–4, `statd` README, `docs/POWERCTL.md` §8 | checked: agree with the code |
| 16 | the registry's `log_buf_len` is not among `present.toml`'s permitted options although the kernel always reads it | whether a `log_buf_len=` boot is inside the reference configuration is not stated; worth a line from the consultant |
| 17 | `dmesg -n LEVEL`, `-D`, `-E` succeed | they change nothing: the console prints every line (`syscall/system.rs:sys_syslog`); a "console log level" setting would do nothing today |
| 18 | the image's `/etc/hostname` (`ferrix`, `initramfs.rs:692`), the Linux convention | read by nothing: the name a machine reports is whatever the kernel starts with until someone runs `hostname` |

## 7. The first ten settings for a Settings app

Chosen for what a person changes on a desktop, what exists today, and what
can be applied without a reboot:

1. **Displays**: `monitor` (resolution, refresh, position, scale,
   transform), live through `hyprctl keyword monitor`, saved in
   `monitors.conf`.
2. **Keyboard layout**: `input:kb_layout`/`kb_variant`
   (us, gb, fr, de, de-nodeadkeys); live and kept through `hyprctl
   setting`, `switchxkblayout` to switch.
3. **Keyboard repeat**: `input:repeat_rate`, `repeat_delay`, live and
   kept through `hyprctl setting`.
4. **Wallpaper**: `hyprctl setting wallpaper '<output>, picture PATH,
   fit fill, colour 1d2027'` (or `colour`, `slideshow DIR, every 30m,
   shuffle 1`, `video FILE.ivf`), live and kept; `hyprix:wallpaper_lock`
   for the lock screen (§3.3).
5. **Appearance**: gaps, borders and their colours, rounding, opacity,
   blur, shadow, animations on/off, title bars (`hyprix:titlebars`), all
   live and kept through `hyprctl setting` (§3.3).
6. **Window layout**: `general:layout` (dwindle, master, monocle,
   scrolling, floating), live.
7. **Idle and lock**: `hypridle.conf` listeners (lock after N minutes,
   screen off) and `misc:lock_grace`.
8. **Game mode**: the existing page.
9. **Power mode**: `ferrix.epp` (performance, balanced, power saving) in
   `CMDLINE.TXT`, applied at the next boot, all four names permitted by the
   certified configuration.
10. **Users and password**: `passwd` through authd; with the computer's
    name (`/proc/sys/kernel/hostname`, which needs a place to persist and
    something to set it at boot) and the time zone (`/etc/TZ`, which only
    lode reads today) on the same "System" page.

Next after these: the Bar (`waybar` config), Notifications (timeout, the
list), Remote login (`sshd.service` on/off), Logs (retention,
`lode-upload` destinations, capture), and an Advanced page for the boot
words of §1 marked admin.
