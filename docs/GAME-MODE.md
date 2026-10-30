# Game mode: what it does, in tiers

**Design for the customer, 2026-10-30, branch `game-mode-design`. Nothing
in it is built.** It answers the customer's orders on game mode:

* 2026-10-11 (`docs/backlog/DECISIONS.md`): game mode should first
  *"disable speculative protections and basically everything that brings
  down latency and brings up fps that might not be acceptable in normal
  high assurance apps"*, behind administrator privilege, per application;
* 2026-10-30: it should also *suspend everything in the background that is
  not required*, and give the game *a fast path everywhere*;
* 2026-10-30, later: *"we own the full stack, so we can go a lot deeper
  than e.g. Linux"*, with Linux's new game schedulers studied for what to
  borrow or beat.

It stands on three documents and does not repeat them:
[SETTINGS.md](SETTINGS.md) (the switch, the user's list, the launcher's
mark, and §7's speculation-domain proposal),
[GAME-PATH.md](GAME-PATH.md) (stage 24: one clock, the game lane, the game
session of §5) and [BUILD-FLAGS.md](BUILD-FLAGS.md) (`runtime_trades`, and
which profiles may carry a trade at all). The box is the customer's Intel
N150 ([N150.md](N150.md)): four Gracemont cores, no SMT, single-channel
DDR4, the ring-3 Intel driver, hyprix, yserver, Steam and Proton running
Arknights: Endfield.

§9 is the answer in one table. §10 holds the decisions.

## 1. What game mode is

A **game session**: a state the machine is in while one game the user
chose is running, entered when the game starts and left when it stops,
crashes or loses the screen. In it, three kinds of thing happen, and they
differ in who may do them, which certified claims they touch, and what a
mistake costs:

| Layer | What | Where the code is | Touches the item? |
|---|---|---|---|
| **A. Policy** | freeze, throttle or quiet what is not needed; order what is | ring 3: a session service, hyprix, lode, the launcher | no: it uses cgroup files, signals and unit commands that exist, and no kernel run-time setting |
| **B. Mechanism** | the kernel told which threads are the frame's, and keeping that promise: placement, slices, wake-ups, priority carried through waits, interrupts, memory | the scheduler, `mm`, interrupt routing | yes, core ring; generic mechanisms only (keep the kernel slim, 2026-10-22) |
| **C. Trades** | the protections a high-assurance program keeps and a game may drop | the speculation code | yes, core ring, and only in the `desktop` profile (`runtime_trades=game_mode`) |

Each layer is worth having without the next. A alone is what the box can
gain soonest; B is where owning the stack beats Linux; C is what the
customer asked for first and buys the least on this box (§7).

## 2. What runs beside a game today

### 2.1 From the code

A sweep of the tree on 2026-10-30 (develop 8d8b0cd7b). "Wakes" is a timed
wait that fires with nothing to do.

| What | Ring, user | Wakes | In a game session |
|---|---|---|---|
| btrfs `disk commit` (`fs/root_disk.rs:93`) | kernel | every 30 s, or after 32 MiB written | keep |
| `mce` poll, one task per processor | kernel | every 5 s (`ferrix.mce.poll`) | defer: a longer period in game |
| `msrlog`, `powerctl` | kernel | 5 s, 1 s, only with their boot words | off on the box unless asked |
| interface tasks: display, input, audio, chardev, block ring | kernel | recheck every 50 ms | keep (the game's path); the recheck is a separate fix |
| `net core`, `net ring`, `render` | kernel | at most 50 ms, 20 ms, 1 s | keep |
| Intel GPU driver | native, root | polls every 1 to 4 ms while a GPU request is outstanding | keep: the game's path |
| yt6801 (network) | native, root | 250 ms link poll after its first interrupt | keep for an online game; the poll can lengthen |
| usbhid | native, root | 8 to 32 ms until its first MSI | keep (input) |
| usbdev | native, root | 100 ms, 2 ms with adb | defer |
| lode | native, root | at least every 250 ms; fsync within 5 s | throttle (§3.3); cannot be frozen by cgroup |
| lode-upload (box only) | Linux, root | 500 ms idle loop | freeze or stop |
| lode-crashd | native, root | blocks on `/dev/ferrix-crashes` | keep (costs nothing idle) |
| sessiond | Linux, root | 200 ms loop | keep: seat and lock relay |
| hyprix | Linux, ferrix | paced to the refresh; input thread every 100 ms; `frames` line each second | keep; quiet the line |
| yserver | Linux, ferrix | Present clock at the refresh | keep |
| yoru-shell dock and two sidebars | Linux, ferrix | (source outside the tree) | freeze |
| notifyd, portald | Linux, ferrix | 5 s and 200 ms retries until connected | freeze |
| wallpaper; `pattern --video` | Linux, ferrix | event-driven; the video decodes continuously | freeze |
| term | Linux, ferrix | **a 4 ms poll loop** (`apps/term/src/client.rs:360`) | freeze; and a fix of its own |
| vdagent | Linux, ferrix | 10 ms while its port is connected | freeze |
| pulsed | Linux, root | every period / 4 | keep if the game plays sound |
| authd, settingsd, sshd, dhcpc | Linux, root | request timers; renew at T1 | keep |
| Steam client, steamwebhelper renderers | Linux, ferrix | outside the tree | freeze the renderers; keep the client |

Mechanisms that exist for suspending (`docs/CGROUPS.md` §11, §12):

* **`cgroup.freeze`** parks every Linux process beneath a cgroup on its way
  back to user mode; a blocking call is interrupted and restarts at thaw;
  a process made in a frozen cgroup starts frozen; `SIGCONT` does not thaw
  it. **A native process is not parked** (lode, every driver), and
  `frozen 1` can be said while one runs.
* **`cpu.max`, `cpu.weight`, `cpu.idle`** throttle and weigh, native tasks
  included.
* **`SIGSTOP`/`SIGCONT`** for Linux processes.
* **init's control socket** stops and starts units (root only). A stop
  loses the process's state; it is not a pause, and `drivers.slice` cannot
  be stopped.
* Native jobs have **no suspend** (`JOB_CREATE`, `KILL`, `FOR_CGROUP`,
  `SET_LIMIT`, `GET_QUOTA`).

Every program hyprix starts is in `app.slice/app-<name>-<pid>.scope`
(`hyprix scope.rs:21`), the session in `user-1000.slice/session-N.scope`.
So "everything the user started except the game" is a set of scopes a
service can name and freeze one by one, and the game is in a scope of its
own already.

### 2.2 Measured

The 2026-10-30 kernel profiles of Endfield's warm-up on the box
(`diag/kernel-profile-*`, four runs, develop kernels) count system calls
per process every 10 s. In every run, the same neighbours, per second:

| Process | Calls a second | What it is |
|---|---:|---|
| the game (one pid, ~1080) | about 7,000, flat | Endfield.exe |
| pid 4 | 4,000 to 44,000, in bursts, nearly all `clock_nanosleep` | the Intel GPU driver (named by §2.3's process list): its completion poll while the GPU is busy |
| pid 7 | 2,000 to 10,000 | the AHCI driver, which polls each request (`ahci/src/main.rs:6`) during the warm-up's disk reads |
| lode-upload (pid 180) | 900 to 3,300 | uploads the log to nazuna; box only |
| hyprix (pid 33) | 900 to 3,300 | |
| **term (pid 48)** | **990, in every window of every run** | the 4 ms poll loop |
| one Wine or Steam thread (pid ~1400) | 650, flat | a poll |

All processes together made 90,000 to 125,000 calls a second; the game is
under a tenth of them. The rest is not all waste (yserver and the driver
serve the game), but the term's thousand a second and lode-upload's are.
The largest caller, the GPU driver's poll at up to 44,000 sleeps a second,
is on the game's path, and is what G1 (§4.1) replaces with the interrupt.

What a neighbour costs on four cores was shown the same morning: Steam's
browser crashing every 3 to 5 s (no fonts) took about **1.8 cores** beside
the game; fixing it cut launch to title from 680 to 487 s
(`endfield-first-sight-stalls`, 3a34e974e, d2a851557). Game mode would not
have fixed that bug, but freezing the renderers would have hidden it.

### 2.3 The whole-system snapshot

Taken on the box on 2026-10-30 at 18:35 (main-94d0c9df0, Endfield in the
world at The Hub, standing; 1280x720, render scale 60%, frame generation
off, Very Low; 25 fps by Wine's counter, GPU 99.9% busy), two snapshots of
every task 38.6 s apart (`gm-snap.sh`; the snapshot's own forks are in the
processor total and in none of the rows):

| Process | Processor (% of one core) | Switches a second | Kind |
|---|---:|---:|---|
| Endfield.exe | 106 | 17,400 | the game |
| hyprix | 33 | 1,314 | compositing each frame: the bar kept direct scanout off (§4.1, G5) |
| wineserver | 21 | 5,468 | Wine's server, on the game's path |
| Intel GPU driver | 18 | 2,300 | on the path |
| the game's own browser (CrBrowserMain, under Wine) | 12 | 4,797 | the game's launcher UI, kept alive by the game |
| Steam client | 3.4 | 1,580 | |
| yserver | 2.8 | 694 | |
| lode | 1.7 | 249 | |
| steamwebhelper (all) | ~2 | ~750 | Steam's browser |
| term, lode-upload, sshdt, login, yt6801 | ~3.5 together | ~1,200 | |
| everything else | <1 | | |

* **The machine:** 64,274 switches, 31,255 interrupts and about 119,500
  Linux system calls a second (`/proc/stat`, `/proc/ferrix-seam`); the four
  processors 62 to 65% busy.
* **What freezing could take back:** the user's extras and Steam's
  browser, about 7% of one core in all. The large neighbours of the game are
  on its path (hyprix, wineserver, the driver) or are the game's own (its
  browser), and freezing does not reach them.
* `VmRSS` read as zero for every process on this image, so the memory
  column is left out.

## 3. Suspending the background (layer A)

### 3.1 Four treatments

| Treatment | For | Mechanism | Comes back by |
|---|---|---|---|
| **Freeze** | the user's own programs not on the frame's path: the bar and sidebars, notifyd, portald, the wallpaper, terminals, vdagent, Steam's browser renderers, any app scope but the game's | `cgroup.freeze=1` on each `app-*.scope` but the game's and the path's | `cgroup.freeze=0`, written by the service at the session's end and at its own start (§8.3) |
| **Throttle** | system services that must keep running but not at the game's cost: lode, lode-upload, sshd, Steam's client | `cpu.weight` low, `cpu.idle=1` where the service tolerates starvation, `cpu.max` for a native one | the values written back |
| **Defer** | periodic kernel and driver work that can wait for minutes: the machine-check poll, usbdev's tick, yt6801's link poll, the `frames` and `freqlog` lines | a longer period while the session lasts, through each one's own setting | the period restored |
| **Keep** | the frame's path and the machine's safety: the GPU driver, hyprix's loop, flip and input threads, yserver, sessiond, authd and the lock screen, input drivers, the network for an online game, sound, devmgr, lode-crashd, the btrfs commit | nothing; layer B orders them | n/a |

The frame's path is not guessed: hyprix knows the game's window, its
client and the client's pid (yserver for an X game), and the GPU driver
knows which processes submit (§5.2). What is not on that path and not in
the keep row is frozen by default; the user may except a scope (a voice
chat, a music player) by a line in the user's list.

### 3.2 What freezing cannot do here, and the fix

`cgroup.freeze` parks Linux processes only. Every native program (lode,
the drivers) keeps running, and `cgroup.events` says `frozen 1` while one
does. For game mode that is acceptable: the native programs are drivers
the game needs, and lode, which is throttled instead. A native freeze is
not proposed.

### 3.3 lode

lode wakes at least four times a second, fsyncs within 5 s, compresses
segments and, through lode-upload, sends the log over the network. In a
session: the upload paused (frozen; it resumes from its cursor), the
segment compression deferred, the fsync interval lengthened to the commit's
30 s, and hyprix's `frames` line and the driver's `freqlog` lowered to one
a minute. lode's crash capture stays: a crash during a game is the case it
exists for. These are lode settings (`docs/RUNTIME-SETTINGS.md` §3.2); the
period changes are new settings, given to the lode agent
(`logging-needs-to-lode`).

### 3.4 Gain

**Measured, 2026-10-30: freezing and nice values gained nothing at 720p,
in one scene.** On the box (§2.3's scene: The Hub, standing), A/B by Wine's
per-second fps counter, one 25 s window per treatment, alternating, with
`SIGSTOP` standing in for `cgroup.freeze`:

| Window | fps, mean (lowest second) | GPU busy |
|---|---|---|
| A1 baseline | 25.25 (24.42) | 99.9% |
| F1 the user's extras and Steam's browser stopped (`SIGSTOP`) | 25.35 (24.63) | 99.9% |
| A2 baseline | 25.21 (24.63) | 99.9% |
| N1 background at nice +19, game path at −10 | 25.25 (24.57) | 99.9% |
| A3 baseline | 25.21 (24.76) | 99.9% |
| F2 stopped again | 23.61 (10.89) | 66 to 99%: a stall in the window |
| A4 baseline | 24.41 (17.73) | 99.9% |
| B0 baseline, bar shown | 25.45 (25.02) | |
| D1, D2 the bar and sidebars closed | 25.54, 25.56 (25.12, 25.18) | |

F2's stall (the GPU idle to 24% for a second) carried into A4 and is not
the freeze's: the same stall shape appears in endfield-60fps's walking
runs. Closing the bar took hyprix from 36% to 27% of a core (its composite
step from 4.5 ms to 0 a frame) and the frame rate by 0.1. The GPU memory
probe (endfield-60fps's `gpb`) gave the same bandwidth with the background
frozen as without: shader read 19.12 and 19.16 GB/s, copy 18.64 and 18.70.

**Why: the box is GPU-bound in the world.** endfield-60fps measured the GPU
99.9% busy at its fused 1000 MHz with frame generation off: 25 fps at
1280x720 and render scale 60%, 30.6 at 960x540, 39.6 at 640x480, where the
four processors reach ~93% and the processor co-limits. So no
processor-side lever raises the average frame rate at 720p. What a processor-side
lever can still win on this machine is twofold: **memory bandwidth**,
since the integrated GPU shares one DDR4 channel with every processor and
every neighbour's cache misses and copies are bandwidth the GPU does not
get (F8's copy alone is 0.5 to 1 GB/s); and **the lows**, the frames where
a CPU-side hop (the render thread's submit, yserver, hyprix) is late
because a neighbour held its processor. Average fps is the wrong figure to
judge layers A and B by on this box.

Freezing stays worth building, for three reasons the measurement leaves
standing: at 640x480 and below the processors co-limit, and there the 7% of
a core it frees is a share of the frame; on a desktop with more running (a
browser, a build, a video wallpaper) the neighbours are larger than on the
box's bare session; and it is cheap, ring 3 and reversible. But it is not
an fps lever on this box at the settings people play at, and the design
does not claim one.

## 4. The fast path, lever by lever

Each row: what it is, the gain on the N150 (measured where marked M,
argued where A, unknown where ?), its cost, and the certified claims it
touches. "Core" means the item's core ring
(`tools/common/data/certification-item.json`).

| # | Lever | Today | Gain on the N150 | Cost | Certification |
|---|---|---|---|---|---|
| F1 | **Background frozen and throttled** (§3) | nothing acts | M: 0.1 fps at 720p (25.25 to 25.35, noise); frees ~7% of a core; matters only where the processors co-limit (640x480) or the desktop runs more | a session service, 8 points | none (ring 3, existing files) |
| F2 | **Nice and weights for the game and the path** (GAME-PATH G0) | `nice` is applied as weight (`syscall/attributes.rs`); a user cannot raise their own | M: nothing at 720p (25.21 to 25.25) | the service holds root for it; 2 points | none |
| F3 | **Applied affinity: cores for the game and the path** | `sched_setaffinity` accepted, not applied (`syscall/limits.rs:335`) | A: on four cores, one core for the system and three for the game removes neighbours from the game's cores; the cost is a quarter of the machine for the rest | 5 points in the scheduler, a check and a control | core (`sched/**`), a design review; new ids |
| F4 | **Interrupts away from the game's cores** (G3) | an MSI is aimed at the processor that allocated it and never moved (`arch/x86_64/msi.rs:52`) | A: small. The box's interrupt rate is mostly the GPU's, which belongs on the path; the NIC's and USB's are low | a retarget call through devmgr, remapping entries rewritten; 5 points | core (`arch/**`, `iommu/**`) |
| F5 | **Timer slack** | none exists: one-shot timers fire at their deadline (`timer.rs`) | nothing to remove | — | — |
| F6 | **The wake-up tail**: a woken path thread waits for the next decision instead of running | light-wake p99 223 µs on Ferrix against 20 to 95 µs on Linux 7.0 (M, KVM, `docs/SCHED-WAKE-AFFINE.md` §8) | A: a tail of a quarter of a millisecond on each hop of a five-hop frame path is 1 ms of a 16.7 ms frame in the worst case | the lag design note's DELAY_ZERO equivalent; p2-151 | core, its own review |
| F7 | **Per-task slice requests** (EEVDF's `sched_runtime`) | one slice per run queue (`ferrix-sched` `Config::slice_ns`), 3 ms | A: a path thread asking for 0.5 ms gets an earlier deadline and preempts on wake (Linux 6.12 [S]) | a field per entity, a check; 5 points | core |
| F8 | **The card's copy removed**: a plane scans the game's buffer | the card still copies each frame, about 5 ms of a processor (`docs/DISPLAY.md` §8.2) | M: 5 ms a frame, 15% of a core at 30 fps and 30% at 60, plus 0.5 to 1 GB/s of a single DDR4 channel | an Intel display driver (paused, `intel-display-paused`) and tiling agreed with anv | ring 3 and the display core's protocol |
| F9 | **Memory: no reclaim of the game's pages, `mlock`, huge pages** (G4) | no `memory.min`, no `mlock` call found; `MADV_HUGEPAGE` a hint ignored | A: on 16 GiB with the game at 8.75 GiB, reclaim does not reach the game today; huge pages would cut TLB misses for an 8 GiB heap, a few per cent on Linux games [S], unmeasured here | `mm/**`: 8 points for the floor and `mlock`, more for huge pages | core |
| F10 | **I/O priority for asset streaming** | `ioprio_set` stored and read back, not acted on (`syscall/attributes.rs`) | A: Endfield's first-sight stalls are CPU, not disk (`endfield-first-sight-stalls`), so little today | a class in the block ring; 3 points | the block ring (item) |
| F11 | **The kernel's hot paths** from the 2026-10-30 profile | heap caches landed (kernel share 48.5 to 43.6%), btrfs `has_free` landed; left: the address-space lock in `fault` (~10%), `futex_waitv` allocating twice per call, masked time at `Reserved::drop`; the region map (`docs/REGION-MAP.md`, p1-037) | M: the kernel is about 40% of the game's processor time; each of these is a slice of it | each its own landing | core; **not game mode**: §4.1 |
| F12 | **The protections** (§7) | `present` on the box: every barrier on | M: about 1% of one core for the relaxable set, no frame at 720p (§7.2) | `desktop` profile only | core; level `none` |
| F13 | **One clock and the game lane** (GAME-PATH §4) | hyprix pacing, direct scanout landed (`docs/DISPLAY.md` §8.2) | A: latency and evenness, not frame rate | stage 24 | ring 3 and the display core |

### 4.1 The GPU path, where the box is bound

In the world the GPU is the limit: about 99.9% busy at its fused 1000 MHz
(endfield-60fps, 2026-10-30), with the DRAM at its top clock
(`endfield-levers-after-fsr3`). The clock cannot rise, so the GPU path's
levers are of three kinds: give the GPU **less to wait for** (bubbles
between its work), **less to fetch** (bandwidth on one DDR4 channel shared
with four processors and the display), and **less to do** (the work a frame
costs). Ferrix writes the GPU driver (`intel-xelp`, ring 3, execlists, no
GuC), the display path and the compositor, so most of these are its own
code, not Mesa's.

| # | Lever | Today | Gain | Cost | Certification |
|---|---|---|---|---|---|
| G1 | **Completion seen at once.** A request's end is seen by a poll every 1 ms (4 ms when nothing waits), because no MSI arrived on the box when the driver was written (`backend.rs` `POLL_NS`, `docs/N150.md` §13) | polled | A: a fence seen up to 1 ms late delays the next submit when the game keeps one or two frames in flight; it is a candidate for part of the GPU's ~6% idle a frame, beside the `EXECBUFFER2` `EAGAIN` on a busy binding (`endfield-levers-after-fsr3`). Also 1,000 driver wakes a second while busy | the GT's user interrupt by MSI, once the box delivers it (yt6801 now reports its first interrupt on every boot); a fallback poll stays | ring 3; the interrupt object is the item's, unchanged |
| G2 | **The idle gaps**: the busy binding that refuses `EXECBUFFER2` with `EAGAIN` | ~6% of a frame idle (M) | up to 6% fps where the GPU is the limit | driver work in the front end | ring 3 |
| G3 | **Lossless colour compression (CCS)**: Gen12's AUX table, which anv manages and the driver invalidates (`engine_cs.rs` `gen12_get_aux_inv_reg`) | believed on; not shown | ? E5 (§11) shows whether anv compresses here at all (`INTEL_DEBUG=noccs` A/B). On a bandwidth-starved part, render-target compression is among the largest single levers Intel ships | none if it already works; a fix in the driver's flushes if not | ring 3 |
| G4 | **Large GPU pages**: 2 MiB (or 64 KiB) page-table entries for the game's buffers | 4 KiB only (`ppgtt.rs`: "64 KiB and 2 MiB pages left out") | A: fewer GPU TLB misses on an 8 GiB working set; Linux gets this only where shmem hands i915 huge pages. Ferrix pins every GEM object anyway, so the driver can ask for them in aligned blocks through `vmo_pin`'s `PIN_BLOCKS` (the Intel GPU session; ledger 5088, 5102), which is best effort, promises no block, and on which no driver may rely. M: shader read 17.7 GB/s on Ferrix against 20.7 on Linux, copy 16.4 against 21.7 (below) | the driver's allocator and PPGTT (2M PDEs), and contiguous pinned memory from the kernel's frame allocator; 8 points | ring 3 driver; `PIN_BLOCKS` is in the core ring, reviewed on its own branch, and game mode claims none of it |
| G5 | **No processor copy of the frame**: a display plane scans the game's buffer (F8) | the card copies each frame, ~5 ms of a processor and ~0.5 GB/s at 30 fps | M for the copy; it is also bandwidth the GPU loses | an Intel display driver (paused) | ring 3, the display core |
| G6 | **Scan out at the game's resolution**: the game renders 1280x720 at 60% scale, the screen runs 1920x1080 | ~0.5 GB/s of scanout reads at 1080p60, plus the copy | A: about half of both at 720p | a mode set per session (needs a display driver; on efifb a reboot with another EDID) | ring 3 |
| G7 | **The processors' traffic out of the GPU's way** | every neighbour's misses share the channel | A: what F1 frees, in bandwidth as well as cores; judge E1 by fps too | F1 | none |
| G8 | **The GPU's clock held at RP0** in a session | i915's up/down policy; warm-up swings 300 to 1000 MHz at ~62% busy (M, 2026-10-30 profile) | none in steady play (already at 1000 MHz); some in warm-up and first-sight stalls | a session setting in the driver's RPS | ring 3 |
| G9 | **Less work a frame**: a newer Mesa than 25.0.7 (Xe-LP's compiler), an Endfield driconf profile (settings only, never replaced shaders: anti-cheat), the game's own settings | — | ? measured one by one (`endfield-levers-after-fsr3`, 3 and 6) | volumes and configuration | none |
| G10 | **The frame's GPU work first.** The driver knows the game's contexts; anything else submitted (a browser, a video) waits behind them, by execlist priority, while the session lasts | no priority between contexts | A: nothing today (hyprix draws in software, the game is the GPU's only client); matters when a second client appears | the driver's queue | ring 3 |

**What decided between them** (endfield-60fps, 2026-10-30, notes
`endfield-60fps-findings.md`):

* **The slope.** About 19 ms per million pixels of render resolution, plus
  about 34 ms of each 720p frame that does not depend on the render scale:
  passes at the output resolution, geometry and shaders. So 720p cannot
  pass ~29 fps at any render scale, and the frame is mostly *not* fill-bound.
  G9 (less work a frame) and G4 (memory reached faster) weigh more than
  G6.
* **G4, measured.** The same Vulkan probe and Mesa 25.0.7 on this box under
  Linux's i915 (execlists, as Ferrix) and under Ferrix: ALU 340 GFLOPS and
  fill 7.9 GB/s equal; shader read 20.7 GB/s on Linux against 17.7 on
  Ferrix, shader copy 21.7 against 16.4. With shmem's huge pages denied,
  Linux falls to 18.2 and 19.7, so most of the read gap is the GPU's page
  size. endfield-60fps bounds the fps gain by the memory-bound share of the
  frame at roughly 5 to 15%. This session's run on a fresh boot of
  main-94d0c9df0 read 19.1 GB/s and copied 18.7: Ferrix's figure moves
  with the boot, which a 4 KiB mapping of scattered frames would explain.
* **G1, G2: no gaps at 720p.** The GPU is 99.9% busy; completion latency
  and the `EAGAIN` refusal show only below 720p, where the GPU idles.

G4 is the GPU-path lever with a measured gap behind it, and the one item
endfield-60fps left as a design task.

Only G7, G8 and G10 are game mode in the sense of §1 (they are what a
session switches); G1 to G6 and G9 make every program's frames cheaper and
are driver and display work, endfield-60fps's and the Intel display
design's, which this design counts on and does not claim.

### 4.2 What game mode should not be

F11 is the largest gain on the list and is not game mode. A faster fault,
futex and region map make every program faster, are certified like any
other change, and need no session. Making them conditional on a game
would put two versions of a hot path in the item for no reason. The
endfield-60fps session owns them; this design counts on them and claims
none of their gain.

So does F-95 (open: woken ring-3 tasks kept off the processors for 1 to
3 s), a lever for the lows beside F11 and not game mode.

The same holds for the term's 4 ms poll: it is a bug to fix (wait on the
connection's descriptor), not something to freeze around, though freezing
also hides it.

## 5. Deeper than Linux (layer B)

Linux's game tooling (Feral's `gamemoded`, gamescope, `scx_lavd`,
cgroups, IRQ affinity) works from outside: each guesses from what it can
observe what the next program in the chain needs. Ferrix writes the
kernel, the scheduler, the GPU driver, the compositor and the X server,
and serves Wine's system calls. So it can know what Linux infers.

### 5.1 Linux's game schedulers: what they detect and what they measured

From a sourced study made for this design (sources in §12):

| Scheduler | How it finds the game's urgent threads | What it measured on games | What to take |
|---|---|---|---|
| **scx_lavd** (Igalia for Valve, sched_ext) | the wake/wait graph: a task that runs briefly, wakes others often and waits on others often is mid-chain and critical; a futex lock holder boosted once | FH5 on a Steam Deck OLED: 59 against EEVDF's 58 fps, stutter count 46 against 87, CPU-bound minimum 56.1 against 51.6; single runs. No published 1% or 0.1% lows; its author asks for a game benchmark suite. Weak with Unreal Engine, blind to ntsync, no chain-wide deadline | the graph, seeded from known threads instead of inferred; the lock-holder boost; the IPI kick of a victim processor for the most critical |
| **scx_bpfland** | voluntary-switch rate; now EEVDF-like with burst-sized requests | no game figures | burst-sized slice requests as the fallback for unknown threads |
| **scx_rusty** | none (a load balancer) | anecdotes, worse than BORE | nothing |
| **scx_flash** | a latency weight that rises when a task yields early | won one community Cyberpunk test | as bpfland |
| **EEVDF**, Linux 6.6 to 7.0 | none; a task may ask for its own slice since 6.12 (`sched_runtime`, 0.1 to 100 ms) | the baseline | the slice request (F7) |
| proxy execution (6.17), PI futexes | only kernel mutexes on one run queue; PI futexes give a fair-class waiter nothing (`__waiter_prio`) | — | beat it (§5.3) |
| macOS Game Mode | the game gets the efficiency cores to itself; low-QoS background threads are held | measured on an M1 Max | core reservation plus held background: F1 and F3 |
| Windows MMCSS, Game Mode | threads opt in ("Games"); bands with a quota, 20% kept for the rest | — | the quota for the rest (§5.5) |

The lesson is narrow: every Linux scheduler built for games spends its
cleverness *finding* the frame's threads, and none can tell what the
frame's deadline is. Their gains on games are small and single-run.

### 5.2 Telling the scheduler which threads are the frame's

Three programs Ferrix writes see the frame being made, each from a
different side:

* **the GPU driver** sees which thread submits the work a frame is (an
  `EXECBUFFER2` on the render node) and which thread waits on its fence;
* **yserver** sees which client thread presents (`PresentPixmap`) and when
  the frame it presents was due;
* **hyprix** sees the fullscreen surface, its commits, and the vblank it
  aims for.

Each can name, for the session service, the threads it saw do their part
of the frame, and the kernel can be told: *these threads are the frame's,
and the frame is due at T*. That replaces lavd's inference for the threads
that matter most, the render thread and the present path, and works for an
Unreal game as well as for any other, because it reads what the game does
to the screen and not how its engine is written. The mechanism the kernel
needs is small and generic: a **frame mark** on a task (named so to keep it
apart from the launcher's mark of SETTINGS.md §4 and the wait-queue marks of
`L.sched.74` to `76`), given by a holder of a right the session service has
(as `JOB_SPECULATION_DOMAIN` needs MANAGE), never asked for by the task.

**The service marks only tasks it has found itself**, in the admitted
game's scope or in the named path programs (yserver, hyprix, the GPU
driver). A pid or thread that hyprix, yserver or the driver reports is
checked against that set, never taken as given: they run as the user
(ledger 698, G2).

What the frame mark buys, in order of cost:

1. **Ordering and placement**: inside the fair class's eligibility rule, a
   marked eligible task is picked before an unmarked eligible one. It is
   never strict priority: an unmarked task keeps its eligibility, and the
   kernel bounds how long it can wait, not the service. A marked thread is
   placed beside the thread it hands over to (Rule A
   of `docs/SCHED-WAKE-AFFINE.md`, which is already there for futex pairs).
2. **A short slice and wake-up preemption** (F7): marked threads ask for
   0.5 ms and preempt an unmarked task at once on waking.
3. **A deadline**: the slice's virtual deadline set from the frame's due
   time rather than from the slice. This is GAME-PATH G1 and G2's
   fixed-priority class seen from the fair class, and it is stage 14's
   ground (question 3 of GAME-PATH §10).

### 5.3 Priority carried through every wait

A game's frame waits on many things that are not locks Linux can see: a
Wine server reply, a fence the GPU driver signals, yserver's answer, a
futex a job worker holds. Linux's PI futexes give a fair-class waiter
nothing, and its proxy execution covers kernel mutexes on one run queue.

Ferrix owns every one of these waits:

* **channels** (the GPU driver's, yserver's and the compositor's
  protocols ride on them, or on sockets the kernel serves): the server
  answering a marked client's request runs with the client's mark until it
  answers. The native fast path's direct switch (`sched/direct.rs`) already
  hands the processor over on `channel_write_read`, but only on a
  processor with nothing else to run; so the lend is a field on the general
  wake path too, not only on that hand-over.
* **futexes and `futex_waitv`** (Wine's fsync): the futex word does not
  name its owner, so the kernel cannot know who will wake a waiter. What it
  can know is who *did* wake it last time: the lavd graph, measured instead
  of guessed, edge by edge, from the known render thread outward. A thread
  that has woken the render thread in most of the last frames is lent the
  mark while the render thread waits.
* **Wine's NT sync objects**: if Ferrix serves them (ntsync's interface,
  or the shared-memory server Proton uses), a mutex's owner is known
  exactly, and the lend is exact. Linux's ntsync knows the owner and does
  not tell its scheduler.

Cost: a lend is a change to every wait path's wake, in the core ring, with
a bound on how long a lend lasts and a check that it ends. It is the
largest item in layer B.

### 5.4 Wake-ups aimed, not broadcast

The present path today wakes by timer and broadcast: yserver's Present
clock fires at the refresh whether a client waits or not; the GPU driver
polls every 1 to 4 ms while a request is outstanding; the interface tasks
recheck every 50 ms; hyprix's input thread every 100 ms. In a session the
waits on the path can become the events they stand in for: the fence's
interrupt wakes exactly the thread waiting on that fence, the flip's
completion wakes exactly yserver's present and the game's swap, and a
timer is armed only when someone waits. This is ring-3 work in the
driver, yserver and hyprix, plus the interface tasks' recheck in the
kernel, and it helps without a session too.

### 5.5 Background work in the frame's idle gaps

The GPU is idle about 6% of each frame on the box
(`endfield-levers-after-fsr3`), and the processors have gaps between the
render thread's submit and the next frame's start. Work that must run but
not now (lode's compression, the btrfs commit's write-out, a deferred
poll) can be given a class that runs only on a processor with no marked
thread runnable, and whose slice ends before the next frame's due time.
That is `cpu.idle` with a deadline: the mechanism is §5.2's due time read
the other way. And a floor for the rest, as Windows keeps 20%: beside the
kernel's bound on an unmarked task's wait (§5.2), the service's own policy
never lets marked threads take more than three of four processors, so
sshd and the lock screen always run (GAME-PATH §5.4, point 2).

### 5.6 The frame's path as one unit

Taken together, 5.2 to 5.5 make the frame's path (game render thread,
Wine, Mesa's submit, the GPU driver, the fence, yserver, hyprix, the
flip) one scheduled unit against one clock: hyprix's vblank, which
GAME-PATH §4.1 makes the real flip time. Each hop knows the frame's due
time and runs on a processor no other program's thread is placed on
(interrupts and per-processor kernel tasks still run there). No Linux desktop does
this, because no single party there owns all the hops. It is also the
most expensive thing in this document, and it should be built only as far
as each step measures (§9).

### 5.7 What game developers did on fixed hardware, and what it gives here

The customer asked (2026-10-30) how game developers pushed hardware past
what it seemed able to do, and what of that applies. Their tricks share one
premise that a general-purpose OS never has and Ferrix on the box does:
**one known machine, and the whole stack in one hand**. Each row is the
trick, what it did, and its counterpart here.

| # | Then | What it did | Here | Gain on the N150 | Cost |
|---|---|---|---|---|---|
| H1 | **The consoles' reserved cores** (PS4 and Xbox One, 2013) | the OS kept two of eight cores; the game had six to itself, and both later gave games part of the seventh [S] | three of four cores for the game and its path, one for the system, while a session lasts (F3, §5.5's floor) | A: the 1% lows, as §3.4 | F3 |
| H2 | **Crash Bandicoot's own virtual memory** (PS1, 1996) | levels of 32 to 40 MB on 2 MB of RAM, paged in 64 KB chunks laid out on the disc in the order the player reaches them, so each arrived before it was needed [S] | **record and replay the game's page-ins**: the first run records which file pages and which anonymous regions the game touches, in order, from launch to the world; the next run's session pre-faults them in that order, in large batches, before the game asks (what Windows' Prefetch does at launch). `MADV_WILLNEED` is a hint ignored today (`syscall/memory.rs`) | A: launch to title is 199 to 213 s on develop kernels with faults a large share of the kernel's time (F11); a batch costs one lock hold where a fault costs one each. Unmeasured | a recorder that lands in the core ring and records one process's page-in order, so it gets its own design review (bounded memory; no new lock or allocation in the fault path; readable only by the session service under a right, and only for the admitted game's job); a ring-3 replayer; acting on `MADV_WILLNEED` is an item change with its own requirement; 8 points |
| H3 | **Shipping compiled shaders for one GPU** (every console) | no compile at run time: the binaries are built for the one GPU the console has | the N150 is one GPU too: the game's pipeline cache and Mesa's are built once, keyed by the device and driver hash, kept on the volume and **replayed before the session** (Fossilize's replay, which Steam's pre-caching uses), so "Shaders are being compiled" and the first sight of a scene find everything compiled. The hardware-hash key is `docs/HOTPATHS.md`'s | A: the warm-up and first-sight stalls where they are compiles; the 2026-10-30 profile says part of them is the game's own CPU work, which this does not reach | a replay step in the session's start; 3 points |
| H4 | **Factor 5's own microcode** (N64, *Rogue Squadron*, 1998) | replaced Nintendo's RSP microcode with its own for its terrain and audio, and ran at 640x480 with the Expansion Pak [S] | the GPU driver's own command streams are Ferrix's: the flushes, invalidations and breadcrumbs it adds around each batch (`engine_cs.rs`, i915's `gen12_emit_flush_rcs` translated) are i915's general case. Emit only what each batch needs: an AUX invalidation only when the AUX table changed, a full pipe flush only before a buffer leaves the GPU | A: a fixed cost per batch, times the batches a frame; unmeasured. E5 measures the per-batch overhead | driver work; 3 points |
| H5 | **The Amiga's copper and blitter** (1985) | the display coprocessor changed registers in step with the beam and the blitter moved memory, neither using the CPU | the card's copy of each frame (G5's 5 ms of a processor) done by the GT's **blitter engine** (BCS0, whose `XY_FAST_COPY_BLT` the driver already runs, M2) into the lit plane, which is reachable through the GGTT; the copy then runs beside the render engine and takes no EU time and no core | M for the processor time freed (~5 ms a frame); the bandwidth stays | a copy path in the driver's card; 5 points; G5 makes it unnecessary when a display driver lands |
| H6 | **Racing the beam** (Atari 2600, 1977) | the program drew each line just before the beam reached it, with no frame buffer at all | the card's copy run **in raster order just behind the beam**: strips copied as the scanout passes them, so a finished frame reaches the screen within a fraction of a refresh instead of at the next one, without tearing. GAME-PATH §4.5's just-in-time swap is the same idea at the swap | A: up to a refresh (16.7 ms) of latency, no frame-rate change | the card's copy timed against the pipe's scanline counter; 5 points |
| H7 | **A locked 30 with even pacing** (most console games) | an even 30 fps reads smoother than an uneven 40 | the panel run at a multiple of the game's rate (48 or 50 Hz for 24 or 25 fps), and the game's cap at a divisor of the refresh (`endfield-levers-after-fsr3`, 4) | A: no frame rate; the judder of 2-3-2-3 gone | a mode per session |
| H8 | **Compiling for the one CPU** (every console title) | the compiler targets the exact processor | Ferrix builds its own Mesa, Wine, DXVK, yserver and hyprix: build them for Gracemont (`x86-64-v3`, AVX2), with LTO and profile-guided optimisation trained on Endfield's own run. CachyOS reports 5 to 20% across its packages for `x86-64-v3` alone [S] | A: the processor-side work (shader compiles at first sight, Wine, the driver's submit); little in the GPU-bound steady state | a build variant of the volumes; the images still run on any x86-64-v3 processor; 3 points |
| H9 | **Every coprocessor put to work** (Saturn, PS2's vector units, PS3's SPUs) | nothing idle that could carry part of the frame | the N150's other engines: the blitter (H5), the video enhancement engine (VECS) for scaling a 720p game to the screen in hardware (G6), the CPU's AVX2 (H8) | as G6 and H5 | as those |
| H10 | **A console's direct command buffers** (libgnm, the Xbox's) | the game writes the GPU's commands straight into its queue; no system call per submit | Mesa writing submits into a ring shared with the driver, which the driver reads (as Xe2's user-mode queues do in hardware), instead of an `EXECBUFFER2` crossing to the ring-3 driver: two address-space switches, with their barriers, per submit | A: tens of microseconds a submit, a few submits a frame: small at 30 fps | a new interface between Mesa and the driver; large |
| — | **Asynchronous reprojection** (VR's timewarp) | the compositor re-aims the last frame with the newest head pose | not for a flat game: the compositor knows no camera, and the game's own FSR3 frame generation already fills frames | — | — |

Sources: Factor 5's microcode and the Expansion Pak, Wikipedia's *Nintendo
64 programming characteristics* and olivieryuyu.blogspot.com (2020-01);
Crash Bandicoot's paging, Andy Gavin's *Making Crash Bandicoot* and Ars
Technica's *War Stories* (via gamedeveloper.com); the consoles' cores,
Tom's Hardware (PS4's seventh core, 2015) and eWeek (Xbox One, 2014);
`x86-64-v3`, wiki.cachyos.org's optimised repositories and Phoronix's
review of them.

What they have in common is what §5.6 says of the frame's path: **work
moved to the place and time where it costs least, because one party knew
all of it.** H2, H3, H5 and H8 are the ones worth doing first: each is
cheap, ring 3 or a probe, and attacks something measured on the box
(warm-up, first-sight stalls, the card's copy).

## 6. What each layer gains, and how we will know

Game fps comes from Wine's fps channel or the JetKVM's changed frames,
never from hyprix's counter (`game-fps-not-hyprix-frames`). For game mode
the figure that matters most is not the average: it is the **1% and 0.1%
lows and the frame-time spread**, taken from the game's own swaps (R1's
frame recorder, GAME-PATH §12) on the same scene, A/B, three rounds each.
Neither lavd nor anyone else has published those, so Ferrix measuring them
from its own present timestamps is itself something to claim.

## 7. The protections the customer agreed to trade (layer C)

### 7.1 What can be traded, by the rules already decided

BUILD-FLAGS §3.1 (DF5, ledger 931) has settled the shape: under
`runtime_trades=game_mode`, per application and audited, **only the
relaxable set around the game's job: the program barrier, SSBD and
STIBP**. The kernel's entry defences (eIBRS, the Spectre v1 clamps, the
SWAPGS fence, register clearing, VERW on RFDS-affected processors,
BHI_DIS_S or the loop, the return-stack refill, SMEP, SMAP, KASLR) are
never traded at run time, because they protect the whole machine from the
game. Turning everything off is `mitigations=off`, a build.

`runtime_trades=game_mode` is level `none`: allowed only in the `desktop`
profile. **The box runs `profile=present`** (`build ... runtime_trades=none`
on its boot line, 2026-10-30): the certified configuration, in which no
trade can be made at all. So layer C on the box means running a `desktop`
image there (§10, question 4).

### 7.2 What each costs on the N150, measured

Measured on the box itself on 2026-10-30: a kernel module timing each
instruction in a tight loop, 20,000 times, interrupts masked, three rounds,
under the box's inspection Linux (7.0.0-29, the same microcode; Linux
reports `RFDS: Clear Register File`, `BHI: BHI_DIS_S`, `IBPB: conditional`,
SSBD by `prctl`). The TSC runs at 806.4 MHz. Steady rounds:

| Instruction | TSC ticks | ns | Rate in game (§2.3) | Cost in game |
|---|---:|---:|---:|---:|
| IBPB (`PRED_CMD`), the program barrier | 91 to 115 | 115 to 140 | at most 64,274 switches a second (every switch, idle and same-process included) | at most 9 ms a second: **under 1% of one core** |
| VERW, the RFDS register-file clear on every return to user mode | 83 to 85 | ~105 | ~119,500 system calls plus 31,255 interrupts a second | ~16 ms a second: **1.6% of one core** |
| LFENCE (for scale) | 8 to 9 | ~11 | | |
| SSBD: a store-then-load loop with `SPEC_CTRL.SSBD` off and on | | | | **none visible** (on/off 99%) |

A tight loop leaves out what follows a barrier: the predictor's cold run
after an IBPB is the game's cost and no loop shows it. So the IBPB figure is
a floor. Even doubled, the whole relaxable set (program barrier and SSBD)
costs about 1 to 2% of one core in game, and the defence DF5 keeps (VERW)
about as much again.

* **STIBP**: the N150 has no SMT. Nothing to trade.
* **BHI**: the box has `BHI_DIS_S`, a control bit, not the loop: nothing
  per entry to save.
* **VERW** is the one DF5 keeps, since what it stops is the game reading
  other programs' and the kernel's register values. A desktop-only opt-out
  "would be a new flag value through a design review, not a switch"
  (BUILD-FLAGS §3.1). At 1.6% of one core it is not worth that review on
  this box.

### 7.3 So, on this box

The trades the rules allow are the program barrier and SSBD, and on the
N150 they buy **about 1% of one core and no frame at 720p**, where the GPU
is the limit; at 640x480, where the processors co-limit, at most a quarter
of a per cent of the machine. Taking every speculation defence off at run
time, VERW included, would buy under 3% of one core. That is the measured
answer to the customer's first order of 2026-10-11 on this machine: the
protections are not where the frames are. **Recommended: do not build
layer C for the N150.** It stays designed (DF5) for a machine where the
figure differs: a processor with a costlier barrier, or a game with far
more switches; the consultant's order for the driver domain stands
unchanged (SETTINGS.md §7.7, ledger 690 and 698). SETTINGS.md
§7 stays the design for sharing a driver's domain; on this box the driver
is the Intel one, and §7's reasoning holds for it unchanged.

## 8. Authority and scope

### 8.1 Who may turn it on

Two lists, as SETTINGS.md §7.3 drew them, extended to the tiers:

| | The user's list (built) | The administrator's list (proposed) |
|---|---|---|
| File | `~/.config/ferrix/game-mode.conf` | `/etc/ferrix/game-mode.conf`, root's, `0644` |
| Written by | the user, Settings | `settingsd`, on an `authd` grant to a `wheel` member with their password (`docs/SETTINGS-WRITER.md`) |
| Unlocks | layer A for the user's own programs: freezing their scopes, quieting their bar; nothing that costs the system | layer A's system part (lode, the deferred kernel polls), layer B's marks and cores, layer C's trades |

An application gets what both lists allow. Freezing the user's own
programs needs no administrator: it is the user's own processor time.
Everything that reaches other users' or the system's work, or a
protection, needs the administrator's line, as the customer decided.

What the administrator's line grants is wider than one application, and
the page will say so (SETTINGS.md §7.3): while the admitted application
runs, the session reaches every program of that user. For layers A and B
that is a share of processor time; for layer C it is a protection, which
is why C stays behind the `desktop` profile as well as the line.

### 8.2 How it starts and stops

The **session service** of GAME-PATH §5.4 does it all; nothing else
writes a cgroup file, a mark or a trade. It is root, started by init,
restarted by init, and holds the right the kernel's marks need.

* **Start.** The launcher's mark (`FERRIX_GAME_MODE_APP`) says which
  application was started; hyprix tells the service when that
  application's window is fullscreen and focused (an X game's window is
  yserver's; hyprix maps it to the client's pid). The service checks both
  lists and acts: freezes, throttles, defers, marks, places.
* **Stop.** The game's exit (the service watches its scope's
  `cgroup.events` `populated 0`), loss of focus or fullscreen, the user's
  key in hyprix, or the service's own end. A focus loss pauses the session
  (background thawed, marks off) and focus back resumes it.
* **One session at a time.**

### 8.3 What happens when something crashes

* **The game crashes**: its scope empties, the service ends the session,
  everything is thawed. lode-crashd, kept running, collects the crash.
* **The service crashes**: frozen scopes stay frozen until something
  thaws them. So before each write the service records every scope it
  freezes and every value it changes, with the value before, in a
  root-only file under `/run`; at every start it restores from that record,
  and init restarts it at once. A scope not in the record (one the user
  froze themselves) is left alone. If init stops restarting the service,
  the scopes stay frozen until the user's key or the user's own action ends
  them, and the page says so.
* **Kernel frame marks** end when the service's handle closes or when the
  marked task execs; fork and clone do not inherit one. Each of these is a
  requirement with a check and a negative control in the frame marks'
  design review (§13).
* **hyprix crashes**: the session cannot be shown, the service ends it.
* **A frozen program is needed**: the user's key ends the session; and the
  lock screen and authd are never frozen.

### 8.4 The certified item and the profiles

| Profile | Layer A | Layer B | Layer C |
|---|---|---|---|
| `max`, `fast` | no desktop; nothing | one of two, chosen in the frame marks' design review: in every profile with the item's evidence, the right that gives a mark held by no program in `max` and `fast` and named deactivated code in the safety manual; or compiled out by a flag declared in `build-flags.toml` through BUILD-FLAGS' review | absent (`runtime_trades=none`) |
| `present` (the box today) | yes: ring 3, existing interfaces, no kernel change | yes, once each mechanism is reviewed into the item: applied affinity, slices and lends are generic scheduler rules with checks | no |
| `desktop` | yes | yes | yes, under `runtime_trades=game_mode` |

Layer A needs a run-time settings writer for its system part, which
`settings_writer=absent` images do not carry; so it is a `present` and
`desktop` feature, consistent with the customer's 2026-10-24 rule that a
`max` build may forbid run-time changes (`max-build-no-runtime-settings`).
Layer B's mechanisms are not trades: a mark that orders threads changes no
isolation claim, but it changes the scheduler, which is in the core ring
with no timing claim (`MEMORY-AND-TIMING.md` §2.1). Every requirement it
writes says its figure is measured under the stated load.

## 9. The tiers

By gain against cost and risk. Points are this design's estimates.

| Tier | What | Gain | Cost | Item | Build when |
|---|---|---|---|---|---|
| **T0** | Measure: done for E1 to E4 (§11); left E5's CCS check and the scanout-resolution run; the term's poll fixed (apps) | the figures every tier below rests on | box time; 1 point for the term | none | now |
| **T0g** | The GPU path's driver and display work (§4.1, G1 to G6, G9): completion by interrupt, the idle gaps, CCS shown working, large GPU pages, plane scanout, scan-out at the game's resolution | where the box is bound: up to the GPU's idle share plus whatever bandwidth buys; E5 decides the order | G1 3, G2 endfield-60fps's, G3 1 to check, G4 8, G5 and G6 the display driver | ring 3 (G4's `PIN_BLOCKS` is core ring, reviewed on its own branch) | now, by endfield-60fps and the display design; not gated on game mode |
| **T0h** | The fixed-hardware tricks of §5.7 that are cheap: H3 pipeline caches replayed before play, H5 the card's copy on the blitter, H8 the volumes built for Gracemont with PGO, H4 the driver's per-batch commands trimmed | warm-up and first-sight stalls, ~5 ms of a processor a frame, the processor-side work | ~14 points | ring 3 | now, beside T0g |
| **T1** | Layer A: the session service; freeze the user's scopes; throttle lode and lode-upload; defer the polls; quiet the log lines; the user's list honoured, the administrator's list for the system part | M: no fps at 720p on the bare box; ~7% of a core freed; a gain where the processors co-limit or the desktop runs more | ~13 points (service 8, lode settings 3, hyprix's request 2) | none | when a game on the box co-limits on the processors, or as the frame for T2 |
| **T2** | Layer B, the cheap half: H2's recorded page-ins replayed, applied affinity (F3), per-task slices (F7), the mark given by the service from hyprix's and the driver's knowledge (§5.2, steps 1 and 2), the wake-up tail (F6) | A: evenness on four cores; E2 and E3 bound it | ~25 points, core ring, design review, ids | core | after T1, on E2/E3 |
| **T3** | Layer B, the deep half: lends through channels and futexes (§5.3), aimed wake-ups on the path (§5.4), idle-gap class (§5.5), deadlines from the frame (§5.2 step 3, = GAME-PATH G1/G2) | where Ferrix beats any Linux; unmeasured | ~50 points and stage 14's question | core | after T2 measures, and GAME-PATH question 3 |
| **T4** | Layer C: the program barrier and SSBD per application, `desktop` only | M: ~1% of one core on the N150, no frame at 720p (§7.2) | ~20 points, F-64 first, SPECULATION's desktop section | core, level `none` | not for the N150; only on a machine where the same measurement shows more |
| — | Not offered: the entry defences per application; `mitigations=off` at run time | — | — | — | never as a run-time trade (BUILD-FLAGS §3.1) |

Recommended, after the measurements: **T0g and T0h now**, G4 (large GPU
pages) first, since it is the one lever with a measured gap where the box is
bound; then **T1** as the frame for the rest; T2 and T3 decided on their own
measurements; **T4 not for the N150**.

## 10. Decisions for the customer

1. **Which tiers to build**, and in which order? *Recommended: T0g (G4
   first) and T0h now; T1 next; T2 and T3 on their measurements; T4 not for
   the N150 (§7.3: the protections cost about 1% of one core there).*
   **Decided by the customer on 2026-10-30: T0g, T0h and T2/T3**; not T1
   or T4 (`docs/backlog/DECISIONS.md`). T2/T3's frame mark needs a
   privileged holder, which §8 gave to T1's session service; the frame
   mark's design review settles who holds it instead.
2. **Freeze by default, or only what the user lists?** *Recommended: by
   default, everything the user started except the game and the keep row
   (§3.1), with exceptions the user lists.*
3. **May freezing the user's own programs need no administrator** (the
   user's list alone), with everything that reaches the system behind the
   administrator's list? *Recommended: yes.*
4. **Layer C needs the `desktop` profile.** May the box run a `desktop`
   image, giving up the certified configuration there? *Recommended: no;
   E4 measured the trades at about 1% of one core (§7.2), and the box stays
   `present`.*
5. **The customer's 2026-10-11 order, against the measurement.** "Disable
   speculative protections" was asked for frames; on the N150 every one of
   them together, VERW included, costs under 3% of one core and no frame at
   720p. Does the order stand for this box? *Recommended: set it aside for
   the N150, keep DF5's design for another machine, and spend the effort on
   G4.*
6. **Steam's browser renderers frozen while a game plays**, at the cost of
   the overlay and in-game browser? *Recommended: yes, with the overlay key
   ending the freeze.*
7. **Cores for the game**: three of four for the game and its path, one for
   the rest, while a session lasts? *Recommended: yes, as GAME-PATH §10
   question 8 says (always one left outside on a four-core machine).*

## 11. Measurements

### 11.1 Done

* The background inventory (§2.1) from the tree.
* Per-process system-call rates from the four 2026-10-30 profiles (§2.2).
* On the box, 2026-10-30 18:25 to 18:58, main-94d0c9df0, shared with
  endfield-60fps with the customer's OK: the whole-system snapshot (§2.3),
  E1 and E2 and the bar closed (§3.4), the GPU probe frozen and not (§3.4),
  and E4's instruction costs under the inspection Linux (§7.2). Raw files:
  `~/.local/share/ferrix/logs/game-mode/` on nazuna (`m-*.txt` per window,
  `snap-*.txt`, `gm-*.sh`, `specbench.c`, `ssbd.c`).
* By endfield-60fps the same day: the scene and method, the render-scale
  slope, the GPU probe against Linux's i915 (§4.1), the settings ceiling
  (no setting reaches 60 fps on this box).

Not done: §6's method (the frame recorder's lows, three rounds) for E1 and
E2, which used Wine's per-second counter and one window each; E3 (the polls deferred: at 99.9% GPU busy it cannot show), E5's
CCS check (`INTEL_DEBUG=noccs` needs Steam restarted with it in the
environment), the scanout at 720p (an EDID change and a reboot), the frame
recorder's 1% lows (Wine's per-second lines are what both sessions used),
and the barriers' cost measured inside Ferrix rather than by the same
instructions under Linux.

### 11.2 The experiments as planned

Each A/B on the same scene and image, Wine's fps, alternating:

* **E1**: freeze the user's scopes but the game's (bar, sidebars, notifyd,
  portald, term, wallpaper) and lode-upload; then also Steam's renderers.
* **E2**: renice the background +19 and the game, wineserver, yserver and
  hyprix −10.
* **E3**: defer the polls: `ferrix.mce.poll` long, `freqlog` off, yt6801's
  link poll at 2 s.
* **E5**: GPU levers: the render-scale slope (60% against 100%);
  `INTEL_DEBUG=noccs` against the default (G3); whether the GT's user
  interrupt arrives (G1); the GPU's idle share a frame before and after
  each.
* **E4**: switches a second in game (`/proc/stat`'s `ctxt`) and the cost of
  a switch barrier on Gracemont: a `present` against a `--set
  program_barrier`-equivalent measurement build, never landed.

## 12. Sources

Linux's schedulers (§5.1): scx_lavd's design and results, LPC 2024
(lpc.events/event/18/contributions/1713) and LPC 2025
(lpc.events/event/19/contributions/2150), sched-ext/scx
`scheds/rust/scx_lavd`, LWN 991205 and 1051430; scx_bpfland's README and
the author's blog (arighi.blogspot.com, 2024-08); scx_rusty and scx_flash
READMEs; EEVDF's slice request, LWN 969062; proxy execution, Linux 6.17
pull; PI futexes, docs.kernel.org/locking/pi-futex.html and `rtmutex.c`;
macOS Game Mode, eclecticlight.co 2023-10-18; MMCSS, Microsoft Learn
"Multimedia Class Scheduler Service". Community game benchmarks are
anecdotes and are quoted as such.

## 13. Review

The certification consultant reviewed this document as a design on
2026-10-30, before it landed. It is an internal review, not independent
verification (F-27, F-29). Ledger line 5115 of
`~/.local/share/ferrix/cert-consultant/reviews.md` on the gate host:

> VERDICT: OK IF (1)-(12), for GAME-MODE.md and the three pointers to land
> on develop as a design; NO CODE of layer B, C, H2 or any core item below
> without its own design review ((12)).

Conditions (1) to (11) are wordings this document carries: no figure
claimed for the item (§8.4); the protections' cost as an instruction floor
and "no frame" as an inference (§7.2, §7.3, F12, T4, §10, §14); page size
as the inferred cause (§4.1, §14); the limits of §3.4's measurement; the
machine-check poll and the audit reader kept, and the btrfs commit out of
the idle-gap class (§2.1, §3.1, §5.5); the frame mark as ordering inside
eligibility, never strict priority (§5.2, §5.5); the service's restore
record and the frame mark's lifetime (§8.3); the service's own check of
every reported thread (§5.2); the two shapes for `max` and `fast` (§8.4);
G4 through `PIN_BLOCKS` (§4.1); H2's recorder in the core ring (§5.7).

**Each of these needs its own design review before code**, each reserving
its requirement ids in `tools/common/data/requirement-reservations.json`
first (condition 12):

* F3, applied affinity;
* F4, MSI retarget, keeping the remapping's requester check (F-57);
* F6, under `docs/SCHED-WAKE-AFFINE.md`'s review (p2-151);
* F7, per-task slices;
* F9: `memory.min`, `mlock` and huge pages, each separately;
* F10, the block ring's I/O class;
* §5.2's frame mark, its right and its lifetime (§8.3);
* §5.2 step 3, the deadlines, together with GAME-PATH question 3;
* §5.3's lends (channels including the general wake path, the futex graph,
  NT sync objects), a lend's end bounded and checked;
* §5.5's idle-gap class;
* H2's recorder and acting on `MADV_WILLNEED`;
* T4: F-64 fixed first, SPECULATION's desktop section, DF5, audited per
  application;
* a VERW opt-out, as a new flag value;
* G4 beyond `PIN_BLOCKS`.

§5.4's recheck of the interface tasks needs only an ordinary code review.
The consultant's lesser points are carried too: the frame mark's own name
(§5.2), §5.6's processors not wholly the path's, the lend on the general
wake path beside the direct switch (§5.3), and F-95 as a lever for the
lows beside F11, not game mode.

## 14. Where it stands

2026-10-30: this document, written in one session (`game-mode-design`)
from the tree, the day's box profiles, two web studies (Linux's game
schedulers; fixed-hardware tricks) and a box turn shared with
endfield-60fps. No product code. What the box said: at the settings Endfield
is played at, the GPU is the limit; freezing, nice values and the
protections each buy at most about 1% of one core and no frame; the GPU's
4 KiB page size costs a measured 15 to 25% of shader memory bandwidth
against Linux. So the order of work is the GPU path (G4 first) and the
cheap fixed-hardware tricks, then the session service, and the protection
trades not on this machine. The certification consultant reviewed it as a
design (OK IF (1) to (12), carried; ledger line 5115; §13): no code of
layer B, C or H2, or of any core item §13 lists, without its own design
review. The customer chose T0g, T0h and T2/T3 the same day (§10,
question 1); nothing is built yet.
