# The N150's display engine: a modesetting driver

Version 1, 2026-10-25, a design before code. Asked for by the customer on
2026-10-25: "a proper display driver: every connected output lit at its
EDID's preferred mode, mode changes, and hotplug". Branch
`docs/intel-display`. Status: **DESIGN, PAUSED** (the coordinator,
2026-10-25: paused until after the 2026-10-27 launch). The certification
consultant has **not** reviewed it: a review was asked for and then
withdrawn before a verdict. §11 says where to pick it up.

## 0. In one paragraph

The Intel driver (`src/user/system/native/drivers/gpu/intel`) already holds
the N150's GPU, 00:02.0, maps its register BAR, pins memory into its IOMMU
domain and writes its GGTT. Its `card0` today is a *copying card over the
plane the firmware lit* (`docs/INTEL-GPU.md` §4.3, `docs/DISPLAY.md` §8.1
and §9.4): one connector, the firmware's mode, smaller modes only by
scaling, hotplug on the lit port only, and an unrun second scanout under
`ferrix.intelgpu.modeset=on` (`docs/DESKTOP.md` §3.5). This design makes the
same driver **own the display engine**: every connected HDMI output lit at
its EDID's preferred mode on a pipe, a plane and two surfaces of the
driver's own, page flips that complete at the vertical blank they latched
in, real mode changes through `SET_MODE`, and hotplug that relights a port
on connect, disconnect or a changed EDID. It stays a ring-3 driver and a
copying card under the display core's existing rules: **kernel lines 0**,
no new kernel interface, no firmware blob.

## 1. What there is, and what is missing

| | Today (develop 67da990cc) | v1 |
|---|---|---|
| Outputs | the one pipe the firmware lit; a second only under `modeset=on`, never run on the box | both combo PHY ports (A and B), each its own pipe, scanout and connector |
| Mode | the firmware's; smaller sizes scaled into it | the EDID's preferred mode at boot or plug; any listed mode by `SET_MODE` |
| Surface | the firmware's framebuffer (the lent BAR 2 window) | two surfaces per output, driver memory pinned into 00:02.0's domain, in the GGTT |
| Flip | copy into the scanned surface inside pipe A's blank (`display/vblank.rs`), `FLIPPED` after the copy | copy into the back surface, `PLANE_SURF` written, `FLIPPED` once the plane latched it (`PLANE_SURFLIVE`) |
| Hotplug | the lit port's `SDEISR` bit, polled; a replug shown only if the pipe still runs | both ports' live state and their latched pulses (`SHOTPLUG_CTL_DDI`); EDID re-read on every pulse; relight on any change |
| EDID | GMBUS, at publish and on plug (3g) | the same, plus a cheap check of block 0's checksum every 2 s |
| Needs | `ferrix.efifb` (K4's lent window) | nothing beyond `ferrix.intelgpu` (§6.3) |

The box (`docs/N150.md`, `~/.local/share/ferrix/n150-linux/evidence/3h-dump-2026-10-23`):
Alder Lake-N, display version 13 (XE_LPD), PCH of the ICP family's south
display. Linux 7.0's i915 names four connectors: `HDMI-A-1` (DDI B, combo
PHY B), `HDMI-A-2` (DDI A, combo PHY A), and `DP-1`, `DP-2`, both always
disconnected there, which the VBT puts on Type-C ports. Its only monitor
now is a JetKVM (EDID "DELL IDRAC", 1024x768), which can emulate any EDID
and resolution and so stands in for every monitor in §8.

## 2. Scope

**v1, the customer's list:**

* take over from the firmware's framebuffer without a black screen where
  the firmware's mode is the one wanted (§4.1);
* EDID over GMBUS (DDC) for HDMI and DVI sinks;
* every connected HDMI or DVI output on a combo PHY port lit at its
  preferred mode, one primary plane each, linear XRGB8888;
* page flips that complete with the vertical blank (§5);
* mode changes by `SET_MODE` to any mode the driver listed (§4.3);
* hotplug by the port's live state and latched pulses, relighting on
  connect, disconnect or an EDID change (§4.4).

**Left out, each a follow-up (§10):** DisplayPort (the box's DP ports are
Type-C: TC power and the FIA, the Dekel PHY and its PLL, the AUX channel,
link training), power gating beyond what lighting needs (no DC states, no
DMC), PSR, DSC, more than one plane, the cursor plane, HDR and wide
colour, HDMI 2.0 scrambling (so TMDS clocks above 340 MHz), audio and
infoframes, a CDCLK change, display core init when the firmware left the
display off, VBT parsing, display interrupts, and scanning a client's
buffer out with no copy.

## 3. Where it lives

The same split as the GT side and as rtl8169 and yt6801: the facts and the
decisions in a pure, host-tested library, the register accesses in the
process.

* `src/lib/drivers/gpu/intel-xelp/src/display/` (pure, no I/O):
  * `state.rs` **(new)**: the display as read -- per pipe A to C: the
    transcoder on or off, its function (port, HDMI/DVI/DP), its timings,
    the source size, the primary plane's control, stride, size and
    surface, the PLL its port clocks from; and the globals: MBUS joined or
    not, DBUF slices powered, DC states, CDCLK (from `CDCLK_CTL`), the
    reference clock (`SKL_DSSM`), each combo PHY's readiness, IPC on or
    off. Read through `io::Mmio`, written nowhere.
  * `plan.rs` **(new)**: from the state and the outputs wanted (port,
    mode, HDMI or DVI) to a target: which pipe each port gets (a running
    one keeps its pipe), which combo PLL, the DBUF slices by Alder Lake-P's
    table (pipe A S1+S2, pipe B S3+S4, MBUS never joined by this driver),
    each plane's DDB share, and whether the mode is possible at all (§4.5).
  * `wm.rs` **(new)**: the plane watermarks from the memory latencies the
    PCU reports, the plane's width and format and the mode: levels 0 to 5,
    the SAGV level, the transition level, the line time.
  * `light.rs` (exists, generalised): the enable steps for any pipe on any
    combo port at any planned mode, the disable steps, the plane update
    that a flip and an adoption are, and the MBUS unjoin (§4.2).
  * `modeset.rs`, `gmbus.rs`, `hotplug.rs` (exist): the PLL dividers and
    timing words, the EDID read, the ports' bits; `hotplug.rs` gains the
    latched pulses.
  * `model.rs` **(new, tests only)**: a register model of the display
    engine (§8.1).
* `src/user/system/native/drivers/gpu/intel/src/display/` (the process):
  `kms.rs` **(new)**, the card that owns the engine: publish, the
  scanouts, flips, `SET_MODE`, the hotplug poll; `surface.rs` (exists,
  generalised to a pair per output, sized to the mode); `regs.rs` (exists).
  The lit-plane card of today stays in `display.rs` for
  `ferrix.intelgpu.modeset=off` (§6.2).

**Code and its sources.** The new modules are written for Ferrix against
the register facts: offsets, fields and the order of each sequence, read
from Linux v7.0's `drivers/gpu/drm/i915/display` (`intel_display.c`'s
`hsw_crtc_enable`/`hsw_crtc_disable`, `intel_ddi.c`, `intel_dpll_mgr.c`,
`skl_watermark.c`, `skl_universal_plane.c`, `intel_hotplug_irq.c`,
`intel_gmbus.c`, the `*_regs.h` headers) and from Intel's public PRMs for
Alder Lake (volume 2c, registers; volume 12, display engine). Each constant
and step names the i915 function or register it was read from; **no code
was taken from i915** for them. The modules already in the crate that
were translated under i915's MIT notice (`light.rs`, `modeset.rs`,
`gmbus.rs`, `hotplug.rs`, `INTEL-GPU.md` §3.6) keep their headers; what
this work adds to them is marked as written here.

## 4. The modeset

### 4.1 Taking over from the firmware

At publish the card reads the state (§3) and each port's sink and EDID. For
each pipe the firmware lit:

* **Its mode is the port's preferred one** (the usual case: the GOP lights
  a monitor at its preferred mode). The pipe is *adopted*: nothing is
  written at publish, and the firmware's picture -- the boot console --
  stays up. At the first `FLUSH` the frame is copied into the driver's
  surface and the plane is pointed at it in one plane update (`PLANE_CTL`,
  `PLANE_STRIDE`, `PLANE_SIZE`, the plane's DDB share and watermarks
  recomputed for the driver's plane, `PLANE_SURF` last, which arms the
  others at the next blank). The screen goes from the boot console to the
  desktop with no black frame. The firmware's own framebuffer cannot be
  read (BAR 2 is not lent without `ferrix.efifb`), which is why the switch
  waits for the first frame rather than copying the console.
* **Its mode is not the preferred one** (a monitor whose EDID the GOP did
  not follow, or the JetKVM's EDID changed before boot): the pipe is lit
  again at the preferred mode when the core's `SET_MODE` or first
  `SCANOUT` asks for it -- the disable steps, then the enable steps. The
  screen is black from the disable until the first frame, about one
  modeset (tens of milliseconds) plus the compositor's first frame.
* **A port the firmware left dark with a sink on it** is lit the same way
  when the core first shows a buffer on it.

A pipe a previous driver lit (a restart, `docs/INTEL-GPU.md` §11) reads the
same as one the firmware lit and is handled the same way; its surfaces are
gone (the dead driver's pins are quarantined until this HELLO, then given
back), so the plane shows refused DMA, black, until the first frame.

The GGTT range the firmware's lit plane scans out of at publish is never
written, now or later: it backs K4's lent window and the panic screen
(§6.4). Surfaces go elsewhere below the driver's own window at 256 MiB,
reclaiming firmware entries that no plane scans, as `light::place` does
today.

### 4.2 Lighting and turning off a pipe

**Enable**, in `hsw_crtc_enable`'s order, as `light::steps` takes it today
for pipe B and generalised to pipe A or B: the pipe's power wells (PW2, and
PW_B for pipe B) with their fuse distribution; its DBUF slices powered;
`PIPE_MBUS_DBOX_CTL`; the combo PLL (power, `DPLL_CFGCR0/1` from
`icl_calc_wrpll`'s dividers, enable, lock); the port's clock select and
ungate in `DPCLKA_CFGCR0`; the port's I/O power well; `TRANS_CLK_SEL`; the
pipe source, misc, timings, `TRANS_MULT`, the frame-start delay,
`TRANSCONF` off; `WM_LINETIME`; `PIPE_CHICKEN`; the plane's DDB share and
watermarks; `TRANS_DDI_FUNC_CTL` (HDMI, or DVI for a sink with no HDMI
vendor block); the transcoder on and its state; the PHY's HDMI signal
levels, lanes up, `DDI_BUF_CTL` on and not idle; the plane last. Every wait
is bounded; a wait that does not end stops the sequence where it is, says
which, and leaves the scanout disconnected.

**Disable**, in `hsw_crtc_disable`'s order: the plane off (`PLANE_CTL` 0,
`PLANE_SURF` 0 to arm it) and one blank waited; the transcoder off and its
state clear; `TRANS_DDI_FUNC_CTL` off; `DDI_BUF_CTL` off and idle;
`TRANS_CLK_SEL` none; the port's clock gated; the port's I/O well released;
the PLL off and unlocked, then its power off. The pipe's own power well and
DBUF slices are left on (power gating is a follow-up).

**MBUS joined by the firmware.** With one pipe lit, a GOP may join MBUS and
give that pipe all four DBUF slices. Adopting it is fine. Lighting a second
pipe then needs MBUS unjoined, which i915 does without blanking the first
(`intel_dbuf_mbus_post_ddb_update`): the first pipe's plane gets a DDB
share inside its own two slices with watermarks for it, armed and latched
at a blank; then `MBUS_CTL` takes hashing 2x2, join off and no pipe
selected, the pipe's `MBUS_DBOX_CTL` takes the unjoined credits, and one
more blank is waited before the second pipe is lit. The driver never joins
MBUS itself.

**Display C-states.** A modeset needs DC5/DC6 off. If the firmware left
them on (`DC_STATE_EN`), the driver turns them off before its first write,
as i915 does before any modeset, and leaves them off (no DMC, §10).

### 4.3 Modes and `SET_MODE`

Each connected scanout's `MODE_LIST` is the EDID's modes
(`ferrix_display_modes::edid_modes`, as today) that the driver can drive
(§4.5), the preferred one flagged; at most 64, in Linux's order. Scaled
modes are no longer listed: every listed mode is a real one. `SET_MODE`
names one of them:

* the mode the pipe runs now: nothing is written (the core sends one after
  every new list, and the compositor's first `SETCRTC` asks for the mode
  the driver already lit);
* another mode: the pipe is turned off and lit again at it (§4.2), onto a
  new pair of surfaces sized for it; the old pair is given back once the
  new mode shows. The screen is black for about one modeset;
* a mode the driver cannot light after all (a wait that did not end): the
  pipe stays off, and the `FLUSH` that follows is answered `FLIPPED`
  `DeviceRefused`, so `SETCRTC` fails `EIO` as the protocol says
  (`docs/DISPLAY.md` §9.2).

### 4.4 Hotplug and EDID changes

Every 500 ms (as today) the card reads, for both combo ports, the live bit
in `SDEISR` and the latched long and short pulse bits in
`SHOTPLUG_CTL_DDI`, which it clears by writing them back. A pulse is what a
monitor -- or the JetKVM -- gives when it changes its EDID without the
cable moving. Then, after the 100 ms debounce of a live change, or at once
for a pulse:

* **disconnected**: the pipe is turned off (§4.2), and the core is told as
  `nvos_display_changed` tells it for NVIDIA: `MODES` with the scanout
  disabled;
* **connected, or a pulse on a connected port**: the EDID is read again.
  A sink that is new, or whose EDID differs from the one the scanout was
  lit for, has its EDID, its `MODE_LIST` and `MODES` (enabled, at the new
  preferred mode) sent, in that order, as nvrm does
  (`ccfad5570`). The pipe is lit at the preferred mode when the core asks
  (§4.1), which hyprix does at once for a screen added or changed. An EDID
  equal to the one before changes nothing.

An EDID changed with neither a pulse nor a cable moving is caught by
reading block 0's checksum byte (offset 127, one byte over GMBUS, about
half a millisecond of the serving loop) on each connected port every 2 s,
and reading the whole EDID only when it differs. A whole EDID read (256
bytes at 100 kHz, about 25 ms) never runs on a timer, because the same
loop serves the render node.

### 4.5 What a mode must meet

A mode is listed and lit only if all of these hold; each refusal is said
once with its reason:

* a TMDS clock at most 340 MHz (no scrambling, §2): 1920x1200@60,
  2560x1440@60 and 3840x2160@30 fit, 3840x2160@60 does not;
* a pixel rate at most twice the CDCLK the firmware set (display 10 and
  later process two pixels a CDCLK; the box's Linux ran 192 MHz, so 384
  MHz), since the driver does not change CDCLK;
* dividers `icl_calc_wrpll` finds for the clock;
* the plane's level 0 watermark inside its DDB share;
* a surface pair that fits the GGTT range below the driver's window
  (§4.1) and the iGPU's pin budget.

## 5. Flips and the vertical blank

Each lit scanout has two surfaces, front (scanned) and back, each the
mode's size at a stride of the width times four rounded up to 64 bytes,
pinned into 00:02.0's domain and mapped write-back in the driver
(`surface.rs`), every written line flushed from the caches (`clflush`) as
today, since the display engine does not snoop.

* `FLUSH` of the shown buffer: the damage, and the damage of the frame
  before it (the back surface is a frame behind), is copied into the back
  surface, clipped to the mode; the plane's `PLANE_SURF` is written with
  the back surface's GGTT offset; the two swap. Nothing waits: the
  hardware latches `PLANE_SURF` at the next vertical blank.
* The flip completes when `PLANE_SURFLIVE` reads the new offset. The card
  asks the serving loop to wake it just after the blank the beam is headed
  for (`PIPEDSL` and `TRANS_VTOTAL`, as `vblank.rs` reads them today) and
  answers `FLIPPED` then; the display core turns that into the DRM page
  flip event. A frame that did not latch by then is looked at again 1 ms
  later.
* A second `FLUSH` before the first latched: its copy waits for the
  latch, so no surface is written while it is scanned, and it is flipped
  at the next blank. Two flips never complete in one refresh.
* No interrupt is used: the MSI belongs to the GT, and enabling display
  interrupts behind it would need the GT's handler to take them. The
  polling costs a register read or two per flip. Display interrupts are a
  follow-up.

The tally `vblank.rs` says every 10 s (flips, waits, gaps over 50 and 66
ms) is kept, per scanout.

## 6. What changes for the rest of the system

### 6.1 The display core and hyprix

Nothing changes in either. `card0` says HELLO with **two scanouts**, one
per combo port, scanout 0 for port A and scanout 1 for port B, whatever
the firmware lit, so a connector's name stays the same across boots
(`HDMI-A-1` is port A, `HDMI-A-2` port B; Linux numbers them by the VBT's
order instead, which this driver does not read). Each is an `HDMI-A`
connector, enabled where a sink is there. `copies` stays set: the core
already lets this node be a copying card because the chardev core serves
its render node (`interfaces/display/mod.rs`, `may_copy`), and under
`ferrix.efifb` by K4's identity rule as well. The messages are the ones
every card uses: `EDID`, `MODE_LIST`, `MODES`, `SET_MODE`, `FLUSH` and
`FLIPPED`. hyprix's runtime screens (`docs/DESKTOP.md` §3a) and its mode
rules (`docs/DISPLAY.md` §9.5) work as they do on virtio-gpu and nvrm.

### 6.2 The command line

`ferrix.intelgpu.modeset` takes a new meaning (`docs/RUNTIME-SETTINGS.md`):

| Value | What |
|---|---|
| `on` | the card of this design |
| `dry` | the lit-plane card of today, and the plan this design would carry out printed, nothing written: the box's first run |
| `off` | the lit-plane card of today |
| absent | `off` until the box runs of §8.2 pass, then `on`, in the landing that says so |

The second-scanout path that `modeset=on` gives today (never run on the
box) is replaced by this design.

### 6.3 `ferrix.efifb`

Not needed any more. Under `ferrix.efifb ferrix.intelgpu` the kernel still
lends the firmware's framebuffer to 00:02.0 (K4); this card does not map
it and does not need the window. Under `ferrix.intelgpu` alone the card is
published because the render node is (§6.1). Without a render node and
without the lent window (K3 refusing the node while 00:02.0's DMA is
untranslated, and no `ferrix.efifb`) no copying card is allowed and no
`card0` is published, as today.

### 6.4 The kernel: nothing, and what it does not claim

**Kernel lines: 0.** Every register written is in BAR 0, which the driver
maps already. The surfaces are the driver's own VMOs pinned into its own
domain through the calls the second surface uses today, charged to the
iGPU's pin budget. The display core's messages exist since displayctl v11.
Nothing in `src/kernel`, the item or a requirement changes.

What the kernel's behaviour now means, said here and in `DISPLAY.md` §8.1:

* **The panic screen.** The kernel draws a panic only into the firmware's
  framebuffer. Once this card has pointed a plane at its own surface,
  that framebuffer is no longer what any plane shows, so a panic is not
  seen on the screen; serial and pstore carry it as always. This is the
  DK1's case (`DISPLAY.md` §2.4, "Real hardware with one display
  controller") and is already what K4's paragraph says the kernel does not
  claim under `ferrix.intelgpu`. The GGTT entries behind the firmware's
  framebuffer are never written (§4.1), so the kernel's window keeps
  writing the pages it was given, never a page of the driver's.
* **The boot console** stops at READY, as for every card
  (`console::screen::stop`).
* **DMA.** The display engine reads only GGTT-mapped pages: the driver's
  pinned surfaces, which its domain maps, and, until the first flip, the
  firmware's framebuffer in stolen memory, which K2's identity domain
  covers (`docs/INTEL-GPU.md` §4.4). Nothing here changes what the IOMMU
  allows. The DMA gate (`docs/DMA-GATE.md`) already admits 00:02.0 only
  translated and remapped.
* **A driver's death.** devmgr's quiesce turns bus mastering off and the
  planes stop fetching; the next driver adopts the pipes (§4.1). As today.

## 7. Failure, said

Every step is said at debug level and every refusal at warning level, as
the modeset lines are today (`intel: modeset: ...`). A sequence that stops
leaves the scanout disconnected and is tried again at the next plug or
`SET_MODE`, never in a loop. The plan is checked against the state before
anything is written (§4.5, `light::preflight` generalised): a pipe or port
in a state the plan does not expect, an MBUS join the driver cannot undo, a
combo PHY the firmware never initialised, or no free PLL writes nothing
and says what was found.

## 8. Tests

### 8.1 On the host: a model of the display engine

QEMU has no Intel display engine, so, as rtl8169's chip model does for its
rings, `display/model.rs` models the registers this driver touches and the
hardware's answers: power well requests and their acks and fuse bits, DBUF
slice power, PLL power, enable and lock, the transcoder's state, the DDI
buffer's idle bit, a scanline that moves and a frame counter, `PLANE_SURF`
latched into `PLANE_SURFLIVE` at the blank, `SDEISR` and the latched pulse
bits for a sink plugged and pulled, GMBUS answering with an EDID per port,
and the PCU mailbox answering the memory latencies and SAGV block time.
It also checks rules, so a test fails on a wrong order rather than a wrong
number: a transcoder enabled with its PLL unlocked or its port clock gated,
a plane enabled with no DDB share or a level 0 watermark past it, a DDB
share outside the pipe's slices or overlapping another pipe's, a PLL turned
off while a port clocks from it, a write into the firmware's scanned GGTT
range.

The tests start from the box's firmware state (one pipe, DVI at 1024x768,
as the JetKVM now) and from Linux's dump (two pipes, both HDMI ports), and
run: adoption with no write at publish and the switch at the first flip; a
second monitor plugged in; MBUS unjoined under a running pipe; a mode
change and back; disconnect and reconnect; an EDID changed by a pulse and
by its checksum alone; a flip latched and a flip that waits for the one
before; every refusal of §4.5. The planner's numbers are held to the dump:
the PLL words, the timings, `TRANS_DDI_FUNC_CTL`, and the watermarks and
DDB shares of both of Linux's pipes, computed from the latencies the dump
implies.

### 8.2 On the box, with the JetKVM

Under the box rules (`BOX-HOLDER`, the JetKVM's one session, deadlines of
about twice the normal time), each with a JetKVM screenshot and the
driver's lines kept in `~/.local/share/ferrix/n150-linux/evidence/`:

1. `modeset=dry`: the plan printed against the real state, nothing
   written; compared with what the model expects;
2. `modeset=on`, the JetKVM's own EDID (1024x768): adoption, the switch at
   hyprix's first frame with no black frame, flips at the refresh with no
   tear, the tally;
3. the JetKVM set to a 1920x1080 EDID before boot: a modeset at publish to
   1080p;
4. the EDID changed while the desktop runs (1080p, then 1280x720, then
   back): a relight each time, hyprix following;
5. a hotplug simulated by the JetKVM (its HPD off and on): disconnect,
   reconnect, the desktop back where it was;
6. `hyprctl keyword monitor` to another listed mode and back;
7. both HDMI ports, if a second monitor is at the box (the customer's to
   say).

### 8.3 Gates

Each landing runs `cargo xtask check` (host tests, clippy, the docs). The
landings that change the driver also run `test-intel-uapi` and
`test-restart --boot intel`, which boot the driver against `intel-fake`
and must show the render node unchanged and no display card refusing to
start (the fake has no display engine: the card says so and the render
node is served alone, as today).

## 9. Points and order

| Slice | What | Points |
|---|---|---|
| D0 | this design, the consultant's review | 1 |
| D1 | the library: `state`, `plan`, `wm`, `light` generalised (enable, disable, plane update, MBUS unjoin), `hotplug`'s pulses, the model and its tests. No runtime change | 8 |
| D2 | the card: `kms.rs`, the surface pairs, adoption, HELLO with two scanouts, the mode lists, `SET_MODE`, flips by `PLANE_SURF` and `SURFLIVE`, behind `modeset=on`; `dry` prints the plan | 6 |
| D3 | hotplug: pulses, relight on connect, disconnect and EDID change, the checksum watch | 3 |
| D4 | the box runs of §8.2; `modeset` on by default once they pass | 3 |
| | **v1** | **21** |

D1 lands alone (a library and its tests). D2 and D3 land behind the word,
off by default, after `dry` has run on the box. D4 turns it on.

## 10. Follow-ups, not in v1

* **DisplayPort.** On this box only on the Type-C ports: TC cold power and
  the FIA's lane ownership (`intel_tc.c`), the Dekel PHY and its PLL or the
  Thunderbolt PLL, the AUX channel (`intel_dp_aux.c`) for the DPCD and the
  EDID, and link training (`intel_dp_link_training.c`). About 15 points,
  and only testable with a DisplayPort sink and cable at the box: the
  JetKVM is HDMI only.
* **Power**: DC5/DC6 and the DMC firmware (`adlp_dmc.bin`, under Intel's
  linux-firmware licence: never shipped without the customer's yes), power
  wells and DBUF slices turned off with the last pipe, IPC.
* **PSR, DSC, HDR, more planes, the cursor plane, scaling by the pipe
  scaler.**
* **HDMI 2.0**: scrambling and the TMDS clock above 340 MHz (4K60), AVI and
  other infoframes, audio.
* **CDCLK** raised for a mode that needs it.
* **Display core init** when the firmware left the display off (a boot
  with no monitor): the PCH handshake, PW1, CDCLK's PLL, DBUF, MBUS and the
  combo PHYs.
* **Display interrupts** for vblank and hotplug, once the MSI is shared
  between the GT and the display.
* **The VBT**, for Linux's connector names and the PHY's signal levels
  rather than the dump's.
* **Scanning out a client's buffer with no copy**: the plane pointed at the
  dmabuf's own pages, which needs the display core to hand a card that
  pins rather than copies a VMO it can pin -- a kernel change, for its own
  review.

## 11. Where it stands, and the next steps (2026-10-25, paused)

Nothing has been built. The box was not taken, and no JetKVM session was
opened. The design's facts were read from Linux v7.0's i915 in
`~/.local/share/ferrix/intel-gpu-ref/linux/drivers/gpu/drm/i915/display`
and from the box's Linux dump (`3h-dump-2026-10-23`).

**Next, in order:**

1. Rebase onto `develop`. Others are working on the same driver:
   `da674893d` (copies inside pipe A's vertical blank, `display/vblank.rs`)
   and `6045d159c` (ioctlstat) landed after the 3f-3h work. Check what
   changed in `display.rs`, `display/` and `serve.rs` first.
2. Send this design to a `certification-consultant` subagent for its design
   review (D0). The questions to ask: kernel lines 0; the panic-screen
   statement of §6.4 (a docs line in `DISPLAY.md` §8.1, or an AoU or
   `VULNERABILITY-ANALYSIS` note as well?); DMA under K2 (ledger 2388); the
   new meaning of `ferrix.intelgpu.modeset` (`build-flags.toml`, the
   profiles); and anything else the item touches. Fold its verdict into a
   new §9a.
3. D1, the library (§3, §9). Facts already gathered for it:
   * **DBUF**: XE_LPD has 4096 blocks in 4 slices of 1024. Alder Lake-P's
     table (`adlp_allowed_dbufs`) gives pipe A S1+S2 and pipe B S3+S4 when
     MBUS is not joined, and a lone pipe A or B all four slices when it
     is. `PLANE_BUF_CFG` start and end are relative to the MBUS offset of
     the pipe's slice pair (`mbus_ddb_offset`), and the end is written
     less 1. Linux's dump: pipe A plane 0..2025 and pipe B plane 0..2011,
     with the cursor holding the rest (`skl_cursor_allocation`, at least 32
     blocks with one pipe active and 8 with more).
   * **Watermarks** (`skl_compute_plane_wm`, `skl_compute_wm_params`): a
     linear plane's blocks a line are `ceil(width*cpp/512) + 1`. Method 1
     is `latency*rate*cpp/(1000*512) + 1`, used while the latency is under
     a line's time; method 2 is `ceil(latency*rate/(htotal*1000)) *
     blocks_per_line`. The result is rounded up plus 1, at least one
     line's blocks, and monotonic over the levels. `min_ddb_alloc` is
     `blocks + ceil(blocks/10) + 1`, and at most 255 lines are allowed on
     display 13. The levels are 0 to 5 (`HAS_HW_SAGV_WM`), plus the SAGV
     level at `PLANE_WM_SAGV` 0x70258 and its transition level at 0x7025c,
     and `PLANE_WM_TRANS` when IPC is on (trans_min 4, amount 10). The
     register layout is enable bit 31, lines in bits 26:14 and blocks in
     bits 12:0. Latencies come from the PCU (`GEN9_PCODE_READ_MEM_LATENCY`,
     sets 0 and 1), then sanitised and made monotonic, with 3 us added to
     every level when level 0 reads 0 (`adjust_wm_latency`). The SAGV
     block time also comes from the PCU. A level whose latency does not
     fit the vertical blank is disabled (`skl_wm_check_vblank`). The dump
     checks the formula: pipe B 1080p, WM0 16 blocks and 1 line, WM1 65
     blocks and 4 lines (method 2).
   * **MBUS** (`pipe_mbus_dbox_ctl`, `mbus_ctl_join_update`,
     `intel_dbuf_mbus_post_ddb_update`). The ADL-P credits are A 4
     unjoined and 6 joined (Wa_22010947358), BW 2 and B 8. Unjoining means
     hashing 2x2, join off and pipe select none, done after the running
     pipe's DDB has shrunk, then one vertical blank waited.
   * **Power wells**: `XELPD_PW_CTL_IDX_PW_A` is 5 and PW_B is 6.
     `hsw_power_well_enable` waits for the fuse distribution after each
     well comes on.
   * **Disable** (`hsw_crtc_disable`, `intel_ddi_post_disable_hdmi`): the
     plane off and one vertical blank, the transcoder off, the transcoder
     function off, the DDI buffer off and idle, the transcoder clock
     select cleared, the port clock gated, the I/O well released, then the
     PLL.
   * **Enable** additions to `light::steps`: infoframes off
     (`VIDEO_DIP_CTL` of the transcoder) before the function, because the
     firmware may leave a stale AVI infoframe on.
   * **Flips**: `PLANE_SURFLIVE` is at 0x701ac (+0x1000 a pipe).
   * **Hotplug pulses**: `SHOTPLUG_CTL_DDI` holds 2 status bits a pin
     (bit 0 short, bit 1 long, write 1 to clear) beside the enable (0x8),
     4 bits a pin.
   * **CDCLK**: `CDCLK_CTL` 0x46000 bits 10:0 hold `(cdclk_khz - 1000) /
     500`. The box's Linux read 0x17e, which is 192 MHz. ADL-P's table runs
     from 172.8 MHz to 652.8 MHz.
   * **Timings read back** for the adoption check: `TRANS_*` give
     `(value & 0xffff) + 1` and `(value >> 16) + 1`, and the clock is the
     DCO over `pdiv*qdiv*kdiv*5` from `DPLL_CFGCR0/1`. The 38.4 MHz
     reference's half-fraction must be undone.
4. D2 to D4 as §9 says. The JetKVM tools (`~/.local/share/ferrix/box/jetkvm.mjs`)
   have no EDID step yet. Its UI's JSON-RPC has one: find the method name
   with the tool's `probe:` step while the EDID is changed in the UI, then
   add an `rpc:` step.

**Open for the customer:** whether a DisplayPort sink and a USB-C to
DisplayPort cable can be put at the box (§10, DP is untestable without
them), and whether a second HDMI monitor can be there for §8.2 step 7.

**The consultant's working notes, given when the review was withdrawn
(not a verdict, not in the ledger).** Fold them into the design before
asking again:

* The GGTT entries this design must never write are **the union** of the
  lit plane's scanned range at publish and **the kernel's panic window's
  aperture range**. That range is the BootInfo framebuffer's physical
  address minus BAR 2's base, for the whole mapped length in pages
  (`panic/screen.rs` maps that address whether or not `ferrix.efifb` is
  set). The two ranges agree only when the GOP's plane offset equals the
  framebuffer's BAR 2 offset. Say where the driver learns the second range,
  and make `model.rs` refuse a write into either.
* Reword §6.4's "the kernel's window keeps writing only the pages it was
  given" as a property of the driver that the kernel does not claim, as K4
  (4) and ledger 2352 (d) already put it.
* Name the case `ferrix.efifb ferrix.intelgpu` with the unit not
  translating: the card is published under K4's identity rule there, and
  V-03's residual ("the driver is trusted like the kernel") applies.
* Check `DMA-GATE.md`'s wording before §6.4 relies on it.
* Questions 4 (the `modeset` word's registry, profile and claim fields in
  `build-flags.toml`) and 5 were not reached.
