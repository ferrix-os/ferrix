# Stage 18 — The compositor ✅  ·  *96 points, spent*

A Wayland compositor in Rust, on `src/lib/`' side of the tree as its own
workspace the way ferrousli is, **written from scratch** rather than on the
Smithay crates: decided on 2026-09-17, the reasoning in `docs/BACKLOG.md`.
The wire protocol, the window management, the layouts, the configuration and
the IPC are all written new, in Rust, to Hyprland's shape; the only C library
allowed is `xkbcommon`.

* **Protocols:** `wl_compositor`, `wl_subcompositor`, `wl_shm`, `wl_seat`
  with keyboard and pointer (the keymap a client compiles with its own
  `xkbcommon`; as built, the compositor links no C at all, and ships keymaps
  libxkbcommon printed on a host, in `src/user/system/linux/compositor/xkb`), `xdg_shell` with toplevels and popups, `xdg_decoration`,
  `wlr_layer_shell` for bars; `zwp_linux_dmabuf` withheld until stage 19.
* **Rendering** on the CPU into stage 17's dumb buffers: damage tracking,
  a pixman-shaped Rust rasteriser (`tiny-skia`), one page flip per frame,
  frame callbacks on vblank.
* **The wire protocol with no libwayland** (`src/user/system/linux/compositor/wire`): the message
  header, every argument type, descriptor passing over `AF_UNIX`, and the
  object map, written from `/usr/share/wayland/wayland.xml` and fuzzed
  (8 points).
* **XKB data in the image:** planned as a pinned subset of xkeyboard-config,
  the files xkbcommon's keymap compiler reads (2 points). Built instead as
  keymaps compiled on a host and committed — `us`, `de`, `de-nodeadkeys`,
  `fr` and `gb` — so the image carries no XKB data.
* **Hyprland's shape:** the dwindle and master layouts, workspaces, the
  keybind and dispatcher model (`movefocus`, `movewindow`, `workspace`,
  `killactive`, `togglefloating`, `fullscreen`), gaps and borders, window
  rules, and a configuration file that parses Hyprland's `hyprland.conf`
  syntax (sections, `$variables`, `bind`, `windowrule`, `exec-once`).
* **IPC:** a Unix socket with `hyprctl`'s request shape (`clients`,
  `workspaces`, `activewindow`, `dispatch`, `keyword`, `reload`) and the
  event socket, so a Rust `hyprctl` and a bar can be written against it.
* **Clients for the tests,** in Rust over `wl_shm`: a solid-colour client
  that draws a given pattern, and a small terminal emulator over the
  console's pty, both static, both under `cargo xtask` like busybox.

**Done — the configuration, before the Smithay decision needs making.**
`src/user/system/linux/compositor/` is a workspace of its own, gated by `cargo xtask check`. Its
first crate, `src/user/system/linux/compositor/config`, parses `hyprland.conf` as hyprlang does:
categories and the `category:key` shorthand, `$variables`, `##` escapes,
`source`, Hyprland's option types and defaults for the options the compositor
implements (integers that are also booleans and colours, floats, gradients,
CSS gaps), `bind` with all fourteen flag letters, `unbind` and submaps, and
the keywords later parts interpret, with Hyprland's diagnostics and the rest
of the file still applied past a bad line. `Config::keyword` is `hyprctl
keyword`. Host-tested and fuzzed (`hyprconf_parse`).

**Done — the layout and dispatcher core.** `src/user/system/linux/compositor/layout` is Hyprland's
window management as rectangles and ids, checked against Hyprland's source:
monitors, workspaces made and dropped on demand, the dwindle tree (split
direction, `preserve_split`, `force_split`, split ratio) and the master layout
(`mfact`, orientations, `new_status`, `new_on_top`) dividing the work area
inside `gaps_out`, `gaps_in` and borders between windows, floating and
fullscreen windows, the focus history, and `movefocus` (edge search, angle
search for floating windows, wrap-around), `movewindow` (dwindle's take-out
and put-back at the focal point, across monitors too), `workspace` and
`movetoworkspace`(`silent`) with `N`, `+N` and `e+N`, `killactive`,
`togglefloating` and `fullscreen`, run from a `Bind`. Every call returns the
changes it caused. Host-tested; where it departs from Hyprland (no cursor
then, so `force_split` 0 took the second half, until 2026-09-18 gave it the
pointer's position; pseudotiling; floating `movewindow`) its crate docs say so.

**Done — the renderer, the last of the pure crates.** `src/user/system/linux/compositor/render`
draws a frame into the `XRGB8888` dumb buffer stage 17 gives it, on the CPU,
with no C: a `Canvas` over a `tiny-skia` pixmap (pinned at `=0.12.0`, default
features off, no build script) with `clear`, `fill`, `border` and `composite`
-- `ARGB8888` source-over, `XRGB8888` copied and made opaque -- each drawn
only inside a `Damage` of disjoint rectangles, so a translucent client blends
every pixel once. `present` writes into a target of any stride, `render` draws
one monitor from `src/user/system/linux/compositor/layout`'s output with `src/user/system/linux/compositor/config`'s
border colours, and `damage_between` two layouts is the region a frame has to
redraw. The everyday gate is pixel comparison on the host: the two pattern
clients stage 18's tests will run are drawn in code, and a run-length expected
image of them tiled dwindle-style at 1024x768 is committed and compared byte
for byte, with a negative control that alters one pixel and requires the check
to name exactly it. Twenty tests then, 86 now. It holds no Wayland object and no
descriptor, so it carries over whichever way the compositor-server decision
goes; its crate docs say how a Smithay `Renderer`/`Frame`/`ImportMem` or a
server written from scratch wraps it.

**Done — the wire protocol, with no libwayland (2026-09-17).**
`src/user/system/linux/compositor/wire` is the bottom of the server: the message header, every
argument type, descriptors travelling beside the bytes rather than in them,
and the per-client object map that keeps a client's ids and the server's in
their own halves. It holds no socket and no descriptor -- an `fd` is an `i32`
here and nothing more -- so it is host-tested and fuzzed as `src/lib/network/netwire`
and `src/lib/proto/inputctl` are, and the part that has to run on Ferrix to be tried
is only the socket above it.

It is written from the protocol and from `connection.c`, so by construction
nothing in it is checked against a real implementation. The check is a probe,
in the shape `src/lib/proto/linux-abi/probe` set: `src/user/system/linux/compositor/wire/probe/wire.c`
drives a real libwayland client and a real libwayland server over socket
pairs it owns and prints the bytes each wrote, `probe/wire.txt` is that
output committed with the libwayland version on its first line, and the tests
require this crate to write the same bytes and to read them back. Fifteen
message shapes, both directions, every argument type: `new_id`,
`wl_registry.bind`'s unnamed `new_id` with its interface string, a null
`object` where the protocol allows one, negative `int`s, a descriptor that is
not in the byte stream, `array`s of three lengths and `fixed` at 1.5 and
-2.25. Its negative control, not committed: with a string's length counting
its bytes without the NUL -- the likeliest way to get the format wrong -- six
tests fail, `every_message_libwayland_sent_is_written_the_same_way_here`
among them.

Reading libwayland taught it two things it would not have got right from the
protocol. It is stricter in one place: a message whose arguments do not use
every byte its header claimed is refused, where libwayland consumes the rest
without looking, because that check catches a wrong signature in this crate's
own tables rather than letting it misread the next message. And it must not
be stricter in another: the padding after a string or an array is never
looked at, because `serialize_closure` steps over it and `wl_closure_queue`,
the path a queued request takes, allocates its buffer with `malloc` -- so a
real client's padding is uninitialised heap, and a server requiring it to be
zero would drop clients at random. Thirty tests (31 now); the `wayland_wire` fuzz
target reads a stranger's bytes against a signature it picks and requires a
failed read to consume nothing, every string it accepts to be UTF-8 without a
NUL, no descriptor to be invented, and everything the writer builds to read
back the same. It ran 65,887,144 inputs in ten minutes without a failure.

**Done — the interface tables, generated from the protocol (2026-09-17).**
`src/user/system/linux/compositor/protocol` is what tells `src/user/system/linux/compositor/wire`'s reader the signature
of the message it is about to read: every interface's requests and events by
opcode, their argument types, their `since` versions, which of them are
destructors, and every enumeration value. It is generated by
`tools/common/gen/gen-wayland-protocol.py` from XML vendored under
`src/user/system/linux/compositor/protocol/protocols/` -- at first `wayland.xml`, `xdg-shell.xml`,
`xdg-decoration-unstable-v1.xml` and `wlr-layer-shell-unstable-v1.xml`, and
56 files by stage 19, each
carrying its own permissive licence, copied into the generated file. The XML
is vendored rather than read from the machine because a table built from
whatever `wayland-protocols` the builder happened to have installed would
change under the compositor without a commit. `cargo xtask check` runs the
generator with `--check`, so a hand edit fails the gate.

A generator can read XML wrong, and nothing in it would notice, so the tables
are checked against an implementation the way `src/user/system/linux/compositor/wire` is:
`probe/interfaces.c` links against libwayland's own compiled
`wl_*_interface` structures -- libwayland's for the core protocol and
`wayland-scanner`'s output from the same vendored XML for the rest -- and
prints every interface's version and every message's opcode, name and
signature. `probe/interfaces.txt` is that output committed, and the tests
require the generated tables to agree: 31 interfaces and 194 messages when
this landed, with the count asserted so a table quietly dropping messages
fails; the count is now checked per interface, over more than a hundred of
them. The one place
the two spellings differ is `wl_registry.bind`'s unnamed `new_id`, which
libwayland expands into the three things it is on the wire and this tree
keeps as the protocol's single argument; the test folds them back and says
why.

Two negative controls, neither committed. With `allow-null` read as its own
opposite in the generator -- a nullable flag is the subtlest thing to get
wrong -- `wl_display.error`'s signature disagrees and the comparison fails.
With `wl_surface.commit`'s opcode changed by hand from 6 to 7, `--check`
reports `src/user/system/linux/compositor/protocol/src/generated/core.rs is stale` and exits 1.

**Done — the connection, `wl_display` and `wl_registry` (2026-09-17).**
`src/user/system/linux/compositor/server` is the protocol half of the compositor and holds no
socket, no descriptor and no pixel: a `Client` is handed the bytes that
arrived and the descriptors that came with them and gives back the bytes to
send, so object lifetimes, versions and every way a client can break the
rules are host-tested, and running it on Ferrix will test the socket rather
than the protocol. A connection starts as libwayland starts one, with
`wl_display` as object 1 and nothing else. `sync` makes a `wl_callback`,
fires it and takes the id back with `wl_display.delete_id`; `get_registry`
announces every global in order; `bind` checks the name, the version and the
interface the client named, because a client binding `wl_shm`'s name while
saying `wl_seat` would otherwise get a `wl_shm` answering seat requests. The
object map is `src/user/system/linux/compositor/wire`'s, now carrying the server's own state for
each object, so there is one map of live objects rather than two that can
disagree about which exist.

A protocol error is the end of a connection -- Wayland has no way to refuse
one request and carry on -- so every refusal queues one `wl_display.error`
and stops reading, and nothing after it is answered. Writing that down found
a defect: `wl_display.error`'s object argument is not nullable, so a client
that sent a `new_id` of zero would have had its error event silently fail to
encode and would have seen the socket close with nothing said. The error now
names `wl_display` where it has no object to name, which is where libwayland
posts one it cannot attribute.

Twenty-four tests then (the crate has 94 now), and the last of them is a real client:
`probe/roundtrip.sh` runs `examples/transcript.rs` to get the server's answer
to the conversation every toolkit opens with, replays it to a real libwayland
client through `probe/roundtrip.c`, and records what the client made of it.
libwayland reported all six globals with their names and versions and
`wl_display_roundtrip` returned with no error. The recording is rebuilt by the
test and compared, so a server that changed its answer cannot leave the
check passing against a conversation that no longer happens. Its negative
control, not committed: with `wl_registry.global`'s name and version written
the other way round, libwayland reports `global 6 wl_compositor 1`.

**Done — surfaces and shared memory (2026-09-17).** `wl_compositor` with
its surfaces and regions, and `wl_shm` with its pools and buffers: everything
a client needs to put a picture somewhere, short of a shell to give it a
window. The double buffering the protocol is built on is written as two
states of the same shape rather than as dirty flags, since flags are where
that bug lives: every request but `destroy` and `frame` changes the pending
state and `commit` makes all of it current at once, taking the damage and the
frame callbacks and leaving everything a commit did not mention alone. A
buffer a commit replaced is the client's again and one committed twice is
not, because a client that committed the same buffer again never got it back.
An object made by another inherits its version, so a `wl_surface` from a
`wl_compositor` bound at 4 is never sent `preferred_buffer_scale`, which
arrived in 6.

**It is stricter than libwayland in one place, on purpose.**
`wayland-shm.c`'s bounds check reads `stride < width`, comparing bytes with
pixels: for a four-byte format a client may pass `stride == width`, a quarter
of the row it needs, and libwayland takes it. The pool then only has to hold
`stride * height` bytes while a compositor reading `width` pixels from each
row reads `width * 4` from the last row's start and runs off the end. Here
the stride must be at least `width * 4` and the arithmetic is checked rather
than guarded by the division libwayland uses to keep its multiply from
overflowing. Every real toolkit sends `width * 4` or more, so the rule costs
nothing and closes an out-of-bounds read. Only `ARGB8888` and `XRGB8888` are
offered, because a format announced and not drawn is a client rendering a
frame nobody can show.

Forty-two tests, and one of them is the whole point: `probe/roundtrip.c` now
drives a real libwayland client through everything it does to show a
window -- bind, create a surface and a region, make a pool and a buffer,
attach, damage, ask for a frame callback, set the scale and commit -- and
records the bytes it wrote. The test replays them into the server and
requires the surface, the region, the pool, the buffer's rectangle and the
commit to be what the client asked for. Not one byte of that test is written
by this tree.

**Done — `xdg_shell` and the socket, and a real client's whole handshake
(2026-09-17).** `xdg_shell` is how a surface becomes a window: the configure
conversation by which the compositor and the client agree on a size, the
serials that pin each one, and the toplevel state a tiling layout needs. Its
rules are the protocol's. A surface may be given one role and no second; a
client may ack a configure it is several behind on, which drops the older
ones with it; and a buffer may not be attached at all until a configure has
been acked, which is the rule that stops a client painting at a size the
compositor never agreed to. `set_max_size` and `set_min_size` are recorded
and not obeyed, and `move`, `resize` and `show_window_menu` are ignored, as
Hyprland ignores them for a tiled window.

`src/user/system/linux/compositor/socket` is the first part of the compositor that has to be on
Ferrix to be tried: an `AF_UNIX` listener and the `sendmsg`/`recvmsg` control
messages that carry descriptors, which the standard library has no stable way
to do. It is the crate's only `unsafe`, split one operation to a block as the
rest of the tree is, and its tests send a descriptor through a socket pair and
read the file on the far side to show it is the same open file and not merely
the same number.

**The whole handshake now runs end to end against a real client.**
`probe/live.c` is a libwayland client that connects to `examples/serve.rs`
over a real socket and does what every application does when it starts: bind
`wl_compositor`, `wl_shm` and `xdg_wm_base`, take `wl_shm`'s formats, make a
surface, give it an `xdg_surface` and an `xdg_toplevel`, set a title and an
app id, commit with nothing attached, take the `xdg_toplevel.configure` and
the `xdg_surface.configure` that follows it, ack the serial, make a pool over
a `memfd` sent through `SCM_RIGHTS`, cut a buffer, attach, damage, ask for a
frame callback and commit. The client was configured at 640x480 with
`activated` and `tiled_left`, acked serial 1, and `wl_display_get_error`
returned zero; the server mapped the surface. Both sides' output is recorded
and the tests require each step. Its negative control, not committed: with the
`xdg_surface.configure` that carries the serial not sent -- the configure
conversation's last message -- the client never acks, the compositor refuses
its buffer, and libwayland prints `xdg_surface#7: error 3: a buffer was
attached before a configure was acked`.

**Done — the compositor runs, and two clients are tiled on it
(2026-09-17).** `src/user/system/linux/compositor/hyprix` is the compositor itself: it reads a
`hyprland.conf`, binds a Wayland socket, starts what `exec-once` names, tiles
what connects to it with `src/user/system/linux/compositor/layout`, draws with `src/user/system/linux/compositor/render`
and puts the frame on a screen. Nothing in it parses a file, works out a
layout, draws a pixel or decodes a message; it is the loop that joins the
crates that do, and the two places the compositor touches the world -- a
client's shared memory, mapped read-only, and the screen.
`src/user/system/linux/compositor/pattern` is the client it draws: a whole Wayland client in one
file, over `src/user/system/linux/compositor/wire` and `src/user/system/linux/compositor/socket` rather than a toolkit,
which means the tests exercise those crates from both ends.

**The headless half of this stage's exit passes.** Two pattern clients
connect over a real socket, are tiled dwindle-style with the configured gaps
and borders, draw into shared memory, and the frame the compositor composed
is compared pixel for pixel against the expected image `src/user/system/linux/compositor/render`'s
own tests bless: 0 differing pixels of 786,432. The two pictures are built by
different paths -- one by calling the renderer with rectangles from the
layout, the other by two programs talking Wayland to a server that works the
same rectangles out from the requests they sent -- and nothing but the pixels
is shared between them. A second test runs one client instead of two and
requires the comparison to notice, so the check is known to fail when the
picture is wrong.

Writing it found two things. A window is reconfigured when *any* window
arrives or leaves, not only when it is made: a client that is not told is one
drawing at the size it had before, which the compositor then draws cropped,
and the first run showed exactly that. And the server refused the client's
own second pool for reusing an object id it had not destroyed, which was the
client's bug and the server being right.

What was left of this stage at that point: `wl_seat`, so a window can be
typed into; the `hyprctl` IPC; and the screen itself, which is
`src/user/system/linux/compositor/blank`'s DRM path moved behind `hyprix`'s backend so the same
frame goes to `/dev/dri/card0` under QEMU. All three have landed since.

**Done — a real toolkit runs on it (2026-09-17).** `src/user/system/linux/compositor/pattern` is
written against the same crates the server is, so a test with it shows the two
halves of this tree agree -- not that the protocol is right. `foot`, a
Wayland terminal built against libwayland and every other compositor, knows
nothing about this one, and running it found four gaps in an afternoon that
the pattern client could never have found:

* `wl_data_device_manager` was not offered at all, and the toolkit refused to
  start without it. The objects were made then and no selection was ever
  sent, which is exactly what a client sees when nobody has copied anything;
  selections landed later the same day (3ae8af90), the host's clipboard
  since (`docs/CLIPBOARD.md`), and a drag between clients after that. A
  compositor that advertises it and then does not answer `get_data_device` is
  worse than one that does not advertise it, because the client only finds
  out at its first copy.
* `wl_subcompositor` was advertised and `get_subsurface` was not answered, so
  the toolkit's first window died on `wl_subsurface#15: error 0: object 15 is
  not live`. Every toolkit makes subsurfaces -- a title bar, a shadow, a
  cursor -- so this was every toolkit.
* `wl_output` described nothing, and the toolkit printed `(null):
  0x0+0x0@0Hz`: a client with no mode has no size to scale against. It now
  sends geometry, mode, scale, name, description and the `done` that says the
  description is whole, and only the ones the version bound can read.
* `wl_seat` announced nothing, because there was no input path until stage
  17's L5 to L7 landed. A client may only ask for a capability the seat
  announced, so asking was `missing_capability` rather than a keyboard that
  never sends a key. It announces what there are devices for now.

With those, `foot` gets a window, works out its cell size from the mode this
compositor gave it, draws its terminal over 690,820 of the screen's 786,432
pixels, and exits by choice with no protocol error.
`src/user/system/linux/compositor/hyprix/probe/real-client.sh` records the run and the test requires
each step of it; the record summarises the busiest frame rather than
committing a picture of somebody else's font rendering.

**Done — `hyprctl`, driven by Hyprland's own client (2026-09-17).**
`src/user/system/linux/compositor/ipc` is the request shape and the answers: the flags in front of
a request, `[[BATCH]]`, and the JSON and readable forms of `version`,
`monitors`, `workspaces`, `clients`, `activewindow` and `activeworkspace`,
with Hyprland 0.56.2's field names in its own order, read from
`src/debug/HyprCtl.cpp`. A bar reads those by name, so a missing one is a
crash in somebody else's program. `dispatch`, `keyword` and `reload` come
back for the compositor to run, because the crate holds no compositor and no
socket; `src/user/system/linux/compositor/hyprix` binds the socket where Hyprland binds it, under
`$XDG_RUNTIME_DIR/hypr/<instance>/.socket.sock`, and a program looks there
and nowhere else.

The answers are checked twice. `src/user/system/linux/compositor/ipc`'s tests parse them back with
a JSON parser written in the tests -- the only way to say "this is JSON"
without the compositor taking a dependency for it -- and require Hyprland's
field order, that a window title holding a quote, a backslash and a newline
comes back as it went in, and that `-j -r` and `-j` are the same document.
Then `src/user/system/linux/compositor/hyprix/probe/hyprctl.sh` runs the real `hyprctl` against the
compositor and records what it printed: `version`, `monitors`, `workspaces`,
`clients` and `activewindow` in Hyprland's own shapes; `dispatch movefocus l`
moving the focus from the second window to the first; and `keyword
general:gaps_in 40` re-tiling both windows from 485 pixels wide to 450 while
the compositor runs. A compositor that took the keyword and did not re-tile
would still have said `ok`, so the test requires the sizes.

**Done — the compositor on a screen, on Ferrix (2026-09-17).**
`src/user/system/linux/compositor/drm` is the card, lifted out of `src/user/system/linux/compositor/blank`: the legacy
mode-setting calls and nothing a compositor does not need. `blank` drives the
screen through it still -- `cargo xtask test-display` passes unchanged,
negative control and all -- and `hyprix` drives it too, with two dumb buffers
drawn into in turn and shown with a page flip. One buffer would tear: the
card scans out of the same memory the compositor is writing. That is still
the software path; since stage 19 the GPU path shows the card the texture
the GPU drew into instead.

`cargo xtask test-compositor` boots the compositor as init on Ferrix with a
virtio-gpu and requires the screen. It opened `/dev/dri/card0` through the
kernel's display core and the ring-3 virtio-gpu driver, set the card's
preferred mode, bound its Wayland socket, drew a frame with
`src/user/system/linux/compositor/render` and flipped it, and every one of QEMU's 786,432 pixels is
the compositor's background.

Two things had to be true for it to run as init that are not true of a
program started from a shell. The kernel starts its first program the way it
starts a shell, so the compositor is handed `sh -i` or `sh -c <script>`: `-i`
alone now means "the defaults", and `-c` means the words after it are the
compositor's own arguments. And `XDG_RUNTIME_DIR` is a session manager's to
set, and a machine that has just booted has neither, so a bare display name
falls back to `/tmp` rather than the compositor refusing to start -- which
would be a compositor that only runs where something else ran first.

**And stage 18's exit criterion passes on the card.** The initramfs carries
`src/user/system/linux/compositor/pattern` at `/bin/pattern` and the compositor's own `exec-once`
starts two of them, so what reaches the screen is two real Wayland clients,
tiled dwindle-style with the configured gaps and borders, drawn from the
shared memory they committed. `cargo xtask test-compositor` compares QEMU's
screendump against the same expected image `src/user/system/linux/compositor/render`'s own tests
bless and `src/user/system/linux/compositor/hyprix/tests/two_clients.rs` compares against on the
host: every one of 786,432 pixels, on x86-64 and on AArch64. Its negative
control, not committed: with one client started instead of two, 362,542
pixels differ and the test says the picture is not the one the renderer
blesses.

Carrying the clients meant one change outside the compositor. `xtask`'s
initramfs wrote the files a caller asked for only when a shell was going in
beside them, and the reason given was that the boot check's archive must not
change -- but the boot check asks for no files at all, so what kept its bytes
the same was the empty list and never the branch. The files go in either way
now, and the test that named the old rule says the new one.

**Done — the seat: a window that can be typed into (2026-09-17).** The input
iteration landed the nodes; this is the compositor reading them. `hyprix`
opens every `/dev/input/eventN` through `src/user/system/linux/compositor/evecho`, grabs it, and
turns its events into `wl_keyboard` and `wl_pointer` ones: `enter` and
`leave` as the layout's focus moves and as the pointer crosses a window,
`key` with evdev's own code, `modifiers` with the masks the keymap declares,
`motion`, `button`, `axis` and the `frame` that groups them, and
`repeat_info` from `input:repeat_rate` and `input:repeat_delay`.

The keymap is a real one. `src/user/system/linux/compositor/xkb` carries the text libxkbcommon
itself printed for the `evdev` rules with the `us` layout -- 34,205 bytes,
from a committed probe -- and hands it to each client in a sealed `memfd`, as
Smithay's `SealedFile` does. The same probe asks libxkbcommon's own state
machine what each key holds and what each lock leaves locked, so which key is
`Shift` and which is `Caps Lock` is a property of the keymap here as it is
there, and the bits in `wl_keyboard.modifiers` are the indices that keymap
gives rather than constants somebody wrote down.

Keybinds fire. A bind matches when the key matches and the held modifiers are
*exactly* the bind's, Caps Lock and Num Lock excepted -- which is what lets
`SUPER, Q` and `SUPER SHIFT, Q` be two different binds -- and it eats its
key, release included, because a client told a key came up that it was never
told went down has that key stuck down for ever. The `r`, `e`, `n` and `i`
flags, `code:NN`, `mouse:NNN` and the wheel directions all resolve, and one
dispatcher path serves both a bind and `hyprctl dispatch`.

`cargo xtask test-seat` is the proof, on x86-64 and AArch64: QEMU's
`input-send-event` puts a key in at the far end of a `virtio-keyboard-pci`,
and the test requires the client to report the keymap it was handed, the
focus it was given, the pointer's position in its own coordinates, the button,
and the evdev code of the key -- and then requires the screendump taken after
the key to differ from the one before, because the client redraws on a key.
Then it presses `SUPER Q` against a carried `hyprland.conf` holding
`bind = SUPER, Q, killactive`, and requires the window to close and the
screen to be the compositor's background and nothing else. 712,932 of 786,432
pixels changed on the key; every one of them was the background after the
bind.

Two things this found. A `wl_keyboard` is usually asked for in the same burst
of requests that maps the window, so an `enter` sent when the layout focuses
that window can reach no object at all; the server now says whether an
`enter` arrived and the compositor asks again until it does, because a focus
remembered but never delivered is a window that can never be typed into. And
a compositor may not open its devices blocking: the first version read the
keyboard once round a loop that also had the clients and the screen in it,
and stopped all three until somebody typed.

**Done — the exit criterion (2026-09-17).** `cargo
xtask test-compositor` now does what this stage's exit asks, on x86-64 and
AArch64. The compositor starts from a `hyprland.conf` carried in the
initramfs; its `exec-once` lines start the two clients; they tile
dwindle-style with the configured gaps and borders; a keybind pressed through
QMP's `input-send-event` moves the focus and another swaps the windows; and
each of the three states is required from QEMU's screendump, pixel for pixel,
against an image `src/user/system/linux/compositor/render`'s own tests bless. Every one of 786,432
pixels, three times over, and the test also requires the three pictures to be
three pictures -- a compositor that ignored both keybinds would otherwise
pass every comparison if two expected images happened to be the same file.

The IPC half runs on the guest. Hyprland's `hyprctl` is not on Ferrix's
image, so `src/user/system/linux/compositor/ctl` is the same program written here: it finds
`$XDG_RUNTIME_DIR/hypr/<instance>/.socket.sock` where Hyprland's looks,
writes one line and prints the answer. `probe/hyprctl.sh` now runs every
read-only command through both clients against one compositor in one session,
and a test requires the two answers to be identical -- which is the whole
claim it makes. On Ferrix, two more binds run it:

    bind = SUPER, C, exec, /bin/hyprctl clients
    bind = SUPER, W, exec, /bin/hyprctl activewindow

`exec` is a dispatcher the compositor answers rather than the layout, because
starting a program is the compositor's to do; it is also how a person opens a
terminal. After the swap, `hyprctl clients` names both windows with the
titles and the class they set, and `hyprctl activewindow` names the
checkerboard -- the same window the third picture draws the active border
round.

A window is reconfigured on a change of *state* as well as of size now. A
window that has just been focused is the same size and a different state, and
a client that is not told has a title bar that never lights up.

**Done — the event socket (2026-09-17).** `.socket2.sock` is the second of
Hyprland's two: a bar connects once and reads a line for every state change,
and never writes. The line is `CEventManager::formatEvent`'s and no more --
`"{event}>>{data}\n"`, the data cut to 1024 bytes, every newline inside it
turned into a space so that one event is always one line however a client
titled its window -- and each payload is the one Hyprland's own `postEvent`
call builds, with the file and line cited on the variant that carries it. The
`v2` forms are sent beside the old ones, because both have readers.

The events are worked out from the difference between two descriptions of
the compositor rather than posted from inside whatever changed. Hyprland
scatters `postEvent` calls through its source and a change made somewhere new
is a change nothing reports; a difference cannot be missed. The cost is that
two changes in one pass are reported together and in one fixed order, which
no reader can tell from two changes a millisecond apart.

`hyprctl subscribe` reads it. That is not one of Hyprland's commands -- its
own readers are `socat - .socket2.sock` -- but Ferrix has no socat, and a
socket nothing on the image can read is a socket nothing proves. `cargo xtask
test-compositor` now starts it as the configuration's first `exec-once`, so
the transcript holds the whole stream: the monitor, the workspace, both
windows arriving with their class and title, and the focus moving each time a
keybind is pressed. The sockets are bound before `exec-once` runs now, which
is what lets a bar started that way find them.

**Done — `zwlr_layer_shell_v1`: the surfaces that are not windows
(2026-09-17).** A bar, a wallpaper, a notification and a launcher are not
windows: they are not tiled, they are not in the focus order, and they sit at
a fixed place on a fixed layer. `wlr-layer-shell-unstable-v1` is how every
wlroots-shaped compositor -- Hyprland included -- lets a client say so, and it
is what `waybar`, `hyprpaper`, `mako` and `wofi` are written against. Without
it a Hyprland user's setup does not start at all, which made it the largest
thing missing.

The protocol is answered whole for the four layers and the placement rules:
the anchors, the size with the protocol's own `invalid_size` rule for an axis
with no size and no two anchors, the margins, the exclusive zone, the
keyboard interactivity, and the configure conversation with its serials.
Where a surface goes is `src/user/system/linux/compositor/layout`'s `layers`, which follows
wlroots' `wlr_scene_layer_surface_v1_configure`: the usable area less the
margins, the size the client asked for on any axis it is not stretched
across, and each exclusive zone taken off the area the next surface is placed
in -- which is what makes two bars on one edge stack rather than overlap. The
zones become the monitor's reserved strips, so the windows tile in what is
left.

`src/user/system/linux/compositor/pattern --bar 30` is a bar, asking for what `waybar` asks for in
the order it asks. On the host it is drawn beside two windows and the frame
is compared against an image `src/user/system/linux/compositor/render`'s own tests bless; on
Ferrix, `cargo xtask test-compositor` boots a second time with a bar in the
`hyprland.conf` and requires the same picture from QEMU's screendump. Every
one of 786,432 pixels, on x86-64 and AArch64.

**Done — the terminal (2026-09-17).** The last thing this stage owed. It
needed pseudoterminals, which the kernel did not have and now does, and a
terminal emulator, which is the term app (ferrix-os/apps since 2026-10-04): stage 19's own entry has the
whole of it, since that is where the work landed. `cargo xtask test-pty`
proves the pair without a window and `cargo xtask test-compositor` boots a
terminal in the compositor and requires the picture it makes.

**Done — ARMv7-A (2026-09-23).** The compositor runs on the 32-bit
architecture too, the one the DK1 board is. ARMv7-A's `virt` machine had
been left without a card on the belief that it has no virtio-gpu, but it has
the same generic PCI host as AArch64's, which the kernel already enumerated
for its disks: the card, the keyboard and the tablet are three more devices
on it, without `iommu_platform=on` for U-Boot's sake like the others, so
their drivers run in degraded trusted mode. The compositor's programs are
built for `armv7-unknown-linux-musleabihf`, hard float; rav1d needs one
feature gate for its CPU probe on 32-bit ARM, which the compositor's cargo
config lets that crate alone have. Nothing in the compositor, the DRM and
evdev layouts or the kernel needed a 32-bit fix: `src/lib/proto/linux-abi` had
carried both pointer widths from the start. `test-display`, `test-input`,
`test-seat`, `test-pty`, `test-video` and every boot of `test-compositor`
pass on ARMv7-A, at four processors and at two. Under TCG on a loaded
example a full 1024x768 frame took 1.3 to 2 s there, about twice AArch64's
0.6 to 0.8 s in the same runs, and the slowest 5.25 s, which is why
`test-compositor` allows ARMv7-A 10 s a frame under emulation where the
64-bit machines get 5: fine for a pixel test and nowhere near interactive.
The board's own numbers wait on its display driver.

This stage's exit criterion is met in full, and its 96 points are spent.
The visible iterations the customer ordered on 2026-09-16 -- a blank screen
on Ferrix in QEMU first, then the protocol server, the seat, the IPC and the
clients -- landed in that order.

**Exit:** in a test of its own on x86-64 and AArch64, the compositor starts
from a `hyprland.conf`, `exec-once` launches two pattern clients, they tile
dwindle-style with the configured gaps and borders, a keybind sent through
QEMU's monitor moves focus and another swaps them, and each state is
required from QEMU's screendump; `hyprctl clients` and `hyprctl
activewindow` over the IPC socket report the same. A person at the serial
console can run the terminal client in it.

---

