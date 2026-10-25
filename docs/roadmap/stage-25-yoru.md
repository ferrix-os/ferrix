# Stage 25 — Yoru, the main UI  ·  *63 points estimated; about 27 built*

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

* **Y1, the tokens (5)**: `yoru.conf`, the title bars, the glow, the
  Nocturne wallpaper, Phosphor, `--theme`; boot `yoru`.
* **Y2, the dock (8)**: `yoru-shell` on tole layer-shell surfaces;
  boot `yoru-dock`. The stash on the dock (the button with its count, files
  dropped on it, its panel; boot `stash`).
* **Y3's first half (3 of 8)**: fuzzel in Nocturne as the launcher; boot
  `yoru-launcher`.
* **Y4, the widget sidebars (8)**: both edges, reveal, pin, tabs, the clock
  and month, the to-do; boot `sidebars`. The weather and quick settings
  tabs say "not kept yet".
* **Y7's wallpaper picker**: `yoru-shell --walls` on SUPER I; boot `walls`.
* **Settings** shows what hyprix has in force, reading each option it shows
  with `hyprctl --batch getoption`.
* **GTK and Chrome in the tokens**: Nocturne for GTK 3 and GTK 4, and
  `portald`, which answers `org.freedesktop.portal.Settings` and no other
  portal (dark, the accent, the theme's name); Chrome's volumes 2 (GTK 3;
  boot `chrome-yoru`) and 3 (GTK's file chooser; boot `chrome-save`).

## Still to do

From `docs/DESKTOP.md` §6.3 and the "Left of" lines of §6.5 (there is no
separate gap list):

* Y1: a window under a title bar has square bottom corners; `popin` and
  `slidefade` are written and not drawn, and hyprix does not slide
  workspaces.
* Y2: the FX mark in the round button, tooltips, the `Bar` layout, the
  clock's time zone beyond UTC offsets.
* Y3's second half (5): the palette in `yoru-shell`, with windows, actions
  and workspaces.
* Y4: weather (opt-in only, §6.6) and quick settings.
* Y5 (13): snapping as the design has it: quarters, the layouts picker,
  guides, linked edges, tabs in the title bar.
* Y6 (8): scratchpad, focus mode, the shortcut sheet, jump hints,
  clipboard history, toasts.
* Y7: the accent from the wallpaper, and parallax.
* Y8 (5): lock, notifications and the switcher in the tokens; sounds.
* The Files app of the customer's design, as a follow-up: the dock treats
  Files as a shelf entry that shows where an image has it, and the app
  itself is not designed or sized here.

Nothing of the design is on a device other than QEMU and the customer's
desktops; the weather's rule (§6.6) holds before any widget exists.
