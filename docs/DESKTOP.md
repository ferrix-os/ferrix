# A GNOME-like desktop on hyprix

The customer's request of 2026-10-23: give hyprix what a floating window
manager like GNOME has, a theme that makes it look like GNOME, and dynamic
multi-monitor support like GNOME's; and, the same day, notifications when a
driver crashes, with a notification manager for hyprix. hyprix stays a
tiling compositor: everything here is something a configuration turns on,
and the default configuration is unchanged.

This document is the design (what hyprix has today for each part, what is
missing, the order, the points) and its "where it stands".

## Where it stands, 2026-10-23

Branch `hyprix-gnome`, not landed. Kernel lines for all four parts: **0**
(each part's section says why). Nothing touches the certified item, so no
consultant review is owed.

**Part 1 is built** (18 points estimated), with 2b brought forward so it
can be seen:

| Step | Commit | What |
|---|---|---|
| 1a | f6584da2c | `general:layout = floating` and `workspace = N, layout:floating`; new windows choose their size, centred and cascaded in the work area; floating windows told no tiled edge |
| 1b | 20d4e496b | `snapwindow max\|left\|right\|restore\|toggle`; `set_maximized` and the taskbar's maximize on floating windows; drag to a top edge maximizes, to a side halves, with a tile preview; a snapped window dragged away takes its old size under the pointer |
| 2b | 521897720 | `hyprix/themes/ferrix-light.conf`; `hyprix:own_frames` (no compositor border round a window that draws its own frame, nothing round a maximized one); GTK 4's identity viewport no longer stops the window-geometry crop |
| 1c | 7459f3c01 | minimize: `set_minimized`, the taskbar's request, `minimize`; focusing brings it back |
| 1d | 76ef1a29b | the overview: `overview toggle`, SUPER alone; a grid of the shown workspace's windows, a strip of the other workspaces in miniature, click to pick, Escape to close |

Gates on 76ef1a29b: `check` PASSED (`hyprix-gnome-check`) and
`test-compositor` PASSED (`hyprix-gnome-compositor`). After a rebase onto
a622d2266 with no conflict, on 88654b7cc: `build --arch all --release`
(`hg-p1-release`), `test-boot` x86_64 KVM (`hg-p1-boot-kvm`) and TCG
(`hg-p1-boot-tcg`), aarch64 (`hg-p1-boot-aarch64`), armv7a
(`hg-p1-boot-armv7a`) and armv7a `--smp 2` (`hg-p1-boot-armv7a-smp2`), all
PASSED. Logs in `~/.local/share/ferrix/logs/queue/`. Host tests: layout 199, hyprix lib
141, `two_clients` 33 (one new: the floating workspace), `theme` 1. foot
against the compositor printed no missing protocol. Screenshots, headless
runs with zenity (GTK 4) and pattern clients:
`~/.local/share/ferrix/hyprix-gnome/shots/`.

Part 1's leftovers were fixed after it landed (995f8012f): the tile
preview is drawn under the dragged window, taskbars are told `maximized`
and `minimized`, a new window on a floating workspace floats from its
first frame, and the theme's page and SUPER binds use the keymap's names.
The overview still has no box behind each workspace miniature, no search
and no drag onto a miniature. Seen on the way, not this branch's: on this
host foot's text did not reach the headless dumps in the tiled layout
either.

**Part 2 is built** (13 points estimated):

| Step | Commit | What |
|---|---|---|
| 2a | aa3ba1296 | `hyprix:titlebars = 1`: a header bar above each floating window that asked for the compositor's frame -- title, minimize, maximize, close drawn in code; press to move, double click to maximize; the layout keeps the bar inside the work area |
| 2b | 521897720 | (with part 1) the theme file and `hyprix:own_frames` |
| 2c | 0d1a18248 | `hyprix/themes/ferrix-light-bar/`: the top bar as a waybar `config.jsonc` and `style.css` |

Gates on 537c767f1: `check`, `test-compositor`, `build --arch all --release`
and `test-boot` on x86_64 KVM and TCG, aarch64, armv7a and armv7a
`--smp 2`, all PASSED (`hg-p2-*`, logs in
`~/.local/share/ferrix/logs/queue/`).

Screenshots of the whole look: `2-ferrix-light-desktop.png`,
`2-ferrix-light-snapped-right.png` and `2-ferrix-light-overview.png` in
the same folder. Left of part 2: pointer motion over a title bar still
reaches the window under it, buttons have no hover state, tiled windows get
no bar, and images do not yet carry the bar's configuration or start it.

**Part 3 to 3g is built** (landing together):

| Step | What |
|---|---|
| 3b | `hyprctl output create headless [NAME]` / `output remove NAME` |
| 3a | screens added and removed at runtime: `wl_output` globals for running clients, per-output objects renumbered, workspaces moved with floating windows pulled inside, a `FALLBACK` stand-in for the last screen; DRM hotplug from the existing connector event, never taking a running screen's CRTC |
| 3c | output management lists every mode and applies `set_mode` |
| 3d | `$XDG_STATE_HOME/hyprix/monitors.conf`: each set of monitors' arrangement, saved when a person arranges them and put back when the set is connected |
| 3f | the Intel driver follows the lit port's hot-plug state (SDEISR bit, polled every 500 ms, 100 ms settle) and tells the core with `MODES` and the mode list; hooks inside `display.rs` and `display/`, one wake-up term in `serve.rs` (`WAKE_AT`, key 0x4850), one line in `main.rs` |
| 3g | the monitor's own EDID over GMBUS, sent to the core as `EDID` at publish and on each plug |

Gates on b92b0467a (3a to 3g on main 3a4a45e88), all `run: PASSED`:
`check` (`hg-p3f-check`), `test-compositor` (`hg-p3f-compositor`),
`build --arch all --release` (`hg-p3f-release`), `test-boot` x86_64 KVM
and TCG, aarch64, armv7a and armv7a `--smp 2` (`hg-p3f-boot-*`), and
`test-restart --boot intel` (`hg-p3f-restart-intel`); rebased onto main
1b2040006 with no conflict. An earlier tip's `test-compositor` failed once
on the Settings step at host load 35 and passed on the next: fl-075.

Neither DRM hotplug nor the Intel steps can be run in QEMU (no device
unplugs a virtio-gpu head from outside, and none has the Intel display
registers); the N150 box test of §3.4 is what shows them.

**3h is under way as agreed with the coordinator**: h1, the planner,
printed under `ferrix.intelgpu.modeset=dry` and writing nothing; then a
read-only register dump of the same registers while Linux drives the
second HDMI port, the ground truth; then h2, the writes, behind
`ferrix.intelgpu.modeset=on`.

Next: h1 and h2 of 3h, part 4 (4a and 4b built on the branch), 3e on the
3060.

## 1. Floating mode

### 1.1 What hyprix has

hyprix already floats windows the way Hyprland does
(`src/user/system/linux/compositor/layout`): every workspace keeps its
floating windows in stacking order with their own rectangles
(`floating_rects`, kept in workspace coordinates so a workspace that moves
monitor takes them along), `togglefloating`/`setfloating`/`settiled`,
`centerwindow`, `pin`, `moveactive`/`resizeactive`, mouse moves and
resizes (`bindm`, a press on a border with `general:resize_on_border`, and
a client's own `xdg_toplevel.move`/`resize` from a CSD header bar,
`hyprix/src/act.rs`), and Hyprland's `general:snap:*` edge snapping
between windows and to the monitor (`layout/src/snap.rs`). Dialogs float
on their own (`float_dialog`): configured 0x0 so the client picks its
size, then centred over the parent or the monitor once the first buffer
says the size (`fit_dialog`). Per-workspace layouts exist
(`workspace = 3, layout:master`). `xdg_toplevel.set_maximized` is
Hyprland's `fullscreen 1`.

### 1.2 What is missing

* **A floating layout.** There is no way to say "every new window on this
  workspace floats". `general:layout = floating` (global) and
  `workspace = N, layout:floating` (per workspace) are new: on such a
  workspace a new toplevel opens floating the way a dialog does now -- it
  chooses its own size, which is clamped to the work area, and is placed
  as mutter places it: centred over its parent or the work area, cascaded
  when that spot is taken. A window tiled by hand there (`settiled`) tiles
  in a dwindle tree, so tiling stays available on the same workspace.
* **What a floating window is told.** Every window is configured
  `tiled_left|right|top|bottom` today, which tells a GTK window to square
  its corners and drop its shadow. A floating window is told no tiled
  state, a maximized one `maximized`, a half-snapped one the two or three
  edges it touches (as mutter does), so CSD apps draw GNOME's own rounded
  corners and shadows.
* **Maximize and half tiling.** A floating window's maximize is its own
  rectangle grown to the work area, not `fullscreen 1`: the old rectangle is
  kept, other floating windows can still go above it, and unmaximize gives
  the old rectangle back. The same with the left or right half. New
  dispatcher `snapwindow max|left|right|restore|toggle` (hyprix's own;
  Hyprland has none), `xdg_toplevel.set_maximized`/`unset_maximized` on a
  floating window, and binds in the theme (`SUPER+Up`, `SUPER+Left`,
  `SUPER+Right`, `SUPER+Down`). Tiled workspaces keep Hyprland's behaviour.
* **Snapping by dragging to an edge.** A floating window moved by the
  pointer to the monitor's top edge maximizes when dropped there, to the
  left or right edge half-tiles; while it is there a translucent preview of
  the rectangle it will take is drawn (GNOME's tile preview). Dragging a
  snapped or maximized window away restores its old size under the pointer,
  at the same relative place across its width.
* **Minimize.** `xdg_toplevel.set_minimized` and the foreign-toplevel
  request hide the window to a per-monitor special workspace; activating it
  from the overview, a taskbar or `focuswindow` brings it back.
* **An activities overview.** GNOME's overview: one key (`SUPER`, a bind to
  the new dispatcher `overview toggle`) or the top bar's Activities button
  shows every window of the active workspace of each monitor scaled into a
  grid, and a strip of the monitor's workspaces at the top; a click or
  Enter on a window focuses it and closes the overview; a click on a
  workspace switches; Escape closes; dragging a window onto a workspace in
  the strip moves it there. It is drawn by hyprix from the windows'
  current buffers (`render::Canvas::composite_scaled`, which exists), with
  the layout untouched: the overview is a way of drawing and of reading the
  pointer, not a layout. Typing to search launches through `fuzzel` (it
  already ranks `.desktop` entries) -- a later step, not in the 8 points.

### 1.3 Order and points

| Step | What | Points |
|---|---|---|
| 1a | `floating` layout, global and per workspace; placement; floating configure states | 3 |
| 1b | `snapwindow`, maximize/half on floating windows, drag-to-edge with preview, restore on drag | 5 |
| 1c | minimize | 2 |
| 1d | activities overview | 8 |
| | **Part 1** | **18** |

Kernel lines: 0. Everything is in the compositor workspace
(`layout`, `config`, `hyprix`, `render`), outside the certified item.

## 2. A GNOME-like theme

### 2.1 What hyprix has

Rounded corners (`decoration:rounding`), Hyprland's drop shadow and blur
(`render/src/canvas.rs`, `blur.rs`), gradient borders, animations, and a
GPU path for all of them. `xdg-decoration` and KDE's server-decoration are
offered and always answered `server_side` -- but nothing is drawn: a client
that does not draw its own title bar (foot, an X program through yserver,
Chrome without its own frame) has none. The top bar is `waybar`, ported
(`docs/DESKTOP-CLIENTS.md`), reading the user's `config` and `style.css`.
`text` draws text from TrueType faces; Inter is vendored under
`assets/fonts/inter` (SIL OFL 1.1).

### 2.2 What is missing, and the licences

* **Server-side title bars** for clients that asked for them: an
  Adwaita-like header bar drawn by hyprix above the window -- the title
  centred in the UI font, close, maximize and minimize buttons on the right
  drawn as shapes in code (as the cursor is), the colours of the active and
  the inactive header -- with a press-and-drag on it moving the window, a
  double click maximizing, and the buttons acting. The window's rectangle
  keeps the client's size and the bar is drawn on top of it in the frame,
  so the layouts do not change. On a tiled workspace the bar is off unless
  the theme turns it on (`hyprix:titlebars = floating|all|none`).
* **A theme file**, shipped as hyprix's own and `source`d from a
  configuration: rounding 12, shadow range 20 at about 30% black, no
  coloured border (a 1-pixel dark hairline instead), gaps 0 on floating
  workspaces, the light Adwaita-like palette as colours of our own, the
  binds GNOME has (`SUPER` overview, `SUPER+Up/Down/Left/Right` snapping,
  `ALT+F4` close, `SUPER+H` minimize), `general:layout = floating`.
* **A GNOME-like top bar** as a waybar `config` and `style.css` of our own:
  Activities and the workspace dots on the left, the date and clock centred,
  network, volume and power on the right, black bar, white text in the UI
  font. The notification list sits under the clock (part 4).
* **Fonts.** Inter (OFL 1.1, already in the tree) is the UI font: Adwaita
  Sans, GNOME's UI font since 48, is Inter with GNOME's tweaks, also OFL
  1.1, and can replace it with one more vendored file and its licence.
  Cantarell (OFL 1.1) likewise if the older look is wanted. OFL fonts may
  be bundled and redistributed with any software as long as they are not
  sold alone and keep their licence and reserved names.
* **Icons.** The title bar's buttons are drawn in code, so no icon theme is
  needed for them. The Adwaita icon theme is dual-licensed LGPL-3.0 or
  CC BY-SA 3.0: it may be shipped, as a copyleft component with its source
  and licence in `release-licences.toml`'s `[sources]` and the attribution
  the licence asks for. Not needed for parts 1 to 3; part 4's icons are
  drawn in code too.
* **What is not taken.** No line of gnome-shell's stylesheets (GPL-2.0) or
  code, no libadwaita CSS copied into the theme (the colours are our own
  values, chosen to read alike, not its variables), no GNOME name or foot
  logo anywhere in the theme, the bar or the docs a person sees. The theme
  is called "Ferrix light" in the configuration.

### 2.3 Order and points

| Step | What | Points |
|---|---|---|
| 2a | server-side title bars | 8 |
| 2b | theme file and binds | 2 |
| 2c | top bar config and style | 3 |
| | **Part 2** | **13** |

Kernel lines: 0.

## 3. Dynamic multi-monitor

### 3.1 What hyprix has

Every connected connector of every card is a screen at start
(`Drm::open_all`), placed and scaled by `monitor =` rules matched on name
or EDID description. A card says its connectors changed with Ferrix's own
`EVENT_FERRIX_CONNECTORS` (`docs/DISPLAY.md` §9.3) -- the core raises it on
every `MODES` from a driver (a virtio-gpu window resized, a scanout enabled
or disabled) and every finished `MODE_LIST` -- and hyprix follows the
preferred mode then (`follow_modes`). `hyprctl reload` and `hyprctl keyword
monitor` change mode, scale and position at runtime. A lost card (its
driver died) is waited for and opened again. The layout can add and remove
monitors (`State::add_monitor`, `remove_monitor`).

### 3.2 What is missing

* **Screens come and go at runtime.** On a connector change hyprix rescans
  every card's connectors: a connector newly connected becomes a screen (a
  new `Drm` on the shared card, the rule for it applied, a monitor added to
  the layout, a `wl_output` global, its `xdg_output`, a head in
  wlr-output-management, `monitoradded` on the event socket); one that went
  disconnected stops being one (its global removed, `monitorremoved`). Its
  workspaces move to the remaining monitor the way Hyprland moves them, and
  every floating window on them is pulled back inside its new monitor's
  work area. A card that is new (a second GPU's driver published late) is
  opened the same way.
* **`hyprctl output create headless` / `output remove`**, as Hyprland has:
  a screen in memory added and removed at runtime. It is how the hotplug
  logic is tested without a device (host tests and a `test-compositor`
  boot), and how a VNC session gets a second screen.
* **wlr-output-management that applies**: mode, scale, position, transform
  and enable from `wlr-randr` or a settings program; today it refuses a
  mode change.
* **A saved layout per set of monitors**, as GNOME's `monitors.xml`: when
  the person changes the arrangement (through output management or `hyprctl
  keyword monitor`), hyprix writes it to
  `$XDG_STATE_HOME/hyprix/monitors.conf`, keyed by the sorted EDID
  descriptions (connector names where there is no EDID) of the set
  connected; when that set is connected again it is applied after the
  configuration's `monitor =` lines, which still win where they name a
  monitor.
* **Drivers that see a plug.** Hotplug detection is the ring-3 display
  drivers': each sends `MODES` with the scanout's `enabled` turned, and a
  `MODE_LIST`, when a display comes or goes. virtio-gpu does already
  (`VIRTIO_GPU_EVENT_DISPLAY`). NVIDIA's `nvrm` must turn NVKMS's
  dpy-change events into the same messages (3 points, needs the 3060).
  Intel: §3.4. The kernel needs nothing: the messages and the event
  exist.
* **No monitor at all for a while.** Unplugging the only monitor leaves
  hyprix with none; it keeps a headless stand-in output (as Hyprland's
  fallback monitor) so windows and workspaces stay put, and gives them back
  to the monitor that comes next.

### 3.3 Order and points

| Step | What | Points |
|---|---|---|
| 3a | screens added and removed at runtime, windows moved off a removed one | 8 |
| 3b | `hyprctl output create headless` / `remove` | 3 |
| 3c | wlr-output-management apply | 5 |
| 3d | saved layout per monitor set | 3 |
| 3e | NVIDIA hotplug in `nvrm` (hardware) | 3 |
| 3f | Intel hotplug on the lit port, §3.4 | 3 |
| 3g | Intel EDID over GMBUS (mode list and description from the monitor) | 3 |
| 3h | Intel display-engine modeset: a monitor on a port the firmware did not light, §3.5 | 21 |
| | **Part 3** | **49** |

Kernel lines: 0.

### 3.4 Intel hotplug (the customer, 2026-10-23: "add Intel hot plug as well")

**Today.** The Intel driver (`src/user/system/native/drivers/gpu/intel`,
`display.rs`) serves `card0` as a copying card on the plane the firmware
lit: HELLO names one scanout, its connector named from pipe A's
`TRANS_DDI_FUNC_CTL_A` (HDMI, DVI, DisplayPort/eDP), its mode list the
plane's mode plus scaled sizes (`docs/DISPLAY.md` §9.4,
`docs/INTEL-GPU.md` §4.3). It reads no hotplug state and no EDID, and it
never programs the display engine (`display/` of i915 is left out,
INTEL-GPU.md §3.6).

**What 3f adds, in the driver alone.** The driver watches the lit port's
hotplug live state as i915 does for Alder Lake-N's pins
(`display/intel_hotplug_irq.c`: the PCH's `SDEISR` live-state bit of the
DDI the transcoder drives, armed through `SHOTPLUG_CTL_DDI`; the north
display's `DE_PORT`/TC live state if the port is a Type-C one). It reads it
at a bounded rate -- every 500 ms, the way i915 falls back to polling
(`intel_hpd_poll`) -- and also on the display interrupt if the MSI ever
arrives (it has not on this box, INTEL-GPU.md §3.4), with a 100 ms
debounce. On a change it sends what virtio-gpu sends: `MODES` with scanout
0's `enabled` off for an unplug, and on a plug `MODES` enabled and a fresh
`MODE_LIST`. The display core turns either into `EVENT_FERRIX_CONNECTORS`
and `GETCONNECTOR`'s connected/disconnected, and hyprix's 3a does the rest.
While unplugged the driver keeps the plane and its GGTT entries as they
are and drops copies; on replug it checks that pipe A and the DDI are still
enabled (`PIPECONF_A`, `TRANS_DDI_FUNC_CTL_A`) before it says connected.
An HDMI sink plugged back into a lit port is shown again with no
programming; a DisplayPort sink needs link training again, which this
driver does not do, so a DP replug is reported connected only if the link
status registers say the link is up, and otherwise stays disconnected with
a line saying why.

**3g, the monitor's own EDID.** Read over GMBUS (`display/intel_gmbus.c`,
the DDC pin pair of the lit port) on start and after each plug: the mode
list is then the EDID's modes that fit the running timing's scaled set,
the description the monitor's own name, and hyprix's per-monitor saved
layout (3d) can tell two monitors apart.

**Kernel lines: 0.** The driver already maps the MMIO BAR, takes its MSI as
a port packet and polls on a deadline; `MODES` and `MODE_LIST` exist. If a
step finds it needs anything new in the kernel it stops and comes back with
a figure.

**Where it lives after the keeper/worker split** (INTEL-GPU.md §12.3, being
built now): in the **keeper**. `card0` and the MSI are the keeper's, and
the worker drives the GT only; hotplug is display state, read from display
registers the keeper maps, so it never depends on a worker being alive.
Until the split lands it is in `display.rs`, beside the code that names the
connector; the split moves it with `card0`.

**Not in 3f: a monitor on a port the firmware did not light.** That is
3h, §3.5.

**The box test (the customer runs it).** On the N150 with one monitor on
the HDMI port the firmware lights, `--everything` desktop running:

1. Unplug the HDMI cable, wait 5 s. Look for `intel: card0: HDMI-A-1
   disconnected` on the serial/console log, `monitorremoved>>HDMI-A-1` on
   hyprix's event socket (`socat - UNIX:$XDG_RUNTIME_DIR/hypr/*/.socket2.sock`
   over ssh), and `hyprctl monitors` listing only the headless stand-in.
2. Plug it back in, wait 5 s. Look for `intel: card0: HDMI-A-1 connected`
   with its mode list, `monitoradded>>HDMI-A-1`, and the desktop back on
   the screen with every window where it was.
3. Repeat quickly (unplug and replug within a second) three times: one
   disconnect and one connect each, no line flood, no stuck black screen.
4. If the box has a second output, plug a monitor into it: 3f says nothing
   about it (the line `intel: card0: a sink on DDI <x>, not lit: no
   modeset yet` is the expected answer).

### 3.5 Intel display-engine modeset (3h; the customer, 2026-10-23: "yes add that as well")

The step that gives the N150 a second monitor, and a DisplayPort replug on
the lit port: the driver lights a port itself rather than inheriting what
the firmware lit. Alder Lake-N is display version 13 (Gen12 display); the
work follows i915 file by file, as the GT side does (INTEL-GPU.md §3.6),
with every register offset and field taken from the Linux v7.0 sources
(`i915_reg.h`, `display/intel_display_regs.h`, `intel_dpll_mgr.c`'s ADL-P
tables), never from memory:

| i915 (v7.0) | What is kept | Points |
|---|---|---|
| `display/intel_display_power*.c`, `intel_display_power_well.c` | the ADL-P power-well table: PW1 (already on), PW2, the DDI and AUX wells of the port, with their request/ack | 3 |
| `display/intel_dpll_mgr.c` (`adlp_*`, combo PHY DPLLs), `intel_cdclk.c` (check only) | a combo-PHY DPLL for the mode's clock, from i915's HDMI and DP divider tables; CDCLK left as the firmware set it, a mode it cannot carry refused | 4 |
| `display/intel_ddi.c`, `intel_combo_phy.c`, `intel_hdmi.c` | DDI buffer translations, `DDI_BUF_CTL`, `TRANS_DDI_FUNC_CTL`, HDMI's TMDS and infoframes off | 4 |
| `display/intel_dp.c`, `intel_dp_link_training.c`, `intel_dp_aux.c` | AUX channel, DPCD read, clock recovery and channel equalization at the sink's rates and lane counts | 5 |
| `display/intel_display.c` (`hsw_crtc_enable` order), `skl_universal_plane.c`, `intel_crtc.c` | a transcoder and pipe with its timings, one universal plane scanning out a GGTT surface the driver owns, the enable and disable order | 4 |
| Driver | a second scanout in HELLO (`scanouts: 2`), MODES/MODE_LIST per scanout, `SET_MODE` driving a real modeset rather than scaling | 1 |
| | **3h** | **21** |

* **Kernel lines: 0.** Every register is in the GPU's MMIO BAR the driver
  maps already; the second scanout is a HELLO field the core has had since
  displayctl v1 (`MAX_SCANOUTS`); the plane's surface is driver memory in
  the GGTT as the first one is. Anything new in the kernel stops the step
  with a figure.
* **Where it lives.** A module tree of its own in the driver,
  `intel/src/display/` (`power.rs`, `dpll.rs`, `ddi.rs`, `dp.rs`,
  `pipe.rs`, `hotplug.rs` for 3f), with the pure tables and arithmetic in
  `src/lib/drivers/gpu/intel-xelp/src/display/` (host-tested, translated
  with i915's copyright and MIT notice as the GT tables are). It belongs to
  the **keeper** of INTEL-GPU.md §12.3 (branch `gpu-opaque`, being built):
  `card0` is the keeper's, so the display engine is too, and nothing of it
  goes in the worker. The overlap with that branch is the place `card0` is
  served from: today `display.rs` and `main.rs`'s start-up, after the split
  the keeper's `card0` code. 3f to 3h keep to the new `display/` modules and
  touch `display.rs`/`main.rs` only where `card0` hands them the
  connector, so a rebase onto the split moves a few calls and no logic.
* **Hardware writes.** Display registers of the Intel GPU only (power-well
  requests, DPLL, DDI, transcoder, pipe, plane, AUX), on the N150 alone and
  through the coordinator; nothing to the PMIC, firmware or flash. Every
  write sequence runs first against the host model, as the GT's do.
* **Order.** After 3f and 3g, which it builds on (hotplug tells it a sink
  arrived; GMBUS or AUX gives the EDID it picks a mode from).
* **The box test** (written when it is built): a monitor on the port the
  firmware does not light, with the cable that port needs (HDMI or a
  DisplayPort/USB-C cable), plugged in while the desktop runs.

## 4. Driver crashes as notifications

### 4.1 What there is

`devmgr` (native, ring 3) sees every driver death and restart: it quiesces
the device, writes DIED (location, exit status) and, once the next driver
publishes, RESTARTED (location, restart count) to the kernel on its
bootstrap channel, and the kernel prints them (`docs/DEVMGR.md` §4). The
kernel keeps nothing a program can read about them, and the
`NETLINK_KOBJECT_UEVENT` socket hears nothing. The session's D-Bus bus is
`dbus-daemon`, started by `sessiond --session-bus` (G1, `docs/GNOME.md`).
There is no notification daemon and no D-Bus code in Rust in the tree.

### 4.2 The design

* **The source: `devmgr` publishes, the kernel adds nothing.** The pattern
  `drvupdated` set (`docs/DEVMGR.md` §4.1): a small native helper,
  `devmgr-events`, that the image carries in `/lib/drivers` and `devmgr`
  starts after REPORT with a channel whose other end `devmgr` keeps. Each
  time `devmgr` writes DIED, RESTARTED or "given up" (the start limit
  reached) to the kernel, it writes the same event to the helper, with the
  driver's name and kind added (which `devmgr` knows from its table and the
  kernel's message does not carry). The helper binds the abstract socket
  `\0ferrix.devmgr.events` and writes each event as one line to every
  connected reader (`died snd 0x00000010 status 137`, `restarted snd
  0x00000010 1`, `gave-up gpu 0x00000008 8`), keeping the last 32 for a
  reader that connects late. Readers are read-only; any uid may read, as
  the events say no more than the kernel log does. A slow reader is
  dropped rather than allowed to hold the helper, and the helper never
  holds `devmgr`: a full channel loses an event and the next carries a
  count of those lost. `devmgr` and its protocol crate are outside the
  certified item (`certification-item.json`: "devmgr and every driver stay
  outside the item"); the kernel's `discovery/devmgr.rs` is untouched.
  **Kernel lines: 0.**
* **The daemon: `notifyd`**, a client of the compositor's toolkit
  (`toolkit`, `text`, as fuzzel and waybar are). It owns
  `org.freedesktop.Notifications` on the session bus when there is one
  (Notify, CloseNotification, GetCapabilities -- `body`, `actions`,
  `persistence` --, GetServerInformation, and the signals
  NotificationClosed and ActionInvoked), so libnotify and GTK applications
  work, and needs no bus for the driver events. D-Bus is spoken by a small
  crate of our own (`compositor/dbus`: SASL `EXTERNAL`, the marshalling of
  the types the interface uses, `Hello`, `RequestName`, method calls and
  signals), host-tested against byte strings recorded from `dbus-send`.
  Banners are layer-shell surfaces at the top centre of the focused
  monitor, GNOME-style: a rounded card with the app's name, the summary in
  bold and the body, actions as buttons under it; urgency low and normal
  time out (the client's timeout, else 5 s), critical stays until
  dismissed; a click on a banner invokes its default action. A list of
  past notifications opens from the top bar's clock (a waybar custom module
  that runs `notifyd --toggle-list`), with "Clear".
* **The bridge** is inside `notifyd`: it reads `\0ferrix.devmgr.events`
  and turns each event into a notification of its own -- "Graphics driver
  restarted (gpu, restart 2)", "Network driver crashed, restarting
  (virtio-net)", critical "Sound driver stopped after 8 restarts" -- and
  replaces the one it showed for the same device rather than stacking
  another, so ten crashes in a row are one banner with a count.

### 4.3 Order and points

After floating mode: none of it is cheaper earlier, and it depends on
nothing in parts 1 to 3.

| Step | What | Points |
|---|---|---|
| 4a | `devmgr-events` helper and devmgr's writes to it | 3 |
| 4b | `dbus` crate, the subset | 5 |
| 4c | `notifyd`: the interface, banners, list | 8 |
| 4d | the bridge from devmgr's events | 2 |
| | **Part 4** | **18** |

## 5. The whole

| Part | Points | Kernel lines |
|---|---|---|
| 1 Floating mode | 18 | 0 |
| 2 Theme | 13 | 0 |
| 3 Multi-monitor | 49 | 0 |
| 4 Notifications | 18 | 0 |
| **All** | **98** | **0** |

Order: 1a, 1b, 2b (the theme makes the result visible), 1d, 2a, 2c, 3b,
3a, 3f, 3g, 3h, 3c, 3d, 1c, 4a to 4d, 3e on the 3060 when the box is free.

**Certification.** No part touches a file in the `core` or `item` ring:
the compositor, its clients, `devmgr` and the drivers are outside the item.
No consultant review is owed unless a step finds it must change the kernel,
which would stop that step and come back with a kernel-lines figure first.

**How it is checked.** The layout's host tests for every new rule
(placement, snapping, restore, monitor removal); hyprix's
`two_clients`-style tests with `pattern` clients (a floating workspace
opening two windows, a drag to the edge, an overview frame against a
blessed image); `test-compositor` boots for the floating layout and for
`output create headless`; `foot` against the compositor after the
title-bar and output-management changes (`hyprix/probe/real-client.sh`);
`test-restart --boot all` with `notifyd` for part 4.
