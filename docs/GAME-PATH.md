# The game path: where a fullscreen game can be faster on Ferrix than on Linux

Version 1, a draft written on 2026-10-10 for the customer, who decides §10.
The customer decided question 1 of §10 the same day: this is a roadmap
stage, [stage 24](roadmap/stage-24-game-path.md). Questions 2 to 10 are
open.
The customer asked that day for "an agent that designs the 'faster than
Linux' path": where and how Ferrix can beat a stock Linux desktop for a
fullscreen game on the same hardware. This document is that design and its
build plan. **Nothing in it is built, measured anew or landed**: every figure
below was measured by another session and is quoted with its source, or is
an estimate and says so.

It stands beside three pieces of work it does not repeat:

* [`docs/NVIDIA.md`](NVIDIA.md), the driver (`nvrm`) and stage 21;
* the request from NVIDIA's libraries into `nvrm`, being cut on branch
  `nv-trip` (notes `~/.local/share/ferrix/steam-race/trip.md` on nazuna);
* yserver's present path and the dmabuf hand-over from an X client to
  hyprix (branch `nv-present`, yserver's `present-pace`; notes `present.md`
  and `zerocopy.md` in the same directory).

§4 and §9 say what this design needs from each. It writes no requirement
ids (they are reserved first, `docs/CONVENTIONS.md`). The roadmap stage and
its SysML entry were added with question 1's decision
(`docs/roadmap/stage-24-game-path.md`, `S24` in
`docs/sysml/10-roadmap.sysml`); they repeat §8's rows and points and add
none.

## 1. What this is, and what it is not

**The short answer.** A game's frame rate when nothing holds it back is
decided by NVIDIA's closed userspace and the card, which are the same
bytes on both systems. There a tie is the ceiling, and Ferrix is 8% under
it today. What a system that owns the kernel, the compositor, the X server
and the driver's glue *can* win is what happens around the frame:

1. **how long an input takes to change the picture**, because Ferrix can
   time the game's frame, the X server's hand-over and the flip against one
   clock, where Linux has four programs each guessing at the next one's
   timing with a safety margin of its own;
2. **how even the frames are, above all while the machine does something
   else**, because Ferrix owns the scheduler and can give a game's threads
   and the present path processors that nothing else may run on, as a
   guarantee with a check, where a Linux desktop offers heuristics;
3. **that this is how the system behaves as shipped**, where on Linux the
   best figures need gamescope, a patched compositor, a scheduler chosen by
   hand and environment variables per game.

It is not a plan to make NVIDIA's code faster, not a plan to beat Linux at
system calls (§6.1 says why that is the wrong fight), and not a promise
about a display's own delay, which is the same on both systems and is
often the largest term.

**Against which Linux.** Two, and every target in §2 names which:

* **stock**: a distribution's desktop as installed. The baseline guest is
  Debian 13, kernel 6.12, NVIDIA 580.173.02, sway 1.10.1 with Xwayland
  (`~/.local/share/ferrix/linux-baseline/README.md`). The customer's words
  were "a stock Linux desktop"; this is the comparison the plan must win.
* **tuned**: the best a person can configure, from published measurements
  (§2.2): direct scan-out, tearing or VRR, a latency tool, a compositor
  patched for late rendering. This is the bar the plan aims at and may
  only match.

## 2. What "faster" means

### 2.1 Where both systems stand tonight

Measured by other sessions on 2026-10-10 on the RTX 3060 passed into a KVM
guest of the same shape for both systems (8 processors, 16 GiB), Teeworlds
0.7.5 at 1920x1080 on NVIDIA's GL, unless a row says otherwise.

| What | Linux (stock guest) | Ferrix | Source |
|---|---|---|---|
| Frames a second, vsync off | 2,400 (2,363–2,426) | about 2,200 | `linux-baseline/README.md`; `present.md`, 22:30 table |
| One frame, vsync off | 417 µs | 455 µs, with about one driver request in it | the coordinator's figures for this design |
| Frames a second, vsync on | 60 steady | 60 steady, since yserver paces completions | the same; `present.md` |
| yserver's time on its thread per shown frame | — | about 2 ms (a two-step read-back) | `present.md`, 22:30 |
| Relative motion, hyprix's event time to yserver's hand-over, vsync on | no figure exists | median 0.67 ms, 99th 1.07, worst 1.3 | `present.md`, 22:30 |
| hyprix's frame through `virgl_test_server` | — | 9.3 ms at best, 12 to 14 ms typical | `compositor-rate.md`; `zerocopy.md` (n3c) |
| `nvrm`'s copy of a frame into VRAM | — | about 1.5 ms | `zerocopy.md` |
| One request into the driver | 110 to 450 ns (host's driver) | 75 to 130 µs before `nv-trip` | `trip.md` |
| A bare system call (`getppid`) | 53 ns | 357 ns (150 ns on the card guest, §6.1) | `softgl-gap.md`; `trip.md` |
| Futex round trip | 12.7 µs | 6.7 µs | `softgl-gap.md` |
| Socket round trip | 13.6 µs | 16.6 µs | `softgl-gap.md` |
| Native channel round trip | — | about 1.05 µs on `main`, 1.3 µs in the brief | `docs/roadmap/ipc-round-trip.md` |
| The WebGL page at 60 frames a second, processors used | 0.25 | 1.9 | `linux-baseline/README.md` |
| Under host load | — | 100 to 250 ms frames until the guest's processors had host cores of their own | the coordinator |

**What no one has measured**, on either system: the time from an input to
the frame that shows it, a frame-time distribution (only one-second
counts exist), a launch time, and anything under a load that is defined
and repeatable. So the first landing is the rig (§8, R1 to R4), and
nothing in §2.3's "today" column for those rows is a measurement.

### 2.2 What a tuned Linux achieves, as published

Read on 2026-10-10 by a research subagent of this session; **[S]** means
the page itself was read that day, **[2]** a secondary report or a search
summary, and what could not be found is listed in §11. None of it was
re-measured.

* **Click to photon, the best published.** Marco Nett, 2026-07-13 [S]: a
  microcontroller that is both a 1000 Hz mouse and a photodiode reader,
  300 clicks a case, an RTX 4070 SUPER on a 500 Hz panel, Plasma 6.7,
  a DirectX 11 game under Proton, tearing allowed. Medians of **4.2 to
  4.9 ms** on X11 and on native Wayland, with VRR worth 0.26 to 0.45 ms;
  **Xwayland costs 3.1 ms more than native Wayland** (8.06 against 4.93 ms
  plain). The 5th to 95th percentile spread is 2.1 to 3.0 ms.
* **Where a compositor's own time goes.** farnoy.dev, 2026-06-07 [S], with
  an open-source LDAT on an LG C1 at 120 Hz: Windows 11 leads Plasma 6.6 by
  about 4 ms in the best VRR runs. KWin predicted 11.35 ms of render time
  at the median where it took 2.06, kept a 1.46 ms margin, a fixed 1 ms
  timer term and a 2 ms floor; with those patched out the floor from input
  to present was about 3 ms. Not upstream at that date. The same post found
  that a software measure, input to present, tracks the photodiode with
  R² of 0.94 or better: the ground §2.4's software method stands on.
* **A scan-out's own cost** in KWin is 40 to 100 µs of compositor time
  (Xaver Hugl, KWin merge request 4293 [2]); gamescope's vblank manager
  keeps about 1.65 ms of "red zone" and assumes 3 ms of draw time by
  default [2, a generated wiki, not the source].
* **At 60 Hz and 144 Hz no instrumented figure was found.** A 240
  frames-a-second camera on GNOME 47 at 144 Hz gave a *cursor* 16.7 ms on
  X11 and 23.2 ms on Wayland (mort.coffee, 2025-01-26 [S]).
* **Latency tools.** NVIDIA Reflex removes the render queue by starting
  the game's simulation just in time; on Linux it reaches DirectX games
  through DXVK, VKD3D-Proton and dxvk-nvapi over `VK_NV_low_latency2`
  [2]. LatencyFleX claims "up to 10 ms", none with vsync, and names an
  anti-cheat risk [S]. A DXVK fork for low latency gained 0.1 to 0.8 ms in
  Nett's runs [S].
* **Scheduling.** Feral GameMode changes the governor, niceness, I/O
  priority and can pin or park cores [S]. Valve's scheduler `scx_lavd`
  orders tasks by how often they wake and wait; its authors' talks give no
  frame-time tables, and LWN's report of 2026-01-07 lists its limits [S].
  **No published figure for frame times under a background load was
  found**, for any Linux scheduler. None of these gives a bound on wake-up
  latency, a reservation or an admission test; they are heuristics (an
  inference, from what each says it does).
* **Windows, as the reference design.** The flip model's independent flip
  lets the compositor sleep while the game's buffer is scanned out, a
  waitable swap chain brings a vsynced game "down to 1 frame of latency",
  and multiplane overlays keep that while something is drawn over the game
  (Microsoft, 2018-04-09 [S]). Hardware-accelerated GPU scheduling moves
  the GPU's context switching onto the card; Microsoft gives no measured
  gain [S].
* **Consoles and Android.** Android paces applications against a frame
  timeline with presentation timestamps (Swappy, FrameTimeline [2]); the
  Xbox runs the game in a partition of its own with reserved resources
  (non-official sources only [2]). Both are the shape §5 takes.

### 2.3 The metrics, and a target for each

All at 1920x1080 on the 60 Hz display the card drives today, in the two
guests of §2.1 with their processors pinned to host cores of their own
(§2.5), then once on bare metal before anything is claimed outside the
tree. *c* is the time the game needs for one frame, swap to swap, with
nothing holding it. "Refresh" is 16.7 ms at 60 Hz.

| # | Metric | Ferrix today | Stock Linux | Tuned Linux | Target for Ferrix |
|---|---|---|---|---|---|
| M1 | **Input to scan-out, vsync on**: from the injected motion to the start of the scan-out that shows it; median and 99th percentile | not measured; about 40 to 60 ms by summing §3.1's stages (an estimate) | not measured; expected 1.5 to 2.5 refreshes (25 to 42 ms) | half a refresh + *c* + its margins: about 12 to 14 ms for a light game with a just-in-time tool, when the game supports one | median ≤ half a refresh + *c* + 2 ms (11 ms at *c* = 1 ms), 99th ≤ one refresh + *c* + 2 ms, **for any game, with no support from it**; and at least one refresh under stock Linux on the same rig |
| M2 | **Input to scan-out, tearing**: the same with vsync off and tearing allowed | not possible (no tearing flip exists) | not measured | 4.2 to 4.9 ms click to photon at 500 Hz [S]; on a 60 Hz panel bounded by *c* and the scan position | median ≤ *c* + 1.5 ms to the flip; equal to tuned Linux within the rig's error. A tie is the ceiling |
| M3 | **Frame-time consistency, idle machine, vsync on**: missed refreshes a minute, 99.9th percentile frame time, worst frame in ten minutes | not measured; one-second counts say 60 | 60 steady by the same counts | — | 0 missed in ten minutes; worst frame ≤ 2 refreshes |
| M4 | **The same under the background load of §2.4** | not measured; the guest stuttered under *host* load | not measured | no published figure exists | missed refreshes ≤ 1 a minute and worst frame ≤ 2 refreshes with the load running; and fewer misses than stock Linux, and than Linux with GameMode, by the rig. **The metric this plan expects to win outright** |
| M5 | **Wake-up latency of the present path's threads** on the game's processors, load running (stage 14's cyclictest shape) | not measured | — | — | a stated bound that a boot test holds: 99.99th percentile ≤ 100 µs under KVM as a first figure, restated from bare metal |
| M6 | **Launch to first frame**, cold and warm | not measured | not measured | — | within 10% of stock Linux. No win is claimed (§9) |
| M7 | **Processors used per frame at 60 frames a second** by everything but the game | compositor path about 1.2 for a composited GPU client (`linux-baseline`) | 0.15 (sway) | — | ≤ 0.05 for a fullscreen game shown by direct scan-out; within 1.25 times stock Linux for a composited one, after §6.3 |
| M8 | **Frames a second, vsync off** | about 2,200 | 2,400 | the same | ≥ 2,330 (97%). A tie is the ceiling |
| M9 | **A timed sleep's overshoot** (`clock_nanosleep` of 1 ms; 99th percentile), which Wine's timers and a game's frame limiter rest on | not measured | not measured (Linux's default timer slack is 50 µs, from memory) | — | ≤ 50 µs on the game's processors |

The order of the targets is the order of what a player feels: M1 and M4
first. M8 stays in the table so that nothing done for the others is paid
for there.

### 2.4 How each is measured, the same way on both systems

Nothing may be claimed that was measured with one tool on Linux and
another on Ferrix. Every tool below is one binary that runs on both: an X
client on glibc, or a Wayland client for a protocol sway and hyprix both
serve.

**With no extra hardware.**

* **Frame times (M3, M4, M8).** The swap counter that exists
  (`linux-baseline/tools/swapfps.c`, an `LD_PRELOAD` library both guests
  already load; `libswapfps2.so` adds the time inside the swap) gains a
  per-swap record: the monotonic time of every swap's entry and return in
  a ring written out at exit, and the Present extension's `CompleteNotify`
  for each (its `ust` and `msc`), which both X servers send. A missed
  refresh is an `msc` that advanced by more than one between two swaps of a
  vsynced game: the same definition on both, with no tracing in either
  kernel. A summariser gives the median, the 99th and 99.9th percentiles,
  misses a minute and the worst frame.
* **Input to scan-out (M1, M2).** Two programs. An injector: the existing
  `vptr` (`~/.local/share/ferrix/yserver-present/vptr`, a
  `zwlr_virtual_pointer_v1` client; sway serves that protocol too), which
  gains a log of each motion's send time. A probe: a small X client that
  takes XInput2 raw motion as a game does, and on each event draws the
  next frame in a colour that encodes the event's number, logging when the
  event arrived, when it swapped and that swap's `CompleteNotify`. The
  latency is the injector's send time to the `ust` of the first completion
  whose frame carries the event. It leaves out the device's polling, the
  driver below the compositor and the display; farnoy's result (§2.2) is
  the ground for trusting it as far as the flip, and R5 below checks that
  once on this hardware.
* **The same from one layer further down**, for the driver and the
  kernel's input core: QEMU's `input-send-event` on the guest's virtual
  mouse, timed on the host. It only works where the mouse is QEMU's and
  not a passed-through device, so it is the gate's method (§8), not the
  card's.
* **The background load (M4, M5).** One script, the same on both: a fixed
  Rust crate built in a loop on every processor, a writer that streams to
  the disk, and a program that holds memory until 80% is in use. It runs
  as an ordinary user's job, started before the game. On Linux it is run
  three ways: stock, with GameMode, and with `scx_lavd` if the guest's
  kernel can load it.
* **Launch (M6).** The preload library's constructor time against the
  first swap's return, after a boot (cold) and again (warm).
* **Processors per frame (M7).** `cpusnap.sh`, which exists on both sides
  (`linux-baseline/ferrix-side`, `linux-side`).
* **A timed sleep and a wake-up (M5, M9).** A cyclictest-shaped program:
  sleep to an absolute time, record the overshoot, as stage 14's exit
  describes.

**With hardware, once, to check the software method (R5).** Two ways, and
question 6 of §10 chooses:

* a microcontroller that is a USB mouse and reads a photodiode on the
  screen, as Nett's and farnoy's rigs are (an RP2040 or a Teensy and a
  BPW34 diode; open designs for both exist [S]). It measures from the USB
  report to the light, on both systems, with the display's delay included;
* the Pixel 7's 240 frames-a-second video of the mouse and the screen,
  which costs nothing and resolves about 4 ms a sample [S, mort.coffee's
  method]. The phone is shared between sessions and its use is the product
  owner's word; filming writes nothing to it.

**What cannot be measured without hardware:** the display's own delay, the
USB polling interval's share, and where in the scan the changed pixel is.

### 2.5 What a virtual machine does to these figures

Every figure in §2.1 is from a guest. Three consequences:

* **Smoothness in a guest is the host's to give.** The 100 to 250 ms
  frames ended when the guest's processors got host cores of their own.
  The rig's guests are therefore both defined with pinned processors and
  an invariant TSC (`softgl-gap.md`: without it Ferrix read its clock at
  22 µs a read), and M3 to M5 are read as *comparisons between two guests
  on one host*, not as what either system does on a machine.
* **What a guest can show:** every comparison of software paths (M1, M2,
  M7, M8), since the card is real and both guests pay the same exits.
* **What needs bare metal before it is claimed:** M4 and M5 in absolute
  terms, anything about interrupts' placement, VRR, and the bound in §5.
  Bare metal is stage 21's exit, not this plan's.

## 3. The path today, hop by hop

### 3.1 Ferrix

Verified against the tree and tonight's notes, except the first row's
figures, which nobody has measured.

| Hop | What happens | Cost, where known |
|---|---|---|
| Device → driver | a ring-3 HID or virtio-input driver reads the report and sends it over `inputctl` to the kernel's input core, which queues it per open (`docs/INPUT.md` §3) | not measured |
| Input core → hyprix | hyprix reads `/dev/input/eventN` when `poll` says so (`hyprix/src/devices.rs`). Its one loop also draws: while it waits for a frame's round trip through `virgl_test_server` (9 to 14 ms) input waits with it | not measured; up to a frame's time when the loop is in a frame |
| hyprix → yserver | `zwp_relative_pointer_v1` motion over the Wayland socket | with the next row: median 0.67 ms, 99th 1.07 |
| yserver → game | XInput2 raw motion over the X socket | |
| Game | samples its input, simulates, draws; with vsync on it does so when its last swap completed | *c* |
| Game → yserver | NVIDIA's GL sends `PresentPixmap` of a DRI3 pixmap (a dmabuf in VRAM) | one driver request a frame, 75 to 130 µs before `nv-trip` |
| yserver | waits for the GPU, **reads the frame back from the card** in two steps and commits it to hyprix as a `wl_shm` buffer | about 2 ms on its thread |
| hyprix, waiting | takes what is committed when its next frame starts. Frames start on a timer one refresh after the last was due (`hyprix/src/pace.rs`), not at the display's vertical blanking: the timer and the screen have no common phase | 0 to one refresh |
| hyprix, drawing | uploads the buffer and composites through `virgl_test_server` on NVIDIA's EGL, then reads its own frame back into the card's dumb buffer | 9 to 14 ms |
| hyprix → kernel → `nvrm` | a page flip is a `FLUSH` over `displayctl`; `nvrm` copies the rectangle into the VRAM surface NVKMS scans out and **answers at once: "the copy is the flip"** (`kms.c` on `nv-present`). Nothing waits for vertical blanking, so the copy can land in the middle of a scan | about 1.5 ms |
| Scan-out | the changed line is reached | 0 to one refresh |
| Display | its own processing | outside both systems |

A frame's pixels cross between the card and system memory four times
(yserver's read-back, hyprix's upload, hyprix's read-back and `nvrm`'s
copy up again), and the picture is paced by two timers and no interrupt
from the screen. The time from the event's arrival at the game to the scan
is, summed from the rows: up to a refresh until the game samples, *c*,
2 ms, most of a refresh until hyprix's next frame, 11 to 15 ms, and up to
a refresh of scan: **about 40 to 60 ms at the median**, an estimate.

**What does not exist yet** (each verified by reading): a buffer handed
from a client to the screen without a copy; a flip that waits for, or
reports, the display's vertical blanking on the NVIDIA card (`FLIPPED` is
a sequence and a status, with no time: `src/lib/proto/displayctl`, and the
kernel stamps the DRM event with its own clock when the answer arrives,
`interfaces/display/drm.rs`); `DRM_IOCTL_WAIT_VBLANK` or a sequence
queue on `card0`; a cursor plane on the NVIDIA card (`display.rs` says
`cursor: false`); tearing flips; VRR. hyprix *advertises*
`wp_tearing_control_manager_v1`, `wp_fifo_manager_v1`,
`wp_commit_timing_manager_v1` and `wp_presentation`
(`hyprix/src/state.rs`), and has `general:allow_tearing` as an option,
but no flip beneath them can tear, and the presentation time it reports is
its timer's.

### 3.2 A tuned Linux

From the published sources of §2.2 and from memory of the designs; not
read in Linux's or gamescope's source for this document.

| Hop | What happens |
|---|---|
| Device → compositor | the kernel's HID driver and evdev, read by libinput inside the compositor |
| Compositor → game | Wayland relative motion, through Xwayland for an X game (3.1 ms dearer than native Wayland in Nett's runs) |
| Game → compositor | the game's buffer is a dmabuf; the X server or the Wayland driver hands it on without a copy, with explicit sync since NVIDIA 555 |
| Compositor | a fullscreen opaque buffer is put on the primary plane as it is ("direct scan-out"); the compositor's work is the commit, 40 to 100 µs, started a margin before vertical blanking (1.5 to 4.5 ms of margins and floors in stock KWin, about 1.65 ms in gamescope) |
| Kernel | an atomic commit to an in-kernel driver; a flip at vertical blanking, or at once with tearing; the flip's time comes back with the event |

The game's buffer crosses to system memory never. What is left is one
refresh of queueing for a vsynced game (it samples its input right after
its last frame was taken, a full refresh before the flip that shows the
next), which only a latency tool inside the game or its translation layer
removes, and the margins each program keeps because it does not know the
others' timing.

### 3.3 Where Ferrix can be ahead, and where only level

* **Level at best:** the copies (Linux has none), the flip itself, tearing,
  VRR, the hardware cursor. These are §4.1 to §4.4 and must be built
  before any comparison is fair.
* **Ahead by design:** the queueing refresh, removed for *any* game by the
  system holding the swap's completion until just in time (§4.5), where
  Linux needs the game or DXVK to cooperate; smaller margins everywhere,
  because the threads that keep them have bounded wake-ups on processors
  of their own (§5) and one clock (§4.1); and the same under load.
* **Behind, and staying so for now:** processes on the path. A game's
  frame on Ferrix passes the X server, the compositor, the kernel and
  `nvrm`; on Linux the driver is in the kernel. One more wake-up each way.
  §6.2 says why that costs microseconds and not frames once §5 bounds the
  wake-ups.

## 4. The design, end to end

One principle: **the display's vertical blanking is the only clock, and
every program on the path schedules backwards from it.** The pieces are
ordered as they depend on each other.

### 4.1 One clock: real flips, and their time

*Needs displayctl v9 from the zero-copy work* (`zerocopy.md`, Step 2: a
`flips` card whose driver flips a dmabuf by identity, approved by the
certification consultant with conditions Z1 to Z10, not built). That work
has `nvrm` answer `FLIPPED` from NVKMS's flip-completion event. This
design adds three things on top of it:

1. **`FLIPPED` carries the time the flip happened**, as the driver read it
   at NVKMS's `flipOccurred` event, and the scan-out's sequence number.
   The kernel's display core puts that time, not its own at the answer's
   arrival, into the DRM vblank event (`CRTC_IN_VBLANK_EVENT` is already
   promised to clients, `docs/DISPLAY.md` §2.3). A protocol version bump.
   Whether NVKMS's event carries a hardware timestamp or `nvrm` must take
   the time in its interrupt thread is **not known**: the KAPI header
   (`nvkms-kapi.h`, 580.173.02) shows the event and no field list was
   read. If it is the thread's time, §5 is what bounds its error.
2. **hyprix's pace follows the flips.** `Pace` keeps its grid, but the
   grid's phase and period are corrected from each flip's time, so "when
   the next frame is due" means the next vertical blanking, and
   `wp_presentation_feedback.presented` and frame callbacks carry the
   display's time. On a card that reports none (virtio-gpu under VNC) it
   stays the timer it is.
3. **A vblank without a flip.** A client that draws nothing new still
   needs the clock. NVKMS offers a per-layer `minPresentInterval` and a
   flip-pending query; whether it offers a vblank callback to a KAPI
   client was **not found** in the header's function table. If it does
   not, the clock between flips is predicted from the last flip's time and
   the mode's period, which is what gamescope does [2].

### 4.2 The game lane: a fullscreen client's buffer is scanned out as it is

When one opaque surface covers an output exactly, nothing is above it but
a cursor on a plane, and its buffer is a dmabuf the card's own driver made
in a format and layout the card scans out, hyprix does not draw. It adds
the buffer as a framebuffer once, flips to it, and gives the client the
flip's time. This is Windows's independent flip and every Linux
compositor's direct scan-out.

* **Who decides that a buffer may be scanned out: the kernel**, by the
  rule the consultant set for v9 (Z1: the dmabuf was made by the live
  control of the driver that serves this card). A game's swapchain images
  come from the same `nvrm` through the render node, so they pass that
  test by construction, and a buffer from anywhere else is refused before
  the driver hears of it. hyprix decides only *whether to try*.
* **Falling back is a frame, not a glitch.** Anything that breaks the
  conditions (a notification, a second window, a cursor on a card without
  a plane, a format the head cannot scan) sends that frame through the
  compositor as today. The fallback and the return are each one flip.
* **An X game.** yserver must hand the X client's pixmap on as a
  `zwp_linux_dmabuf_v1` buffer instead of reading it back: the hand-over
  the present session is designing. **What this design needs from it:**
  one `wl_buffer` per pixmap, kept across frames; the buffer's release
  turned into `PresentIdleNotify`; the surface committed in the same loop
  iteration the `PresentPixmap` arrives in, with no GPU wait on yserver's
  thread (the kernel-side fence is the driver's business once nothing
  reads the pixels); the Present option for an asynchronous flip carried
  to the surface as a tearing hint (§4.4); and `CompleteNotify` filled
  from `wp_presentation`'s time and sequence, not yserver's own clock.
* **NVIDIA's buffers.** NVIDIA allocates swapchain images block-linear in
  video memory; `nv-vram-dmabuf` exports those as name-only dmabufs and
  proved an export and import on the card (`vramdmabuf.md`). That NVKMS
  scans out such a client-made image through `createSurface` is **assumed**
  from Linux, where nvidia-drm does it for KWin and gamescope; it has not
  been tried in `nvrm`, and zero-copy's own note found that NVIDIA's EGL
  drew into a *copy* of a foreign dmabuf on the host (`zerocopy.md`, last
  entry). It is the first thing P2 proves on the card, before anything is
  built on it.

**Rejected: an output lease.** hyprix could hand the fullscreen client's
server (yserver) the right to flip the card itself and leave the frame's
path, as DRM leases do for headsets. It saves one wake-up of hyprix a
frame, tens of microseconds once §5 holds, and costs a revocation protocol
between three programs and a second holder of the card, which
`docs/DISPLAY.md` §5 has as one exclusive open by design. Not worth it.

**Rejected: presenting past the X server.** NVIDIA's GLX and its Vulkan X11
presentation speak DRI3 and Present to an X server and nothing else; a
shim that answered them inside the game would be a second X server. The
way to have no X server on the path is a Wayland client (§7.3), not a
bypass.

### 4.3 The cursor, and anything else drawn over a game

* **The cursor on NVKMS's cursor plane.** KAPI has a cursor configuration
  per head beside the layers; `nvrm`'s card says it has none today.
  "A hardware cursor through NVKMS" is already second in the customer's
  order for stage 21, and hyprix, the display core and `displayctl` have
  carried a cursor plane since 2026-09-23 (`docs/GPU.md` §3.10). This
  design depends on it: a game that shows a pointer (a menu, a strategy
  game, Endfield's interface) would otherwise leave the game lane every
  time the pointer appears.
* **Later: an overlay on a second layer.** NVKMS offers up to eight layers
  a head. A notification or a volume bar drawn into a small buffer on a
  layer above the game would keep the game's buffer on the primary layer,
  as Windows's multiplane overlays do; KWin has this as an opt-in since
  Plasma 6.5 [2]. It is P9, behind everything else, because a
  notification over a game is rare and the fallback is one frame.

### 4.4 Tearing, and VRR

* **Tearing where the game asks.** A Wayland client asks with
  `wp_tearing_control_v1`, an X client with Present's asynchronous option
  (NVIDIA's GL with vsync off sends `target_msc` 0, `present.md`). With
  `general:allow_tearing` set and the surface in the game lane, hyprix
  flips with a new displayctl flag that `nvrm` passes to NVKMS as the
  layer's `tearing` (the field exists in KAPI's layer configuration, and
  nvidia-drm sets it for an asynchronous flip). The flip does not wait
  for vertical blanking. This is what makes M2 possible at all.
* **VRR.** KAPI has `vrrEnabled` in the mode-set configuration, a
  `vrrFlip` reply and a VRR semaphore some cards require. With VRR a
  vsynced game's flip happens when its frame is ready, and §4.5's margin
  stops mattering. It needs a display that does it and a mode set that
  says so; whether the customer's television offers it over HDMI is not
  known (question 5 of §10). The cursor under VRR is its own problem on
  Linux (amdgpu, KWin and GNOME each work around it [2]); with NVKMS it is
  to be found.

### 4.5 Just in time: holding the swap, for any game

With §4.2 a vsynced game is where a tuned Linux leaves it: it samples its
input when its last frame was taken, draws in *c*, and waits most of a
refresh for the flip. The picture is a refresh older than it needs to be.

**The design.** For a client in the game lane, the program that completes
its swap (yserver for an X client, hyprix for a Wayland one) holds the
completion, and so the start of the game's next frame, until

    next vertical blanking − (the game's recent c, at its 95th percentile) − a margin

so that the game samples its input, draws, and its buffer reaches the
kernel just before the flip that shows it. The controller is small and
conservative:

* *c* is measured per client from completion to the next `PresentPixmap`
  or commit, over the last second;
* the margin starts at 2 ms and is the sum of what the path itself needs
  (yserver, hyprix, the kernel and `nvrm`'s flip request; microseconds
  each when nothing preempts them) and the wake-up bound of §5;
* a missed vertical blanking doubles the margin for a second and is
  counted; a client that misses repeatedly, or whose *c* is above 80% of
  a refresh, is released at once, as today;
* it is off unless a window rule or the game session (§5.4) turns it on,
  because a game that samples its input elsewhere in its loop gains
  nothing.

**Why this is Ferrix's to win.** Reflex and its kin need the game or the
translation layer to call them; LatencyFleX says of itself that it does
nothing with vsync. KWin does the same trick only for its *own* rendering.
A compositor on Linux could hold a client's frame callback this way and
none does for games, because the margin it would need against the
scheduler's jitter eats the gain: farnoy measured KWin keeping about
4.5 ms of margins and floors to be safe (§2.2). Here the three programs
on the path share one clock and, with §5, a bound on their wake-ups, so
the margin can be the path's real cost plus that bound.

**For a game that has a latency tool** (a DirectX game under DXVK with
Reflex, §7.2) the tool does the same from inside and knows the game's
simulation better. Then the hold stays off, and what Ferrix adds is the
tight clock under it.

**Rejected: making the compositor's frame callback earlier or the game's
queue shorter by default for every client.** It trades missed frames for
latency on programs that never asked; the lane is opt-in.

### 4.6 When the compositor must draw: backwards from the deadline

A windowed game, or one with something over it, is composited. hyprix
today takes what clients committed at the *start* of its frame, right
after the last one, so a commit waits most of a refresh. It will instead
start a frame at the next vertical blanking minus its own predicted
drawing time minus a margin, and take commits up to that moment, as KWin
and gamescope do. Two things make the prediction worth having: §6.3 (a
frame through `virgl_test_server` takes 9 to 14 ms, which leaves nothing
to schedule) and §5 (the margin).

### 4.7 Input

* **Time carried end to end.** The driver stamps a report when it reads
  it; the stamp is the evdev event's time (the input core queues what the
  driver sends), hyprix sends it as the Wayland event's time, yserver as
  the X event's. Each program logs, on request, the event's age when it
  passes. This is what turns "the mouse lags" into a hop and a number.
* **Input never waits for a frame.** hyprix reads its devices and forwards
  a locked pointer's motion on a thread that does nothing else, so a frame
  in progress (§3.1's second row) cannot hold an event. hyprix has threads
  for flushing and for its scope only today (`flush.rs`, `scope.rs`); the
  split between its front and its drawing was proposed on 2026-10-03 for
  another reason (surviving a stalled driver) and serves this one.
* **No coalescing for a locked pointer.** A 1,000 Hz mouse sends a
  thousand relative motions a second, and a game that holds a pointer lock
  gets each one with its time. A pointer over the desktop is still drawn
  once a frame.
* **Late delivery is the game's sampling, not the system's batching.**
  Events are delivered as they arrive; §4.5 makes the game read them late.
* **Kept for after the rig: a tap.** While a client holds
  `zwp_locked_pointer_v1` on a focused fullscreen surface, relative motion
  needs no decision from the compositor. The input core could give that
  client's server a second, read-only reader of the mouse that carries
  relative axes and buttons only, made and revoked by hyprix with the
  lock, so that motion goes from the kernel to yserver with hyprix off the
  path. It is built only if the rig shows hyprix's hop above about 0.3 ms
  at the 99th percentile once it has its own thread: a tap is a new
  interface into the kernel's input core, and one more way for input to
  reach a program. **Never for a keyboard**: keys carry passwords, and the
  compositor's shortcuts must see them first.

### 4.8 What `nvrm`'s display side offers, and what is missing

Read on `nv-present` (`os/glue/kms.c`, `os/nvos/src/display.rs`) and in
NVIDIA's `nvkms-kapi.h` for 580.173.02.

| Needed | In KAPI | In `nvrm` today |
|---|---|---|
| A flip of a client's surface | `createSurface` over imported memory, `applyModeSetConfig` with a layer's `surfaceChanged` | one surface of `nvrm`'s own, set once; frames are copied into it |
| The flip's completion | the `flipOccurred` event, `getFlipPendingStatus` | answered at once; no event subscribed (`declareEventInterest` is not called) |
| A vblank with no flip | not found in the function table | none |
| Tearing | the layer's `tearing` field | not set |
| VRR | `vrrEnabled`, `vrrFlip`, `signalVrrSemaphore`, `vrrSupported` | not set |
| A cursor | the head's cursor configuration, `maxCursorSizeInPixels` | `cursor: false` in the card's HELLO |
| Overlays | up to 8 layers a head | the primary layer only |
| A flip that waits for the GPU | semaphore surfaces and sync points in the layer's configuration | the semaphore-surface callback works since `nv-present` 93d67a66b (the lost MSI) |
| A mode list | `getDisplayMode`, `validateDisplayMode` | the one running mode |

## 5. A game session the kernel knows about

### 5.1 What Ferrix has

Verified in the tree:

* **One scheduling domain**, `Throughput`, over every processor
  (`src/kernel/src/sched/mod.rs`): EEVDF with a 3 ms target latency, a
  queue per processor and work stealing. `SoftRt` and `HardRt` exist as
  names in `src/lib/kernel/sched/src/domain.rs` and as a design in
  `docs/ARCHITECTURE.md` §5; stage 14, which builds them, is not started.
* **A task's processors are a set inside the scheduler** (`may_run_on`,
  `is_pinned` in `sched/queue.rs`), but `sched_setaffinity` is "validated,
  accepted, and not applied", and `sched_setscheduler` refuses everything
  but `SCHED_OTHER`, on purpose: "a promise the scheduler has no class to
  keep" (`syscall/limits.rs`).
* **cgroups v2**: `cpu.weight` and `cpu.max` for a job, native tasks and
  ring-3 drivers included (`docs/CGROUPS.md` §12); `memory.max`,
  `memory.high` and reclaim. No `cpuset`, no `memory.min` or `memory.low`.
* **Memory**: no `mlock` in the system-call table; no compaction and no
  transparent huge pages were found in `src/kernel/src/mm` (a search, not
  a reading), so there is nothing of that kind to switch off.
* **Time**: one-shot timers armed for the next deadline, no periodic tick
  owed (`src/kernel/src/timer.rs`); the TSC as the clock under KVM with an
  invariant TSC, read through the vDSO in 20 ns.
* **Interrupts**: a device's line belongs to its ring-3 driver, which
  waits on it; an edge MSI is bounded by `STORM_BOUND`
  (`docs/certification/MEMORY-AND-TIMING.md` §2.2). Whether an MSI's
  destination processor can be chosen per line was **not established**.
* **What the item claims about time: nothing**, deliberately
  (`MEMORY-AND-TIMING.md` §2.1). It promises admission control,
  partitioned scheduling in `HardRt`, bounded sections on the real-time
  path and a preemptible kernel, as design, with stage 14 unbuilt.

### 5.2 What a game session gets

A **game domain**: a set of processors taken out of the `Throughput`
domain at run time, in `SoftRt` mode, for as long as one game is
fullscreen and focused.

* **Who may run there.** The game's job, and the threads of the present
  path that must wake on time: yserver's loop, hyprix's flip and input
  threads, `nvrm`'s dispatch and interrupt threads, the sound server's
  mixer and the mouse's driver. Nothing else: no stealing into or out of
  the domain. `docs/ARCHITECTURE.md` §5 says that of `HardRt` only, and
  `Throughput` steals work; for a `SoftRt` domain cut out of `Throughput`
  at run time it is something G1 must add and prove. It is what makes a
  statement about those processors possible.
* **How they are ordered.** The path's threads run in the fixed-priority
  class above the fair one: they wake, work for microseconds and block.
  Each is under a `cpu.max` budget (the mechanism exists), so that one of
  them looping costs its budget and not the game's frames. The game's own
  threads stay in the fair class among themselves. A closed program's
  threads are not made real-time: nobody knows what they do, a game engine
  expects ordinary scheduling between its workers, and a priority it did
  not ask for can invert its own locks.
* **Wake-up latency.** The claim is a measured bound for the path's
  threads on the game's processors while the `Throughput` side is
  saturated: M5, held by a boot test of stage 14's cyclictest shape with
  the bound in its text. Not a worst-case execution time, which the item
  says it does not have.
* **No migration** for the path's threads; the game's threads move only
  inside the domain.
* **Interrupts.** The card's and the mouse's lines are delivered on the
  game's processors, and every other device's elsewhere. This needs a
  line's destination to be choosable (§5.1's unknown) and is G3.
* **Memory.** While the session lasts, reclaim does not take the game's
  or the path's pages (a floor per job, cgroup v2's `memory.min`, which is
  not built), and `mlock` exists for programs that ask (Wine and
  PulseAudio do). Under memory pressure the load is reclaimed or killed
  first; the session ends rather than the system if nothing else is left.
* **Timers.** One-shot timers already fire at their time; M9 measures it.
  If a slack is found it is removed for the domain.

### 5.3 A first step with what exists

Before any domain: `sched_setaffinity` applied, the game's job given a
high `cpu.weight` and the load's a low one, and the path's processes
weights of their own. This is what GameMode does on Linux, costs little
(G0), and gives no guarantee. It is in the plan to learn, on the rig, how
much of M4 a heuristic already wins and what the domain must add.

### 5.4 Granting and revoking it

* **A program cannot ask for itself.** `sched_setscheduler` stays refused.
  The right to make a game domain is a handle a single system service
  holds, as `devmgr` holds devices: the *session service* (a small
  program init starts; whether it is a part of hyprix's session daemon or
  its own is a detail for the landing).
* **hyprix asks, for a window.** The conditions: the surface is
  fullscreen and focused; and a window rule names it, the user turned the
  session on for it, or it declared itself a game
  (`wp_content_type_v1`, which hyprix already reads). The service moves
  the client's job, which init or the launcher made, into the domain.
* **What stops a program from starving the system.**
  1. *The partition itself.* A game can use every cycle of its own
     processors and none of anyone else's.
  2. *A floor for the rest.* The service never gives a domain more than
     the machine less two processors, and refuses on a machine with fewer
     than four. The compositor's other work, a terminal and `sshd` always
     have somewhere to run.
  3. *Budgets* on every thread in the fixed-priority class.
  4. *One session at a time.*
  5. *It ends with its reason.* Losing focus or fullscreen, the game's
     exit, or the service's death each end it: the kernel dissolves the
     domain when the handle that made it is closed, so no failure of the
     service leaves processors fenced off. Ending is the same quiescent
     switch as starting (`docs/ARCHITECTURE.md` §5).
* **A key that always works.** The compositor's own shortcut to leave
  fullscreen is handled on hyprix's input thread, which is on the path and
  cannot be starved by the game.

### 5.5 How this differs from Linux

| | Linux | Ferrix's game session |
|---|---|---|
| Real-time priority | `SCHED_FIFO` to whoever has the capability or asks rtkit; a looping thread is stopped only by the global 95% throttle | only the path's threads, each under its own budget; never the game |
| Processors set aside | `isolcpus` and `nohz_full` at boot; cpuset partitions at run time, which no desktop sets up; kernel threads, timers and interrupts need separate care | made and dissolved per session by one service, with interrupts and timers part of the same switch |
| What is promised | nothing measurable on a desktop kernel; a bound only with PREEMPT_RT, which desktops do not ship (from memory, not re-read) | a wake-up bound in a boot test's text, and a refusal rather than an overcommitted set |
| GameMode | governor, niceness, I/O priority, optional pinning: heuristics [S] | §5.3 is its equal; §5.2 is what it cannot do |
| `scx_lavd` and its kin | better ordering by observed behaviour; no reservation [S] | no heuristic to tune per engine |

Linux can be brought close by hand. The claim is not that it cannot, but
that it does not by default and cannot say what it guarantees.

### 5.6 What the certified item allows

* **The scheduler is in the item's core ring** (`sched/**`, `arch/**`,
  `mm/**` in `tools/common/data/certification-item.json`). A domain made
  and dissolved at run time, a second class and applied affinity are
  structural: a design review by the certification consultant before code,
  requirement ids reserved first, a check and a negative control for each
  rule (the partition holds; a thread outside the domain never runs
  inside; a budget stops its thread; the domain dissolves with its
  handle).
* **No worst-case execution time is claimed**
  (`MEMORY-AND-TIMING.md` §2.1), and the text of every requirement written
  for this will say that its figure is measured under the stated load; that
  phrase is this design's, and no such requirement exists yet. The session is `SoftRt`, never
  described as `HardRt`: there is no admission test for a closed game.
* **The interrupt rule stands**: a handler's time is charged to whoever it
  cut (`MEMORY-AND-TIMING.md` §2.2). Placing the card's line on the game's
  processors makes the game pay for its own card, which is the fair
  direction; the storm bound is unchanged.
* **The session service and hyprix's half are ring 3**, outside the item.
  The handle that makes a domain is a new native interface, inside it.
* **Stage 14 is on the roadmap as a month.** This section is a slice of
  it (the `SoftRt` class, a run-time partition, threaded and placed
  interrupts) taken early for a use it was not written for, without EDF,
  CBS admission or priority-inheritance mutexes. Whether to open stage 14
  this way is question 3 of §10.

## 6. The structural deficits, and whether to attack them

### 6.1 The system call

**What the 357 ns is made of.** Not measured as a breakdown; put together
from two sources that were measured.

* *The defences are about 5 ns of it.* `docs/certification/SPECULATION.md`
  §8: two million `read` and `write` calls took 1.015 s with
  `--mitigations off` and 1.025 s with them on, on a Zen 5 host under KVM.
  On x86-64 an entry adds fourteen `xor`s and an `lfence`; AutoIBRS costs
  nothing per entry; `VERW` runs only on Intel parts with MDS; there is no
  KPTI. The same test puts a `read` or `write` at about 510 ns either way.
* *The rest is the path's software.* The channel work's stamps
  (`docs/OPAQUE-KERNEL.md` §9.10) give, for a native call that does not
  switch: `SYSCALL`, `SYSRET`, the stub and ring 3's loop 61 to 86 ns;
  the entry's bookkeeping (filter, vector mark, `call_entered`, the
  current thread and process) 58 to 62 ns; the exit (the frame, the look
  for pending work) 45 to 67 ns. That is 165 to 215 ns before a Linux call
  does anything, by summing rows of a table made for another purpose. The
  Linux personality then adds what `syscall/linux.rs` shows: a seam hook,
  two reference-counted lookups (`process::current`, `thread::current`),
  the signal table's match, the dispatch chain, and for `getppid` a pid
  namespace translation.
* *Two measurements disagree.* `softgl-gap.md` has 357 ns on an xtask KVM
  boot; `trip.md` has 150 ns on the card's guest the same night. The CPU
  model differs between the two boots. R4 measures it once on the rig's
  guest and names the difference.

For scale: the one published breakdown found has Linux at 355 ns for a
no-op call with its default mitigations on a Haswell, and 46 ns with them
off (Miemietz et al., arXiv 2112.10106 [S]). Linux's 53 ns on this host is
what a processor that needs no page-table isolation gives it.

**What a game calls at a high rate** (from knowledge of game loops and of
tonight's notes, not from a trace; stage 23's E0.5 will count it for one
game): `clock_gettime`, which is already the vDSO at 20 ns; futex waits
and wakes between the engine's threads, where Ferrix's round trip is
ahead (6.7 against 12.7 µs); one request to the driver a frame at an
uncapped rate (§6.2); `poll`, `recvmsg` and `writev` on the X socket, a
few a frame; `nanosleep` from a frame limiter. Some thousands of calls a
second. At 20,000 calls a second 300 ns more each is 6 ms a second: 0.6%
of one processor.

**Configuring defences per process or per deployment.**

* *Per deployment* it exists: `--mitigations off`, one build switch,
  outside the certified reference configuration
  (`docs/certification/SPECULATION.md` §1). It buys 1 to 3% on system-call
  and process-heavy work and nothing a game can feel.
* *Per process* it would be a new configuration space in the item's core,
  against §3's statement that the item "cannot know which of its programs
  run a JIT or a sandbox", for 5 ns a call. The speculation domain
  (`docs/OPAQUE-KERNEL.md` §9.2) already is the per-job form of the one
  defence that costs real time, the barrier at a switch between address
  spaces: **the game, yserver, hyprix and `nvrm` in one speculation domain
  would skip `IBPB` on every switch among them.** That is an existing
  mechanism with an existing security argument (programs in one domain do
  not distrust each other), and whether a game and the compositor belong
  in one is question 4 of §10. The recommendation is no for the game
  (closed code beside the compositor that sees every window) and yes for
  yserver, hyprix and `nvrm` among themselves, if the rig shows the
  barrier in M1's tail.
* *A switch for it* exists since 2026-10-11 as a place and not as an
  effect: Settings' game-mode page (`docs/SETTINGS.md`) keeps the user's
  list of games and the launcher marks what it starts from it. The mark
  changes no defence. How an opt-in that puts one game in `nvrm`'s domain
  could attach to it, who would have to grant it, and why the user's own
  list must not, is `docs/SETTINGS.md` §7: a proposal, not built, reviewed
  as a design (ledger line 698), which narrows question 4 to a choice of
  two and finds §5.4's move of the client's job in conflict with it.

**A vDSO or a shared ring to remove calls.** The clock is done. A ring
for socket traffic to the X server would save a few 300 ns calls a frame
and change nothing a player sees; the per-frame crossings that matter are
whole *switches* between programs, which the channel work and `nv-trip`
are cutting.

**Verdict: not in this plan.** Expected gain for a game under 1% of a
processor. The entry path's cost continues to fall under the channel
round trip's work, which owns it. This plan takes only the measurement
(in R4).

### 6.2 The crossing into a driver in ring 3

`nv-trip` owns it: a request was 75 to 130 µs against Linux's 110 to
450 ns, through four switches and a new thread per request (`trip.md`).

* **Expected gain for a game:** at 2,200 frames a second, most of the 38 µs
  a frame that Ferrix is behind (M8). At 60 frames a second, one request a
  frame is 6 ms a second, nothing; what matters there is that a request's
  time has no long tail, because the flip and the game's swap both pass
  through `nvrm`.
* **What this design needs from it:** no thread made and no memory
  allocated per request; a 99th percentile under 20 µs on the game's
  processors; `nvrm`'s dispatch and interrupt threads nameable, so that
  §5 can place them; and **the flip never queued behind a client's
  request**: the card's `displayctl` channel is already separate from the
  chardev's, and the flip must not take RM's API lock behind a client's
  long control call. Whether NVKMS's flip takes that lock was not read.
* **Verdict:** in the plan as a dependency, not as work of its own.

### 6.3 The compositor's processor time through `virgl_test_server`

hyprix is a static program and NVIDIA's libraries need glibc and
`dlopen`, so hyprix draws by sending a Gallium command stream over a
socket to `virgl_test_server`, which replays it on NVIDIA's EGL
(`docs/GPU.md` §3.3, `compositor-rate.md`). The WebGL page at 60 frames a
second costs 0.82 processors in the server, 0.17 in hyprix and 0.17 in
`nvrm` where sway costs 0.15, and a frame's round trip is 9 to 14 ms.

* **For a fullscreen game in the lane: nothing**, because hyprix does not
  draw. This is why the lane comes first.
* **For everything composited** (a windowed game, the desktop, a game
  under a notification): the round trip alone is most of a refresh, which
  makes §4.6 impossible and costs a refresh of latency.
* **What a native path would change.** A render process of Ferrix's own
  on glibc, speaking hyprix's own renderer interface over shared memory
  to NVIDIA's Vulkan: no command-stream translation, no per-frame fence
  made and exported, client buffers imported as images, the result
  flipped (§4.1). A composited frame should then cost what it costs sway:
  about a millisecond of GPU time and a few hundred microseconds of
  processor. An estimate; nothing of it is designed here.
* **Cost:** large, 34 points as a first guess, and it replaces a renderer
  that works and has three gates.
* **Verdict:** in the plan, late (S2), after the lane and the rig show how
  often a game is composited in practice. Question 7 of §10.

## 7. Windows games through Wine

Stage 23 leads to a DirectX 11 game through Wine and DXVK. Its page owns
that road; this section says what on the game path is particular to it.

### 7.1 Synchronisation: the NT primitives

Wine must give Windows programs NT's events, semaphores and mutexes with
"wait for any" and "wait for all". Through wineserver alone every wait is
a round trip to another process. Linux's answer since 6.14 is the
`ntsync` driver, which Wine 10.16 and 11 use [2]; its author's figures
against plain wineserver run from 1.2 to 7.8 times the frame rate [S, as
relayed], and no controlled comparison with the older futex-based way
(`futex_waitv`) was found. Stage 23 has "fsync or ntsync, if measurement
asks" as E3.5.

**Could Ferrix offer them natively and cheaper?**

* *Natively, yes, and the shape fits.* Ferrix's native ABI already has
  kernel objects, handles and waits on them (stage 9). NT's three objects
  and their two waits are a small, closed set with exact semantics.
* *But the interface must be Linux's.* The game needs a patched Proton
  for its anti-cheat (stage 23), and a Wine that speaks a Ferrix-only
  call is a Wine nobody else builds. So the plan is `/dev/ntsync` with
  Linux's ioctls, answered by the kernel's Linux personality (the load
  ring), with the objects implemented once underneath.
* *Cheaper: only in part.* An uncontended operation is one system call,
  where Ferrix is 100 to 300 ns behind Linux. A contended one is a wake
  and a switch, where Ferrix's futex round trip is ahead (6.7 against
  12.7 µs in the guest), and on the game's processors §5 bounds it. Games
  hand work between threads through these objects every frame, so the
  contended case is the one a frame feels. **Expected: level on
  throughput, ahead on the tail under load.** A prediction; W1 measures
  it.
* A risk the study recorded: ntsync was named in a February 2026
  anti-cheat failure of this very game under one Proton build
  (`~/.local/share/ferrix/endfield/STUDY.md`).

### 7.2 Shader compilation, and the latency tool

* **Stutter from compiling shaders** is NVIDIA's compiler and DXVK's
  threads inside the game's process, with caches on disk. Ferrix cannot
  make it faster. It can keep it from costing frames: DXVK lowers its
  compiler threads' priority, so per-thread niceness must be honoured
  (`setpriority` is in the table; whether it reaches a thread's weight
  was not read), and the cache's writes must not block the render thread
  (btrfs's write path, not examined here). On the game's processors the
  compilers compete only with the game's own threads.
* **Reflex.** A DirectX game that supports NVIDIA Reflex reaches it on
  Linux through DXVK and dxvk-nvapi over `VK_NV_low_latency2` in NVIDIA's
  own driver [2]. That is closed userspace talking to RM: it should work
  on Ferrix unchanged, and it has not been tried. If it does, such a game
  gets its just-in-time start from inside, and §4.5's hold stays off for
  it. W2 tries it.

### 7.3 Wine's X11 driver, or its Wayland driver

* **X11 first**, as stage 23 has it: yserver has the extensions, and it is
  the driver patched Proton builds use by default.
* **What the Wayland driver would save.** yserver leaves the path of both
  the frame and the mouse: one process and two socket hops fewer each
  way. On Linux, native Wayland was 3.1 ms faster than Xwayland in Nett's
  runs (§2.2); yserver is not Xwayland, and after §4.2 its share should
  be far smaller, so the gain here is to be measured, not assumed.
* **What it needs from hyprix:** `wp_linux_drm_syncobj_v1` (not among its
  globals), and `zwp_linux_dmabuf_v1` version 4's feedback, which NVIDIA's
  Wayland presentation reads (hyprix serves version 3; version 4 is an
  open backlog row). Relative pointers, pointer locks, the viewporter and
  fractional scale are there. This is P10.
* **Its state upstream:** Wine 11 still calls the driver experimental and
  Proton needs a switch for it [2]; one Proton release note says an
  anti-cheat of this game misbehaved "if wayland wasn't enabled"
  (the study). Both are reasons to build P10 after the X path works and
  to compare the two on the rig, not to choose now.

### 7.4 Where Ferrix could be better for them

The same three places as for any game, and one more: Wine's input and its
waits pass through *more* processes than a native game's (wineserver
beside the game), so the tail under load, which §5 bounds, is longer on
Linux for a Windows game than for a native one. That is an argument, not
a measurement.

## 8. The build plan

Points are first guesses in the roadmap's sizes, made before any of it
ran. "Consultant" means a review by the certification consultant before
it lands; "design" means a design review before code. Nothing here is
counted in another stage's points; where a landing is another session's,
the row says so and gives no points.

### 8.1 First: the rig

| # | Landing | Points | Proved by | Needs |
|---|---|---|---|---|
| R1 | The frame recorder: per-swap times and Present completions in the preload library, the summariser, both brought into the tree under `tools/common/` with the baseline's scripts | 5 | the same binary's record on both guests; negative control: a 50 ms sleep injected every ten seconds is counted exactly, as three missed refreshes each (at 60 Hz; at hyprix's 75 Hz 50 ms is 3.75 refreshes, three or four by the frame's own work) | nothing; no card. Built 2026-10-14 (§12) |
| R2 | The latency probe and the injector's log: input to scan-out on both systems | 5 | the probe on both; control: a 16.7 ms delay added in the probe moves the median by one refresh; a gate without the card (`test-xwindow`'s boot, counting *frames* from injection to the frame that shows it) | nothing; no card. **Built 2026-10-14.** The probe is `xlatency`, a static X client in the compositor's workspace speaking X11 itself (raw motion on the root, one frame in flight, `PresentPixmap` for the refresh after the last completion, each frame's colour the count of events it carries); the injector is `vptr` there too, logging each motion's `CLOCK_MONOTONIC` send time; `xlatency summarize` pairs the two. Linux: `tools/common/test/latency-sway.sh`, headless sway 1.10 with Xwayland at 60 Hz on nazuna (measured refresh 16.3 to 16.9 ms), 211 motions paired one to one: send to scan-out median 24.3 ms, arrival to scan-out 23.2 ms, 2 refreshes; with `--delay-us 16700` 40.9 and 40.8 ms, 3 refreshes. Ferrix: `cargo xtask test-xlatency`, `test-xwindow`'s boot, 40 QMP tablet motions each timed on the host to the probe's line for its frame, then `vptr` with the probe in the guest; on R1's yserver clock (2966c377; hyprix's virtual screen at 75 Hz, 13.4 to 14.7 ms measured while drawing): host-timed median 23.9 ms, arrival to scan-out 20.6 ms, 2 refreshes, in-guest send to scan-out 23.2 ms; with the delay 44.1, 37.3 and 38.0 ms, 3 refreshes. On the clock kept to one refresh (yserver d2bf56e2, which measures 13.334 ms; gates `yc-xlat1` and `yc-xlat2`): host-timed medians 21.8 and 22.5 ms, arrival to scan-out 20.4 and 20.3 ms, 2 refreshes, in-guest send to scan-out 19.5 and 19.2 ms. The delay moves each median by one refresh on both, in the probe's own count. The 99th percentiles (up to 400 ms) were taken with nazuna's load at 30 to 60 and are the host's, not the systems'. The pinned yserver (1a8752e8) panics on the first `PresentPixmap` (`get_semaphore_fd_khr` on lavapipe), which R1's yserver fixes: the gate passes from that pin on |
| R3 | The load script, the launch timer, the sleep and wake-up program, and both guests defined with pinned processors and an invariant TSC | 3 | the load holds every processor at 100% on both; the clock line says TSC. **Built 2026-10-14**: §12 | nothing |
| R4 | The baseline: M1 to M9 on both guests as they are, the system-call breakdown of §6.1, and the record written into §12 | 3 | the table, with its logs | the card (the product owner's word); the Linux guest; `nv-present` or its successor on the card |
| R5 | The software measure checked against light, once | 3, or 5 with a built device | the two agree within the device's error on both systems | question 6 of §10 |

### 8.2 Then, by gain for effort

| # | Landing | Points | Gain expected | Proved by | Needs |
|---|---|---|---|---|---|
| — | displayctl v9, flips of a driver's own dmabuf | another session's (`zerocopy.md`) | removes 9 to 15 ms and three copies for hyprix's own frame | its own Z1 to Z10 | `nv-vram-dmabuf` first |
| P1 | The flip's time: `FLIPPED` carries it, the DRM event reports it, hyprix's pace follows it (§4.1) | 8 | M1: up to a refresh of phase error gone; the base for everything timed | host tests of the protocol; a boot where the reported time is the driver's and not the answer's arrival (control: the kernel's own clock substituted); on the card, completions' `msc` advancing by one | v9; design, consultant (the display core's protocol and checks) |
| P2 | The game lane in hyprix: a fullscreen dmabuf surface flipped as it is, with the fallback (§4.2) | 8 | M1: hyprix's frame out of the path; M7: about 1.2 processors | `test-compositor --gl --boot` on virtio-gpu's render objects, which already take this road: a fullscreen client's frames reach the screen with hyprix drawing none (control: eligibility forced off, "drew N frames"); a second window makes it draw again; then on the card | v9, P1; the first card run decides whether NVKMS scans out a client's image |
| — | yserver hands an X client's pixmap on as a dmabuf | another session's (`present.md`) | the read-back's 2 ms and yserver's thread | §4.2's list of needs | `nv-present`, yserver's fork |
| — | A hardware cursor through NVKMS | stage 21's, second in the customer's order | the lane survives a visible pointer | `test-compositor --boot cursor`'s shape on the card | — |
| I1 | Input: time carried end to end, hyprix's input thread, no coalescing under a pointer lock (§4.7) | 5 | M1's tail; the mouse's lag becomes a number per hop | the probe's per-hop ages; a boot check that a motion is forwarded while a frame is held (control: the thread removed) | nothing; yserver's fork for its half |
| P6 | Tearing: the hint from Wayland and from Present to an NVKMS tearing flip (§4.4) | 5 | M2 exists | on the card, completions faster than the refresh with `allow_tearing`, none without; host tests of the flag | P2; consultant for the protocol flag |
| P7 | Just in time: the hold, in yserver and in hyprix, with its controller and its rule (§4.5) | 8 | **M1: about one refresh**, for any game | R2 on both systems, hold on and off; misses a minute no higher with it on; control: the margin set to zero must miss and back off | P1, P2, I1 |
| G0 | The session with what exists: affinity applied, weights for the game, the path and the rest (§5.3) | 8 | M4, as far as a heuristic goes | R1 under the load, on and off, against Linux's GameMode | design, consultant, ids reserved first (the scheduler applies affinity: core ring) |
| P5 | hyprix draws backwards from the deadline (§4.6) | 5 | M1 for composited clients: up to a refresh | a boot where a commit a few milliseconds before the deadline is in that frame (control: the old latch) | P1; worth little before S2 |

### 8.3 The game domain, which is stage 14's ground

| # | Landing | Points | Proved by | Needs |
|---|---|---|---|---|
| G1 | A domain made and dissolved at run time through a handle; the partition's rules | 21 | boot checks: a task outside never runs inside, the domain dissolves with its handle, a machine too small is refused; a control for each | question 3 of §10; design, consultant, ids reserved |
| G2 | The fixed-priority class in that domain with budgets, and the wake-up test | 13 | M5 held by a boot, load on the other processors; a looping thread stopped by its budget (control: the budget removed) | G1; design, consultant |
| G3 | Interrupts placed with the domain | 5 | the card's and the mouse's lines counted per processor | G1; whether a line's destination can be set |
| G4 | Memory under the session: a reclaim floor for a job, `mlock` | 8 | a game job's pages survive the load's pressure (control: the floor at zero) | design, consultant, ids reserved first (`mm/**` is core ring) |
| G5 | The session service, hyprix's request and the rule | 5 | `test-compositor` boot: fullscreen makes the domain, losing focus dissolves it; M4 on the card | G1 |

### 8.4 Later, each on a measurement

| # | Landing | Points | Built if | Needs |
|---|---|---|---|---|
| P8 | VRR through NVKMS | 8 | the display offers it | question 5; bare metal or the card |
| P10 | `wp_linux_drm_syncobj_v1` and dmabuf feedback version 4 in hyprix | 13 | a Wayland-native game or Wine's Wayland driver is wanted | P2 |
| I2 | The pointer tap (§4.7) | 8 | hyprix's hop is above 0.3 ms at the 99th percentile after I1 | design, consultant (a new way into the input core) |
| P9 | An overlay on a second NVKMS layer | 8 | games are seen leaving the lane for overlays | P2 |
| S2 | A native render process in place of `virgl_test_server` (§6.3) | 34 | composited games matter after the lane | question 7; a design of its own |
| W1 | `/dev/ntsync` | stage 23's E3.5, which has 8 for "fsync or ntsync"; ntsync alone is nearer 13 | stage 23 reaches it | consultant if it adds a check the evidence relies on |
| W2 | Reflex through dxvk-nvapi, tried | 2 | stage 23's step 3 has a game with it | the card |

**Sums.** The rig 19 to 21. The first cut that can be compared with
Linux (P1, P2, I1, P6, P7, G0, P5) 47. The game domain 52. Later 73 if
all of it is built. About 190 in all, of which about 68 before the
customer needs to decide on stage 14.

**What stands on unlanded GPU branches:** everything but R1 to R3 on the
card; P1 and P2 on displayctl v9, which stands on `nv-vram-dmabuf`; the X
game's lane on yserver's hand-over. P2, I1, P5 and G0 to G5 have gates
that need no card.

## 9. What cannot be won, and the risks

**Cannot be won.**

* **GPU-bound frames.** The same closed userspace draws them on the same
  card. 100% of Linux is the ceiling; 97% is the target.
* **Anything inside NVIDIA's libraries**: shader compilation, their
  internal queues and waits (`compositor-rate.md` found a 17 ms wait
  inside them that nobody can read), their driver's request count.
* **The display.** A television's processing can exceed everything in
  this document. It is the same for both systems and is not in any
  target; "game mode" on the set is the customer's to switch on.
* **The system call.** Ferrix will stay several times dearer per call
  than Linux on processors that need no page-table isolation, and this
  plan does not try to change it.
* **The driver in ring 3.** One more switch each way than Linux, for as
  long as `nvrm` is a process, which is the design.
* **Launch time.** Nothing structural favours Ferrix: process start,
  dynamic loading, the driver's start-up requests (hundreds, each through
  `nvrm`) and disk reads are all places where Linux is at least level.
  Parity is the target.
* **Breadth.** Linux's twenty years show in what this plan does not
  touch: every card and every monitor's quirks, HDR, multiple monitors
  with different rates, gamepads and wheels and their force feedback
  (Ferrix's input core has no `EV_FF`), per-game fixes in Proton and
  Mesa, hardware video overlays, power management. On one card, one
  display and one game Ferrix can be ahead; across the field it will not
  be for a long time.

**Risks.**

1. **NVKMS may not scan out a client's image in `nvrm`** as nvidia-drm
   does on Linux, or may copy it (§4.2). Then the lane needs an
   intermediate copy on the card: still far better than today, no longer
   level with Linux. Known after P2's first card run.
2. **The margin may not shrink.** §4.5's gain over a tuned Linux rests on
   §5's bound, which rests on stage 14's ground. Without G1 and G2 the
   hold still removes the queueing refresh but with Linux-sized margins.
3. **A guest hides and invents effects.** M4 and M5 in a guest say which
   system is ahead on this host, not what either does on a machine
   (§2.5).
4. **Timing the game from outside can hurt a game** that paces itself
   another way. That is why the hold is a rule per game, with a
   controller that gives up.
5. **A core-ring change for a game.** The domain touches the scheduler
   the certification rests on. The reviews are in the plan; the cost of a
   mistake there is higher than anywhere else in it.
6. **Three sessions on one path.** This design, `nv-trip` and the present
   work each change what a frame passes through. §4.2 and §6.2 list what
   this one needs; nothing has been agreed with them yet.
7. **Stage 23's anti-cheat** may refuse a guest, a custom kernel, ntsync
   or the Wayland driver, each of which this plan would otherwise use.

## 10. Decisions for the customer

Each with a recommendation. Question 1 is decided; none of the others is
taken.

1. **Does this become a roadmap stage, or stay a design with backlog
   rows?** *Recommended: a stage of its own after stage 21's zero-copy
   landings, "the game path", with the rig as its first step and M1 and
   M4 against stock Linux as its exit;* about 68 points before the game
   domain. **Decided by the customer on 2026-10-10: a roadmap stage.** It
   is stage 24, `docs/roadmap/stage-24-game-path.md`, as recommended: the
   rig first, M1 and M4 as the exit, G1 to G5 listed there as stage 14's
   ground and outside its 68 points until question 3 is decided. The
   status table and the SysML model's roadmap carry it.
2. **Which Linux is the opponent in the exit?** *Recommended: stock for
   the exit (Debian's sway and Xwayland, and Plasma once as the common
   desktop), tuned as a published bar the page reports against.*
3. **May the game domain open stage 14 early** (§5, 52 points in the
   item's core ring, without EDF or priority inheritance)?
   *Recommended: yes, after G0's measurement shows what a heuristic
   leaves on the table; and that G1 and G2 are written as stage 14's
   first slices, not as a side road.*
4. **Speculation domains for the present path** (§6.1): may yserver,
   hyprix and `nvrm` share one, and may a game join it? *Recommended:
   the three system programs yes, if the rig shows the barrier's cost in
   M1's tail; the game no.*
5. **VRR:** does the display on the card offer it, and is it wanted?
   *Recommended: find out with the display's own menu; build P8 only
   then.*
6. **Hardware for the check against light** (R5): a microcontroller and a
   photodiode (parts for a few tens of euros, an evening's build), or the
   Pixel 7's 240 frames-a-second camera, or neither. *Recommended: the
   phone first, since it is there; the device if the plan's figures are
   ever to be published.*
7. **A native render process in place of `virgl_test_server`** (S2, 34
   points). *Recommended: decide after P2, with the rig's count of how
   often a game is composited.*
8. **The session's defaults:** on for every fullscreen program that
   declares itself a game, or only by a rule the user writes; and how
   many processors a session may take. *Recommended: by rule at first,
   at most half the machine's processors, and always two left outside.*
9. **A bare-metal run** before any figure leaves the tree (§2.5).
   *Recommended: yes; it is stage 21's exit and this plan waits for it
   for M4 and M5 only.*
10. **Coordination:** who settles §4.2's and §6.2's needs with the
    present and `nv-trip` sessions. *Recommended: the product owner, by
    sending them those two lists.*

## 11. Verified, assumed, and not found

**Verified by reading the tree** (`main` d532a830f, and `nv-present` by
`git show`): everything in §3.1's "what happens" column; §4.8's third
column; §5.1 but for the two items it marks; the rings in §5.6; §6.1's
entry-path description and the figures quoted from
`docs/certification/SPECULATION.md` and `docs/OPAQUE-KERNEL.md`; the
protocols hyprix advertises; that `FLIPPED` carries no time.

**Read in NVIDIA's header** (`nvkms-kapi.h`, open-gpu-kernel-modules
580.173.02, on nazuna): §4.8's second column, as names of fields and
functions. What each does at run time was not tried.

**Taken from other sessions' notes of 2026-10-10, not re-measured:** every
figure in §2.1.

**Read by a research subagent on 2026-10-10** and relayed: §2.2 and the
marked statements elsewhere, with the marks it gave. Sources: Nett,
<https://marco-nett.de/blog/measuring-input-latency-on-linux-x11-vs-wayland-vrr-dxvk/>
(2026-07-13); farnoy.dev, <https://farnoy.dev/posts/linux-latency>
(2026-06-07); mort.coffee,
<https://mort.coffee/home/wayland-input-latency/> (2025-01-26); KWin
merge request 4293, <https://invent.kde.org/plasma/kwin/-/merge_requests/4293>;
gamescope's README, <https://github.com/ValveSoftware/gamescope>;
LatencyFleX's README, <https://github.com/ishitatsuyuki/LatencyFleX>;
Feral GameMode, <https://github.com/FeralInteractive/gamemode>; LWN on
`scx_lavd`, <https://lwn.net/Articles/1051430/> (2026-01-07); Microsoft
on the flip model, <https://devblogs.microsoft.com/directx/dxgi-flip-model/>
(2018-04-09), and on GPU scheduling,
<https://devblogs.microsoft.com/directx/hardware-accelerated-gpu-scheduling/>
(2020-06-30); Miemietz et al., arXiv 2112.10106 (2021-12-19); the ntsync
figures as relayed by GamingOnLinux,
<https://www.gamingonlinux.com/2024/01/proposed-windows-nt-sync-driver-brings-big-wine-proton-performance-improvements/>.

**Assumed, from memory or by inference:** §3.2's table as a whole; that
NVKMS scans out a client-allocated block-linear image; that a game
samples its input right after its swap returns; the estimate of 40 to
60 ms in §3.1 and every "expected" in §2.3; Linux's 50 µs timer slack;
what desktops ship of PREEMPT_RT and cpuset partitions; the list of calls
a game makes; that Reflex's path works on Ferrix unchanged; S2's shape
and cost.

**Looked for and not found:** an instrumented click-to-photon figure at
60 or 144 Hz on any Linux desktop; any gamescope latency figure; any
frame-time figure under background load for any Linux scheduler; a
controlled ntsync against fsync comparison; a published launch-time
figure; a per-frame system-call profile of a game; a system-call cost on
a recent processor with and without mitigations; a vblank callback for a
KAPI client; whether NVKMS's flip event carries a hardware time.

## 12. Where it stands

2026-10-10: this document, written in one session from the tree, the
night's notes and a web search. No code, no measurement, no card run.
It was written on branch `faster-path-design`. The customer decided the
same day that it is a roadmap stage (§10, question 1): stage 24,
`docs/roadmap/stage-24-game-path.md`, not started; questions 2 to 10 are
open. The certification consultant has seen the document and the stage only
as a docs landing, not as a design; §5 and P1 will need a design review
before anything is written. The present and `nv-trip` sessions have not seen
§4.2's and §6.2's lists. The next step that needs nobody's decision is
R1 to R3.

2026-10-11: §5.4's "the user turned the session on for it" has a place to
be written: Settings' game-mode page and the launcher's mark
(`docs/SETTINGS.md`). It is a list and a mark with no effect; G0 to G5 and
every question of §10 stand as they were, and `docs/SETTINGS.md` §8 adds
the decisions the switch raises.

2026-10-14: R1, the frame recorder, in `tools/common/game/` (`docs/LAYOUT.md`).
`framerec.c` is §2.4's library: the baseline's swap counter, its
`swap-fps` lines kept, with a record of every swap's monotonic entry and
return, written from a ring at exit, on SIGTERM or when full, and for a GLX
window the msc and ust of a `PresentNotifyMSC` (target 0) it sends after
each swap on an X connection of its own, `libxcb` and `libxcb-present`
opened at run time; the same connection selects `CompleteNotify`, so a
program's own `PresentPixmap` completions are recorded too. Its serials
carry the top bit, because the server sends every NotifyMSC completion to
every client that selected them on the window. It costs about 10 µs a
swap (Xvfb, an unpaced client: 8,480 frames a second without it, 7,700 to
7,930 with it). `framegl.c` is a GLX client paced to the msc: after each
swap it waits for the first later msc, as a driver with vsync on waits,
with at most one frame in flight; Xorg answers a NotifyMSC for now + 1 at
the next refresh, Xwayland one refresh early (its copies complete a frame
after they run), so it asks again for now + 2. `cargo xtask frames` builds
both with the host's `cc` and nothing else (reproducibly: the same SHA-256
each time) and summarises a record; `cargo xtask test-frames` is the gate
on Ferrix; `linux-side/measure-frames.sh` runs the same binaries on the
Linux guest.

Missed refreshes are counted two ways. By msc, as §2.4 defines them. By
time: from the refresh the server said the earlier swap fell in (its ust)
to the later swap's return, in refreshes, less one; the refresh is the
median ust step of swaps one msc apart. **On Xwayland, the Linux
baseline's server, the msc is not a clock**: it counts the compositor's
frame events while a presentation waits and stands still while none does
(`xwl_present_msc_bump`, Xwayland 24.1), so a program that stalls misses
no msc there. Measured, not argued: on the Linux guest the three 50 ms
stalls were 0, 0 and 0 by msc and 3, 3 and 3 by time. By msc §2.4's
definition undercounts on exactly the system Ferrix is compared with; M3
and M4 should be read by time on both, with msc as the cross-check where
the server's msc is a clock (Xorg, Xvfb, yserver).

The records, libframerec.so `c28129b3…` and framegl `68c9f69c…` on both:

| | Ferrix (`test-frames`, KVM, llvmpipe through yserver on hyprix; gate `r1-frames-clock3`) | Linux guest (Debian 13, Xwayland of a headless sway at 60 Hz, llvmpipe; no card) |
|---|---|---|
| Plain, 30 s: swaps | 1,909 | 1,844 |
| Frame time, median / 99th / 99.9th / worst | 15.68 / 17.13 / 18.29 / 18.70 ms | 16.22 / 19.26 / 24.90 / 25.44 ms |
| Refresh from the completions | 15.679 ms | 16.241 ms |
| Missed refreshes, by msc / by time | 0 / 0 | 0 / 2 |
| 50 ms stalls, missed by msc / by time | 3/3, 3/3, 3/3 at 13.342 ms | 0/3, 0/3, 0/3 at 16.322 ms |

Two earlier runs on Ferrix of the same binaries are kept for what they
show of yserver's clock: one outside the pool (stalls 4/4, 3/3, 4/4 at
13.376 ms; the plain run's refresh 15.297 ms with 41 misses by msc and 18
by time), and `r1-frames-clock2` (stalls 4/3, 3/3, 3/3 at 13.979 ms),
which failed a judge that wanted the two counts equal; the gate now wants
the msc count in the range the refresh allows and the time count within
one of it (`compositor/frames.rs` says why).

On Xvfb on the host, whose msc is a clock at 16.35 ms, the two stalls were
3/3 and 3/3. The Linux guest's own session could not serve: without the
card it shows on virtio-gpu, whose flips complete at once (frames every
5.5 ms, no refresh to count), so `frames.sh` with `HEADLESS=1` starts a
second sway on wlroots' headless backend for the run.

yserver's clock at pin 2966c377 ran at hyprix's 75 Hz when nothing drew
and followed hyprix's frame callbacks, about 16 ms apart, while something
did, so a paced client's refresh read 15.7 ms in one run and 13.3 to
14.0 ms in the next, and misses by msc could be the clock stepping between
the two. hyprix was not slow: it paces at its screen's 13.33 ms and draws
when a commit comes, and the callbacks came when yserver's commit came,
which was after the clock's msc by the readback and hand-over, so taking
their times as the msc's moved each msc that much later. From d2bf56e2 the
clock is a timer at the screen's refresh, as Xorg's `present_fake` is,
and hyprix's frames follow it: three runs of `test-frames` on 2026-10-14
measured 13.335 ms in all six records, and each of the nine stalls missed
3 refreshes by msc and by time. Before and after, three runs each, both
records of each (plain and stalled), the same binaries; an msc is the time
between the answers to two swaps in a row over the mscs they moved:

| | Before: yserver 2966c377 (gates `r1-frames-clock3`, `r1-frames-main`, `rf-frames`) | After: yserver d2bf56e2 (gates `yc-frames1` to `3`) |
|---|---|---|
| An msc one apart, median | 14.14 to 15.73 ms plain, 13.34 to 13.86 ms stalled | 13.335 ms in all six |
| An msc, 1st to 99th percentile | 12.21 to 19.39 ms | 13.334 to 13.335 ms |
| Longest msc less the shortest | 3.4 to 18.0 ms | 0.001 ms |
| The client's frame (swap to swap), median | 13.33 to 15.72 ms | 13.29 to 13.43 ms |
| The client's frame, 1st to 99th percentile | 10.90 to 20.01 ms | 10.47 to 16.83 ms |

The client's own frame still spreads by llvmpipe's and the loaded host's
time, which is the client's and no longer the clock's. `test-frames` now
fails when the longest msc is more than 1% of a refresh longer than the
shortest ("yserver's Present clock stepped"), and its control, the pin
set back to 2966c377 on that commit's volume, fires on it (gate
`yc2-frames-ctl`). The clock's frame-callback path is in yserver's
`WaylandLink`, which needs a live compositor, so this gate is its test;
the fork's own `wayland/clock.rs` tests hold the timer (one period to the
microsecond for an hour, a client woken late and stalled measuring one
refresh).
The figures above are under KVM on a loaded host and say nothing of M3
or M4 yet: that is R4, on the card. The baseline's Teeworlds scripts now
preload `libframerec.so` and were not run (they need the card). A
pipelined driver (NVIDIA, swap returning before the frame shows) has not
been recorded; its per-pair counts may move a miss to the next pair while
the sum stays, which R4 will see.

2026-10-14: R3 is built, in `tools/common/game/`. `rig.c` is one static program for both guests: the
load's writer and holder, a cyclictest-shaped timed sleep (`rig wake`, an
absolute `CLOCK_MONOTONIC` time every millisecond), each processor's busy
share from `/proc/stat`, and the launch timer (`rig launch`, which passes
the preload library `RIG_LAUNCH_FD` and takes back one line, `ctor <ns>
swap <ns>`: the interface R1's library is to implement). `load.sh` is the
load, `load-crate.rs` the crate it compiles as an rlib with the rustc
volume's compiler on both systems, `measure.sh` the check inside a guest.
`linux-3060.xml` is the Linux guest, and `nvidia.rs` writes ferrix-3060
with the same processor, topology (four cores of the second CCD, both
threads: host processors 6 to 9 and 18 to 21, QEMU on 10, 11, 22, 23) and
invtsc; a test holds the two equal. Neither domain is redefined in
libvirt: the card's guests need the customer.

Measured without the card, on nazuna under the gate pool's load:

| | Ferrix (`test-game-rig`, KVM, 4 processors, 8 GiB) | Linux guest (`linux-guest.sh`, KVM pinned, 8 processors, 16 GiB) |
|---|---|---|
| Clock line | `TSC, calibrated against the HPET at 4400.017 MHz` | `tsc` |
| Without invtsc (control) | `TSC, vouched for and rated by KVM's paravirtual clock`, the gate fails | `kvm-clock` |
| Busy under the load, least and mean, of the time the host gave | 97.5 to 97.9%, 99.3 to 99.4% (twice) | 99.5%, 99.8% (twice) |
| Without the compile loops (control) | about 22%, the gate fails | — |
| Holder | 80% of 8,093 MiB in use | 80% of 15,992 MiB |
| Timed 1 ms sleep, idle: median, 99th, max (µs) | 14.5 to 16.4, 28 to 133, 771 to 20,987 | 64 to 67, 1,331 to 1,928, 3,632 to 8,746 |
| The same under the load | 13.6 to 17.4, 892 to 3,977, 206,392 to 550,785 | 60.0 to 62.4, 79 to 3,004, 10,320 to 146,450 |
| Launch timer, a stand-in swapping 100 ms after its constructor | +1.8 and +101.8 ms; +2.0 and +102.0 ms | +0.3 and +100.4 ms |

The overshoot is printed, not judged: its bound is G2's. Linux's median is
its 50 µs timer slack; Ferrix has none. Both guests' tails under the load
reach 70 to 137 ms at the 99.99th percentile, so they are the
host's: the pinned host processors are not isolated, and the gate pool's
boots ran on them. Tails are compared only once the host keeps those
processors for the guest (`isolcpus` or a cgroup partition, the customer's
host). Two findings of the rig's own: a cold compiler started together with the writer and the holder made
no build in 50 s on Ferrix (`/proc/meminfo` counts no page cache, and the
holder takes what is free), so `load.sh` builds once alone first; and the
load runs as root on Ferrix's gate, whose initramfs has no ordinary user,
and as the image's user `ferrix` on Linux.

2026-10-14: R1 and R3 meet at the launch timer. `framerec.c` implements
the protocol `rig launch` defines: with `RIG_LAUNCH_FD` naming an open
pipe it takes `CLOCK_MONOTONIC` in a constructor and when the first swap
returns, writes `ctor <ns> swap <ns>` and closes the pipe; without it, or
when the number is no pipe, it writes nothing (the library still asks only
for `GLIBC_2.2.5`). `test-frames` starts each framegl run with `rig
launch`, so M6 on Ferrix is framegl under the real recorder, through
yserver on hyprix, with llvmpipe, from yserver's volume: cold is the first
run after the boot, warm the second. Gate `rf-frames` (KVM, commit
`7fa46130`):

| | Constructor | First swap returned |
|---|---|---|
| Cold (first launch after the boot) | +2.56 ms | +395.6 ms |
| Warm | +2.28 ms | +67.6 ms |

The constructor is the exec, the loader and glibc; the rest is the X
connection, GLX on llvmpipe and the first frame, so cold is mostly the
volume's libraries read for the first time. The control, the library
without its report (`rf-frames-ctl2`), fails the gate: the plain run
printed no `launch:` line with a first swap. `test-game-rig` keeps its
stand-in (+1.7 and +101.8 ms, `rf-rig`): its boot is the console's, with
no X server and no Mesa on the rustc volume. `measure.sh` times a real
program where a guest names one in `RIG_GAME`; the Linux guest's M6 is not
taken yet, and with it whether Ferrix is within §2.3's 10%.
