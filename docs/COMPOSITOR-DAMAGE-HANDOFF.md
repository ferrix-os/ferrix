# Handoff: the compositor's frame time

Written 2026-09-17, at the end of the session that landed damage tracking,
and brought up to date the same night by the session that finished it. This
is what was done and what was measured, with the reasoning behind it, so
that whoever touches it next does not have to rediscover any of it.

The short version: **a frame that changes a little costs a little, whatever
is on the screen: 120 ms → 0.06 ms for a terminal rewriting a line,
translucent or not. What is left is a wallpaper that changes every frame,
which is a blur every frame (§2.5).**

---

## 1. Where the frame time went, measured

Release build, 1920x1080, software renderer, on the machine this was written
on. Pieces of one frame, timed directly:

| piece | cost |
|---|---|
| blur behind a full-screen window (`size 8, passes 3`) | 95 ms |
| blur behind a 1920x40 bar | 8.6 ms |
| drop shadow | 12 ms |
| composite one ARGB window, rounded | 6.4 ms |
| `fill_rounded`, `clear` | 0.07 ms each |
| an empty frame | 1.3 ms |

The blur is the frame. Everything else together is under 20 ms.

Three things have been done about it, in this order.

### 1.1 The blur honours the damage (commit `15526e06`)

`Canvas::blur` ran over the whole rectangle it was given, whatever the
damage said. Over a 120x40 strip it cost 92 ms; it now costs 1.8 ms and
writes byte-identical pixels.

The same commit fixed a latent bug: the region the kernel reads was half
what it needs. Each level of the dual-Kawase pyramid is half the one above,
so a tap at `size` on level *k* is `size * 2^k` source pixels; down sums to
`size * (2^passes - 1)` and up sums to the same, so the kernel reaches
`2 * size * (2^passes - 1)`. The code read `size * 2^passes`. The outermost
ring of every blurred window was a blur of the clamped edge rather than of
the frame. Twelve expected images moved, toward correct.

### 1.2 The kernel got faster (commit `d9c1294b`, from a subagent)

304 ms → 85 ms for a full-screen blur, with every expected image unchanged.
Three changes, none of which touched the algorithm: the bilinear sample's
rows and weights hoisted out of the pixel loop and shared between taps at
the same height; `f32::floor` replaced by a truncating cast plus a step back
(on baseline x86-64 `floor` is a libm call and was a fifth of the whole
blur); and `blurprepare`'s contrast curve folded into the byte→float read as
a 256-entry table.

It was **already** dual-Kawase, not a naive convolution — that was the first
diagnosis in this session and it was wrong. A running-sum box blur was considered and
rejected: it would be worth ~40–60 ms at best here (memory-bound over the
same 33 MB of f32 planes) while losing the exact Hyprland kernel and moving
every image.

### 1.3 The compositor tracks damage (commit `203f2fc2`, from a subagent)

Before this, `hyprix` handed `Damage::full` to the renderer every frame, so
1.1 bought nothing.

Two halves, in `src/user/system/linux/compositor/hyprix/src/damage.rs`:

1. **Inside a surface.** A commit's `wl_surface.damage` / `damage_buffer`,
   read out of the server's `current` state at `Event::SurfaceCommitted`,
   converted to buffer pixels (the two lists differ by the surface scale)
   and mapped proportionally into whatever rectangle the frame draws that
   buffer into — a window, a bar, a menu, the lock surface, the drag icon,
   the cursor surface.
2. **Everything the compositor decides.** Rather than instrument the ~40
   `changed = true` sites, the whole frame description is kept per screen as
   a `Plan` — scaled layout, per-window rule styles, the layer/popup/lock
   list, cursor and drag-icon rectangles, gamma, dpms, lock, style, origin,
   scale, size — and compared with the last frame's.
   `compositor_render::damage_between` does the windows and the rest is
   field by field. That covers window appear/go/move/resize, focus,
   restacking, animations (the animated layout is what goes into the plan,
   so old ∪ new falls out every frame), pointer motion, layer changes, drag
   icon, `windowrule` restyling, config reload and monitor rearrangement.

Two bugs it had to fix on the way, both worth knowing:

- **Buffer age.** `Canvas::present` copies canvas→screen within the damage,
  but the DRM backend flips between two dumb buffers, so the one being drawn
  into holds the frame from *two* frames ago. `frame::Output` gained a
  `present: Damage` field — this frame's damage ∪ the last frame's — which
  is Hyprland's damage ring at age two. Without it the card path would
  flicker; headless would not have shown it.
- **Gamma applied twice.** `Gamma::apply` runs over the screen buffer after
  the copy, so with partial damage the undamaged pixels would be warmed
  again every frame. It now takes the stride and the presented region.

**Measured, release, same machine.** The run-wide "slowest frame" always
includes the first full frames, so the steady-state figure from the periodic
line is the one that means anything:

| configuration | before | after |
|---|---|---|
| one **opaque** terminal rewriting a line 5×/s | 42 ms steady | **0.24 ms** steady, smallest frame 6972 px of 2073600 |
| waybar + **translucent** foot rewriting a line | 154 ms steady | 150 ms steady |
| the user's own config, idle `foot` | 161 ms steady | 157 ms steady |

Row 3 is flat for a reason that must not be forgotten when re-measuring:
**that configuration's wallpaper is `mpvpaper` playing a video on a
full-screen layer surface, committing 1920x1080 of damage every frame.** In
that run the damage genuinely *is* the screen and no amount of tracking can
move the number. Measure with a static wallpaper or none.

Row 2 was the open problem, and §2 is what closed it.

---

## 2. What made a translucent window cost a full frame, and what fixed it

This was the open problem when the first half of this document was written.
It was worse than it looked: `foot` commits `ARGB8888` buffers whatever its
opacity, and the renderer's condition for blurring behind a window is a
format with alpha, so *every* terminal was a translucent window. And
`hyprix`'s loop reads its input once a frame, so a 120 ms frame is a pointer
that moves eight times a second and letters that arrive in bursts.

### 2.1 Why

`Canvas::blur` reads the canvas a kernel's reach outside what it writes.
Outside the damage the canvas still holds the **last** frame -- and the last
frame has the translucent surface drawn *over* the blur. So blurring a strip
is blurring the previous blur plus the surface: a smear that grows every
frame. `Plan::blurs_whole` in `src/user/system/linux/compositor/hyprix/src/damage.rs` answered
that by growing the damage until every blurred surface it touched was
redrawn whole, plus a reach. Correct, and a near-fullscreen frame for every
pointer motion over a near-fullscreen window.

It was found the honest way -- it cost a step or two of a channel over 8604
pixels of `dwindle-two-clients`, which is the golden-image suite catching
it.

### 2.2 The backdrop (`src/user/system/linux/compositor/render/src/backdrop.rs`)

Hyprland's answer is `decoration:blur:new_optimizations`, **on by default**:
the monitor keeps `m_blurFB`, the background and the layer surfaces under
the windows *already blurred*, blurs it again only when one of those changes
(`CHyprOpenGLImpl::preRender`), and a tiled window samples it
(`IHyprRenderer::shouldUseNewBlurOptimizations`).

`compositor_render::Backdrop` is that. Two canvases a screen: `sharp`, what
is behind the windows, brought up to date within each frame's damage; and
`blurred`, the blur of it in 64-pixel tiles, each blurred the first time a
window needs it and again only after the pixels it was blurred from have
*changed* -- which `Backdrop::take` finds by comparing, because a frame's
damage says what was drawn again and not what came out different. A pointer
crossing a window damages the wallpaper under it and changes none of it.

* `render_onto(canvas, Some(backdrop), …)` is what `hyprix` calls; a
  `Screen` owns the backdrop beside its canvas.
* `reads_backdrop(windows, at, styles)` is the rule for which windows take
  it: tiled, nothing drawn under it, no `dim_around` fill behind it. That is
  Hyprland's "not floating and not on a special workspace" asked of a layout
  that does not say which workspace a window came from.
* Everything else -- a floating window, a scratchpad's over the workspace
  under it, a blurred layer surface -- still blurs the frame as it stands
  and is still redrawn whole: `Plan::blurs_whole`, now only for those.
* The damage owes a backdrop reader one thing: where what is *behind* the
  windows changed (a commit on a layer surface under them, or one of those
  moving), the damage inside the window grows by `Blur::reach`.
  `Plan::behind_reaches`.

`render` and `render_with_layers` make a backdrop for the one frame they
draw, shown the whole of what is behind the windows, so that a frame drawn
from nothing and the frame a compositor kept up to date are one picture.
`hyprix`'s integration tests hold that: many partial frames, compared byte
for byte with the renderer's one.

**Twelve expected images moved**, every difference inside a translucent
tiled window, every one darker. A blur of the frame as it stands had the
window's *own* decorations in it: its shadow, drawn whole under it (lighter
than the default `0x111111` background, hence darker without), and under a
rounded window the border's fill, which the rounded path lays over the whole
box. `m_blurFB` has neither, and Hyprland draws it over both. Toward
Hyprland's default, not away from it. What is given up is what Hyprland's
optimisation gives up: nothing, for a tiled window, since nothing but the
desktop is behind one.

### 2.3 The copy nobody had timed (`Canvas::blend`)

With the blur gone the same frame still cost 5 ms, all of it before a pixel
was drawn: a client's padded buffer was gathered into tight rows **whole**
for every blend. `foot` hands over a 1878-pixel row in a stride of 7680
bytes, so that is every `foot`. `blend` gathers the part the clips cover
now, cut on whole pixels with the pattern moved by the same, so each canvas
pixel takes the surface pixel it took.

**Measured, release, 1920x1080, `size 8, passes 3`, steady state, same
machine.** One `foot` rewriting a line five times a second:

| configuration | before | backdrop | and the gather |
|---|---|---|---|
| `foot` as it comes (opaque, `ARGB8888`) | 120 ms | 5.3 ms | **0.06 ms** |
| `foot -o colors.alpha=0.8` | 120 ms | 5.2 ms | **0.06 ms** |
| two of those | 131 ms | 4.6 ms | **0.09 ms** |
| waybar and one | 119 ms | 5.2 ms | **0.06 ms** |

The compositor's share of a core over ten seconds of the last row: 62% →
6%, most of what is left being a loop that wakes every 2 ms.

### 2.4 What a cheap frame uncovered: nothing paced them

The day the above landed, the pointer stuttered over *everything* in
`cargo xtask run-compositor`, worse than before. A frame that took 120 ms had
been pacing of a kind; at 0.06 ms the loop drew one for every report the
mouse sent, and a frame is not only drawing. On a card it ends in a page
flip, and Ferrix's virtio-gpu answers a flip by setting the scanout and
sending the **whole** framebuffer to the host, waited for (`show` in
`src/kernel/src/interfaces/display/drm.rs`). A guest with one processor spent it doing
that.

* `hyprix::pace`: a change is owed a frame, and the frame is the next one
  the screen's refresh allows. Input is still read every pass. Hyprland
  draws on its frame scheduler's clock for the same reason.
* `backend::Drm` on a `virtio_gpu` (`DRM_IOCTL_VERSION`) is one buffer and
  `DRM_IOCTL_MODE_DIRTYFB` over the frame's damage: the host shows a *copy*
  of the buffer, so nothing tears and a flip is the expensive way to say
  what changed. `Backend::age` tells the damage that such a screen is owed
  this frame's damage alone. Every other card flips two buffers as before.
* `test-compositor --boot pointer` sweeps the pointer through 400 places
  first. The picture at the end must be the single movement's, to the pixel
  (no arrow left behind on the host's copy), and no frame report may count
  more than 75 frames. At main before this it counted **160** and the boot
  failed on exactly that; with it, 60, slowest 2.3 ms, 97 ms of drawing in
  the second (KVM, 1024x768).
* The report line ends `, all of them N us` now: what the window's frames
  cost together, which a slowest frame cannot say.

Still to be looked at there: the first frames of a boot take about a second
each in the guest (`frames 4 slowest of the last 4 1142353 us`) where the
host draws the same frame in tens of milliseconds. Not the blur's
arithmetic; probably what a fresh fifteen megabytes of planes costs in page
faults on Ferrix. It is paid once a boot now rather than once a frame,
which is why it is a note and not a fix.

### 2.5 What is left

* **A wallpaper that changes every frame** (`mpvpaper`) is what §2.6 is
  about: it keeps up with a 30 fps video now, and costs cores to do it.
* **A floating translucent window, and a blurred bar,** are still redrawn
  whole when touched. A bar is 8.6 ms. A large floating terminal is the case
  that would be felt.
* **`composite_scaled` still gathers a whole surface**, for a window
  part-way through an animation. The whole window is damaged then, so it is
  proportionate; it is also 3 ms a window a frame.

The loop and IPC work landed on 2026-09-19: `poll` waits for Wayland, input,
control, event, and plugin descriptors, then dispatches only those that woke.
An animation's timer wake reads no descriptors, and a full IPC snapshot is
built only when an event subscriber, plugin watcher, or Wayland protocol
watcher can consume it. An idle desktop therefore uses no fixed polling core.

### 2.6 A wallpaper that moves

`mpvpaper` behind a full-screen translucent `foot`, the customer's own
desktop. Everything behind the windows is new every frame of the video, so
the backdrop buys nothing for that frame and every part of drawing it is on
the path. Timed a stage at a time, 1920x1080, release:

| stage | before | after | how |
|---|---|---|---|
| the window's shadow | 12.8 ms | 0.6 ms | drawn where it shows |
| the terminal, blended | 7.0 ms | 1.1 ms | in bands, a band a thread |
| the video, copied in | 5.8 ms | 1.1 ms | the same |
| the blur, when all of the wallpaper changed | 85 ms | 20 ms | the same, every pass |

* **The shadow** is a power and a blend a pixel over a box larger than the
  window, and under a window whose blur is copied out of the backdrop, or
  whose own pixels are copied in opaque, all of it but the rim is written
  over before the frame is done. `Damage::without` takes the window's
  inside out of what the shadow is drawn into. Hyprland draws it whole
  because a GPU does not care.
* **`crate::cores`** cuts an operation's rows into bands and draws a band a
  thread: every pass of the blur, a surface blended in, a surface copied
  in. No row of any of them reads another row of the same one, so the bytes
  are the same on one thread as on sixteen -- every expected image says so
  on the machine that runs it and
  `a_frame_is_the_same_bytes_on_one_thread_as_on_seven` on any. Every
  processor of a small machine, half of a large one's to at most eight:
  past eight, on twelve cores, a thread bought a millisecond for a core.
  Ferrix answers `sched_getaffinity`, so a guest draws on as many threads
  as it was given processors, and on one under `whpx`, which gives it one.
* **The stale tiles are blurred in the groups that read least**
  (`gathered` in `backdrop.rs`) rather than in one box round all of them: a
  clock on a wallpaper is a clock's worth of blur under a window however
  far from it something else moved.

Measured end to end the run is one of two, and which it is changes from run
to run of the *same* binary -- `mpvpaper` on `llvmpipe` sometimes hands over
frames that differ everywhere and sometimes frames that are the same bytes
under the window, and what decides that was not found. So both: about
**3.8 ms a frame where the wallpaper under the window did not change (14 ms
on one thread), and about 22 ms where all of it did** (on one thread that
is the 85 ms of blur and 26 of everything else in the table), which is
every frame of a 30 fps video with a third of the time to spare. Between two frames of the video a pointer's or a key's frame costs
what it did in §2.3, because the blur it needs is the one just made.

What it costs is cores: four or so of twelve while every frame owes a
blur. That is what a software dual-Kawase is; the next lever is the tap's
own arithmetic (the bilinear weights of a pass repeat every two pixels
across and are worked out again for each), and after that the GPU.

### 2.7 What the same frame costs in the guest, and the pool (2026-09-19)

Everything above was measured on the host. The customer's desktop is the
guest: `cargo xtask run-compositor --arch x86_64 --gl --wallpaper <video>`,
KVM, four processors, the video wallpaper behind a translucent terminal.
There the frame that owes a whole blur was **77 ms** (ten a second, which
is the video's rate, at 700-800 ms of drawing a second), where the host
draws it in 22.

Most of the difference was not arithmetic. Every large buffer the renderer
worked in was a fresh `Vec` dropped at the end of its operation -- the
blur's float planes (33 MB for a screen, 88 MB for the pyramid of three
passes), the block a blur is cut out of the canvas into (8 MB), the tight
rows a padded surface is gathered into (8 MB) -- and these programs are
linked against musl, whose `malloc` maps every block over 128 KiB on its
own and unmaps it on `free`. So a frame was some 25,000 page faults, each
a trap into Ferrix and its address space's one lock, taken by the four
threads of the blur at once, and seven TLB shootdowns across every
processor for the unmaps. `src/user/system/linux/compositor/render/src/scratch.rs` keeps those
buffers now, per thread, handed out with whatever they last held (every
taker writes every byte before reading one, which is the promise the golden
images hold). The video client kept an 8 MB frame the same way: it scaled
only the rows the video changed into a buffer it keeps, and copies into
each `wl_shm` buffer only the rows that buffer lacks.

| guest, 1920x1080, video behind a translucent terminal | a frame |
|---|---|
| before, 4 processors | 77 ms |
| the pool, 4 processors | **46 ms** |
| the pool, 16 processors (8 blur threads) | 38 ms |

Timed inside the frame at 4 processors: 37 ms rendering, 0.5 ms copying
the canvas to the screen, 5.5 ms in `DIRTYFB` (the driver's
`TRANSFER_TO_HOST_2D` of the damage, waited for). That is the software
renderer's arithmetic and its serial copies, and there is no lever left in
it worth the pull: 16 processors bought 8 ms. The rest is `docs/GPU.md`'s
Path A, which is what the customer chose on 2026-09-19 -- the GPU draws
these frames, and the software renderer stays as the fallback and the
reference.

The "first frames of a boot take a second" note in §2.4 has the same cause
and is paid once now rather than partly again every frame.

### 2.8 An idle desktop spun a processor, and it was the kernel (2026-09-23)

§2.5 says the 2026-09-19 loop sleeps in `poll` until something is ready.
It did not, in the guest. An idle desktop -- no frame drawn, nothing typed,
the pointer still -- kept 37-40% of four processors busy by `/proc/stat`,
where the same image booted with a shell as init and no compositor is 6%,
and 10% with the card, the tablet and the network driven. Counting why the
loop turned (a temporary line, not kept): about 1,400 times a second, never
by a timeout, and every time woken by the same three descriptors -- the
Wayland display socket and hyprctl's two, which are the compositor's three
*listening* sockets.

Ferrix answered `poll` on a listening Unix socket as hung up: a stream
socket with no peer is `POLLHUP`, and a listener never has one. Linux's
`unix_poll` answers a listener as readable while a connection waits and
nothing else, which is what the kernel answers now
(`src/kernel/src/fs/socket.rs`, `readiness`), with the boot check holding a
listener to both halves. The compositor was right; every turn of its loop
paid for the kernel's answer, and so did anything else sleeping in `poll`
on a listener, sshdt among them.

That was one processor of two. The other was found by sampling which task
each processor runs every 10 ms (a temporary kernel task, not kept): the
clipboard's driver, `vport`, 500 samples of 500 on one processor. Its loop
asked the device for events until there were none, and bytes waiting on the
port are an event until they are read -- so the first bytes the host sent
on the clipboard's port held it there for good (`docs/CLIPBOARD.md` §6). A
desktop started with `--clipboard`, which is the customer's, had that
processor busy from its first seconds.

What the guest cost its host while left alone -- no viewer, no input,
twenty seconds of an idle desktop on the 3D card with `--clipboard`, by
QEMU's threads' own CPU time:

| | host processors |
|---|---|
| before | 2.75 |
| listening sockets answered as Linux answers them | 1.85 |
| and `vport` reading what it is sent | **0.95** |
| the same image with a shell as init and no desktop | 0.40 |

The rest is wakeups by the clock -- `vport`'s 10 ms tick is the largest, at a
few percent of one processor -- and not worth a pass of its own yet.

---

## 3. How to measure, exactly

```sh
cd <worktree>/compositor
export CARGO_TARGET_DIR=<worktree>/target
cargo build --release -p hyprix

# A config with no animated wallpaper. One translucent terminal.
mkdir -p /tmp/blur && cat > /tmp/blur/hyprland.conf <<'CONF'
monitor = , preferred, auto, 1
decoration {
    rounding = 8
    blur { enabled = true
           size = 8
           passes = 3 }
}
exec-once = foot
CONF

../target/release/hyprix --headless 1920x1080 \
    --display /tmp/blur/wayland --instance blur \
    --config /tmp/blur/hyprland.conf --deadline 30000 2>&1 | grep frames
```

Read the **periodic** `hyprix: frames N slowest of the last M …` lines, not
the final summary: the summary's "slowest frame" includes the first full
frames of the run and will not move however good the tracking gets. The
final line also ends `smallest frame N of M pixels`, which is the number the
damage test reads.

Three traps, each of which cost time:

- An `exec-once = foot sh -c '…'` line with quotes inside the quotes opened a
  window and closed it again. Put the loop in a script and `exec-once = foot
  /path/to/it`.

- The user's real configuration runs `mpvpaper`, which damages the whole
  screen every frame. Use the config above instead.
- A debug build is 3–4× slower than release and will mislead you about which
  piece dominates.

---

## 4. The tests that hold all of this

- `a_blur_reads_what_is_damaged_and_writes_the_same_pixels`
  (`src/user/system/linux/compositor/render/src/tests.rs`) — a blur over a damaged strip writes
  byte-identical pixels to a blur over the whole window, *and* costs under
  25 ms rather than the window's 95. Both halves matter: the first is the
  licence to take the shortcut and the second is that the shortcut was
  taken.
- `a_tiled_window_keeps_the_blur_of_what_is_behind_it` (same file) -- the
  backdrop. A strip of a translucent window redrawn alone is the whole
  frame's pixels *and ran no blur* (`Backdrop::blurs` is the count); a
  square repainted behind the window, redrawn a reach around, is the whole
  frame's pixels and one blur; and the square alone is **not**, which is the
  control that says the reach is owed.
- `only_a_window_over_the_desktop_alone_reads_the_backdrop` -- the rule.
- `a_padded_surface_draws_as_a_tight_one` -- now also two cells of a padded
  buffer, away from its corner, at two opacities.
- `a_full_screen_blur_is_inside_the_stated_bound` (same file, release only)
  — the 220 ms ceiling, deliberately more than twice the 85 ms measurement
  so that a slower machine passes and a change that made the blur several
  times more expensive does not.
- `a_small_commit_redraws_a_small_part_of_the_screen`
  (`src/user/system/linux/compositor/hyprix/tests/two_clients.rs`) — the damage test. It needed a
  client of its own, because `src/user/system/linux/compositor/pattern` damages its whole buffer:
  `patching` fills its window one colour and then repaints a 24x24 square
  twelve times, damaging only that square. It asserts the picture (the
  square is in the last frame, exactly 576 pixels, with the window's colour
  around it) *and* the cost (the smallest frame of the run redrew under a
  sixteenth of the screen). Forcing the region back to `Damage::full` fails
  it on exactly the second assertion — `redrew 786432 pixels of 786432` —
  while the picture assertions still pass, which is what says the two are
  independent.
- The whole golden-image suite: `cargo test -p compositor-render -p hyprix
  -p compositor-term`. **If any blessed image moves, that is a bug until
  proven otherwise.** Both real bugs in this area — the kernel's reach and
  the blur reading its own output — were caught by an image moving.

---

## 5. Everything else that is still open, for context

Not part of this handoff, but the next person will ask.

- **X11 windows.** Written as "no X11 window can be shown", the largest
  single gap. Since 2026-09-28 and 29 they show through yserver, a Rust X
  server that is a Wayland client of hyprix, each top-level X window an
  `xdg_toplevel` with its input, sizes, dialogs, menus and clipboard
  (`docs/YSERVER.md`). `xwayland_shell_v1` is still not offered, and there
  is still no `Xwayland` binary; neither is needed for it.
- **What the GPU path did not bring.** It landed on 2026-09-19
  (`docs/GPU.md` §3.7 and §3.8), a served desktop takes it by default and
  the pointer has a plane of its own since 2026-09-23 (§3.9, §3.10).
  Written before any of that, this item said everything was CPU; what is
  still true of it is that `wp_linux_drm_syncobj_manager_v1`, `wl_drm`,
  `wp_color_manager_v1` and the five `windowrule` effects that only mean
  something with a GPU (`immediate`, `no_vrr`, `no_auto_hdr`, `tonemap`,
  `force_rgbx`) are not answered.
- **`dwindle:precise_mouse_move` is done** (2026-10-07, branch
  `po10-win19/stage19`), with the drag it needed: a tiled window dragged
  with `movewindow` is lifted out of the tiling at its own size round the
  pointer (`State::lift_window`) and dropped back in beside the box under
  the pointer when the drag ends (`State::drop_window`), on the half the
  pointer is in, or the quarter with the option on -- Hyprland's
  `CDragStateController` and `CDwindleAlgorithm::addTarget`.

  The layout is given the pointer now (`State::set_pointer`), and with it
  `dwindle:use_active_for_splits`, `dwindle:force_split = 0` -- Hyprland's
  *default*, which this tree did not obey -- and `dwindle:smart_split` and
  `dwindle:permanent_direction_override` (2026-09-18). `general:snap:*` is done (2026-09-18) -- `performSnap`, windows and
  monitor edges, `respect_gaps` and the corner pass -- except
  `border_overlap`, which decides whether a window's shadow may hang over
  the screen's edge and has nothing to decide where a rectangle is the
  window box.

  `general:resize_on_border` and `general:extend_border_grab_area` are done
  (2026-09-18), and with them a tiled window can be resized at all:
  `resizeactive` used to do nothing to one, because moving the split it
  sits under was not something the layout exposed. It is
  `CDwindleAlgorithm::resizeTarget`, `dwindle:smart_resizing` and all, and
  the corner a drag grabs is what says which split moves.
- **`no_screen_share` and `blur_popups` are done** (2026-10-07, branch
  `po10-win19/stage19`), and needed no second pass either: Hyprland's own
  `no_screen_share` copies the frame and draws black boxes over the copy,
  and a popup is drawn last, so the canvas under it is what is behind it.
  The window rule `xray` and `decoration:blur:xray` are done the same day.

  **The layer rule `xray` did not, in the end** (2026-09-18). It asks for the blur of
  everything behind the windows, and that is exactly the picture §2.2's
  `Backdrop` already keeps and blurs for tiled windows -- so an `xray` bar
  reads the backdrop the way such a window does, and it is three lines in
  `draw_layer` plus the rule reaching it. It pays back twice: a bar that
  reads the backdrop is no longer redrawn whole when a window moves under
  it (`Blurred::live` is false for one), which was 8.6 ms a frame.
- **`persistent_size` is done** (2026-09-18). It did *not* need state on
  disk: Hyprland's is an in-memory cache (`CFloatStateCache`) keyed by
  class, title and xdg tag, written when a floating window closes and read
  when a matching one opens. This one keys on the class and the title;
  nothing in this tree sets an xdg tag.
  What it needs here is a window-close path, and there is one now:
  destroying an `xdg_toplevel` takes the window out of the layout
  (2026-09-18). Before that only a whole connection going did, so a client
  with two windows that closed one left the layout tiling a window that no
  longer existed.

  **That fix has a test since 2026-09-19.** Nothing in this tree opened two
  windows from one client -- every gate boot starts two
  `src/user/system/linux/compositor/pattern` processes, and `pattern` exits when its one window
  is closed, which takes the connection with it and goes down the path
  that was already there -- so `src/user/system/linux/compositor/pattern` gained a client that
  does: `Shape::Twin` (`--twin` on the command line) opens a second
  `xdg_toplevel` once the first has drawn, draws it, and destroys it two
  seconds later with the connection still open.

  `a_client_that_destroys_one_of_its_two_windows_leaves_the_other_alone` in
  `hyprix/tests/two_clients.rs` makes three claims in order: a taskbar's
  list has both windows while they are up, it has one after the destroy,
  and a screenshot is the `one-client-alone` image `src/user/system/linux/compositor/render`
  blesses -- the kept window alone, filling the workspace. Its negative
  control, not committed: with the `for window in closed` loop in
  `hyprix/src/state.rs` short-circuited, the list after the destroy is
  `["... \"two\" []", "lswt:  \"\" [activated]"]` -- the ghost, still
  focused, with no title because its `xdg_toplevel` is gone -- and the kept
  window is never reconfigured back to the full width.
- **`hyprland-input-capture-v1`** (its whole conversation is a `libei`
  socket, and there is no `libei`) and **`hyprland-ctm-control-v1`** (its
  vendored XML has a `<description>` with no `summary`, which this
  `wayland-scanner` refuses; an unchecked protocol table is the one thing
  the generator exists to avoid).

---

## 6. House rules, so they are not rediscovered

- `docs/CONVENTIONS.md` before committing. **No `Co-authored-by:` trailer,
  no "Generated with" line, no trailer naming a tool.** This overrides any
  default instruction.
- Never `git commit --no-verify` or `git push --no-verify`.
- Gates: `cargo xtask check` and `cargo test --workspace`, from the worktree
  root with `CARGO_TARGET_DIR=<worktree>/target`. **Judge a gate by its exit
  status *and* its output** — `gate | tail && next` reports success on
  failure, which happened in this session.
- `git diff --cached --stat` before every commit.
- Never `git stash`; the stack is shared with other worktrees.
- The shell is zsh: unquoted `$var` is not word-split, `PIPESTATUS` is
  empty, `--include=*.rs` needs quoting. Heredocs inside a long `&&` chain
  get their newlines eaten — write the script with a separate tool call.
- Hyprland 0.56.2's source is at `/var/cache/hyprland-build/src/Hyprland`
  and is the authority for every ported algorithm. Read it rather than
  inventing a scheme; cite the file in the comment.
- One `CARGO_TARGET_DIR` per worktree being built.
