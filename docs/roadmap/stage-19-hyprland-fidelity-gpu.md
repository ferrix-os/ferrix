# Stage 19 — Hyprland fidelity, and the GPU  ·  *178 points, about 16 left*

What makes it Hyprland rather than a tiling compositor: animations with its
bezier curves, rounded corners, blur and shadows, dimming and opacity rules,
special workspaces, groups, multi-monitor with per-monitor workspaces and
scaling, the plugin-shaped extension points, and the rest of `hyprctl`.

**Done — the window rules that were read and not obeyed (2026-09-17).**
`WindowRuleEffectContainer.cpp` has 55 effect strings. When the merged
0.56 grammar landed, 19 of them were carried out and the rest were kept by
name so that one unsupported word could not cost a person the matchers
beside it. Sixteen more are carried out now, and the ones that are not
each have a reason written down.

The ones that change how a window is *drawn* go through
`src/user/system/linux/compositor/render`: `rounding_power` (a superellipse rather than a
circle, which is the "squircle" a person sets the option for),
`border_color`, `decorate false` for a window that draws its own frame,
`opaque` for a client that leaves rubbish in its alpha channel,
`nearest_neighbor` for pixel art, and `dim_around`.

`dim_around` was on the list of effects said to need a second render
pass, and it does not: Hyprland darkens what is *behind* the thing that
asked for it, this renderer draws in order, so "behind" is "already
drawn" and one fill of the canvas just before that thing is the whole
effect. Both halves work -- `windowrule` for a dialog and `layerrule` for
a launcher.

The ones that change where a window *is* go through `src/user/system/linux/compositor/layout`:
`monitor`, `min_size`, `max_size`, `no_max_size`, `keep_aspect_ratio`,
`fullscreen_state` and `scrolling_width`. The size limits belong with the
window rather than with the rule that set them -- Hyprland clamps at
every point a size could change, and a rule fires once while a window is
resized many times -- so every floating rectangle in the layout goes
through one call that holds it down.

And two that are bookkeeping: `group set` makes a window a group of one
so the next window opened onto it joins it rather than splitting the
workspace, with all seven of Hyprland's group words read as
`applyDynamicRules` reads them; and `no_close_for` holds a window open,
with `killactive` saying why rather than doing nothing.

What is left, of the 55, is read and kept but not acted on:
`no_screen_share`, which does need a second pass -- drawing the frame again
without one surface in it; the five that ask a display for what virtio-gpu
does not offer -- tearing, variable refresh or HDR (`immediate`, `no_vrr`,
`no_auto_hdr`, `tonemap`, `force_rgbx`); `xray`, `content`, `animation`,
`idle_inhibit`, `no_anim`, `sync_fullscreen` and `render_unfocused`; and the
input ones. `persistent_size` is done (2026-09-18). `xray` is half done: the
layer rule landed on 2026-09-18, since it asks for the blur of what is
behind a bar, which is the picture the renderer's `Backdrop` already keeps,
but the window rule of the same name is still only recorded.

**Done — all four tiling layouts, and the options that shape them
(2026-09-17).** Hyprland 0.56 has four: `dwindle`, `master`, `monocle` and
`scrolling`. This compositor had two, and a person who wrote either of the
other names got dwindle with no diagnostic.

**Monocle** is the small one: every window fills the workspace and the
focused one is shown. What makes it a layout rather than a fullscreen
window is that the windows are still tiled -- `cyclenext` walks them,
closing one shows the next, and the gaps and the border are the
workspace's.

**Scrolling** is the largest of the four and the one no other tiling
compositor has: a *tape* of columns wider than the screen, with the screen
a window onto it. A column's width is its own, so a wide editor and a
narrow terminal sit side by side and a third column scrolls in beside them
without either of the first two changing shape.
`calculateCameraOffset` is the rule that makes it look right -- a tape
narrower than the screen is centred rather than pushed left, and a tape
wider than it never scrolls past its own start -- and eleven `layoutmsg`
words move windows between columns, resize them and scroll the tape.

The master layout grew the masters it was always meant to have: `addmaster`
and `removemaster` did nothing, and two masters sharing the master column
is the whole reason those messages exist. With them came
`master:orientation = center` (the masters in the middle with the stack in
two columns beside them, once there are `slave_count_for_center_master` of
them), `center_master_fallback`, `always_keep_position`, `new_on_active`,
`focus_master_on_close` and `allow_small_split`.

And twelve more options across the other categories: `dwindle:split_bias`,
`general:float_gaps`, `misc:background_color`,
`misc:close_special_on_empty`, the two `special_scale_factor`s that make a
scratchpad look like one, `binds:workspace_back_and_forth`,
`binds:hide_special_on_workspace_change`, `binds:allow_pin_fullscreen`,
`binds:movefocus_cycles_fullscreen`,
`binds:window_direction_monitor_fallback`, and `workspace previous`,
`next`, `empty` and `name:` as dispatcher arguments -- `workspace,
previous` being the commonest keybind in any Hyprland configuration after
the numbers themselves.

The whole `binds` category was unreachable before this: `bind` takes its
flags as letters glued to the keyword -- `bindl`, `bindrm`, `bindel` -- and
the parser reached for a bind before an option, so
`binds:workspace_back_and_forth` was read as `bind` with a flag `s` and
answered `invalid flag :`. A keyword never has a colon in it and an option
always does.

**Done — a person's own configuration, run (2026-09-17).** The test of a
clone is not a checklist, it is somebody's real file. This one is
`~/.config/hypr/hyprland.conf` on `example`: 377 lines, 55 binds, two window
rules, a bar, a dock, a wallpaper daemon and a desktop-effects daemon. It
runs, with no diagnostic at all, and the screenshot has waybar across the
top, the wallpaper behind it and a terminal tiled under it with the
gradient border and the graded blur the file asks for.

Getting there found six things, each of which was a person's configuration
being read and quietly not obeyed.

* **The window and layer rules were 0.55's.** 0.56 merged `windowrulev2`
  into `windowrule` and gave `layerrule` the same grammar -- comma-separated
  fields, `match:` for what a thing must be, snake_case names -- and this
  read the older one. `layerrule = ignore_alpha 0.2, match:namespace waybar`
  was refused as a line. Both halves are `Rule.cpp`'s whole matcher list and
  `WindowRuleEffectContainer.cpp`'s whole effect table now, and an effect
  Hyprland has that this compositor does not carry out is kept by name
  rather than refused: one unsupported word must not cost a person the
  matchers written beside it.
* **`suppress_event maximize` had nothing to suppress.** A client asking
  `xdg_toplevel.set_maximized` reached the loop and nothing read it, so the
  rule -- the first line the file writes -- was a rule about a thing that
  never happened. A client asking for fullscreen or maximize gets it now,
  unless a rule says otherwise.
* **`wl_shm_pool.destroy` unmapped the memory.** The protocol says the
  mapping goes when the last *buffer* made from the pool does, and a client
  that makes its buffers and throws the pool away is not unusual -- it is
  what `grim` does between asking for a screenshot and taking it, and what
  most toolkits do. Nothing could take a screenshot of this compositor but
  its own client.
* **`wl_output.description` was a fixed sentence.** A bar told to be on one
  screen matches on the description, not the connector: waybar's `"output"`
  is `Lenovo Group Limited R27qe Gen2 UTP03KBB`, it matched nothing, and
  waybar correctly drew no bar and said nothing about it. A monitor's
  description comes out of its `EDID` now, read through
  `DRM_IOCTL_MODE_GETPROPBLOB`, and `monitor = desc:` matches the start of
  it as `CMonitor::matchesStaticSelector` does.
* **`input:kb_layout = de` was read and never looked at.** Every client was
  handed the `us` keymap and every bind resolved against it, so a German
  keyboard typed `y` where its key says `z`. The probe takes a layout now
  and the compositor ships one keymap per layout, `layout()` picks one, and
  a layout that is not shipped says so rather than quietly typing English.
* **The option table held 71 of Hyprland's 348.** A configuration naming any
  of the other 277 was told `config option does not exist`, which is the
  right answer for a typo and the wrong one for an option Hyprland has. Of
  the 71 that were there, 70 already held Hyprland's default exactly.

"No diagnostic at all" was not so (found 2026-09-26). The parser gave the
same file 31. 24 were its banner lines -- `################` over
`### MONITORS ###` -- which it read as a literal `#` where hyprlang
takes any line that begins with one as a comment. The other 7 were its
`bezier` and `animation` lines, written inside `animations { }` and read as
the options `animations:bezier` and `animations:animation`, so its curves
and speeds were refused and Hyprland's defaults drawn instead. hyprlang
tries the category's option first and then a keyword by the bare name,
wherever the line stands. Both are fixed and the file reads with none.

And the blur, which is the compositor's whole frame budget, ran over the
whole window whatever the damage said: a terminal's cursor blinking cost 92
milliseconds of a 1920x1080 screen where blurring what changed costs 1.8.
The pixels are the same either way, because a blurred pixel depends on
nothing further than the kernel's reach -- which turned out to be twice what
was being read, so the outermost ring of every blurred window was a blur of
the region's clamped edge rather than of the frame.

That was not enough to make a blurred desktop usable, and two more things
were (2026-09-17). A blur of the frame as it stands cannot be redrawn a strip
at a time -- outside the damage the canvas holds the window drawn over its
own blur -- so every pointer motion over a translucent window, and every
letter typed into one, redrew and blurred the whole of it: 120 ms a frame
at 1920x1080, in a loop that reads its input once a frame. And `foot`
commits `ARGB8888` whatever its opacity, so that was every terminal. A tiled
window now takes its blur from `compositor_render::Backdrop`, which is
Hyprland's `m_blurFB` (`decoration:blur:new_optimizations`, on by default):
what is behind the windows and the blur of it, kept from frame to frame and
blurred again only where a wallpaper or a bar under the windows changed.
The other was a copy nobody had timed: a client's padded buffer was gathered
into tight rows *whole* for every blend, three milliseconds for a
full-screen terminal's one changed cell. The same frame is 0.06 ms now, and
the compositor's share of a core with a bar and a terminal went from 62% to
6%. Twelve expected images moved, all of them darker inside a translucent
tiled window and nowhere else: the blur they held had the window's own
shadow and, under a rounded window, its border's fill in it, and Hyprland's
has neither. `docs/COMPOSITOR-DAMAGE-HANDOFF.md` has the measurements.

A cheap frame then showed what a dear one had hidden: nothing paced them. A
frame for every report of the mouse, each ending on Ferrix's virtio-gpu in
the whole framebuffer sent to the host, and the pointer stuttered over
everything in `run-compositor`. A change is owed a frame at the screen's
refresh now (`hyprix::pace`), and a virtio-gpu is one buffer told what
changed with `DRM_IOCTL_MODE_DIRTYFB` rather than two flipped whole. The
pointer boot sweeps the pointer through four hundred places and holds both:
160 frames for the sweep before, 60 after, and the last picture exact.

And a wallpaper that moves -- `mpvpaper` behind a translucent terminal,
which changes everything behind the windows every frame -- took each part
of a frame in turn. A window's shadow is drawn where it shows rather than
under the whole of a window that is about to be written over it (12.8 ms to
0.6); and `compositor_render`'s large operations, every pass of the blur
and a surface blended or copied in, are cut into bands of rows drawn a band
a thread, which is the same bytes on one thread as on sixteen (the blur 85
ms to 20, the terminal 7.0 to 1.1, the video 5.8 to 1.1). A frame that owes
a whole blur is 22 ms, which is a 30 fps video kept up with.

**Done — the screen under QEMU is the customer's monitor (2026-09-26).**
`run-compositor` reads the host's R27qe EDID by its description, carries it
to `/lib/firmware/edid/` with the host's `pnp.ids`, and names it with
`drm.edid_firmware=`, which the display core reads as Linux's DRM core does
and serves as the connector's `EDID` property and `GETPROPBLOB` blob. The
screen is then `Lenovo Group Limited R27qe Gen2 UTP03KBB (Virtual-1)`, so
the customer's `monitor = desc:` line and waybar's `"output"` find it.
hyprix now picks a monitor's rule as Hyprland does -- a named rule over the
catch-all below it -- and moves the pointer over a lone screen at `2560x0`
rather than the empty space left of it. `test-compositor --boot edid` is the
gate; `docs/DISPLAY.md` §7 has the decision (kernel, not driver) and what is
left.

**Done — the frame stopped mapping its memory every time (2026-09-19).**
In the guest -- KVM, four processors, the video wallpaper behind a
translucent terminal -- the frame that owes a whole blur was 77 ms where the
host draws it in 22, and most of the difference was `mmap`: every plane of
the blur and every gathered surface was a fresh allocation over musl's
128 KiB threshold, so a frame was some 25,000 page faults under the address
space's lock and seven shootdowns. `compositor_render::scratch` keeps those
buffers per thread, and the video client keeps its scaled frame and copies
into each buffer only the rows it lacks. 46 ms at four processors, 38 at
sixteen; 37 of the 46 is the renderer's own arithmetic and 5.5 the card's
transfer. That is the software renderer's floor, and the customer's answer
to it is `docs/GPU.md`'s Path A, taken first before the AV1 wallpaper
(`docs/COMPOSITOR-DAMAGE-HANDOFF.md` §2.7).

**Done — a video wallpaper stopped being everybody's problem (2026-09-19).**
Software-rendered video playback did not merely cost the frames it drew: it
made the whole desktop stutter, the pointer and the bar with it. Three things
were wrong, and only the first is about drawing at all.

* **A thread per band, per operation, per frame.** `compositor_render` spread
  its rows with `std::thread::scope`, which starts the threads and joins
  them. A blurred frame is six blur passes and the conversions either side of
  them, so a 30 fps wallpaper that moves was on the order of a thousand
  threads started and ended a second. The bands are the same bands now, given
  to workers that are already there: `src/user/system/linux/compositor/fan`, started once, waiting
  on a condition variable between frames. The thread that asks for the work
  is one of them, which is why the pool starts one fewer than there are
  cores and a single-processor guest starts none.
* **Every one of those exits interrupted the whole machine.** Ferrix frees an
  exited task's kernel stack by invalidating its address on every processor
  and waiting for each to answer, and the idle loop's reaper did that one
  stack at a time -- so a program whose threads were short-lived spent every
  other program's time, which is the cost landing on the wrong task. It frees
  a batch of sixteen under one shootdown now, and while any other processor
  is still working a partial batch waits rather than interrupting it; when
  the machine is quiet, whatever is there goes under one. `cargo xtask
  test-boot` prints the number on its `tasks` line, and stage 5's thousand
  threads went from 1015 shootdowns to 826 -- a boot that is idle between its
  phases, and so the mild end of what this costs.
* **Nothing could be told to matter less.** `setpriority` stored a nice value
  and the scheduler never read it, and `sched_setscheduler` refused every
  policy but `SCHED_OTHER`, so a video decoder and the compositor drawing its
  frames competed on exactly equal terms. `ferrix_sched` has carried weights
  since stage 5; a nice value now becomes one, on every task of the process
  and on the threads it starts afterwards. `RunQueue::set_weight` keeps what
  an entity is owed in *real* time across the change, as Linux's
  `reweight_entity` does, so a renice neither hands out a turn nor takes one
  away. Namespaces and cgroups are still stage 13; this is what there is
  before them.

**Done — the rest of Hyprland's dispatcher table (2026-09-17).**
Twenty-seven names in Hyprland's `m_dispMap` had no answer here; every one
of them does now. The split is by what they touch. `src/user/system/linux/compositor/layout`
answers the ones that move windows -- `layoutmsg` (both layouts' own
messages: `togglesplit`, `swapsplit`, `movetoroot`, `preselect` for dwindle,
and `swapwithmaster`, `focusmaster`, `mfact`, `orientation*`, `swapnext`,
`rollnext` and the rest for master), `moveintoorcreategroup`,
`movewindoworgroup`, `focusworkspaceoncurrentmonitor`, `movewindowpixel`,
`resizewindowpixel` -- and a new `hyprix::act` answers the ones that reach
past it: starting a program, signalling one, turning a screen off, moving
the pointer, dragging a window with the mouse, writing a line on the event
socket, and ending the session.

Four dispatchers name a window with one of Hyprland's *window expressions*
rather than a direction, and the layout holds neither a title nor a class
nor a process. `hyprix::select` is `CViewQuery::bySelector` written out:
`class:`, `initialclass:`, `title:`, `initialtitle:`, `tag:`, `address:`,
`stableid:`, `pid:`, `floating`, `tiled`, `active`, and a bare expression
read as a class -- each matched against the *whole* field, as RE2 matches
one. To answer `pid:` at all, the socket now asks `SO_PEERCRED` who
connected, which is also why `hyprctl clients` stopped printing `pid: 0`.

Two things this found. The dwindle layout could not exchange two windows, so
`swapwindow` did nothing in the default layout; it swaps the two leaves now,
the way `switchWindows` does, leaving every split's ratio alone. And
`misc:focus_on_activate` is *off* in Hyprland -- a program asking for
another's window makes it urgent rather than taking the focus -- which this
compositor had as always-on; it is the option now, with the urgency list
`focusurgentorlast` reads.

`toggleswallow` keeps its flag and says that swallowing a terminal is not
implemented, because it is not. A dispatcher that quietly did nothing would
be worse than one that says so.

**Done — the protocols a desktop session asks for (2026-09-17).** Eleven
more, each small and each bound by something a person runs. `xdg-output`
gives a bar the screen's *logical* position, size and name, which on a
scaled monitor is not what `wl_output.mode` says. `presentation-time` says
when a frame actually reached the screen, which is what a toolkit that
animates needs and what a frame callback does not say. `ext-idle-notify` and
`idle-inhibit` are the two halves of "is anyone there": a locker waits on
the first, a video player holds it off with the second, and the `forceidle`
dispatcher drives the clock so a person can test a locker without waiting
ten minutes. `single-pixel-buffer` is a `wl_buffer` that is one colour and
has no pool at all. `content-type` and `alpha-modifier` are a client saying
what it is showing and how much of it shows -- the second is drawn, so a
client can fade itself. `xdg-dialog` floats a modal dialog, which is
Hyprland's `windowrule = float, xdg_dialog` said by the protocol itself.
`xdg-system-bell` and `xdg-toplevel-tag` are the terminal bell and the name
a window keeps across restarts. `kde-server-decoration` is KDE's own
`xdg-decoration`, answered with the same `Server`.

**And the frame callbacks, which were never fired.** A client that asks for
`wl_surface.frame` and waits for it before drawing again -- which is every
toolkit -- drew one frame on this compositor and then stopped. The tree's
own clients draw once and never noticed. `foot` now draws five frames in the
same run where it drew one, and every surface on the screen is told: the
windows, the bars, the menus and the lock's own.

**Done — the pointer and keyboard protocols beyond `wl_seat` (2026-09-17).**
`wl_pointer` says where the pointer *is*, which is the wrong question for a
game, a 3D modeller or a remote-desktop viewer: they want how far it moved,
and they want it to stay inside their window while they have it.
`zwp_relative_pointer_v1` and `zwp_pointer_constraints_v1` are that pair, and
both are carried out rather than answered: a locked pointer does not move at
all and the client is told the distance instead, a confined one is clamped to
its window's rectangle, and a one-shot constraint is destroyed by the event
that ends it. A constraint is in force only while its own surface has the
pointer, which is the compositor's judgement and not the client's.

`zwp_keyboard_shortcuts_inhibit_manager_v1` is how a virtual machine or a
nested compositor gets `SUPER` instead of the compositor eating it: while the
inhibiting surface has the keyboard, no bind fires at all.

`zwp_virtual_keyboard_v1` and `zwlr_virtual_pointer_v1` are a client acting
as a device -- `wtype`, `ydotool`, an on-screen keyboard, a remote viewer.
What they report goes to the seat as a person's input would, keybinds and
all, which is what makes them worth having and what wlroots gates behind a
compositor's policy; this one offers it to every client, as Hyprland does.

`zwp_pointer_gestures_v1` is offered and never sent to, and says so: a
touchpad's swipe and pinch come from libinput's gesture recogniser and this
compositor reads evdev directly. A toolkit that binds it and hears nothing
behaves as it does on a machine with a mouse; one that finds no global warns
on every start.

That machinery is also what `pass`, `sendshortcut` and `sendkeystate` needed.
A `wl_keyboard` has one surface at a time, so sending a key to a window that
is not focused means handing it the keyboard for the length of the key and
handing it back -- which is what Hyprland does too. `pass` sends *the key
that fired the bind*, so the seat now carries the trigger through to the
dispatcher.

**And a bug the clipboard boot found while this landed.** A connection
ending compacts the slot list, and three things held a client by its *place*
in that list and were never moved with it: the clipboard's owner, the
session lock's client and the input method's. A window's `Source` and the
keyboard focus had already been fixed for exactly this; these three had not.
What it looked like was a paste answered by nobody while the program that
copied sat waiting to be asked -- once in about ten boots, whenever a
clipboard client happened to exit before another pasted. `Clipboard::renumber`
is host-tested, and the test fails without the fix.

**Done — what a taskbar, a clipboard manager, a night-light and a settings
panel ask (2026-09-17).** Seven more protocols, each bound by a program
people run rather than chosen from a list.

`ext-foreign-toplevel-list-v1` is the window list as the newer specification
has it -- the same job `zwlr_foreign_toplevel_v1` does with the acting-on-a-
window half taken out, and the one a taskbar written this year binds. Both
are published from the same list each pass.

`wlr-data-control` and `ext-data-control` are the clipboard as a *manager*
sees it. `wl_data_device` gives a client the selection only while it has the
keyboard, which is right for an application and wrong for `cliphist` or
`wl-paste --watch`: they have no window at all. Both selections are carried
to them whether or not anything is focused, and a manager can set either as
well as read it. They are the same protocol twice -- wlroots wrote the first
and the `ext` namespace standardised it -- so they are one module with a
table of interfaces, the way `src/user/system/linux/compositor/clip` is one program with a flag.

`wlr-gamma-control` is `gammastep` and `hyprsunset`. The client hands over
three ramps on a descriptor and every pixel is looked up in its channel's
ramp on the way to the screen. On hardware the connector does that; here the
screen is memory, so the compositor does it once a frame over the pixels it
drew -- the same picture by a slower road.

`wlr-output-power-management` is `wlopm`, which is the `dpms` dispatcher
reached from a program instead of a keybind.

`wlr-output-management` is `kanshi` and `wlr-randr`: every screen with its
mode, its position and its scale, published with a serial, and a whole
arrangement taken back at once. A configuration made against a stale serial
is `cancelled` rather than applied, which is the one part of that protocol a
compositor must not skip. Moving a screen is carried out --
`State::move_monitor` keeps the workspaces where they are, because moving a
screen is not unplugging it.

`ext-workspace-v1` is the workspace numbers a bar draws, one group a
monitor, which until now every Hyprland bar read out of `hyprctl`.

**The eighteenth boot.** `zwp_virtual_keyboard_v1` had no proof that a
client's keys reach the seat, and that is the whole point of the protocol.
So: one key starts `/bin/vkbd`, `vkbd` types `SUPER Q` on the Wayland
socket, the bind fires, and `closewindow, title:^(one)$` closes the window
that expression names. The screen must be the picture `src/user/system/linux/compositor/render`
blesses for the window that is left. Two new things in one picture -- a
client acting as a device, and a dispatcher picking a window out by title.

**Done — Hyprland's own protocols (2026-09-17).** Six, each written for
something Hyprland does that no other compositor had a protocol for.

`hyprland-global-shortcuts-v1` is how a screen recorder or a push-to-talk
program has a key without reading the keyboard: it registers a *name*, the
person binds a key to `dispatch global <app_id>:<id>`, and the program hears
`pressed`. That is also what finally makes the `global` dispatcher mean
something.

`hyprland-focus-grab-v1` is a launcher holding the focus on its own surfaces
until a click lands outside them. `hyprland-lock-notify-v1` tells a program
that is *not* the locker when the screen locks -- a recorder or a notifier
has no other way to know, because `ext-session-lock-v1` is the locker's own
protocol and says nothing to anybody else.
`hyprland-toplevel-mapping-v1` joins a toplevel handle from either window
list to the address every other protocol calls that window by.
`hyprland-surface-v1` is a surface asking to be drawn see-through, which is
the same field `wp_alpha_modifier_v1` sets.
`hyprland-toplevel-export-v1` is `zwlr_screencopy_v1` for one *window*,
which is what a recorder uses for "share this window": the same two halves,
answered out of the same pixels, with the window's rectangle in place of a
screen's.

Two are not offered, and the module says why. `hyprland-input-capture-v1`'s
whole conversation is a `libei` socket the compositor hands over, and there
is no `libei` on Ferrix; offering the global and never sending the
descriptor would leave a client waiting for ever.
`hyprland-ctm-control-v1`'s `blocked` event has a `<description>` with no
`summary`, which this `wayland-scanner` refuses -- so its table could not be
checked against libwayland's, and an unchecked table is the one thing the
generator exists to avoid. The colour work it does is
`wlr-gamma-control`'s as well, and that one is offered.

**Done — `layerrule` (2026-09-17).** The `zwlr_layer_shell_v1` half of
`windowrule`. A layer surface has no title and no application id -- it has a
*namespace*, which is what it passed to `get_layer_surface`, and that is what
a rule matches on, as a regular expression.

Three are drawn. `blur` blurs what is behind a translucent bar, which is what
makes one look like Hyprland's, and is a rule rather than the default because
blurring behind an opaque bar costs a pyramid of passes and changes not one
pixel. `above_lock` draws the surface *over* the session lock -- the whole
reason an on-screen keyboard can be used on a lock screen, and until now the
compositor drew nothing over a lock at all. `order` decides where a surface
goes among its own layer's, a higher number nearer the top, with the sort
stable so that surfaces with the same order keep the sequence their clients
made them in.

The rest were read, kept and not acted on when this landed, and the module
says why each. Since then `dim_around` and `xray` are drawn too, from the
renderer's `Backdrop`. Still only kept: `no_anim`, which has nothing to turn
off; `blur_popups` and `no_screen_share`, which need a second render pass --
they read what is *behind* the frame being drawn, or need the frame drawn
again without one surface in it; and `ignore_alpha` and `animation`. They are
parsed rather than refused so a person's configuration is not a wall of
diagnostics. The names are Hyprland 0.56's, in snake case (`above_lock`,
`dim_around`, `no_anim`); the older run-together spellings are refused.

**Done — drag and drop (2026-09-17).** The most intricate conversation in
core Wayland, and the one every file manager, browser and editor uses.
`wl_data_device.start_drag` was read and dropped; there was no drag at all.

Three objects talk at once and the two clients cannot see each other, so
every step is the compositor's. It makes the *offer* for whichever client
the pointer is over, names every type the source put on it, says what the
source can do, and enters -- in that order, because a client reads the types
inside its `enter` handler and one told afterwards would have nothing to
read. It tells the source which type that client said it would take, settles
the action the two agreed on (the target's preference where both offered
it), and, when the last button comes up, tells one to drop and the other
that the drop happened. A drop over nothing, or over a client that would
take no type, cancels the source -- which is what stops a file manager
deleting the original after a move that went nowhere.

Two rules that are easy to miss and were written down here. While a drag is
on, the pointer enters and leaves nothing: a window told `wl_pointer.enter`
mid-drag would think the person had clicked it, so the pointer's own events
stop for the length of the drag. And the offer is destroyed by the `leave`,
so a client that walked the pointer across three windows does not end up
holding three offers.

The icon the source gave is drawn at the pointer and under it, because what
a drag *looks* like is a thing following the pointer.

**Done — the last of the protocols (2026-09-17).** Nine more, which brings
what this compositor offers to **60 globals** against Hyprland's 60.

`wp_pointer_warp_v1` is a client putting the pointer somewhere *inside its
own window*, which a game's settings panel and a drawing program both want;
the surface has to be one the client owns, and one it does not is a protocol
error. `ext-background-effect-v1` is a surface asking for what is behind it
to be blurred -- `layerrule = blur` said by the protocol instead of by the
person -- and it is drawn.

`tearing-control-v1`, `fifo-v1` and `commit-timing-v1` are a client saying
how it would like its frames scheduled. Each is read and recorded and acted
on by nothing, and the module says why: acting on any of them means choosing
*when* to put a frame on the screen. When this landed the compositor drew
when something changed and presented at once; since 2026-09-18 it paces its
frames to the screen's refresh by its own clock (`hyprix::pace`), which
still takes no account of what a client asks about timing.

`wp_security_context_manager_v1` is a sandbox handing over a socket of its
own; the descriptors are taken and closed and the sandbox is named in the
log, because this compositor accepts on one listener and telling a flatpak's
clients apart would mean a second. `vicinae-hotkey-v1` is a launcher asking
for a key by keysym rather than by registering a name.

`ext-image-capture-source-v1` and `ext-image-copy-capture-v1` are
screenshots as the `ext` namespace has them: a *source* -- a screen, or a
window from either toplevel list -- and a session that copies frames out of
it one after another. That is what a recorder actually needs, and it is what
a `grim` or an `xdg-desktop-portal` written this year binds. It is answered
out of the same pixels `zwlr_screencopy_v1` is.

**What is left, and why.** Six of Hyprland's globals are not offered, and
`zwp_linux_dmabuf_v1` besides. `wl_drm`, `wp_linux_drm_syncobj_manager_v1`
and `wp_color_manager_v1` wait on clients that draw on the GPU themselves --
Mesa on ferrousli and `zwp_linux_dmabuf_v1` -- since the compositor's own
GPU path (2026-09-19) needs none of them; `xwayland_shell_v1` waits on
XWayland. `hyprland-input-capture-v1`'s whole conversation is a `libei`
socket, and there is no `libei`. `hyprland-ctm-control-v1`'s XML has a
`<description>` with no `summary`, which this `wayland-scanner` refuses, so
its table could not be checked against libwayland's -- and an unchecked
table is the one thing the generator exists to avoid.

**Done — a terminal (2026-09-17).** Stage 18's exit asked for one, and it
needed pseudoterminals the kernel did not have. It has them now:
`/dev/ptmx` gives a master, `TIOCGPTN` says which pair it is, `TIOCSPTLCK`
unlocks it and `/dev/pts/<n>` is the slave. What the master writes goes
through the same line discipline the console's terminal has -- `ICANON`,
`ECHO`, `ISIG` and the rest, honoured exactly as they are there -- and the
echo goes back to the master, because on a pseudoterminal the *terminal* is
the program at that end. What the slave writes has `OPOST` applied and is
read by the master. Closing the master takes the pair away: the slave's
reads end and its writes fail, which is what a shell reads as "the terminal
has gone". The slave answers every terminal request, including the
session and process-group ones, with a pair's own session and foreground
group; the master answers those that act on the pair, as Linux's does.

The term app (ferrix-os/apps since 2026-10-04) is the terminal: a character grid with the escape
sequences a shell and its programs actually send (the cursor, the erases,
the colours, the cursor's visibility), drawn with Hack, antialiased --
The app's `tools/gen-font.py` rasterises the TrueType faces vendored beside
it into coverage cells, and the terminal blends them over the cell's
background -- into a `wl_shm` buffer. It starts a program on a pair with
the slave for its session and its three descriptors, sends what is typed
back through the master, and tells the program when the window is resized.
`--headless` runs the program with no window at all, which is what
`cargo xtask test-pty` boots: a program's output, through the pair, into the
grid, printed a row at a time.

`cargo xtask test-compositor` boots a ninth time with `exec-once = /bin/term
/bin/hyprctl version` and requires the picture the terminal makes, pixel for pixel,
on x86-64 and on AArch64.

**Done — scrollback, the pointer and the clipboard in the terminal
(2026-09-26).** The grid keeps the last 10,000 rows that scroll off the top,
and the wheel (three rows a notch) and shift with Page Up and Page Down look
back through them. A drag selects, a double click takes a word and a triple
click a line, held in absolute line numbers so the selection moves with its
text. Control-shift-C copies it through `wl_data_device` and control-shift-V
pastes, bracketed when the program asks with `CSI ? 2004 h`. The program on
the terminal now gets the terminal's environment -- it had `TERM` alone, so
nothing the shell started had a `PATH` or a `WAYLAND_DISPLAY`. Host tests
hold the grid's rules; a boot driven over VNC checked the rest. Left: the
primary selection and its middle click, an alternate screen (so a full-screen
program's rows do not fill the scrollback), and bracketed paste in zinc,
which never asks for it, so a pasted block runs a line at a time.

**Done — the eight protocols a real toolkit asked for (2026-09-17).** Not
chosen from a list: `foot`, a Wayland terminal written against libwayland and
every other compositor, prints a warning line for each protocol it wanted and
did not find. It printed six, and the seventh and eighth are the two halves
of the one it named last. `hyprix/probe/real-client.txt` is that log, and it
now has no warning in it at all.

* **`wp_cursor_shape_v1`** -- a client *naming* the cursor it wants rather
  than drawing one, which is what a toolkit would rather do: it has no idea
  what the person's theme looks like and the compositor does. This one draws
  its own arrow for every shape and says which was asked for; there is one
  shape and no theme to pick another from.
* **`zwp_primary_selection_device_manager_v1`** -- the middle-click paste,
  which is the clipboard's older and simpler sibling and the same protocol
  under another name. `src/user/system/linux/compositor/clip` grew `--primary` rather than a twin,
  and the compositor's clipboard carries both selections apart.
* **`xdg_activation_v1`** -- one program asking for another's window to be
  raised: a link opened from a chat window raising the browser. The token is
  a string the compositor makes and only it can make, and one it did not make
  is refused, which is the whole of what stops any program stealing the focus
  whenever it likes.
* **`wp_viewporter`** and **`wp_fractional_scale_v1`** -- a client saying its
  buffer is to be cropped or scaled into its surface, and the compositor
  telling it a scale that need not be a whole number. This compositor's
  monitor scales are whole, so the preferred scale is that number in the
  protocol's 120ths and is sent at once rather than left for the client to
  wait on.
* **`xdg_toplevel_icon_v1`** -- the icon a taskbar draws beside a window's
  name. The name is kept, which is what a taskbar looks up in an icon theme;
  the buffers are accepted and not kept, because this compositor draws no
  icon itself and holding a client's pixels for something nobody draws is
  memory nobody asked for.
* **`zwp_text_input_v3`** and **`zwp_input_method_v2`** -- the two halves of
  typing through an input method. The application says it wants text, the
  method says what was typed, and the compositor is what joins them: they are
  two connections and neither can see the other. One input method a seat; a
  second is told `unavailable`. With none running, a text field is told
  nothing, which is a session with no IME and is the truth rather than a
  pretence.

The eleventh boot of `cargo xtask test-compositor` now copies to both
selections and pastes each back, with different text in each: a compositor
that answered a primary paste from the clipboard would pass with one string
and fail with two.

**Done — the pointer (2026-09-17).** A compositor with a mouse and no arrow
on the screen is one a person cannot use, and there was none: `wl_pointer`
carried motion and buttons to the clients and nothing was ever drawn.

The arrow is in `src/user/system/linux/compositor/render`, in code, as a shape rather than as a
file: Hyprland loads an XCursor or a `hyprcursor` theme and Ferrix has
neither the files nor a library to read them with, so the one this draws is
written out -- a 24x24 left-pointing arrow with a black outline and a white
fill, every pixel opaque or clear, its tip at the pointer. A client replaces
it with `wl_pointer.set_cursor`, which is how a text field shows an I-beam
and a link a hand; a client that asks for a null surface gets no pointer at
all, which is what a video player playing full screen does.

It is drawn over everything -- windows, bars and menus -- because a pointer
that goes under a menu is one nobody can follow, and it is not drawn while
the session is locked, because a lock screen draws its own.

**It is also not drawn until it has moved.** The seat starts the pointer in
the middle of the screen, which is a guess: nothing has said where the mouse
is until a device does. An arrow drawn at a guess is worse than none, and a
machine with a mouse plugged in and never touched should look like a machine
with no mouse.

`cargo xtask test-compositor` boots a seventeenth time and takes two
pictures: the tiled pair with nothing on it, and then -- after QMP moves the
mouse -- the same pair with the arrow's tip where it was put, which must be
the picture `src/user/system/linux/compositor/render` blesses for exactly that.

**Done — menus (2026-09-17).** `xdg_popup`, which is what every right-click
menu, dropdown, tooltip and combo box in every toolkit is. The objects were
being made and nothing else: a positioner was a bag of numbers nobody read,
`get_popup` handed back an id and never a `configure`, and a client that
asked for a menu waited for ever. On a compositor like that a person right-
clicks and nothing happens.

Where a popup goes is `xdg_positioner`'s arithmetic and nothing else -- a
rectangle on the parent to hang off, a point of it to anchor to, a direction
to grow in, an offset, and what to do when the result falls off the screen
-- so it is written in `src/user/system/linux/compositor/layout` with the rest of the geometry,
where it is tested against the rules rather than against a screenshot. The
order is the protocol's: anchor, offset, gravity, then `flip`, `slide` and
`resize`, each on the axis that is off the screen and only if the client
asked for it. A flip that would not help either is not made, which is what
stops a menu jumping to the other side for no gain; a slide takes the far
edge first, so a popup wider than the screen ends flush with the near one;
a resize is the last resort and the only one that gives the client
something other than the size it asked for.

The compositor places each popup against its parent's rectangle -- a
window's, or another popup's, since a submenu is a popup on a popup --
clips it to the monitor that parent is on, and draws it over the windows
with no border and no gaps, which is what a menu is. `grab`, `reposition`
and `popup_done` are all answered; `set_parent_size` and
`set_parent_configure` are read and dropped, because this compositor places
a popup against the parent's geometry as it is.

`cargo xtask test-compositor` boots a sixteenth time with `exec-once =
/bin/pattern checkerboard one --menu 200`: the window asks for a menu the
moment it has drawn, as a toolkit would, and the screen must be the picture
`src/user/system/linux/compositor/render` blesses -- which it builds by calling the same placement
rules -- with the client having been told where it was put. A host test
compares a screenshot of the same thing.

**Done — the screen lock (2026-09-17).** `ext-session-lock-v1`, which is
what `hyprlock` speaks and `swaylock` speaks, and the one protocol whose
whole point is that the compositor stops drawing everything else.

A program binds the manager and asks for the lock; from that moment the
compositor draws no window, no bar and nothing of what was on the screen a
second ago -- *before* the program has drawn anything, which is the part the
protocol is most particular about. It makes an `ext_session_lock_surface_v1`
for each screen, is configured at that screen's exact size, and when every
screen is covered it is told `locked`, which is the compositor's judgement
and nobody else's. `unlock_and_destroy` gives the screen back.

The keyboard goes with it. While the session is locked the only binds that
fire are the ones written `bindl` -- which is what that flag has always been
for, and what keeps the volume keys working on a locked screen -- and every
other key goes to the lock's own surface and to no window. `hyprctl locked`
says so.

A lock whose program *dies* leaves the screen locked with nothing drawn on
it. That is the one thing the protocol insists on, and the one thing a
compositor gets wrong by doing nothing: an unlocked session is not what a
crash is allowed to produce. A second program asking to lock while one is
held is sent `finished` and refused.

`src/user/system/linux/compositor/lock` is `hyprlock` with the password taken out: it locks, draws
a checkerboard over every screen, holds it, and unlocks. It asks for no
password because Ferrix has no notion of one yet; the part that can be
tested is the part that matters to the compositor.

`cargo xtask test-compositor` boots a fifteenth time and takes three
pictures: the windows, the lock over them, and the windows again. The middle
one must be the picture `src/user/system/linux/compositor/render` blesses for a locked screen and
not one pixel of either window, and a keybind pressed while the screen was
locked must have done nothing -- the window it would have closed is still
there in the third. On x86-64 and on AArch64. A host test compares a
*screenshot* of the locked screen against the same image, which is the
compositor's own answer to a program rather than what QEMU read off the
scanout.

**Done — who draws the title bar (2026-09-17).**
`zxdg_decoration_manager_v1` was vendored, generated and checked against
libwayland, and never offered: the compositor's list of globals did not have
it. That is a gap a screenshot does not show and a person notices at once,
because a toolkit that does not find the manager assumes the job is its own
and draws a title bar, a shadow and a resize border *inside* the rectangle
the tiling gave it. GTK and Qt both do.

It is offered now, and the answer is always `server_side`: a tiling
compositor draws the border and the client draws nothing. The decoration is
configured the moment it is made, which the protocol allows and which saves
a round trip before the first frame, and a client that asks for
`client_side` is told `server_side` just the same -- the answer does not
depend on what was asked. A mode that is neither is the interface's own
`invalid_mode`.

**Done — the rest of what `hyprctl` reads (2026-09-17).** Seven more
commands, which between them are what a bar and a script ask for that is not
a window: `binds`, `devices`, `layers`, `cursorpos`, `locked`,
`workspacerules` and `globalshortcuts`. Each is answered in Hyprland's own
shape, readable and JSON, with its field names -- a script reads them by
name, and a close-enough name is a script that prints nothing.

`binds` lists what the *configuration* parsed rather than what the seat
resolved, as Hyprland's does, so a bind naming a key this keymap does not
have is still listed -- which is what makes the list worth reading when a
bind is not firing. `devices` puts each `/dev/input/eventN` in the group its
capabilities put it in, which is libinput's rule and the one the seat
already uses. `layers` groups by monitor and then by
`zwlr_layer_shell_v1`'s four levels, which is how a bar finds its own
surface. The last two had nothing to list when this landed -- there was no
`workspace =` keyword and no global-shortcuts protocol -- and were answered
with an empty list rather than `unknown request`, because a bar asking for
them should get an empty answer and carry on. Both have lists now:
`workspacerules` prints every `workspace =` line in Hyprland's shape, since
the keyword was carried out later the same day.

**And the rest of them (2026-09-17).** `getoption` answers in Hyprland's own
shape -- the value under the key its *type* names, with a `set` flag saying
whether the configuration said anything or it is the table's default, which
is the one field a script reads to know whether a line took effect.
`descriptions` lists every option with its type and value; Hyprland prints a
sentence about each, written beside its default in its own table, and this
compositor's table has no such sentences, so what is printed is the truth
rather than a row of empty strings. `animations` prints the whole tree with
the `overridden` flag and then the beziers, which is how a person finds out
that their `animation =` line named a bezier that does not exist.
`configerrors` is what could not be read, `rollinglog` the last five hundred
lines the compositor said -- everything it says now goes to whoever is
watching *and* into a rolling buffer -- and `systeminfo` and `status` what it
is and how long it has been up. `globalshortcuts` is no longer empty:
`hyprland-global-shortcuts-v1` fills it.

`notify`, `dismissnotify` and `seterror` come back for the compositor, which
says the message and puts it on the event socket. Hyprland draws a rectangle
over everything; this compositor draws none, and a line a notification daemon
or a bar can pick up is more use than an overlay only this compositor can
draw. `decorations` lists what is drawn around a window, which here is a
border and nothing else.

`getprop` answers one property of one window -- the value bare in the
readable form and under its own key in JSON, which is Hyprland's shape --
and a property nothing set is answered with the compositor's own, since that
is what a person asking "what is this window drawn with" wants to know.

Three say plainly that they act on something this compositor does not have,
rather than pretending or refusing: `output` (the screens are the card's),
`setcursor` (the cursor is drawn in code and there is no theme) and `kill`
(click-to-kill needs a pointer grab). `switchxkblayout` was a fourth until
the keymaps gained layouts to switch between (2026-09-17); it switches them
now.

That left **two** of Hyprland's thirty-eight unanswered, `eval` and `repl`,
and they are answered now too. They run a line of Lua in the configuration's
own interpreter, which Hyprland has only with a Lua configuration; against
the file format this compositor reads, Hyprland answers *eval is only
supported with the lua config manager*, and so does this.

The second boot of `cargo xtask test-compositor` has the bar, so it is the
one that asks: a keybind runs `hyprctl --batch binds ; devices ; layers ;
cursorpos ; locked`, and the transcript must name the bind's dispatcher and
key, the keyboard QEMU published, the bar's own layer at level 2 under the
namespace it asked for, and the pointer in the middle of the screen -- with
the picture unchanged, because asking a compositor about itself must move
nothing.

**Done — screenshots (2026-09-17).** `zwlr_screencopy_v1`, which is what
`grim` speaks, what `hyprshot` wraps, and what every screen recorder and
screen-sharing portal on wlroots goes through. The compositor says what
buffer to make -- `XRGB8888` at the screen's size, since its canvas is
opaque and an alpha channel that is always `0xFF` is a larger file saying
the same thing -- the client makes one in `wl_shm` and hands it over, and
the compositor writes the screen into it row by row and answers `ready`.
`capture_output_region` is the same with the rows clipped. A frame may be
copied into once: a second `copy` is `already_used`, which is a protocol
error, because a client that sent one has lost track of an object it owns.

This is the one place the compositor *writes* into a client's memory, and it
is a mapping of its own: `src/user/system/linux/compositor/hyprix`'s pool mapping is read-only and
says why, so a screenshot maps the same pool a second time, writable, for
exactly as long as the copy takes. The rule that the compositor never writes
into a window's buffer still holds everywhere else.

`src/user/system/linux/compositor/shot` is `grim` without the file format: it binds the manager
and a `wl_output`, makes the buffer it is told to make, and reads back what
was written into it. It prints the size and an FNV digest of every pixel,
which is how a whole screen is compared through a serial port.

And that is the strongest picture proof in this tree. Every other boot
compares QEMU's *screendump*, which reads the virtio-gpu's scanout; this
compares what the compositor handed a program **through the Wayland
protocol**, against the image `src/user/system/linux/compositor/render` builds on the host by
calling the renderer with rectangles. A compositor that drew the right thing
and answered screencopy with rubbish is caught here and nowhere else.

`cargo xtask test-compositor` boots a fourteenth time: a keybind runs
`/bin/shot`, and the digest it prints must be the digest of the expected
image, on x86-64 and on AArch64. A host test in `src/user/system/linux/compositor/hyprix` compares
the screenshot against the same image pixel by pixel.

**And a second real bug came out of it.** The seat turned a whole batch of
input events into actions *before* any dispatcher ran, so a key pressed
after `submap` in the same batch was judged against the map that was in
force before it. On a machine fast enough to see each key on its own the two
are the same; under emulation a whole sequence arrives in one read, which is
where it was found. Each input is now carried out before the next is read,
which is what Hyprland does and what anything a dispatcher changes about the
meaning of the *next* key requires.

**Done — the window list a bar draws (2026-09-17).**
`zwlr_foreign_toplevel_management_v1`, which is the other half of a bar's
job: `zwlr_layer_shell_v1` puts the bar on the screen, and this tells it what
to draw on it. Waybar's `wlr/taskbar`, eww's window list and every panel that
shows what is open read this protocol and nothing else, so a compositor that
does not offer it is one whose bar shows a clock and an empty strip.

The compositor makes a `zwlr_foreign_toplevel_handle_v1` for each window --
a server-side object, as it must be, out of the server's half of the id space
-- and sends the title, the application id and the four states, followed by
`done`, which is what makes the three one atomic change. A client is told
about the windows that already exist *inside the pass its bind arrived in*,
before the `wl_display.sync` every such client sends after binding is
answered: a list that arrives after that callback is a list the client has
already stopped waiting for. Nothing is written to a client that has already
been told the same thing, because a bar woken by `done` on every frame of an
animation is a bar that burns a core.

The requests come back the other way: `activate` is `focuswindow`, `close` is
`killactive`, and the two state requests focus the window and then run the
dispatcher a keybind would. `set_rectangle` is accepted and dropped -- it
says where the window's icon is on the bar so a minimise can fly to it, and
there is no such animation here -- and `maximized` and `minimized` are
reported false and refused rather than faked, because this layout has one
fullscreen state and no minimised one, and a wrong tick in a taskbar's menu
is worse than none.

`src/user/system/linux/compositor/lswt` is `lswt`, Leon Henrik Plickat's "list wayland toplevels",
which is a taskbar with the drawing taken out: `lswt` prints a line a window,
`lswt activate <title>` focuses one and `lswt close <title>` asks one to
close. It is how the protocol is tested with no screen and no panel.

**It found two real bugs, which is why the protocol was worth writing.**
Both are about a client *leaving*, which nothing in the tree had tested:
every picture until now was made by clients that all stayed.

The compositor holds a client by its *place* in the list of connections --
a window's `Source`, the keyboard focus -- and a connection that ended was
taken out of that list with `retain`, which moves every connection after it
down one. Every such place then named the wrong client: a window that
outlived an earlier client was drawn from somebody else's buffer and typed
into by somebody else's keyboard. The places are renumbered in step with the
list now.

And the window left behind was never told it had grown. The loop
reconfigures the workspace when something changed, and a connection ending
was handled *after* that, so the one case where the layout changes without
anyone asking -- a neighbour going away -- set the flag a moment too late.
The window kept drawing at the size it had and the compositor drew that
buffer into a rectangle twice as wide. The connection that ended is handled
before the reconfigure now, with everything else that changes the layout.

`cargo xtask test-compositor` boots a thirteenth time: a keybind runs
`/bin/lswt`, which must name both windows with the focused one marked, and a
second keybind runs `/bin/lswt close one`, after which the screen must be the
picture `src/user/system/linux/compositor/render` blesses for one window left alone -- on x86-64
and on AArch64. A host test in `src/user/system/linux/compositor/hyprix` runs the same program
against the compositor in one process and requires the window that went to be
the one it named.

**Done — submaps (2026-09-17).** Hyprland's modal keybinding: `submap =
resize` puts every bind after it in a map of its own, `submap = reset` goes
back to the global one, and the `submap` dispatcher moves between them while
the compositor runs. Only one map is in force at a time -- while a submap is
entered the global binds do not fire and the submap's do -- which is what
makes it a mode a person is *in* rather than a prefix they hold.

The configuration already parsed the keyword and the `u` flag, and every
bind written in a submap was being dropped on the floor, so a `hyprland.conf`
with a resize mode in it started a compositor where those keys did nothing
and nothing said why. They are kept now and gated at match time.

The details are `setSubmap`'s. A name nothing was bound in is refused with
Hyprland's own sentence -- `Cannot set submap <name>, submap doesn't exist
(wasn't registered!)` -- rather than entered, because entering one leaves a
keyboard on which nothing works and no bind written to get out of it. The
`u` flag fires whichever map is in force, which is how the bind that leaves
a submap is written once. Reading the configuration again leaves the map, as
it must: a submap the new file does not have is one nothing could leave.
`hyprctl submap` prints the name or `default`, and in JSON a bare string, as
`submapRequest` does; the event socket says `submap>>resize` on entering and
`submap>>` on leaving. What is not done is the per-submap `reset` target
0.56 added, which leaves a map automatically after any bind in it fires.

`cargo xtask test-compositor` boots a twelfth time with `L` bound in the
submap and nowhere else: it does nothing before `SUPER R` is pressed, swaps
the windows after it, and does nothing again once `Escape` has left the map,
with `C` bound in both maps to `hyprctl submap` so that one key names the
map it was pressed in -- `default`, then `resize` -- and the event socket
carrying both changes. On x86-64 and on AArch64.

**Done — the clipboard (2026-09-17).** Copy and paste between two programs,
which is `wl_data_device_manager`'s selection and the thing a person notices
is missing before anything else on this list.

Wayland's clipboard is a promise and not a buffer, and the implementation is
shaped by that: the program that copies keeps the data and says which types
it can give it in; the compositor remembers *who* that is and tells every
client with a `wl_data_device` what is on offer; a program that pastes asks
for a type and hands over a pipe, and the compositor passes that pipe to
whoever copied, who writes to it and closes it. No byte of what is copied
ever passes through the compositor, which is the point -- a selection can be
a gigabyte of video and the compositor's memory does not move.

`wl_data_device_manager`, `wl_data_device`, `wl_data_source` and
`wl_data_offer` are all four implemented for the selection. The offer is a
server-side object, as it must be: the compositor makes it, names it in
`wl_data_device.data_offer`, sends an `offer` event per type and then
`selection`, which is the order libwayland's own clients rely on. A client
that copies while another holds the selection is given it and the previous
owner is sent `cancelled`; a client that goes away while holding it clears
it. Drag-and-drop is the other half of the same four interfaces; it was not
done when this landed, and a drag between two clients was carried later the
same day (`src/user/system/linux/compositor/hyprix/src/dragging.rs`).

`src/user/system/linux/compositor/clip` is `wl-copy` and `wl-paste`, neither of which is on Ferrix:
`clip copy <text>` offers the text as `text/plain;charset=utf-8` and stays
alive to answer, because it must; `clip paste` waits to be told what the
selection holds, asks for it on a pipe it makes, and prints what comes back.

Getting it right was a question of who owns a descriptor. One that arrives
over a socket is owned by `src/user/system/linux/compositor/socket`'s `Connection` until a message
claims it, and a claim is what `Connection::consume`'s second argument says:
a program that reads an event carrying a descriptor and then forgets to say
so has the same descriptor owned twice, closed twice, and -- since Rust 1.86
checks -- aborts the process. Two places had it wrong and both are fixed:
`src/user/system/linux/compositor/clip` now claims what `Reader::descriptors_taken` counted, and
the compositor no longer claims for the two clipboard events that carry no
descriptor at all.

`cargo xtask test-compositor` boots an eleventh time with `exec-once =
/bin/clip copy ...` and `exec-once = /bin/clip paste` beside the two windows,
and requires that the compositor say it took the selection and passed the
pipe on, that the copying program say it was asked for its data exactly once,
that the pasting program print the text the other one copied, and that the
windows still be drawn pixel for pixel while all of that happens -- on x86-64
and on AArch64. A host test in `src/user/system/linux/compositor/hyprix` runs the same two programs
against the compositor in one process.

**Begun — plugins (2026-09-17).** A plugin is a program the compositor
starts and talks to, not a shared object it loads into itself. Hyprland's
plugins are C++ objects `dlopen`ed into the compositor, which hook its own
functions. When this was decided Ferrix had no dynamic loader to `dlopen`
with; ferrousli's loader has had `dlopen` since 2026-09-22, but the
compositor is still a static binary, which loads nothing. The reason that
remains is the other half: a `dlopen`ed plugin takes the compositor down with
it when it dereferences a bad pointer, and one at the end of a socket cannot.

The keyword is Hyprland's: `plugin = /bin/plug` starts the program, after the
sockets are bound and before `exec-once`. The plugin connects to the same
`.socket.sock` every `hyprctl` connects to and keeps the connection:

* `[[PLUGIN]]name,author,version,description` -- what `PLUGIN_INIT` returns
  in Hyprland, in one line;
* `handle <dispatcher>` -- Hyprland's `addDispatcher`: from then on
  `dispatch <name>`, from a keybind or from `hyprctl`, is written to the
  plugin as `dispatch>><name>,<argument>` rather than refused as unknown;
* `subscribe` -- `registerCallbackDynamic`: the lines `.socket2.sock`
  carries, on this connection;
* anything else -- an ordinary request, so a plugin asks `clients` and runs
  `dispatch movewindow r` the way a bar does.

`hyprctl plugin list` prints them in Hyprland's own shape, with a
`Dispatchers:` line Hyprland has no need for. A plugin that goes takes its
dispatchers with it and the compositor carries on.

`src/user/system/linux/compositor/plug` is the example: it adds `swapthem`, and answers it with the
two dispatchers that exchange the focused window with its neighbour.
`cargo xtask test-compositor` boots a seventh time with `plugin = /bin/plug`
and `bind = SUPER, P, swapthem` -- a dispatcher nothing in the compositor
knows -- and requires the picture a swap makes, pixel for pixel, on x86-64
and on AArch64.

**Begun — scaled monitors (2026-09-17).** `monitor = name, resolution,
position, scale` is read: the name (empty for every monitor no other rule
names), `disable`, the resolution as `preferred` or `WxH[@R]`, the position
as `auto` or `XxY`, and the scale as `auto` or a number. What is not done --
`mirror`, `auto-left` and the rest -- says so rather than being read as if
it were not there. (`transform` was on that list until 2026-09-23; see
turned monitors below.)

A scaled monitor is laid out in logical pixels and drawn in the screen's
own: a 1024x768 screen at `scale = 2` tiles its windows in 512x384 and draws
each of those pixels as two, with the border, the rounding, the shadow and
the blur scaled with them, as Hyprland scales its decorations by the
monitor's scale. Everything above the renderer -- the layouts, the
dispatchers, `hyprctl`, the layer surfaces -- works in logical pixels and
never learns the difference.

The clients are told: each `wl_output` carries its own scale, and
`src/user/system/linux/compositor/pattern` now reads it, sends a buffer that many times the size
and says so with `wl_surface.set_buffer_scale`, which is what a client on a
scaled monitor does. `hyprctl monitors` prints the scale it is at.

`cargo xtask test-compositor` boots a sixth time with `monitor = ,
preferred, auto, 2` and requires the picture `src/user/system/linux/compositor/render`'s own tests
bless for a scaled monitor, pixel for pixel, on x86-64 and on AArch64.

**Begun — turned monitors (2026-09-23).** `monitor = name, res, pos, scale,
transform, N` is read with Hyprland's values, `wl_output.transform`'s 0 to
7, and so is the short form `monitor = name, transform, N`, which turns the
monitor an earlier line named. A monitor turned a quarter is laid out with
its mode's width and height exchanged and then divided by the scale -- a
1920x1080 connector stood on its edge is a monitor 1080 wide and 1920 tall
-- which is Hyprland's `m_transformedSize` and `m_size`, and is what the
workspaces, the tiling, the layer surfaces and `zxdg_output_v1` see.
`wl_output.mode` stays the connector's own and `wl_output.geometry` carries
the transform, as Hyprland sends them; `hyprctl monitors` prints the mode
unturned with `transform: N` under it, plain and JSON. The pointer lives in
the laid-out space, as Hyprland's does, so a mouse moves the way the person
reading the monitor expects.

Where the picture is turned is the one place this differs from Hyprland.
Hyprland folds the transform into its projection matrix and draws every box
turned on the GPU; the software renderer here draws the frame upright on a
canvas the monitor's laid-out size, exactly as for an upright monitor, and
turns it once, as it is copied into the connector's buffer
(`compositor_render::transform`), with the frame's damage turned alongside
for the card and the night-light. Each pixel lands where Hyprland's matrix
puts it -- transform 1 is the picture turned counter-clockwise into the
buffer, for a monitor turned clockwise onto its right-hand edge -- and an
upright monitor never takes that path, so its frames are the bytes and the
cost they were. A frame drawn on the GPU is drawn upright the same way and
fetched and turned on its way to the card rather than scanned out where it
was drawn. What is not done: changing a transform while the compositor runs
(a `wlr-output-management` client asking for one is refused), a client's own
`wl_surface.set_buffer_transform`, and so `preferred_buffer_transform`,
which Hyprland sends and a client would answer with a buffer this renderer
cannot yet turn; and `input:touchdevice:transform` and
`input:tablet:transform`, Hyprland's own answer for a touchscreen on a
turned monitor.

`cargo xtask test-compositor --boot transform` boots twice, with `transform,
1` and `transform, 3`, and requires QEMU's screendump -- the connector's
buffer -- to be the turned picture `src/user/system/linux/compositor/render` blesses, pixel for
pixel, and `hyprctl monitors` in the guest to say `transform: N` beside the
1024x768 mode.

**Begun — more than one monitor (2026-09-17).** A screen a connected
connector, across every card: the compositor opens every `/dev/dri/cardN`,
takes each connected connector with a mode and a CRTC of its own, and drives
one canvas and one pair of dumb buffers for each. The monitors are laid out
side by side from the left in the order the kernel lists them, which is
Hyprland's `auto`, and each is named after its connector -- `Virtual-1`,
`Virtual-2` -- with each connector type numbered from one across the whole
machine, as wlroots numbers outputs.

The card grew heads to match: `/dev/dri/cardN` publishes one connector, one
encoder, one CRTC and one primary plane per scanout the driver reported,
each in a block of four ids of its own, and `SETCRTC` and `PAGE_FLIP` carry
the head's scanout number down to the driver. `docs/DISPLAY.md` §2.3 has the
table.

Every monitor is a `wl_output` global of its own, so a client is told there
are two screens and which is which, and a layer surface is placed on the
screen its `wl_output` names -- a bar on one monitor reserves a strip of that
monitor and moves no window on the next. `hyprctl monitors` lists them all
with their names, positions and active workspaces, and the event socket
announces each.

The dispatchers that name a monitor are Hyprland's, with
`CMonitorQueryCore::fromConfigString`'s argument forms -- `current`, a
direction, `+N`/`-N` along the list, an id counting from zero, or a name:
`focusmonitor`, `movewindow mon:<monitor>` with `silent`,
`movecurrentworkspacetomonitor`, `moveworkspacetomonitor` and
`swapactiveworkspaces`.

The proof is a fifth boot of `cargo xtask test-compositor`: two virtio-gpu
devices, so two cards and two monitors in the guest, two windows tiled on the
first, and then a keybind moving one to the second -- with each screen
required, pixel for pixel, to be the picture `src/user/system/linux/compositor/render`'s own tests
bless for it, on x86-64 and on AArch64. QEMU enables a second *output* of one
virtio-gpu only when a host window manager resizes its window, which a
headless test cannot do; two devices are two consoles, and a screendump names
each.

**Begun — window rules (2026-09-17).** `windowrule = <effect> [value],
match:<prop> <value>, ...`, which is Hyprland 0.56's own form: comma-separated
fields, each a name and a value, with `match:` in front of the ones the
window must be. `windowrulev2` is refused with Hyprland's own sentence,
because 0.56 merged the two syntaxes and took the old one away.

Matching is by regular expression for the four names a window has -- `class`,
`title`, `initial_class`, `initial_title` -- and a yes-or-no for `float`,
`fullscreen` and `focus`. The expressions are `src/user/system/linux/compositor/regex`'s, which is
RE2's syntax as far as a window rule uses it: literals and escapes, `.`,
classes with ranges and negation, `*`, `+`, `?`, groups with alternatives,
and the anchors every rule carries and a full match makes redundant. What is
not done -- counted repetition, `\d`, lookaround, non-greedy -- is refused
with a sentence rather than matched wrongly, because a rule that silently
matched everything would float every window a person owns. The matcher
counts its steps and gives up rather than hanging the compositor on a
pattern that backtracks for ever.

A rule is applied where Hyprland applies one: when the window maps, which is
where the client has finished saying what it is called. `float`, `tile`,
`size`, `move`, `center`, `workspace` (with `silent`), `fullscreen`,
`maximize` and `no_focus` go to the layout, through the same calls a
dispatcher makes; `opacity`, `rounding`, `border_size`, `no_blur`,
`no_shadow` and `no_dim` go to the renderer, which now draws each window
with what a rule gave it and every other window with the configuration's
style.

`cargo xtask test-compositor` boots a tenth time with six rules -- one
window floated at a size and a place and drawn at `opacity 0.6`, the other
with its corners cut and no shadow -- and requires the picture they make.
`src/user/system/linux/compositor/render` blesses it by calling `State::float_window` and handing
the renderer the same per-window styles, which is what the rules do, so the
two pictures are made by one piece of code.

**Begun — window groups (2026-09-17).** Hyprland's tabs: windows that share
one slot in the tiling, of which one is drawn. Only the head is in the
dwindle tree, and the slot draws whichever member is active, so cycling a
group changes the picture and not the layout -- a group that moved the
windows each time would be a workspace switch with extra steps.

`togglegroup` makes a group of the focused window and dissolves the one it is
in, putting every member back beside the head; `moveintogroup <direction>`
takes the neighbour in that direction and adds the focused window to its
group; `moveoutofgroup` puts one back in the tiling, and a group of one is no
group, which is what Hyprland leaves behind; `changegroupactive
[f|b|<index>]` cycles, wrapping both ways, with the index one-based as
Hyprland's is; `lockgroups lock|unlock|toggle` is read and reported. Every
direction search maps a grouped window through its slot, so `movefocus` and
`movewindow` work from inside a group and move the whole of it.

`hyprctl clients` grew the `grouped` field and the `hidden` one that goes
with it: the members a group does not draw are listed as hidden windows with
the group's box rather than left out, which is what a bar drawing the tabs
reads. The event socket says `togglegroup>>1,<head>` when one is made,
`moveintogroup>><window>` and `moveoutofgroup>><window>` as it fills and
empties, and `togglegroup>>0,<head>` when it goes. `hyprctl --batch` landed
with them, because one keybind running three dispatchers over the control
socket is how the Ferrix proof presses them.

`cargo xtask test-compositor` boots a fourth time: two windows tiled, one
keybind, and then both of them in one slot with the one that was moved in
drawn -- pixel for pixel the image `src/user/system/linux/compositor/render`'s own tests bless, on
x86-64 and on AArch64, with `hyprctl clients` naming the group from inside
the guest and the event socket carrying both events.

**Begun — shadows, dimming and blur (2026-09-17).** The three decorations
that needed no GPU, each ported from the shader that is the only description
of it there is.

`decoration:shadow:*` is `shadow.glsl`'s `getShadow` and
`pixAlphaRoundedDistance`: a box the window's rectangle grown by
`shadow:range`, with a falloff of `((radius − d) / range)^power` in the
corners and `(smallest / range)^power` along the edges, `radius` being the
range plus the window's own rounding. Drawn under the border and the window,
as Hyprland draws it. This is the one place in the renderer that blends a
pixel by hand -- every pixel has an alpha of its own and tiny-skia's shaders
take one colour for a rectangle -- so the arithmetic is written out to be the
same source-over its `f32` pipeline does.

`decoration:dim_inactive` and `dim_strength` lay black over a window that is
not focused, over its surface and inside its rounding.

`decoration:blur:*` is the dual-Kawase pair, `blur1.glsl`'s five taps down
and `blur2.glsl`'s eight up, `blur:passes` times each way at `blur:size`.
It reads the frame so far from under a translucent window and writes it back
before the window is drawn, which is what Hyprland does and why the pass has
to sit between the two. The two passes do *not* use the same offsets --
`blur1`'s `halfpixel` is four times `blur2`'s -- and using one for both turns
a bright block into a dark hole with a bright halo, which is what this looked
like before the shaders were read again. The colour grading (`noise`,
`contrast`, `brightness`, `vibrancy`) was not done at first, since it changes
the blur's colour and not its shape; it followed later the same day, with
Hyprland's defaults and `vibrancy_darkness` (`src/user/system/linux/compositor/render/src/blur.rs`).

The compositor now reports its slowest frame in microseconds, which is the
stated bound this stage asks each software effect to have: a number measured
on the machine that ran it rather than one somebody hoped for.

**Begun — special workspaces (2026-09-17).** Hyprland's scratchpad: a
workspace shown *over* the monitor's own rather than instead of it, with a
negative id and a `special:` name. `togglespecialworkspace [name]` shows and
hides it, `workspace special:name` and `movetoworkspace special:name` reach
it, and `hyprctl monitors` says which one a monitor has over it -- in the
readable form and in the JSON, both in Hyprland's own shape, because a bar
reads that field to know whether the scratchpad is up.

The ids are Hyprland's: `special:special` is `SPECIAL_WORKSPACE_START`, −99,
and every other name counts up from there towards −2. Three things had to
change for a workspace that is shown beside another rather than instead of
it: focusing a window on one shows it rather than switching to it, a monitor
showing one has two workspaces the focus can be on, and an empty one is not
pruned while it is being shown -- an empty scratchpad is a scratchpad you can
put something in.

**Begun — animations with Hyprland's curves (2026-09-17).** `src/user/system/linux/compositor/anim`
is the curves, the tree and the values they move, and it holds no window and
no clock: a value is asked what it is at a time the caller gives it, so every
curve and every inheritance rule is host-tested.

The curve is `hyprutils`' `CBezierCurve` to the point: 255 points baked at
`t = (i + 1) / 255`, the binary search over their `x`s, and the linear
interpolation between two of them. `default` is `DEFAULTBEZIERPOINTS`,
`(0, 0.75)` and `(0.15, 1.0)`, which puts a quarter of the time at 0.843 of
the distance -- a number worked out from the control points by hand and
pinned by a test, because a curve that is nearly Hyprland's is an animation
that looks nearly right and cannot be compared against anything.

The tree is `AnimationTree.cpp`'s names and parents, so `animation = windows,
1, 3, myCurve` reaches `windowsMove` and leaves `fade` alone, and `global` is
on at speed 8 with the default curve. Speed is in deciseconds, which nothing
in Hyprland's configuration says and only `getPercent` does:
`clamp((ms / 100) / speed, 0, 1)`. Every refusal is Hyprland's
`handleAnimation` word for word -- `no such animation`, `invalid animation
on/off state`, `invalid speed`, `no such bezier` -- and a bad line is
reported with the rest of the file still applied.

A window the layout moves slides there along `windowsMove`'s curve. The
client is configured at the goal and draws once, as Hyprland's is, and the
renderer scales its surface into the rectangle while it moves -- the one
place in this renderer where a pixel is not a pixel, and the only place it
can be. A window that is not moving goes through the exact path, which is why
every expected image in this tree still holds to the byte.

The proof is the frames themselves. The compositor writes a PPM a frame, and
a test swaps two windows through `hyprctl dispatch movewindow l` and follows
the moving window's left edge across them: it must be at more than three
places, it must not go backwards, and by the middle frame it must be more
than half way -- which a straight line is not. With `animations:enabled = 0`
the same swap puts it at two places and no more.

**Begun — rounded corners and opacity (2026-09-17).** `decoration:rounding`
cuts a window's corners and the border follows them, at the window's rounding
plus the border's width as Hyprland draws it; `decoration:active_opacity`,
`inactive_opacity` and `fullscreen_opacity` multiply the surface's alpha as
it is drawn. Anti-aliasing stays off, as everywhere in this renderer: a row's
inset is the circle's at that row's centre rounded to the nearest pixel, so
coverage is all or nothing and every frame is exact. `cargo xtask
test-compositor` boots a third time with both set and requires the picture
`src/user/system/linux/compositor/render`'s own tests bless, and a test pins what a rounded corner
must show: the compositor's background where the corner was cut, and the
border along the same edge away from it.

Those are shaders. This stage brings the GPU, and the choice the earlier text
here left to the customer was made on 2026-09-18 (`docs/GPU.md`, and the
decision in `docs/BACKLOG.md`): **the host's GPU driver first, through
virtio-gpu's 3D commands**, which puts the NVIDIA driver of the machine
Ferrix is watched on behind the guest's rendering without porting a line of
it; and a driver for a card of Ferrix's own later, in stage 21, when Ferrix
runs on bare metal. Every effect keeps its software fallback with a stated
frame-time bound, so the compositor is never GPU-only.

**Done -- the GPU, Path A (2026-09-19).** The 52 points this paragraph used
to owe are spent, in the four pieces `docs/GPU.md` §3 named: virtio-gpu 3D in
the ring-3 driver, `/dev/dri/renderD128` with the `virtgpu` ioctls and
scanout of a 3D resource, the host half in xtask, and a Rust virgl encoder
behind a renderer trait with the software renderer still under it. The
desktop composites on the GPU and the screen is shown the very texture the
compositor drew into: a 1920x1080 frame of a video wallpaper behind a
blurred translucent terminal went from 39 ms in software to 12, where 60 fps
is 16.7 (§3.7 and §3.8). `cargo xtask test-compositor --gl` judges it from
inside the guest, because QEMU cannot screendump a GL console. Mesa on
ferrousli and `zwp_linux_dmabuf` come after, when clients render on the GPU
themselves; they are the only part of the GPU road left.

**Done -- the watched desktop draws on the GPU by default (2026-09-23).** The
customer's served desktop had been the software renderer all along, for want
of `--gl`: a video wallpaper was 38 frames a second with the slowest at
60-95 ms, and is 61 with the slowest at 7-17 on the GPU. A served screen now
takes the 3D card wherever a QEMU on `PATH` has it and the host has a render
node (`docs/GPU.md` §3.9, which also has the viewer-side measurements).

**Done -- the pointer on virtio-gpu's cursor plane (2026-09-23).**
`MODE_CURSOR` and `MODE_CURSOR2` on the card, `CURSOR` and `MOVE` in the
display protocol, the cursor queue in the ring-3 driver run with no
interrupt and one doorbell a batch, and a compositor that puts the pointer
there wherever a screen has a plane. Sweeping the pointer over the served
desktop for 25 seconds drew 4 frames where it drew 61 a second, and sent the
viewer no updates but the pointer's shape, which it draws where its own mouse
is. `test-compositor --boot cursor` judges it through a VNC viewer, since a
screendump cannot see a plane (`docs/GPU.md` §3.10). 13 points.

**Done -- a frame waits once (2026-09-24).** A frame on the GPU was an
upload or two, a command stream and a flush, and each was a round trip of
its own through the render or display core, the driver process, the device
and back, with the driver taking one command at a time. The driver now
keeps eight commands in flight on the control queue behind one doorbell,
the way seL4's sDDF runs a queue, and has the device read a command stream
where the render core wrote it; `VIRTGPU_TRANSFER_TO_HOST` and
`VIRTGPU_EXECBUFFER` return once the work is on its way, as on Linux, and
`VIRTGPU_WAIT` waits for it. A frame waits once, for its flush
(`docs/GPU.md` §3.11).

**Done -- an idle desktop idles (2026-09-23).** Two loops kept a processor
each busy on a desktop nobody was touching: the kernel answered `poll` on a
listening Unix socket as hung up, so the compositor's loop never slept, and
the clipboard's driver never read what the host sent it, so its loop never
left. Left alone, the served desktop cost its host 2.75 processors and costs
0.95 (`docs/COMPOSITOR-DAMAGE-HANDOFF.md` §2.8).

**Done -- the desktop is the size of its window (2026-09-26).** QEMU's
window stretched a 1920 × 1080 desktop to its own size, a pixel at a time,
and small text looked unsmoothed. A resized window, or a VNC viewer's
resize request, now reaches the compositor. It goes from the device's
display event through the driver's new `MODES` message (display protocol
version 6) and the card's preferred mode, with Ferrix's own DRM event
`EVENT_FERRIX_CONNECTORS` on the card's descriptor. hyprix sets the new mode
on a monitor whose line says `preferred`, the GPU path included, and
`run-compositor` writes `preferred` unless given `--size`
(`docs/DISPLAY.md` §2.3).

The protocols and keywords this paragraph used to list as left --
`layerrule`, `zwp_virtual_keyboard`, `zwp_pointer_constraints` and
`relative-pointer` (a game that grabs the pointer), `presentation-time`,
drag-and-drop and `hyprctl getoption` -- have all landed, each written the
way the four before them were: the XML vendored, the tables checked against
libwayland's own, a program in `src/user/system/linux/compositor/` that speaks it with no screen,
a host test against the image the renderer blesses, and a boot of `cargo
xtask test-compositor` that does it on Ferrix.

**Begun -- the desktop's own clients, in Rust (2026-09-26).** The
customer asked for waybar, fuzzel, hyprlock and hypridle, written for
Ferrix and reading their own files unchanged: five streams, one a program
and one for the foundation they share -- `src/user/system/linux/compositor/toolkit` (a Wayland
client runtime), `src/user/system/linux/compositor/text` (fonts, shaping, layout, Pango markup),
`src/user/system/linux/compositor/hyprlang` and `src/user/system/linux/compositor/image`. `docs/DESKTOP-CLIENTS.md`.

**hypridle (2026-09-26).** `/bin/hypridle` reads the user's
`hypridle.conf` unchanged over `ext-idle-notify-v1` and
`hyprland-lock-notify-v1`, and `/bin/loginctl lock-session` reaches its
`lock_cmd` over a socket in place of logind. The `idle` and `idle-user`
boots of `test-compositor` show a listener blacking the screen with `dpms
off`, a key bringing it back, and the lock chain
(`docs/DESKTOP-CLIENTS.md` §6).

**What this stage still owes** is the desktop's speed as a person watching
it feels it (`docs/GPU.md` §3.9) -- a client's own pages as its texture's
backing, so its pixels are not copied in the guest (8), the device queue
being done (§3.11) -- then the second-pass effects
(`no_screen_share`, which means drawing the frame again without one surface
in it, and `blur_popups`, which reads what is behind the frame being drawn),
`dwindle:precise_mouse_move`, which waits on dropping a dragged window
back into the tiling, and the window rule `xray`. Mesa on ferrousli and
`zwp_linux_dmabuf`, for clients that draw on the GPU themselves, come after
and are priced outside the stage (`docs/BACKLOG.md`, 8 points and 40 or
more). `docs/COMPOSITOR-DAMAGE-HANDOFF.md` §5 says why each is where it is,
though its GPU item was written before Path A. The X server this paragraph
once listed, 40 as a first guess, is yserver, done on 2026-09-29 as a
rootless Wayland client of hyprix rather than through
`xwayland_shell_v1` (`docs/YSERVER.md`).

**Exit:** the stage 18 test with animations on, requiring a sequence of
screendumps to show a window moving along the configured curve with rounded
corners and blur behind a translucent client, at the stated frame rate under
the GPU path and inside the stated bound under the fallback; two monitors on
QEMU with independent workspaces; a plugin-shaped extension loaded from the
configuration.

**waybar in Rust draws the user's bar (2026-09-27).**
The waybar app (ferrix-os/apps since 2026-10-04) reads the user's own `~/.config/waybar/config.jsonc`
and `style.css` as waybar does: jsoncpp's JSONC, `src/config.cpp`'s search
path, `include` merging and `output` matching, libfmt's format strings, a
GTK3 stylesheet with `@define-color`, `alpha()`, `calc()`, layered `url()`
and gradient backgrounds and GTK's cascade, GTK's box model and box layout,
the painter, and every module the user's file names (`custom/*`,
`hyprland/window`, `cpu`, `memory`, `network`, `pulseaudio` with a
PulseAudio-protocol client, `tray`, and `clock`). It draws on the toolkit,
with `:hover`, the hand cursor, clicks, scrolls and tooltips as popups of
its layer surface, tested against hyprix in-process. `run-compositor`
carries it as `/bin/waybar`, and `cargo xtask test-compositor --boot waybar`
boots it with the user's stylesheet and icons over a test config whose
scripts print fixed answers: the guest's screen shows exactly the pixels
the host's `waybar --render` draws from the same files. What is not
carried out on Ferrix -- the user's three Python scripts, a sound server
where `pulsed` does not run, load averages, wifi, a D-Bus tray -- is listed
in `docs/DESKTOP-CLIENTS.md` §3, and `waybar-probe` lists it from the real
files.

**fuzzel, and the host's own desktop on `--everything` (2026-09-27).**
`/bin/fuzzel` reads the user's `fuzzel.ini` and draws its window on a layer
surface as upstream's `render.c` does. The user's `SUPER R` runs their own
`hypr-launcher` script, carried unchanged, whose fuzzel path links to
`/bin/fuzzel` (`--boot fuzzel-user`). `run-compositor --everything` now
starts this machine's own `hyprland.conf` with its dotfiles, fonts and
monitor EDID, so the user's waybar draws their bar. `exec-once =
/bin/vdagent` and a `SUPER RETURN` terminal are added to it, and
`/bin/foot` is term, so their `$terminal = foot` opens one
(`--boot everything-desktop`). Chrome's `HOME=/dev/shm` goes on its own
command, which hyprix reads as `sh` does, with leading `NAME=value` words as
that program's environment. As a global `env =` line it had hidden the
user's `~/.config` from waybar and hypridle. hyprix reports a program that
does not exist, or a script whose `#!` interpreter does not, once per
program rather than on every press of a bind that runs it. Still to do: the
script's `pkill -x fuzzel` toggle does not close fuzzel yet. hyprlock's
lock over `authd` (P1.5) landed on 2026-10-03.

**Where the exit stands (reviewed 2026-09-21).** The existing exit criterion
is met: `cargo xtask test-compositor` covers the non-GPU path on x86-64 and
AArch64, and `cargo xtask test-compositor --gl` covers Path A from inside the
guest. The stage remains under way for the scope named above. The non-GPU
evidence is:

* the sliding window with its decorations on, as a sequence of screendumps,
  with the guest's own frame times reported and the renderer's software
  bound stated and checked in release by `src/user/system/linux/compositor/render`;
* two monitors, each with a workspace of its own and each required to be the
  picture blessed for it;
* a plugin loaded from `plugin = /bin/plug`, adding a dispatcher a keybind
  presses.

**Where the points stand (reviewed 2026-09-30).** The X server is done: it
is yserver, a Rust X11 server with a rootless Wayland backend of Ferrix's
own, 36 points against the 40 guessed for XWayland here (`docs/YSERVER.md`,
done 2026-09-29), so about 16 of the 178 are left -- client pages as
texture backing (8) and the small remainder (about 8). The count of
2026-09-23 follows. Of the stage's 178, about 56 were
left then: 8 of the 34 added on 2026-09-23 for the desktop's speed as it is
watched -- client pages as texture backing, the cursor plane's 13 and the
device queue's 13 being spent (`docs/GPU.md` §3.9 to §3.11) -- XWayland's 40, and about 8 for the
small remainder, whose items have changed since it was counted, as below. The GPU path's 52 are spent -- Path A landed on 2026-09-19, and what
is left of that road is `zwp_linux_dmabuf` and a Mesa on ferrousli, for
clients that render for themselves. What remains of the stage besides is XWayland,
40 as a first guess -- `xwayland_shell_v1` on the
compositor's side and an X server on Ferrix, which stage 22 is what finally
needs; and the small remainder `docs/COMPOSITOR-DAMAGE-HANDOFF.md` §5 lists,
about 8 when it was counted, of which `resize_on_border` and
`extend_border_grab_area` landed on 2026-09-18 -- with the tiled resize
they turned out to need, since `resizeactive` did nothing to a tiled
window until the dwindle tree would move a split. `general:snap` followed the same day, on the
drag the border grab had just built. Dwindle's cursor-placed splits followed:
the layout is given the pointer, and `use_active_for_splits`,
`force_split = 0` and `smart_split` read it. What is left of it is
`precise_mouse_move`, which waits on dropping a dragged window back into
the tiling; `no_screen_share` and `blur_popups`, which need a second pass,
and the window rule `xray` -- the layer rule turned out not to need one,
since the blur optimisation's backdrop is the picture it asks for.
`precise_mouse_move` and `blur_popups` were not in the 8 as it was counted,
and are of the same size as what landed out of it. `persistent_size`
followed once that close path existed, and the close path was a bug of its
own: only a whole connection going took a window out of the layout, so a
client that closed one of two left the layout tiling a window that was not
there. That close path has its own test since 2026-09-19:
`src/user/system/linux/compositor/pattern`'s `Shape::Twin` is a client that opens a second
`xdg_toplevel` once the first has drawn and destroys it with the connection
still open, and the compositor must show the window that was kept, alone and
filling the workspace. Nothing else in the tree opens two windows from one
client, which is why the fix went in without one. The frame rate the exit
asks for under the GPU path came with the GPU path.

---

