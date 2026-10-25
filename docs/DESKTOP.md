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

**A minimized window can be found again** (the customer's bug of
2026-10-24: once minimized, nothing brought a window back). 1c restored a
window that was focused, but nothing a person could press focused one: the
overview listed only the shown workspace's windows, `cyclenext` walks one
workspace and a minimized window waits on `special:minimized`, our waybar
had no taskbar, and -- found on the way -- `bindr = SUPER, Super_L` never
fired at all, since a release bind was matched on the modifiers held after
the release. Now, in the GNOME and Hyprland ways:

* **The overview** has each workspace's minimized windows in its grid,
  after the others, at half opacity (`overview::MINIMIZED_OPACITY`). A
  click, or `Return` on the tile the arrow keys or `Tab` picked, focuses
  the window and brings it back; `overview next|prev|pick` are the same
  for a bind or a script. The keys it takes have their release taken too.
* **`ALT+Tab` and `SUPER+Tab`** (and with `SHIFT`, back) in Ferrix light
  run hyprix's new `switcher` dispatcher (`hyprix/src/switcher.rs`): every
  window on an ordinary workspace and every minimized one, most recently
  focused first, in a row over the focused monitor with the window before
  the focused one picked; `Tab` again moves on, letting go of `ALT` or
  `SUPER` (or `Return`, or a click) focuses and restores the picked one,
  `Escape` changes nothing. A quick tap goes straight back.
* **A taskbar**: waybar's `wlr/taskbar` in the apps repository's waybar,
  over `zwlr_foreign_toplevel_management_v1` on the bar's own connection:
  a button a window with `active`, `minimized`, `maximized`, `fullscreen`
  and its application id as classes, and waybar's `minimize-raise` on a
  click (the default here). It is not in Ferrix light's bar, which is
  GNOME's top bar and has no window list, nor in the customer's own
  `~/.config/waybar`, which says its window list is the dock along the
  bottom (`hypr-dock`); `--everything` and the dotfiles desktop carry that
  config as it is. A bar config that wants one names `wlr/taskbar`.
* **Release binds** are matched on the modifiers the key went down with,
  as Hyprland matches them, and one on a modifier's own key fires only when
  that key was let go of alone, so `SUPER H` no longer opens the overview
  as `SUPER` comes up.

Tests: the overview's and the switcher's input and drawing (hyprix lib),
`two_clients`'s `a_minimized_window_is_found_again_in_the_overview_and_the_taskbar`
and `the_switcher_brings_a_minimized_window_back`, the seat's release
binds, waybar's taskbar module and two `against_hyprix` clicks, and a
`test-compositor` boot, `minimize`: a taskbar (`lswt minimize`) hides a
window, the overview shows it dimmed, `Left` and `Return` bring it back,
and `lswt unminimize` does again. The overview's two pictures are blessed
from the compositor's own frame
(`render/tests/data/overview-one-minimized.xrle` and
`overview-one-minimized-picked.xrle`, the minimized window dimmed in the
second slot), the renderer having no layout of the overview to bless them
from.
Gates (2026-10-24): on ed553b158 and 2091b7601, `check` PASSED (its first
run met fl-080, nvos's timer test, unrelated), `test-compositor` x86_64
PASSED, every boot `minimize` among them (`lode-viewer-ssh` onward rerun
alone after building `sshdt`, which the host lacked), `test-apps` PASSED;
waybar's 5 `against_hyprix` and hyprix's `two_clients` passed 3 of 3. The
rebase onto 253a51630 needed `BOOTS` counted again (another boot had
landed), so `check` and `--boot minimize` were run again on it, PASSED.
Logs in `.claude/mr-gate-*.log` of the root checkout. Not done here: a
`test-compositor --arch aarch64` stopped building `tole-examples` (no
aarch64 musl linker on this host), before any boot.

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

h1 is built (57ce6f566): the WRPLL, DPLL, transcoder timing, DDI, pipe
source and primary plane words for the mode the EDID prefers, printed under
`modeset=dry`.

**h2 is built**, after the ground truth: the box's inspection Linux with
a monitor on each HDMI port (`~/.local/share/ferrix/n150-linux/evidence/3h-dump-2026-10-23`;
Linux had a 1024x600 DVI monitor on pipe A, DDI B, DPLL1 and the TV on
pipe B, DDI A, DPLL0). The planner's PLL words for both clocks, the
transcoder's timings and source, `TRANS_DDI_FUNC_CTL`'s port, sync and
mode, and the plane's size and stride all equal Linux's. What it lacked,
now there:

* `DVI` mode for a sink with no HDMI vendor block, `PLANE_CTL`'s
  `ARB_SLOTS(1)`;
* i915's whole `hsw_crtc_enable` order for HDMI on a combo port
  (`intel-xelp/src/display/light.rs`): PW2 and pipe B's well with their
  fuses, DBUF slices S3 and S4 (pipe B's beside pipe A,
  `adlp_allowed_dbufs`), `PIPE_MBUS_DBOX_CTL`, the PLL's power, dividers
  and lock, `DPCLKA_CFGCR0`'s select and ungate, the DDI I/O well,
  `TRANS_CLK_SEL`, pipe source and misc, timings, `TRANS_MULT`, the frame
  start delay, `WM_LINETIME`, `PIPE_CHICKEN`, the plane's DBUF share and
  watermarks, the function, the transcoder on, the PHY's HDMI signal level
  and lanes, `DDI_BUF_CTL`, and the plane last;
* from the dump rather than arithmetic: the PHY's signal level (Linux's,
  identical on both PHYs), pipe B's DBUF share and watermarks (i915's for
  1080p at 148.5 MHz, so modes above that clock are refused).

`ferrix.intelgpu.modeset=on` reads the display first and writes nothing if
it is not as the steps expect (pipe B or port B on, MBUS joined, DC
states on, no free PLL, a clock above 148.5 MHz), then takes each step,
saying it, and stops at the first wait that does not end. The new monitor
shows the lit pipe's own surface, a mirror of its top-left, until `card0`
has a second scanout (HELLO `scanouts: 2`, the last row of the table in
§3.5). Host tests run the steps on a model of the firmware's display and
compare every value with the dump. The box run is the coordinator's.

**Part 4 is built** (18 points estimated):

| Step | Commit | What |
|---|---|---|
| 4a | 1f92468ad | `devmgr-events`, a native helper `devmgr` starts after REPORT: each DIED, RESTARTED and gave-up as one line to every reader of `\0ferrix.devmgr.events`, the last 32 kept for a late reader; `devmgr` never waits on it |
| 4b | 19e64e50b | `compositor/dbus`: SASL `EXTERNAL`, the little-endian marshalling, `Hello`, `RequestName`, calls, returns, errors and signals; tested against byte strings and against the host's `dbus-daemon` |
| 4c, 4d | ec382429d | `notifyd`: `org.freedesktop.Notifications`, GNOME-like banners at the top centre (a fixed-size overlay surface whose input region is only the cards), the list from the bar's clock (`notifyd --toggle-list`) with Clear, and the bridge: one banner per device with a count, gave-up critical, a reconnect's replay dropped. The theme starts it; the compositor's test image carries it |
| | 09c64c30e | found on the way: a layer surface with a zone of 0 is placed after every exclusive one, as wlroots does, so a banner that started before the bar is not under it |

Screenshots, a headless hyprix with waybar, a private `dbus-daemon`,
`gdbus call ... Notify` and a stand-in for `devmgr-events`:
`4-notify-banners.png` and `4-notify-list.png`. Left of part 4: notification
icons are not drawn, the list's close crosses do nothing, and no image but
the compositor's test image carries `notifyd` (as for the bar, nothing
installs the theme yet).

Found on the way and landed: the shipped wallpaper (89073b34e,
`docs/brand/wallpaper/`, `compositor/wallpaper`, a `test-compositor` boot);
a buffer destroyed after its commit stays shown and a placed surface with
nothing to draw is said once (e4f11d32d, swaybg on the N150); a zone of -1
covers the monitor (4bac20cb0); a monitor plugged back in after its card's
last screen went is found again, at its old place (f5accc7ef).

**The second scanout is built** (the customer, 2026-10-23: extended
monitors, not mirroring). Under `ferrix.intelgpu.modeset=on`, `card0`
says HELLO with two scanouts:

* pipe B gets a surface of its own: 1920x1200 at a 7680-byte stride,
  pages pinned into 00:02.0's domain and entered into the GGTT below the
  driver's own window at 256 MiB, in the first 2 MiB-aligned run the
  firmware left free -- or, where it left none (the box's firmware keeps
  entries far past its lit framebuffer: 78559 present below 0x23000), the
  first run no enabled plane or cursor of pipes A to D scans out, its
  firmware entries taken over (`light::scanned`, `light::place`); the range
  chosen is logged. BAR 2 is not lent on the box, so the driver writes the
  surface through its own cached mapping and flushes every line it writes
  (`clflush`, as i915's `drm_clflush_virt_range`)
  (`intel/src/display/surface.rs`). A surface that cannot be made leaves
  scanout 1 announced but dark, its port watched and its EDID sent, and is
  tried again at the next plug;
* the port the firmware left dark is lit onto it -- port B where it lit A,
  port A where it lit B (the box's boot of 2026-10-23: the 1024x600 monitor
  on B lit, the TV on A dark) -- at boot where a monitor is there (h2's
  steps, on pipe B with the free PLL), or when one is plugged in later;
  where the firmware cloned one surface onto both pipes, the running pipe B
  is pointed at the new surface instead (`light::running`,
  `light::repoint`). A port whose combo PHY the firmware never initialised
  is refused, said;
* both combo ports are watched for hotplug, each telling its own scanout's
  `MODES`, mode list and EDID; SCANOUT, FLUSH and SET_MODE act per
  scanout, so hyprix places two screens side by side.

Host tests place the surface in a GGTT model with firmware entries past
the lit framebuffer (a hole found; none, and a range reclaimed past every
scanned plane), read the planes' scanned ranges, repoint a cloned pipe,
and light the TV on
port A when the firmware lit B -- the dump's own layout, every value
Linux's; the rest runs on the box only.

Next: the box run of h2 and the second scanout; 3e on the 3060.

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
  from the overview, a taskbar or `focuswindow` brings it back. The
  overview shows a minimized window in its workspace's grid, dimmed, and
  the window switcher (`ALT+Tab`) lists it, so a person can always find it
  again.
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

## 6. Yoru, the main UI

The customer's request of 2026-10-24: their design "Yoru WM" (Claude
Design project c2446633-7510-48b1-be87-c1906fddac9b, kept with its
`support.js`, the Nocturne token sheet and a render at 1920x1080 in
`~/.local/share/ferrix/design/yoru-wm/`) becomes the look of Ferrix's
desktop on hyprix: the default for `run-compositor --everything` and the
desktop image, with Ferrix light (§2) still selectable, and a person's own
`~/.config/hypr` and `~/.config/waybar` still winning where they exist.

Kernel lines: **0**. Everything is hyprix, its clients, tole and xtask, all
outside the certified item, so no consultant review is owed.

### 6.1 What the design is

A floating window manager with tiling, keyboard first ("Alt is the Yoru
key", every action also has a place to click), dark and compact, on the
Nocturne design system: ground `#161826`, surface `#232532`, text
`#e9e9ed`, accent `#9184d9`, 100-900 OKLCH ramps for neutral, accent and
two more accents, Inter at 500, 0.7x spacing, radii 4/8/14, elevation as a
hairline and ambient darkness, the accent as a line and a glow and never a
flood, rules fading over 48 px at each end, Phosphor icons. The script
derives every colour at run time from three hues (`applyTheme`): a named
theme's triad (Nocturne 289/200/340, Sakura, Mint, Sky, Sunset) or the
wallpaper's own hues, light or dark from the wallpaper's luminance, with a
vibrance factor. Its parts, as the template and the script draw them:

| Part | What it does | How it moves |
|---|---|---|
| Wallpaper | a picture, a gradient of the theme's colours or a looping video, darkened 15% to 50% top to bottom; scaled 1.08 and shifted 1.2% a workspace (parallax); dimmed and desaturated in focus mode | transform .8s `cubic-bezier(.2,.8,.2,1)` |
| Windows | surface ground, radius 8 (0 when snapped with no gap); the focused one a 1 px accent-600 ring, an accent glow up-left and an accent-2 glow down-right and a deep drop shadow, a 1 px accent gradient line along its top fading in 48 px; others `shadow-md` | geometry .2s on the curve above, shadow .2s |
| Title bar | 34 px, a tab per grouped window (icon and title, the active one tinted, an accent underline when focused), an auto-tile mark, minimize, maximize, close as Phosphor glyphs in neutral-400, a hover tint, close's hover in the accent | colours .15s |
| Snapping | drag to an edge or corner (halves, quarters, top for full screen) with a preview; Alt+arrows halves then quarters; hovering maximize or Alt Z opens six snap layouts; drag edges to magnetic guides; neighbours whose edges touch resize together (Shift for one alone); a window dropped on another's title bar groups as tabs, double click pops one out | preview .14s |
| Auto-tile | Alt T: master and stack per workspace, swap by drag or Alt+arrows | .2s |
| Workspaces | four, named code, docs, misc, free | slide 40 px and fade, .28s |
| Dock (layout `Dock`, the default) | a bar 52 px high floating 6 px above the bottom: a search button (the design's mascot crop of a third-party picture), the four workspaces with a dot per window and a tile mark, launchers for Terminal, Editor, Browser, Notes, Files with a running dot, Stash with a badge and a shelf, tools (auto-tile, scratchpad, clipboard, shortcuts, wallpapers), focus mode, a pomodoro with its progress, the clock and date; a gradient frame of the three accents at 26% under a 22 px blur | left/right .3s when a sidebar is pinned |
| Bar (layout `Bar`) | the same as a 40 px bottom bar with a task list instead of launchers | |
| Widget sidebars | 320 px columns on the left and right, shown while the pointer touches the screen's edge, on an empty workspace, or pinned (pinned reserves their room); tabs of widgets: clock and month calendar; weather (sample data); quick settings (brightness, volume, microphone, appearance Auto/Dark/Light, theme colour, Wi-Fi, Bluetooth, do not disturb, night light, focus mode, auto-tile); to-do. A blurred scrim fades in beside an unpinned one | a clip wipe .42s, content .3-.5s delayed .12s |
| Command palette | Ctrl K or Alt Space: windows, stash, new apps, tiling, helpers, theme, workspace moves, fuzzy matched, shortcuts shown | |
| Switcher | Alt Tab or Alt `: cards of the workspace's windows | .12s |
| Helpers | Alt J letters on every window; Alt / the shortcut sheet; Alt V clipboard history; Alt S a scratchpad terminal dropped from the top; Alt F focus mode (others at 20%, blurred); Alt P pomodoro; toasts at the top centre; night light and brightness as overlays | scratchpad .26s |
| Wallpaper picker | Alt I: a cover flow in 3D or a grid, folders, colour filters, sorting by hue or brightness, adding pictures and videos; picking one re-derives the accent | .45s |
| Sounds | short sine chimes on snap, open, close, workspace | |

Not in the design, though the brief names them: a lock screen and
notifications beyond the toast. Both are made in its tokens (§6.2).

### 6.2 Where each part goes

| Part | Piece | New or reused |
|---|---|---|
| Tokens | `yoru` theme for hyprix (`hyprix/themes/yoru.conf`) and a Nocturne token set for tole, generated from the same table | new theme; tole's `Theme` reused, given Nocturne's values |
| Wallpaper | `/bin/wallpaper` (`wallpaper = , picture ...`): a Nocturne gradient of our own as an SVG; a person's picture or video through the same line | reused; the SVG is new |
| Video wallpaper | `pattern --video`, AV1 in IVF, decoded on the CPU by rav1d (dav1d in Rust); `cargo xtask wallpapers` converts a person's MP4 or WebM | reused. Cost: decoding and colour conversion on the CPU and a full-screen upload every frame, and the screen never idles, so the compositor composites every frame; an estimate of a third of an N150 core at 1080p30, to be measured in Y7, and real battery on a laptop. Not the default |
| Window frames, shadows, rounding, glow | hyprix's renderer: `decoration:rounding`, `shadow`, `col.active_border` gradients | reused, configured |
| Title bars | hyprix's `titlebar.rs` (§2.2), given `hyprix:titlebar:*` options for the colours, the height, the title's side and the top line | extended |
| Tabs | hyprix's groups and groupbar, styled as tabs in the title bar | reused, restyled later |
| Snapping, previews | hyprix 1b (`snapwindow`, drag to edge); quarters, layouts picker, guides and linked edges | quarters, picker and linked edges new |
| Auto-tile | the per-workspace layout (`workspace = N, layout:master`) switched by a dispatcher | reused, one dispatcher new |
| Workspace slide, parallax | hyprix animations; the parallax is new | |
| Focus mode | `decoration:dim_inactive` through `hyprctl keyword` | reused |
| Dock and bar, sidebars, palette, cheat sheet, wallpaper picker, toasts | **`yoru-shell`**, a new app on tole: layer-shell surfaces (dock at the bottom with its exclusive zone, a sidebar per edge, an overlay for the palette, sheet and picker), hyprix's IPC sockets for workspaces, clients and events as waybar's hyprland modules read them | new; tole gains layer-shell windows |
| Launcher | fuzzel in Nocturne colours first (its `fuzzel.ini`); the full palette (windows and actions) in `yoru-shell` after | reused, then new |
| Switcher, overview | hyprix's own (1d, SUPER+Tab), drawn in Nocturne | restyled |
| Scratchpad | a special workspace with a terminal, slid from the top | reused |
| Clipboard history | `yoru-shell` reading the selection through `ext_data_control_v1` | new |
| Lock | hyprlock with a Nocturne `hyprlock.conf` | reused |
| Notifications | notifyd's banners in the tokens | restyled |
| Quick settings | volume through pulsed, Wi-Fi and Bluetooth through what Settings reaches; brightness and night light where a backlight or gamma exists | partly later |
| Icons, fonts | Phosphor (MIT) as its font, vendored pinned with its licence beside Inter (OFL, `assets/fonts/inter`) | Phosphor new |

What is not shipped: the design's pictures and videos (`assets/walls/*`,
`uploads/*`) are third-party art and stay out of the tree and every image;
the dock's mascot, a crop of one of them, becomes the FX mark. The sample
weather, files, stash and code in the design are demo content, not parts.

### 6.3 Order

Slices, each landed and working on its own, each with a `test-compositor`
boot that screenshots it:

| Slice | What | Points |
|---|---|---|
| Y1 | tokens: `yoru.conf` (floating, rounding, shadows, glow border, blur, curves) and its keys, themable title bars, the Nocturne wallpaper, Phosphor vendored; Yoru the default of `--everything` and the desktop image; boot `yoru` | 5 |
| Y2 | the dock: tole layer-shell windows; `yoru-shell` with workspaces, launchers, tools, pomodoro, clock; boot `yoru-dock` | 8 |
| Y3 | the launcher: fuzzel in Nocturne; then the palette in `yoru-shell` | 3 + 5 |
| Y4 | widget sidebars: clock and calendar, quick settings, to-do | 8 |
| Y5 | snapping as the design has it: quarters, layouts picker, guides, linked edges; tabs in the title bar | 13 |
| Y6 | helpers: scratchpad, focus mode, shortcut sheet, jump hints, clipboard history, toasts | 8 |
| Y7 | wallpaper picker (built: `yoru-shell --walls` on SUPER I, cover and grid, folders, colour filter and sorts, boot `walls`) and the accent from the wallpaper; parallax | 8 |
| Y8 | lock, notifications and switcher in the tokens; sounds | 5 |

Measured on the GPU path where it costs: the dock's and sidebars' blur
(`layerrule = blur`), the focused window's glow, and the workspace slide,
as frame times from hyprix's frame statistics with and without each.

### 6.4 The questions, answered (the customer, 2026-10-24)

1. **SUPER is the Yoru key, never Alt.** Alt+letter binds would take
   those keys from every program (a terminal's Alt+B, an editor's Alt+F),
   so Yoru binds none, and no Ctrl key either: the palette is `SUPER
   Space`, the workspaces `SUPER 1-4`, snapping `SUPER+arrows`, the
   scratchpad `SUPER S`. The design's `Alt` hints are drawn as `Super`.
2. **A person's own configuration wins.** Yoru is sourced first and their
   `hyprland.conf` after it; their own binds are the only ones (Yoru's
   keys are left out); the dock starts only where their file starts no
   bar; `--theme` overrides.
3. **The dock shows only what is installed**: the terminal always, the
   browser, Files (shelf) and Settings where the image has their desktop
   entries.
4. **Weather is Open-Meteo, strictly opt-in** (§6.6).
5. **The dock's round button is the FX mark**, with the person's own
   picture as a setting.

### 6.5 Where it stands

**Y1, the tokens, is built** (5 points estimated):

* `hyprix/themes/yoru.conf` (the look) and `yoru-keys.conf` (SUPER as the
  Yoru key; Alt bound at first, dropped on the customer's answer, §6.4): floating, rounding 8, the focused
  window's accent-600 ring, its deep drop shadow and accent glow, a lighter
  shadow elsewhere, blur for the shell's layers, Nocturne's curve, the
  ground `#161826`;
* the renderer draws `decoration:shadow:color_inactive` and a
  `decoration:glow` (a second shadow, centred, in its own colour, round the
  focused window; `color_inactive` for others), options that were read and
  not drawn;
* title bars styled by `hyprix:titlebar:*` (height, rounding, colours, the
  buttons' size and discs, the title's side and face, a top line of the
  accent's gradient fading over 48 px, a hairline at the foot); the
  defaults are Ferrix light's, so its look is unchanged;
* `docs/brand/yoru/yoru-nocturne.svg`, the wallpaper: the design's Nocturne
  gradient in the tokens, with a faint grain against banding (0.25 s to
  draw at 1080p);
* Phosphor 2.1.1 vendored in `assets/fonts/phosphor` with its licence;
* `--theme yoru|ferrix-light|none` on `run-compositor`, `flash
  --compositor` and `release-image`, Yoru by default: the theme is
  sourced first, so the person's own lines win, and a configuration of
  their own takes its look and not its keys; Yoru's wallpaper stands where
  the shipped Ferrix one would have been chosen, and a kept picture or
  video still wins;
* `test-compositor --boot yoru`: two pattern windows in the theme,
  screenshotted and judged by Nocturne's ground, the bars' surface and the
  accent's line (`~/.local/share/ferrix/yoru/shots/Y1-yoru-1024x768.png`).

Left of Y1: a window under a title bar has square bottom corners (as in
Ferrix light: the renderer rounds all four corners or none); the
animations' styles (`popin`, `slidefade`) are written and not yet drawn
by hyprix, which moves windows and does not yet slide workspaces.

**Y2, the dock, is built** (8 points estimated):

* tole windows can be layer-shell surfaces (`WindowOptions::layer`,
  `Window::set_layer`, `Window::set_input_region`; ferrix-os/tole);
* `yoru-shell`, a new app (ferrix-os/apps) on tole: the design's dock as
  a `yoru-dock` layer surface 60 px high, 6 px above the foot, its
  exclusive zone keeping windows clear of it; the frame of the three
  accents; the search; the workspaces 1 to 4 (and any other that exists)
  with a dot a window and the shown one lit; the launchers found among the
  desktop entries (terminal always, browser and Settings where the image
  has them) with a mark under a running one; the search, scratchpad and
  Settings tools; focus mode (`decoration:dim_inactive`); the pomodoro
  (25 and 5 minutes, its progress under it); the clock and date. It
  follows hyprix's `.socket2.sock` and asks `j/workspaces`,
  `j/activeworkspace` and `j/clients` after each event that moves
  something; a press is a dispatcher. `--render FILE` draws a sample dock;
  host tests for the model, the IPC events and a drawn dock;
* every desktop carries `/bin/yoru-shell` as it carries waybar; Yoru's
  theme starts it unless the configuration starts a bar of its own
  (`exec-once` of waybar);
* the title bars' face is `Inter Variable`, the family name of the
  vendored Inter;
* the `yoru` boot now carries the dock and judges it too, once the screen
  has stopped changing (`~/.local/share/ferrix/yoru/shots/Y2-yoru-dock-1024x768.png`).

**What it costs** (`compositor-render`'s ignored `yoru_frame_cost`, a
whole 1920x1080 frame of two windows over a wallpaper, the median of 20 on
virglrenderer's test server over this host's GL -- the path a guest's
frames take through QEMU -- and of 7 in software on this host):

| Frame | GPU path | Software |
|---|---|---|
| no decorations | 8.0 ms | 8.0 ms |
| + Yoru's shadows | 8.7 ms | 11.8 ms |
| + the focused window's glow | 8.3 ms | 13.3 ms |
| + the dock's blur behind it | 8.6 ms | 39.5 ms |

On the GPU path the look is within the noise of none; in software the
dock's blur is three quarters of the frame. A guest drawing in software
(no `--gl`) should keep the blur off: a layer rule after the theme,
`layerrule = blur false, match:namespace ^(yoru-.*)$`, or `decoration:blur:enabled = false`.
A host test also checks that the glow and the inactive shadow are the same
picture on the GPU as in software.

On the answers to §6.4: no Alt or Ctrl bind in `yoru-keys.conf` (the
theme's test refuses one), the dock's hints say `Super`, Files is shelf,
and the shell has the weather's opt-in rule (§6.6) and its test.

Left of Y2: the FX mark in the round button (a search glyph for now),
the dock's tooltips, its popovers (the stash) and the `Bar`
layout; auto-tile, the shortcut sheet, clipboard and wallpaper tools come
with their slices (Y5 to Y7); the clock is UTC or `TZ=UTC±h`, as waybar's.

**Y3's first half, the launcher in Yoru's look, is built** (3 points
estimated): `hyprix/themes/yoru-fuzzel.ini`, fuzzel drawn as the design's
command palette -- a card on the surface high on the screen, Phosphor's
search glyph as its prompt in accent-300, the picked row tinted with the
accent, Inter, radius 14 -- which `SUPER Space` and the dock's
search start fuzzel with (`--config`); a person's own `fuzzel.ini` is what
their own binds use. `test-compositor --boot yoru-launcher` opens it over
the Yoru desktop and judges its card
(`~/.local/share/ferrix/yoru/shots/Y3-yoru-launcher-1024x768.png`). The
palette's second half -- windows, actions and workspaces beside the
applications, in `yoru-shell` -- is still to come (5 points).

### 6.6 Privacy: the weather

The design's weather widget (Y4) asks Open-Meteo, on these terms (the
customer, 2026-10-24), which `yoru-shell`'s `weather.rs` holds before any
widget exists:

* **Off by default.** No network request of any kind until the person
  turns weather on *and* types a location. Nothing is asked at start, and
  there is no "detect my location": no IP-based geolocation, ever.
* **What goes where.** The coordinates, rounded to one decimal (about
  10 km), to `api.open-meteo.com`; or the place's name as typed to
  Open-Meteo's geocoding host, `geocoding-api.open-meteo.com`. The
  setting says so in those words. Nothing else is sent.
* **Kept locally.** The location is in the person's own settings file and
  nowhere else; turning weather off stops every request.
* **Checked.** A host test holds that an unconfigured shell, one turned
  off with a location kept, and one on with no location make no request,
  and that coordinates leave rounded.
