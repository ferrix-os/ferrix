# Stage 25 — Yoru, the main UI  ·  *63 points estimated; about 28 built*

The customer's design "Yoru WM" becomes the look and the shell of Ferrix's
desktop on hyprix: the default of `run-compositor --everything` and of the
desktop image, with Ferrix light still selectable and a person's own
`~/.config/hypr` and `~/.config/waybar` still winning where they exist.
[`docs/DESKTOP.md`](../DESKTOP.md) §6 is the design, the order of its
slices Y1 to Y8 and the customer's answers of 2026-10-24; this page is that
plan as a stage. Kernel lines: 0. It is hyprix, its clients, tole and xtask,
all outside the certified item.

**Exit.** Every slice of §6.3 landed with its `test-compositor` boot, so
that the desktop on x86-64 is the design's: tokens, dock, palette,
sidebars, snapping, helpers, wallpaper picker, and the lock, notifications
and switcher in the tokens, with GTK programs and Chrome drawn in the same
tokens. Not met.

## Built

The count is by slice: Y1 5, Y2 8, Y3 3 of 8, Y4 7 of 8 (the sidebars, the
clock, the to-do, the weather and most quick settings; volume, the
microphone, Wi-Fi, Bluetooth, night light and a light look not), Y7 5 of 8 (the
picker; the accent from the wallpaper and parallax not). The stash, Settings
live, and GTK and Chrome in the tokens are of the exit, outside the slices'
estimates. `docs/DESKTOP.md` §6.7 is the design feature by feature.

* **Y1, the tokens (5)**: `yoru.conf`, the title bars, the glow, the
  Nocturne wallpaper, Phosphor, `--theme`; boot `yoru`.
* **Y2, the dock (8)**: `yoru-shell` on tole layer-shell surfaces;
  boot `yoru-dock`. The stash on the dock (the button with its count, files
  dropped on it, its panel; boot `stash`).
* **Y3's first half (3 of 8)**: fuzzel in Nocturne as the launcher; boot
  `yoru-launcher`.
* **Y4, the widget sidebars (8)**: both edges, reveal, pin, tabs, the clock
  and month, the to-do; boot `sidebars`. The weather, strictly opt-in
  (§6.6); boot `weather`. Quick settings: focus mode, auto-tile, the theme
  colour (kept as Settings keeps them), do not disturb (notifyd's file),
  brightness where there is a backlight; boot `quick`.
* **Y7's wallpaper picker**: `yoru-shell --walls` on SUPER I; boot `walls`.
* **Files is shelf** on Yoru: the dock's Files, the stash's folder button
  and a folder's default handler open it; boot `files`.
* **Settings** shows what hyprix has in force, reading each option it shows
  with `hyprctl --batch getoption`.
* **GTK and Chrome in the tokens**: Nocturne for GTK 3 and GTK 4, and
  `portald`, which answers `org.freedesktop.portal.Settings` (dark, the
  accent, the theme's name) and `FileChooser` with Ferrix's own dialog,
  which GTK 3, GTK 4 and Chrome use on Yoru; Chrome's volumes 2 (GTK 3;
  boot `chrome-yoru`), 3 (GTK's file chooser) and 4 (a session bus, GTK 4;
  boots `chrome-save`, `gtk-chooser`).

## Still to do

From `docs/DESKTOP.md` §6.3, the "Left of" lines of §6.5 and the gap list
of §6.7:

* Y1: a window under a title bar has square bottom corners; `popin` and
  `slidefade` are written and not drawn, and hyprix does not slide
  workspaces.
* Y2: the FX mark in the round button, tooltips, the `Bar` layout, the
  clock's time zone beyond UTC offsets.
* Y3's second half (5): the palette in `yoru-shell`, with windows, actions
  and workspaces.
* Y4: the quick settings' volume and microphone (pulsed; capture first),
  Wi-Fi, Bluetooth, night light (gamma), a light Nocturne for Auto and
  Light; auto-tile per workspace rather than for all.
* Y5 (13): snapping as the design has it: quarters, the layouts picker,
  guides, linked edges, tabs in the title bar.
* Y6 (8): scratchpad, focus mode, the shortcut sheet, jump hints,
  clipboard history, toasts.
* Y7: the accent from the wallpaper, and parallax.
* Y8 (5): lock, notifications and the switcher in the tokens; sounds.
* The file chooser: several files at once, and choosing a folder.

Nothing of the design is on a device other than QEMU and the customer's
desktops; the weather's rule (§6.6) holds before any widget exists.
