# The GPU compositor on any Vulkan driver: hyprix-gpu

Status: protocol agreed with agent B and landed (compositor-gpu-protocol, d71d5bd0b); hyprix's side built behind `hyprix:gpu` (off), 2026-10-28. Owner of the hyprix side and the protocol:
agent A; owner of the helper and of Mesa in the image: agent B. Switched off
by default (`hyprix:gpu = off`) until it is proven on the N150.

## 1. Why

hyprix draws its frames through `compositor_render::Painter`, which has two
painters: `Canvas` (tiny-skia, the CPU) and `gpu::Canvas` (virgl TGSI
streams, `docs/GPU.md` §3). The second runs only where the render node's
driver is `virtio_gpu`. On the Intel N150 the node's driver is `i915`
(Ferrix's ring-3 Intel driver), so hyprix says

    hyprix: Virtual-1: no GPU to draw on (the render node's driver is `i915`, not virtio_gpu); drawing in software

and every frame, blur included, is drawn on the CPU. The driver that would
draw it is anv, Mesa's Intel Vulkan driver, which games already use through
the same render node. hyprix cannot load it: hyprix is a static musl
binary, and anv is a glibc shared library that wants the Vulkan loader.

The customer chose (2026-10-28) a separate process: **hyprix-gpu**, a
glibc program that opens Vulkan through `ash`, reuses tole's Vulkan
renderer (quads, SDF rounding, dmabuf textures; boots on lavapipe since
bb2934faa), and draws hyprix's frames. hyprix sends it each frame's display
list over a socket and gets back a dmabuf that `card0` shows.

What this buys beyond the N150: any machine with a Vulkan driver (anv,
radv, NVIDIA's, lavapipe as a last resort) composites on its GPU, and a
GPU or driver crash takes the helper down, not the compositor and its
clients (`ferrix-desktop-survives-driver-stalls`).

## 2. Shape

```
 clients ──wl_shm / zwp_linux_dmabuf──▶ hyprix (static musl)
                                         │  render_onto(scene, &mut Recorder)
                                         │      Recorder: impl Painter → ops
                                         │
                                         │  SOCK_SEQPACKET pair, fds by SCM_RIGHTS
                                         ▼
                                   hyprix-gpu (glibc, ash, Vulkan loader + anv)
                                         │  replays ops into its canvas image,
                                         │  copies the damage into an output dmabuf
                                         ▼
 hyprix ◀── Done{frame} ── output dmabuf (linear XRGB8888, imported into card0)
   │
   └─▶ card0: adopt (copying card: DIRTYFB of the damage) or page flip
```

* **One display list, two painters.** hyprix already draws every frame as
  `compositor_render::render_onto(scene, painter)`, a fixed sequence of
  `Painter` calls. The helper backend is a third `Painter`, the
  **Recorder**, which turns each call into an op. The software canvas
  stays the reference: the helper's frame is judged against it within a
  channel step or two, as the virgl one is (`render/src/gpu/mod.rs`).
* **One helper per hyprix, several canvases.** One process holds the
  Vulkan device; each screen is a *canvas* in it, numbered by hyprix.
* **The protocol is a crate**, `compositor/gpu-protocol`
  (`compositor-gpu-protocol`), `no_std` + `alloc`, no dependencies, so
  hyprix (musl, static) and hyprix-gpu (glibc) share the encoder and the
  decoder byte for byte. It has no socket code: each side does its own
  `sendmsg`/`recvmsg`.

## 3. Transport

* hyprix makes a `socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC)`,
  clears `CLOEXEC` on the helper's end, and spawns the helper with that end
  as **fd 3**, `HYPRIX_GPU_FD=3` and `HYPRIX_GPU_NODE=/dev/dri/renderD128`
  (the render node to pick the Vulkan device by) in its environment. No
  arguments: the helper takes none and never parses any (a Ferrix program
  run with an unknown flag on the box hung it on 2026-10-28).
* Each message is one record: a 16-byte header, then the body.
  ```
  u32 magic  "HXG1" (0x31475848 little-endian)
  u16 kind   the message
  u16 fds    how many descriptors ride on this record (SCM_RIGHTS)
  u32 len    bytes of body after the header
  u32 serial counted up by the sender from 1; replies name the request's
  ```
  All integers little-endian, all floats IEEE `f32` little-endian.
  A record is at most **`MAX_RECORD` = 60 KiB**; a display list longer
  than that is sent as several `Ops` records between `FrameBegin` and
  `FrameEnd`.
* A record that does not decode, or a descriptor count that does not match
  what arrived, is a protocol error: the receiver closes the socket. For
  hyprix that is a helper failure (§8).
* hyprix's end is non-blocking and is in its poll set; the helper's may
  block.

## 4. Messages

`→` is hyprix to helper, `←` helper to hyprix. Versions: `VERSION = 1`;
a helper that is sent a version it does not speak answers `Error` and
exits.

### 4.1 Start

| kind | dir | body | fds | meaning |
|---|---|---|---|---|
| `Hello` | → | `version u32` | 0 | first record hyprix sends |
| `Ready` | ← | `version u32, caps u32, device: str, driver: str` | 0 | the device is up. `caps`: bit 0 `SYNC_FILE` (it can hand out fences, §7), bit 1 `HOST_MEMORY` (the arena is imported, not copied) |
| `Modifiers` | ← | `fourcc u32, n u32, n × (modifier u64, external_only u32)` | 0 | the modifiers it can *sample* a client's dmabuf of this fourcc with; one record per fourcc (`AR24`, `XR24`), sent after `Ready`. hyprix offers clients these (`zwp_linux_dmabuf_v1`, as `Drawn::Server` does for the test server) |

A string is `u32 len` and that many UTF-8 bytes.

### 4.2 Canvases and outputs

| kind | dir | body | fds | meaning |
|---|---|---|---|---|
| `Configure` | → | `canvas u32, width u32, height u32, outputs u32` | 0 | make (or make again at a new size) canvas `canvas`: its frame image, its backdrop, and `outputs` output buffers (1 to 3, §6). Everything of the canvas's old size is dropped |
| `Outputs` | ← | `canvas u32, width u32, height u32, fourcc u32, modifier u64, n u32, n × (stride u32, offset u32)` | n | the output dmabufs, in order: what hyprix imports into `card0`. Linear `XR24` unless hyprix is told otherwise later |
| `Drop` | → | `canvas u32` | 0 | the screen is gone |

### 4.3 Client buffers

A **texture** is named by hyprix with a `u64` that is never reused while
the helper runs: the surface's `Surface::name` for shared memory, the
dmabuf's `OnDevice::key` for a dmabuf (both are unique for the buffer's
life in hyprix today).

| kind | dir | body | fds | meaning |
|---|---|---|---|---|
| `Arena` | → | `arena u32, size u64` | 1 | a memfd hyprix writes pixels into for the helper to read (§5.1). The helper maps it read-only, or imports it as host memory. A frame whose uploads outgrow the arena gets a new one with the next number; older arenas are not used again once the frame in flight is done, and an `Arena` with a number the helper has replaces it |
| `ImportDmabuf` | → | `texture u64, width u32, height u32, fourcc u32, modifier u64, planes u32, planes × (offset u32, stride u32)` | planes | sample this client dmabuf as `texture` from now on. One plane for `AR24`/`XR24`; the field is there for more |
| `CreateTexture` | → | `texture u64, width u32, height u32, fourcc u32` | 0 | an image of the helper's own that `Upload` ops fill: a `wl_shm` buffer's copy |
| `Forget` | → | `texture u64` | 0 | hyprix is done with the texture. The helper frees it once no frame in flight uses it |

### 4.4 A frame

| kind | dir | body | fds | meaning |
|---|---|---|---|---|
| `FrameBegin` | → | `canvas u32, frame u64, output u32` | 0 | a frame of canvas `canvas`; when drawn, its damage goes into output `output` |
| `Ops` | → | ops (§4.5), back to back | 0 | part of the display list |
| `FrameEnd` | → | `frame u64, n u32, n × rect` | 0 | the frame's *present* damage: what changed on the screen. The helper submits |
| `Done` | ← | `canvas u32, frame u64, output u32, flags u32` | 0 or 1 | the frame is in the output buffer — **the GPU has finished**, unless `flags` bit 0 says a sync_file rides along that signals when it has (§7). Every client buffer the frame read may go back to its client once the frame is done |
| `Failed` | ← | `canvas u32, frame u64, code u32, why: str` | 0 | the frame was not drawn. `code`: 1 a bad op, 2 out of memory, 3 device lost. Device lost is followed by the helper exiting |
| `Release` | → | `canvas u32, output u32` | 0 | the card is done reading that output (its flip or copy landed). All outputs start released; the helper never writes an output it has not got back |
| `Read` | → | `canvas u32, rect, arena u32, offset u64, stride u32` | 0 | copy `rect` of the canvas's frame image into the arena: a screenshot, a night-light's ramps, a turned monitor |
| `ReadDone` | ← | `canvas u32, serial-of-read u32` | 0 | the pixels are there |

A `rect` is `x i32, y i32, width u32, height u32`, in the canvas's pixels.

### 4.5 Ops

Each op is `u8 tag`, three bytes of zero, then its fields, so that every op
starts on four bytes. They are the `Painter` trait's calls, one for one, in
the order hyprix made them, plus the two that move pixels. A shape is cut
exactly as `compositor_render::Canvas` cuts it (the superellipse at pixel
centres, `Rounding::power`); colours are `compositor_render::Color`'s
`0xAARRGGBB`, not premultiplied; blending is source-over on premultiplied
values, into an opaque frame.

| tag | op | fields | `Painter` call |
|---|---|---|---|
| 1 | `Damage` | `n u32, n × rect` | the damage every following op is cut to, until the next `Damage`. Disjoint rectangles: a translucent op must not blend twice over a pixel |
| 2 | `Clear` | `color u32` | `clear` |
| 3 | `Fill` | `rect, color` | `fill` |
| 4 | `FillRounded` | `rect, rounding, color` | `fill_rounded` |
| 5 | `FillRoundedGradient` | `rect, rounding, gradient` | `fill_rounded_gradient` |
| 6 | `BorderGradient` | `rect, width i32, gradient` | `border_gradient` |
| 7 | `Shadow` | `rect, rounding, range i32, power u32, color u32, offset_x i32, offset_y i32` | `shadow` |
| 8 | `Blur` | `rect, rounding, blur` | `blur` (of the frame as it stands) |
| 9 | `Composite` | `texture u64, src rect, rect` | `composite`: 1:1 |
| 10 | `CompositeScaled` | `texture u64, src rect, rect, rounding, opacity f32, nearest u32` | `composite_scaled` |
| 11 | `KeepBackdrop` | — | `keep_backdrop`, inside the current damage |
| 12 | `BlurBackdrop` | `rect, rounding, blur` | `blur_backdrop` |
| 13 | `Upload` | `texture u64, rect, arena u32, offset u64, stride u32` | none: copy that part of a `CreateTexture` image from the arena, before any later op samples it |

`rounding` is `radius i32, power f32`; `gradient` is `degrees i32, n u32,
n × color` (n ≤ `compositor_config::MAX_GRADIENT_COLORS`); `blur` is
`size i32, passes u32, noise f32, contrast f32, brightness f32, vibrancy
f32, vibrancy_darkness f32`. `src rect` is the part of the texture drawn:
all of it, or a crop (`OnDevice::x/y`, a subsurface's viewport).

`Format::Xrgb8888` (texture fourcc `XR24`) is opaque: its X byte is read
as 255. `AR24` is premultiplied, as `wl_shm` says.

## 5. Buffers and who owns them

### 5.1 Shared memory (`wl_shm`)

hyprix copies the part of a client's `wl_shm` buffer under the frame's
damage into the **arena**, a memfd it made and sent once (`Arena`), and an
`Upload` op tells the helper where it is. This is the same "move what is
under the damage" rule `gpu::Canvas` follows. The helper never maps a
client's pool: a client that truncates its pool would SIGBUS the helper,
and hyprix already reads pools safely.

The client's buffer goes back to it as it does today, since its pixels
were copied before the frame was sent. On a machine where the GPU reads
system memory (the N150: LLC, one memory), the helper imports the arena
with `VK_EXT_external_memory_host` and samples from it with one copy fewer
(`caps` bit 1).

The arena is reused frame after frame: in version 1 a frame is finished
(`Done`) before the next is sent (§6), so nothing in it is still read. It
grows by a new `Arena` when a frame needs more (first size: four screens'
worth of the largest canvas).

### 5.2 Dmabufs (`zwp_linux_dmabuf_v1`)

A client's dmabuf is imported once (`ImportDmabuf`, a dup of the fd) and
sampled where it lies. **It goes back to its client only once every frame
that read it is done** — the lesson of yserver's bug of 2026-10-28, where a
buffer went back while the GPU still read it. hyprix keeps, per frame in
flight, the buffers it sampled, and sends their `wl_buffer.release` at that
frame's `Done` (and, where `Done` carries a sync_file, when it signals).
A buffer drawn again by the next frame stays held. The direct-scanout path
holds its buffer the same way already.

`Forget` follows the client destroying the buffer, or the buffer going
unsampled for 120 frames; the helper drops its import once no frame in
flight uses it. A texture forgotten and seen again is imported again.

### 5.3 Output buffers

The helper allocates them (`VK_EXT_image_drm_format_modifier`, linear,
exportable with `VK_EXT_external_memory_dma_buf`) because it is the one
that knows the device; hyprix imports them into `card0`
(`PRIME_FD_TO_HANDLE`, `ADDFB2`), exactly as it imports a client's buffer
for direct scan-out today.

The helper keeps each canvas's frame in an image of its own, the way
`gpu::Canvas` and the software `Canvas` keep theirs: ops draw into it, and
at `FrameEnd` the helper copies the present damage — plus whatever damage
that output missed while others were written — into the output named in
`FrameBegin`. So hyprix never reasons about buffer ages, and the backdrop
and blur read the whole frame as they do on the other painters.

Where the device exports no dmabufs (lavapipe on Ferrix), the outputs are
memfds and `Outputs` says `MODIFIER_MEMFD`: hyprix does not import them
into the card but copies the frame out (version 1 through `Read`).

**The card's copy and the GPU's writes (2026-10-28).** Mesa writes an
exported or imported object with uncached MOCS, past the LLC, and the
N150's `card0` copies a frame with the processor. Without help the copy
read lines it had cached from an earlier frame: trails and stale
wallpaper. The Intel driver now evicts the source rows of a copy while
any request writes an exported object (`intel: card0 evicts a
GPU-written buffer's lines before copying them`), at no cost to a desktop
drawn in software. The frame must also be *finished* before the card
copies it, which `Done` after the helper's fence guarantees; a card that
waits on the buffer's implicit fence itself is backlog.

## 6. Frame flow and pacing

hyprix's loop is unchanged: it draws a screen when there is damage and
the backend has no frame in flight. With the helper:

1. `render_onto(scene, recorder)`: ops are appended to a buffer; shm
   pixels under the damage are copied into the arena; new dmabufs and
   textures are sent ahead of the frame.
2. `FrameBegin`, the `Ops` records, `FrameEnd` go out.
3. hyprix waits for `Done` (version 1, below), then hands the output to
   the card:
   * **copying card** (the N150's `card0`, efifb, nvrm, the virtio card
     without 3D): one output, adopted once (`Backend::adopt`), and each
     frame a `present` with the damage, which the card copies from it. The
     card's copy and the next frame's copy into the output never overlap,
     because hyprix draws no frame while one is in flight;
   * **scanning card** (a later Intel display driver, KMS on real
     hardware): two or three outputs, page-flipped in turn
     (`Backend::scan_out`), the next frame drawn into one the card does not
     show.

**Version 1 waits.** `Painter`'s calls answer nothing and `gpu::Canvas`'s
`finish` is already synchronous, so the Recorder's `finish` sends
`FrameEnd` and waits on the socket for `Done`, at most **250 ms** (hyprix's
`HOLD`). A 1080p frame on the N150 should take a few milliseconds of
GPU. A wait that runs out is a failed frame (§8). **Version 2** puts the
socket in the event loop and makes "in flight on the GPU" a state of the
screen like "in flight on the card", so hyprix serves clients while the GPU
draws; the protocol is already shaped for it (frames are numbered, `Done`
is asynchronous).

## 7. Fences

`Done` is sent once the helper's own `vkWaitForFences` for the frame has
returned: the socket *is* the fence. That needs nothing of the kernel and
is what version 1 uses.

Where the driver can export one (`caps` bit 0: `vkGetSemaphoreFdKHR` with
`SYNC_FD`, on Ferrix through `SYNCOBJ_HANDLE_TO_FD` and the K5 sync files,
`docs/INTEL-GPU.md`), the helper may send `Done` at *submit* with a
sync_file that signals at completion, so hyprix can hand it to the card
(`IN_FENCE_FD`) or poll it, and the helper's own wait goes. hyprix then
releases client dmabufs when the sync_file is readable, never before.

A client's dmabuf carries its own implicit fence (`EXPORT_SYNC_FILE`); the
helper imports it into the frame's wait semaphore before sampling, as
Mesa's WSI does, so a game's half-drawn frame is never composited.

## 8. Failure and restart

The desktop must outlive the GPU, the driver and the helper
(`docs/RELIABILITY.md`, 2026-10-03). Each failure is answered at once by
the software painter, and the helper is started again later:

| what | how hyprix sees it | answer |
|---|---|---|
| a frame not done in 250 ms | the wait runs out | the frame is dropped and drawn again whole; `GPU_TRIES` = 3 in a row give the helper up |
| `Failed` with code 1 or 2 | a record | the frame is dropped; counts as above |
| `Failed` code 3 (device lost), the socket's end (EOF, `EPIPE`, `ECONNRESET`), a record that does not decode | at once | the helper is given up now, not after three |
| the helper did not say `Ready` within 5 s of starting | start-up | no helper; software |

Giving the helper up is today's `Screen::give_up_gpu` for every screen it
drew: draw in software, `Backend::forsake` so the card shows hyprix's own
buffer again, a whole frame next. The helper is sent `SIGKILL` by its pid
(it is hyprix's child; never by name) and reaped; held client dmabufs are
released, since no process can still be reading them through the helper
(a hung engine is reset by the driver). It is started again after the
`GPU_AGAIN` back-off, 5 s doubling to 60 s, as a lost virgl device is; a
helper that comes back gets every texture imported again and every canvas
configured again, and draws its first frame whole.

A crash in anv or a GPU hang takes only hyprix-gpu: hyprix and its clients
carry on, the screen in software for a few seconds.

## 9. Switching it on

* `hyprix:gpu = off | auto | on` in `hyprland.conf` (hyprix's own option,
  beside `hyprix:blur_in_software`). **Default `off`** until the N150 has
  run it for a while; read at start and when the configuration is
  reloaded.
  * `off`: never start the helper. Today's behaviour, byte for byte.
  * `auto`: on a screen with no virgl render node (the virgl painter keeps
    priority where it works), start the helper if its binary is there and
    a render node exists; any failure is software.
  * `on`: the same, and also say loudly (once) why it did not start.
* `--renderer helper` (beside `auto|software|gpu|vtest`): for tests; a
  helper that does not start ends hyprix, so a gate cannot pass in
  software.
* The helper is `/usr/libexec/hyprix-gpu`; `HYPRIX_GPU_HELPER` names
  another (the tests' fake helper, a development build).
* The log says which painter draws each screen:
  `hyprix: eDP-1: frames are drawn by hyprix-gpu on Intel(R) Graphics (ADL-N) (anv)`.

## 10. Where Mesa lives

The desktop must not depend on the Steam volume (`/data/usr`), which a
person may never mount. The helper needs, in the image (agent B):

* the Vulkan loader, `libvulkan.so.1` (Apache-2.0);
* anv, `libvulkan_intel.so`, and its ICD JSON (MIT); and anv's own
  dependencies: libdrm (MIT), zlib (zlib), zstd (BSD), expat (MIT),
  libelf if the build wants it (LGPL-3.0+/GPL-2.0+, dynamic linking only);
* glibc and libgcc_s (LGPL-2.1+ / GPL-3.0 with the runtime exception;
  shared, unmodified);
* lavapipe as a further fallback is *not* proposed: it would draw on the
  CPU what hyprix's own software painter already draws.

Each goes into `docs/LICENSES.md` with its source package and version,
taken from the same pinned Debian/Ubuntu packages the Steam volume's Mesa
comes from, under one prefix (`/usr/lib/hyprix-gpu/`) that the helper's
`RPATH` and `VK_ICD_FILENAMES` point at, so nothing else in the image sees
them.

## 11. Steps and tests

1. **Protocol crate + this document** (agent A). Unit tests: every message
   and op round-trips; truncated and oversized records are refused; a
   display list longer than one record is split and joined.
2. **hyprix backend** (agent A), behind `hyprix:gpu = off`: the Recorder,
   the helper's spawn and supervision, imports and releases, the output
   into `card0`. Tests: the Recorder against a fake helper — a thread at
   the other end of the socket pair that records the ops and answers
   `Done` with a canned buffer — checks the ops for a known scene; killing
   the fake mid-frame must land the next frame in software; a fake that
   never answers must drop three frames and give up. (Built:
   `hyprix/src/helper/tests.rs` -- a frame's ops and uploads, a helper that
   dies, one that never answers, a lost device, an empty frame.) `test-iso` and
   `test-iso-install` must be unchanged with the option off.
3. **hyprix-gpu** (agent B): ash on anv, tole's renderer, the ops,
   outputs. Tests on the host against lavapipe.
4. **Equivalence**: the same scene through the software painter and
   through hyprix-gpu on lavapipe, within the virgl painter's tolerance
   (`render/src/gpu/tests.rs`).
5. **On the N150** with the option on: frame times against software, a
   helper killed by hand, a GPU hang (`intel` driver's engine reset).

## 12. Where it stands (2026-10-29)

**hyprix's side** (agent A): `helper::Canvas`, a third `Painter`, behind
`hyprix:gpu = off | auto | on` (off). One helper for every screen,
started on demand with the configuration's `env =` lines; a helper that
times out, fails, loses its device or dies fails the frame as a GPU does
and is given up for software after three, then started again 5-60 s
later. Tests against a fake helper in `hyprix/src/helper/tests.rs`.

**hyprix-gpu** (agent B, landed d71d5bd0b and c91bc141d). The helper draws
`compositor_render::gpu::Renderer`'s shapes -- the virgl painter's quads,
constants and GLSL, compiled to SPIR-V -- through a Vulkan backend on the
device tole picks (`tole::gpu::Core`). Its outputs are linear exported
dmabufs (`XR24`, modifier 0), or memfds (`MODIFIER_MEMFD`) where the
device exports no dmabufs, which is lavapipe on Ferrix. Client dmabufs
are imported in place and their implicit fences are waited for, at most
100 ms, before a frame samples them. Tests: on lavapipe on the host
(check's hyprix-gpu unit) every painter call matches the software painter
(0 pixels more than 3 apart) and frames go through the socket into
dmabuf and memfd outputs; `test-compositor --boot gpu-helper` runs those
on Ferrix; `--boot helper-desktop` is hyprix end to end on lavapipe. On
anv on the N150 (by hand, 2026-10-28) a whole 1080p desktop frame
(wallpaper upload, backdrop, shadow, three-pass blur, a translucent
window) took 9.1 ms median, against 17.7 ms on lavapipe on nazuna.
Packaging is opt-in: `FERRIX_GPU_HELPER=1` at build carries
`/usr/libexec/hyprix-gpu` with anv and 22 libraries from the pinned
intel-uapi Mesa 25.0.7 under `/usr/lib/hyprix-gpu` (29 MiB,
`docs/LICENSES.md`). The default image and the ISO do not carry it.

**The card** (eb555dc28): `card0` evicts a GPU-written buffer's lines
before copying them (§5.3).

**Next:** the N150 desktop with `hyprix:gpu = on`; frames in flight
(version 2, §6) and sync_file fences (caps bit 0); a state-level test of
the give-up and restart; an Endfield window measured; then the default.
