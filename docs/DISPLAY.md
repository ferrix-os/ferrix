# The display: `/dev/dri/card0` over a ring-3 virtio-gpu driver

Version 1. Written by the GUI session (os-e5) for iteration 1 of the
compositor's path, the customer's order of 2026-09-16: *a blank screen on
Ferrix in QEMU*, pulled forward from stage 17. Approved by the product owner
(os-f6) on 2026-09-16, with the decisions in §5. Implemented: L5 and L6 landed
together as one reviewed stack (os-02), since L5 alone would have been dead
code or the kernel drawing; `cargo xtask test-display` passes on x86-64 and
AArch64. That is iteration 1. Iteration 2's planes and properties (E4, §2.3)
are implemented too, reviewed by os-02.

## 1. What this is, and what it is not

Stage 17 gives a Linux compositor what it needs to draw: `/dev/dri/card0` with
the DRM/KMS subset a software-rendered compositor uses. `docs/ARCHITECTURE.md`
§7 puts the driver in ring 3, and §1 says the kernel draws nothing but a panic.
This document specifies iteration 1's cut of that:

* **the kernel's display core**, which owns buffers and the card node and
  speaks DRM to the compositor;
* **the control protocol** between the core and the ring-3 virtio-gpu driver;
* **the DRM subset** iteration 1 answers;
* **what a panic does** once the driver owns scanout.

It is not the virtio-gpu protocol, which the driver speaks to the device
(`src/lib/drivers/virtio::gpu`, to be written). It is not the GPU: no 3D, no render node,
no dmabuf, no PRIME (stage 19; how the GPU comes, and in what order, is
decided in `docs/GPU.md`). It is not input (virtio-input is its own
iteration).

**Exit of iteration 1:** on x86-64 and AArch64, the compositor's first binary
opens `/dev/dri/card0`, finds the connector and its preferred mode, creates a
dumb buffer, maps it, fills it with one colour, adds a framebuffer and sets the
CRTC. `cargo xtask test-display` reads QEMU's screendump of the virtio-gpu head
and requires every pixel to be that colour. `cargo xtask run --display` shows
the same on a person's screen.

## 2. The four questions

### 2.1 Who owns the scanout memory: the core, in one VMO per card

**The display core owns the pixels.** Each card has one kernel VMO, the *card
VMO*: anonymous, sparse and committed on demand. It is sized to the card's
buffer budget (iteration 1: 256 MiB), which is also the most memory an opener
can make the card commit. A dumb buffer is a page-aligned range of it, the
first free one that fits. `MODE_MAP_DUMB` returns the range's offset. A buffer
is at most `MAX_BUFFER_PAGES` (8192 pages, 32 MiB: a 4K buffer), which the
driver sizes its backing lists for.

* **The compositor maps it with no new mapping code.** `card0`'s devfs inode
  answers `Inode::mapping()` with the card VMO. `mmap(fd, len, PROT_READ |
  PROT_WRITE, MAP_SHARED, offset)` then goes through the file-backed
  `MAP_SHARED` path that already landed (`src/kernel/src/syscall/memory.rs`,
  `map_file`). DRM's "fake offsets" become real offsets into one object.
  mmap asks the *inode*, not the per-open object, so the buffers live in
  the node's card state, not in the opener's `File`.
* **The driver pins it read-only.** The core sends the driver a duplicate of
  the card VMO handle once, with `READ | TRANSFER` rights only, never `WRITE`
  or `MAP`. For each buffer the driver calls `VMO_PIN(device, card_vmo,
  offset, length, PIN_READ_ONLY)` and `VMO_PIN_ADDRESSES`. It then gives the
  device those pages as the resource's backing (`RESOURCE_ATTACH_BACKING`,
  one entry per run of consecutive device addresses). The device only reads
  guest backing, so the driver can neither write nor see the pixels.
  `vmo_pin` already allows exactly this: `READ` on the VMO, `MANAGE` on the
  device.
* **One driver kind copies (displayctl v8).** A display engine that reads
  only its own memory -- NVIDIA's, whose scan-out is in the GPU's VRAM --
  cannot be given the card's pages. Its driver, `nvrm` (`docs/NVIDIA.md`
  §4.5), copies each flushed rectangle into VRAM with the processor, so it
  must see the pixels. Its HELLO sets the `copies` flag, and the core then
  hands it the card VMO with `READ | MAP | TRANSFER`, still never `WRITE`;
  it maps the card read-only. A driver without the flag gets exactly the
  rights above. The boot log says which: `display  card0 copies frames`.
* **No copy in the guest.** The compositor writes pages that are the device's
  backing. virtio-gpu 2D does copy on the host side (`TRANSFER_TO_HOST_2D`),
  which is QEMU's business.
* **Freeing.** `MODE_DESTROY_DUMB` sends `DETACH` without waiting, or, while
  a framebuffer still refers to the buffer, only takes the handle away and
  detaches with the last `RMFB`, as Linux keeps the object. Closing the card
  turns the scanout off and detaches every buffer the open made. A range
  comes back when `DETACHED` reports success, or when `ATTACHED` reports a
  failure: the card's task decommits it, so the next buffer there starts
  zeroed and shows nothing of the last one, then gives it back. A refused
  `DETACHED` keeps the range, and the id, out of use for good.
* **Buffer ids are the card's, not the open's.** The core numbers buffers on
  the card and never reuses an id, so no reply is taken for a later buffer's
  request and the driver never sees an id again that the device may still
  hold pages under. The DRM handle is the open's own name for one.
* **Nothing is left behind by a timeout.** A request waits 5 seconds for its
  reply. An attach that times out, and a buffer let go of while a flush of
  it is still in flight, become the card's orphans: the card's task detaches
  each as soon as the session allows. A reply nobody waits for any more is
  dropped.
* **A driver that is behind is not a dead one.** The kernel is the control
  channel's only writer and asks for room before the session commits to a
  request: a full channel answers `EBUSY` to the program, and a closed
  open's scanout-off and detaches wait for room instead of taking the card
  down.
* **A range a driver still pins is never reused.** After the decommit the
  core asks the card VMO whether any page of the range is still held for
  the device; one that is means the driver replied before it unpinned, and
  the range stays out of use for good, with a line on the console.
* **A driver refused before READY does not stop the boot.** devmgr waits for
  PUBLISHED or for the driver's exit, whichever comes first, and counts a
  driver that exits unpublished as failed.

**The size `card0` reports.** `card0`'s inode reports `st_size` equal to the
card VMO's size, `CARD_BUDGET`. `map_file`'s bounds check, which is what makes
touching a `MAP_SHARED` mapping past the end raise `SIGBUS`, then holds with no
devfs special case (os-f6's decision).

The budget is one named constant with its arithmetic beside it: a 4K mode is
3840 × 2160 × 4 bytes, about 33 MiB a buffer, so double-buffered 4K is 66 MiB.
256 MiB leaves room for a cursor and a third buffer and still bounds what one
opener can commit.

### 2.2 The driver protocol: a control channel, `src/lib/proto/displayctl`

Frames do not move, so there is no data ring. There is one control `Channel`
per card, with the message shape `src/lib/proto/blkring/src/control.rs` uses: a
fixed-size little-endian message, a type, a version, validation in the order
fields are read, and handles alongside. It goes in a new host-tested crate,
`src/lib/proto/displayctl`, with a fuzz target, as `src/lib/proto/blkring` did.

**Bring-up follows blk.** devmgr's table gets `(0x1AF4, [0x1050], b"gpu",
Kind::Display)`. `DEVMGR.md` already names 0x1050. `start_display` asks the
kernel for the control channel with a new native call,
`DISPLAY_CONTROL_CREATE` (0x104C, beside `BLOCK_RING_CREATE`). It then sends
`START` (blk's layout: the four capability `Block`s, MSI-X size, device id,
location, name) to `/lib/drivers/gpu` and waits for `PUBLISHED`, like the
others.

| Type | Direction | Body | Handles |
|---|---|---|---|
| `HELLO` | driver → core | version; scanout count (1 in iteration 1); for each scanout the preferred mode `width, height` from `GET_DISPLAY_INFO` and whether it is enabled; what the card said about 3D; whether it has a cursor plane; each scanout's refresh in millihertz, 0 when unknown (protocol version 7, 2026-10-02) | driver port (`WRITE \| TRANSFER`) |
| `READY` | core → driver | card id | card VMO (`READ \| TRANSFER`), core port (`WRITE`) |
| `REFUSED` | core → driver | reason | — |
| `ATTACH` | core → driver | buffer id, offset, length, width, height, stride, format (`XRGB8888` only) | — |
| `ATTACHED` | driver → core | buffer id, status | — |
| `SCANOUT` | core → driver | scanout, buffer id (0 turns the scanout off), source rectangle | — |
| `FLUSH` | core → driver | buffer id, damage rectangle, sequence | — |
| `FLIPPED` | driver → core | sequence, status | — |
| `DETACH` | core → driver | buffer id | — |
| `DETACHED` | driver → core | buffer id | — |
| `STOP` / `STOPPED` | as blk | | |
| `CURSOR` | core → driver | scanout, buffer id (64 × 64; 0 for none), sequence, hotspot, place; only to a card whose `HELLO` said it has a cursor plane | — |
| `MOVE` | core → driver | scanout, place; nothing answers it; the same | — |
| `MODES` | driver → core | each scanout's preferred mode and refresh, as `HELLO` gives them, in `HELLO`'s place; sent unasked when the device's displays change, and nothing answers it (protocol version 6, 2026-09-26; the refresh since 7) | — |

**Each message maps to device commands:**

* `ATTACH`: pin, then `RESOURCE_CREATE_2D` (format `B8G8R8X8_UNORM`, which
  is little-endian `XRGB8888`), then `RESOURCE_ATTACH_BACKING`. A refused
  create is unpinned; refused backing is unreferenced, then unpinned; either
  way `ATTACHED` carries the failure.
* `SCANOUT`: `SET_SCANOUT`.
* `FLUSH`: `TRANSFER_TO_HOST_2D` over the damage, from the offset of the
  rectangle's first pixel in the backing, then `RESOURCE_FLUSH`. `FLIPPED`
  is sent when the flush's response arrives; a refused transfer skips the
  flush and `FLIPPED` says so.
* `DETACH`: `RESOURCE_DETACH_BACKING`, `RESOURCE_UNREF`, then the pin's
  handle is closed.
* `CURSOR` (protocol version 4, 2026-09-23): `TRANSFER_TO_HOST_2D` of the
  whole 64 × 64 image, then `UPDATE_CURSOR` on the cursor queue. It takes a
  sequence from the flushes' count and `FLIPPED` answers it in their line,
  so the image's buffer cannot be detached while its pixels are on their
  way. Its place is given to the driver the moment the message is read.
* `MOVE`: `MOVE_CURSOR` on the cursor queue, posted when the message is
  read, never behind the control queue. A move carries the cursor's
  resource, since QEMU's `update_cursor` hides a cursor whose move names
  none.
* `MODES` comes the other way, from the device: `VIRTIO_GPU_EVENT_DISPLAY`
  in the configuration's `events_read`, which QEMU raises when the window
  or VNC viewer a scanout is shown in is resized. The driver asks for
  configuration changes on the control queue's MSI-X vector, reads
  `events_read` after every interrupt, and once nothing is in flight runs
  `GET_DISPLAY_INFO` and sends what it says. The core takes a list that fits
  the card `HELLO` described and refuses the rest, as it refuses a bad
  `HELLO`.
* **The refresh** (protocol version 7, 2026-10-02) comes from `GET_EDID`.
  The driver takes `VIRTIO_GPU_F_EDID` when it is offered and, beside each
  `GET_DISPLAY_INFO` for `HELLO` and `MODES`, asks every enabled scanout's
  EDID and reads the refresh its preferred timing makes: the base block's
  first detailed timing, or a DisplayID extension's first type I timing
  where QEMU put it there for a clock a descriptor cannot hold
  (`ferrix_displayctl::edid::preferred_refresh_mhz`). QEMU makes that timing
  at the refresh of the host monitor its GTK window is on, and at 75 Hz
  where no window says, as under VNC. A refusal, or an EDID that is short,
  corrupt or interlaced, is a refresh of 0, never a reason to stop.
  `xtask` puts `edid=on` on the device rather than rely on QEMU's default.

**Pages the device may still hold are never unpinned.** If the device
refuses `RESOURCE_DETACH_BACKING`, the driver does not unreference the
resource and does not close the pin: the range stays pinned for good, and
`DETACHED` reports `DeviceRefused`, which tells the core never to hand that
range out again. Unpinning it would let the device write into memory that
belongs to someone else (os-f6's decision, 2026-09-16; `src/lib/drivers/display/virtio-gpu`'s
`pipeline` implements it and its fuzz target checks it).

The driver runs commands one at a time on the control queue: a frame is two
commands, and at 60 frames a second a queue per frame is not worth its
complexity. The cursor queue is run the other way round (`src/lib/drivers/display/virtio-gpu`,
"The cursor queue is the other way round"): sixteen slots on a page of their
own, everything owed posted behind one doorbell, and no interrupt at all --
the queue is given no MSI-X vector and asks for none, and a slot comes back
when the next command is posted or the control queue interrupts anyway. A
pointer moving a hundred times a second is a hundred posts and no wakeup.

**Doorbells.** A message on the channel is the doorbell both ways, and each
side waits on its port for `PACKET_SIGNAL` on the channel. This is blk's STOP
path, which already works. The ports in `HELLO` and `READY` are for later
(display-change events) and cost nothing now; the cursor queue turned out to
need none, since nobody waits for it.

**What the core never trusts:** a buffer id it did not send, a sequence out
of order, a `HELLO` with more scanouts than it can publish, or a mode above
8192×8192. Any of these is `REFUSED`, then quiesce, the same as a blk driver
that lies.

**`PUBLISHED`** is sent when the core has registered `card0`, which happens
after `HELLO` is accepted, as blk registers its disk.

### 2.3 The DRM subset

`card0` answers these ioctls. Numbers and layouts go into `src/lib/proto/linux-abi`
(`drm` module), from a probe compiled on example against `/usr/include/drm`
(`linux-libc-dev`), at both pointer widths, and pinned by tests. The probe
source is committed this time.

| ioctl | Iteration 1 |
|---|---|
| `DRM_IOCTL_VERSION` | name `virtio_gpu`, so drm-rs and Smithay identify the card |
| `DRM_IOCTL_GET_CAP` | `DRM_CAP_DUMB_BUFFER` = 1, `DUMB_PREFERRED_DEPTH` = 24, `DUMB_PREFER_SHADOW` = 0, `TIMESTAMP_MONOTONIC` = 1, `CRTC_IN_VBLANK_EVENT` = 1, and on a card with a cursor plane `CURSOR_WIDTH` and `CURSOR_HEIGHT` = 64; others `EINVAL` |
| `DRM_IOCTL_SET_CLIENT_CAP` | `UNIVERSAL_PLANES` takes 0 or 1 (E4) and changes only what `GETPLANERESOURCES` lists; `ATOMIC` refused with `EOPNOTSUPP`, so clients fall back to legacy; the rest `EINVAL` |
| `DRM_IOCTL_SET_MASTER`, `DROP_MASTER` | succeed; the exclusive open (below) is the master |
| `MODE_GETRESOURCES` | a CRTC, an encoder and a connector per scanout the driver reported, and the framebuffer ids |
| `MODE_GETCONNECTOR` | `Virtual-<n>`, connected when the scanout is enabled, the preferred mode from `HELLO` at the refresh the driver reported (60 Hz when it reported none), the same size at 60, 120 and 144 Hz where not listed already, then the standard sizes at 60 Hz; each mode's `clock` is what its refresh takes over its totals and its `vrefresh` what that clock gives |
| `MODE_GETENCODER`, `MODE_GETCRTC` | the head's own, each encoder driving the one CRTC of its head |
| `MODE_CREATE_DUMB`, `MODE_MAP_DUMB`, `MODE_DESTROY_DUMB` | §2.1; `bpp` 32 only |
| `MODE_ADDFB`, `MODE_ADDFB2`, `MODE_RMFB` | `XRGB8888` only; a framebuffer names one dumb buffer |
| `MODE_SETCRTC` | `SCANOUT` then `FLUSH` of the whole buffer |
| `MODE_PAGE_FLIP` | `SCANOUT` if the buffer changed, then `FLUSH`; `DRM_MODE_PAGE_FLIP_EVENT` queues a `drm_event_vblank` when `FLIPPED` arrives |
| `MODE_DIRTYFB` | `FLUSH` of the clip rectangles |
| `MODE_CURSOR`, `MODE_CURSOR2` | the head's cursor plane, as Linux's `drm_mode_cursor_universal` sets it: `MOVE` is a `MOVE` to the image's top-left corner and waits for nothing; `BO` is a `CURSOR` of a 64 × 64 dumb buffer, or none for handle 0, with `CURSOR2`'s hotspot (`CURSOR` has none), and returns at `FLIPPED`; both flags set the place first. Another size `EINVAL`, a handle this open has not got `ENOENT`, a card with no cursor plane `ENXIO`, as Linux answers for a CRTC with none; the image is shown until the next one, and a card may read it from the buffer itself (the DK1's LTDC does), so a program draws each image into a buffer the card is not showing |
| `read()` | `drm_event_vblank` records, as many as fit; 0 when one is queued that does not fit, which stays; blocks while none are queued, `EAGAIN` under `O_NONBLOCK`, whatever the count. Since 2026-09-26 a connector change comes first, as Ferrix's own eight-byte event `0x8000_0000` (`EVENT_FERRIX_CONNECTORS`), once however many `MODES` there were since the last: Linux leaves types from `0x8000_0000` to drivers, and libdrm steps over one it does not know. Linux says a connector changed with a udev uevent, which Ferrix does not send |
| `MODE_GETPLANERESOURCES` | E4: one primary plane a head to an open that set `UNIVERSAL_PLANES`; no plane to one that did not |
| `MODE_GETPLANE` | E4: format `XRGB8888`, `possible_crtcs` the bit of its head's CRTC, and the CRTC and framebuffer `SETCRTC` or `PAGE_FLIP` last showed on that head, 0 and 0 while nothing is; the formats are copied only into an array with room for all of them; another id `ENOENT` |
| `MODE_OBJ_GETPROPERTIES` | E4: the plane has `type` (property 5) at `Primary` (1); the CRTC and the connector have none; the encoder, a framebuffer and the property have no property list, `EINVAL`; an id of another type than the one asked for, or no object, `ENOENT` |
| `MODE_GETPROPERTY` | E4: property 5, `type`, `DRM_MODE_PROP_ENUM \| DRM_MODE_PROP_IMMUTABLE`, values 0, 1 and 2 named `Overlay`, `Primary` and `Cursor`; another id `ENOENT` |

**Iteration 2's planes and properties (E4, 3 points, the GUI session
os-e5; decided by os-f6, 2026-09-16; implemented and reviewed by os-02 the
same day).** This table first assumed Smithay's legacy path needs no planes. It
does: `create_surface` enumerates planes even there, reads each plane's
properties to find `type` and reaches `unreachable!()` for a plane without
one, and keeps only primary planes when universal planes are refused; with
none it fails with `NoPlane`. It also reads the connector's properties to look
for `DPMS`, where an empty list is fine (`docs/INPUT.md` §2.3 has the source
lines). So `card0` has one primary plane with a `type` property, and answers
the four ioctls above. `GETPROPBLOB` and `OBJ_SETPROPERTY` stay out while the
plane has no `IN_FORMATS` or `SIZE_HINTS` and the connector no `DPMS`. The
numbers and `struct drm_mode_get_plane_res`, `drm_mode_get_plane`,
`drm_mode_obj_get_properties`, `drm_mode_get_property` and
`drm_mode_property_enum` come from `probe/drm.c`, extended, at both widths.
The plane type values, which the UAPI headers do not export, are written down
from the kernel's `enum drm_plane_type`, as the connector status values were.

**What Linux does, read before the code** (`drivers/gpu/drm/` at `master` on
2026-09-16 and at `v6.12`, the same in both):

* **The primary plane is hidden without the capability.**
  `drm_mode_getplane_res` (`drm_plane.c`) skips every plane whose type is not
  `DRM_PLANE_TYPE_OVERLAY` unless `file_priv->universal_planes` is set. Smithay
  asks for the capability first (`device/mod.rs`) and keeps only primary
  planes when it is refused, so refusing it would leave Smithay with no plane
  at all. So `card0` accepts it, the product owner's choice.
* **The capability drags in nothing atomic.** `drm_setclientcap`
  (`drm_ioctl.c`) takes `DRM_CLIENT_CAP_UNIVERSAL_PLANES` with a value of 0 or
  1, anything larger `EINVAL`, and sets only `universal_planes`. The implication
  runs the other way: `DRM_CLIENT_CAP_ATOMIC` sets `universal_planes` too, and a
  driver without `DRIVER_ATOMIC` refuses it with `EOPNOTSUPP`, as `card0` does.
  No deviation is needed.
* **A legacy primary plane carries `type` and, unless the driver opts out,
  `IN_FORMATS`.** `__drm_universal_plane_init` (`drm_plane.c`) attaches
  `plane_type_property` to every plane; the `FB_ID`, `CRTC_ID`, `CRTC_*` and
  `SRC_*` properties only under `DRIVER_ATOMIC`; and `IN_FORMATS` whenever the
  plane has format modifiers, which it has unless the driver set
  `mode_config.fb_modifiers_not_supported`. That flag is also what
  `DRM_CAP_ADDFB2_MODIFIERS` reports. `card0` is such a driver: no modifiers,
  no `IN_FORMATS`. Linux would answer that capability 0 where `card0` answers
  `EINVAL`, and Smithay reads `IN_FORMATS` only when the answer is 1, so both
  lead it to the plane's format list from `GETPLANE`.
* **`type` is an immutable enum.** `drm_mode_create_standard_properties`
  (`drm_mode_config.c`) makes it with `drm_property_create_enum` and
  `DRM_MODE_PROP_IMMUTABLE`, which adds `DRM_MODE_PROP_ENUM`, from
  `drm_plane_type_enum_list`: `Overlay` 0, `Primary` 1, `Cursor` 2.
  `drm_property_add_enum` keeps the values in that order in `values`.
* **Counts, then arrays.** `drm_mode_obj_get_properties_ioctl`
  (`drm_mode_object.c`) finds the object with `drm_mode_object_find`, which
  answers nothing for an id of another type unless `DRM_MODE_OBJECT_ANY` was
  asked (`ENOENT`), and refuses an object with no property list (`EINVAL`).
  `drm_mode_object_get_properties` skips `DRM_MODE_PROP_ATOMIC` properties
  for a client without atomic, copies each id and value while the caller's
  count has room, and writes back the full count. `drm_mode_getproperty_ioctl`
  (`drm_property.c`) copies the name and flags, each value while
  `count_values` has room, each enum record while `count_enum_blobs` has room,
  and writes back both counts. `drm_mode_getplane_res` and the formats of
  `drm_mode_getplane` count the same way, except that the formats are copied
  only when all of them fit. drm-rs's `get_plane_resources`, `get_plane`,
  `get_properties` and `get_property` (drm-ffi 0.9.0, which Smithay 0.7.0
  takes) call each twice, counts first.
* **A legacy plane shows what the legacy calls set.** `drm_mode_getplane`
  reads `plane->crtc` and `plane->fb` for a plane without atomic state, which
  `__drm_mode_set_config_internal` (`drm_crtc.c`, under `drm_mode_setcrtc`)
  sets to the CRTC and framebuffer, or to none when the CRTC is turned off.
* **CRTCs and connectors.** `__drm_crtc_init_with_planes` attaches CRTC
  properties only under `DRIVER_ATOMIC`, so a legacy CRTC's list is empty, as
  `card0`'s is. `drm_connector_init_only` (`drm_connector.c`) attaches
  `DPMS`, `link-status`, `non-desktop` and `TILE` to every connector, and
  `EDID` unless it is virtual. `card0`'s connector has none, as decided above:
  Smithay's legacy path sets `DPMS` only on a connector that has it, and
  `EDID` and `TILE` are blobs, which need `GETPROPBLOB`.

**Object ids.** Linux numbers all of a device's mode objects from one idr, so
an id names one object whatever its type, and `DRM_MODE_OBJECT_ANY` lookups
are well defined. `card0` keeps that: each head has a block of four ids of
its own, starting at 1 -- the CRTC, the encoder, the connector and the
primary plane, so head 0 is 1 to 4 and head 1 is 5 to 8 -- the `type`
property is above every head's block, the ids below 128 are kept for fixed
objects, and framebuffers are numbered from 128 up and never reused within an
open. In iteration 1 framebuffers were numbered from 1, the same ids as the
CRTC, encoder and connector, which no call could tell apart until
`OBJ_GETPROPERTIES` took `DRM_MODE_OBJECT_ANY`.

**More than one head (2026-09-17).** A card publishes one connector per
scanout the driver's `HELLO` reported, which is what Linux's own virtio-gpu
driver does: a scanout the host has nothing attached to is a connector
reporting `disconnected`, not a connector that is missing. `SETCRTC` and
`PAGE_FLIP` name a head's CRTC and carry its scanout number to the driver, so
two monitors on one card are two framebuffers flipped apart from one another.

QEMU enables a virtio-gpu's second *output* only when a host window manager
resizes its window, which a headless test has nothing to do, so
`cargo xtask test-compositor --screens 2` gives the guest two virtio-gpu
*devices* instead: two cards, one screen each, which is the other shape a
two-monitor machine comes in and the one a test can drive.

**A desktop at its monitor's refresh (2026-10-02).** Until protocol
version 7 every virtual card's mode was listed at 60 Hz, so a compositor
pacing to its mode could never run faster on a 120 Hz host monitor. The
driver now reports each scanout's refresh from the EDID QEMU makes (§2.2),
and the card lists the preferred size at it first -- `1920x1080` at
74.998 Hz under VNC, at the host monitor's rate under a GTK window -- with
60, 120 and 144 Hz of the same size after it, so that a `monitor = ,
1920x1080@120, auto, 1` line can ask for 120 on any host. The kernel's boot
line says it: `display  card0 scanout 0: 1920x1080 at 74.998 Hz`, and
`... is now ...` the same after a `MODES`. Real hardware cards' timings are
listed as before.

**A desktop the size of its window (2026-09-26).** QEMU's GTK window
stretched a 1920 × 1080 desktop to whatever size the window was, sampling
the nearest pixel, and small text came out looking unsmoothed; the customer
took it for fonts without antialiasing. The desktop now follows the window.
QEMU tells the card the window's size whenever it changes, and a VNC
viewer's resize request (`SetDesktopSize`) does the same. The driver sends
`MODES`, the card lists the new size as its preferred mode and makes its
open readable with `EVENT_FERRIX_CONNECTORS`, and hyprix, on a monitor
whose `monitor =` line says `preferred`, sets the new mode: the buffers,
canvas, backdrop and GPU target made again, the GPU's frame adopted again,
the monitor moved in the layout, the pointer's bounds and every client's
`wl_output` sent again. A line that names a mode keeps it, as Hyprland's
does. `run-compositor` writes `preferred` unless given `--size`; judged
boots keep their fixed sizes. Checked by a VNC viewer asking for 2560 ×
1400, 1280 × 720 and 1600 × 900 in turn on the desktop with Chrome: each
time the kernel said `card0 scanout 0 is now ...`, hyprix `Virtual-1 is
... now, as its monitor prefers`, and the screen was the new size with the
windows tiled on it and the GPU still drawing.

**How it is checked.** `src/user/system/linux/compositor/blank` asks for universal planes after its
modeset, reads the planes as Smithay does, and ends its marker line with
`plane 4 Primary`: the plane whose `type` value is named `Primary` in the
property's own enum list, and which must show the framebuffer and CRTC the
program set. `cargo xtask test-display` requires `plane <id> Primary` at the
end of the marker line on x86-64 and AArch64.

**Legacy is not rework.** Linux keeps `SETCRTC` and `PAGE_FLIP` alongside
atomic, and Smithay's DRM backend falls back to them. Atomic commit is an
additive later landing. Stage 17's text names it and stays as written.

**Kernel plumbing this needs:**

* **devfs subdirectories.** `lookup` refuses anything but the root today, so
  `/dev/dri/` becomes the first subdirectory.
* **An ioctl branch** in `sys_ioctl` for the card node, beside the console
  branch. A general `Inode::ioctl` hook is better, but it's a separate
  refactor that I'd rather not smuggle in.
* **An exclusive open.** A second `open` gets `EBUSY` until the first closes.
  That stands in for DRM master and keeps one opener's buffers from another
  in iteration 1. It does not end a mapping: a program that mapped the card
  and closed it can still read whatever the next opener draws. The node is
  `0660` root's, as Linux's `video` group has it, until per-open windows onto
  the card VMO (stage 19's render node) close that.
* **`poll`/`epoll` readiness** on the node when os-26's epoll lands. The
  compositor already waits on its Wayland, input, control, event, and plugin
  descriptors, so an inactive desktop does not scan them on a fixed timer.

### 2.4 A panic once the driver owns scanout

The kernel still draws only a panic, and only into the firmware's framebuffer
from `BootInfo`. What changes is what a person sees:

* **QEMU, x86-64.** q35's default VGA stays (no `-vga none`) and virtio-gpu
  is a second display device. The firmware framebuffer is the VGA BAR. A
  panic is drawn there as today, on QEMU's VGA console, while the virtio-gpu
  console keeps its last frame. The panic is **not lost, but it's on the
  other head**. Serial carries it as always, and the screendump test names
  the virtio-gpu device explicitly.
* **QEMU, AArch64.** The same, with `ramfb` as the firmware's head, once the
  loader chooses it (below).
* **The hazard, which the first run found.** If the firmware binds the
  virtio-gpu itself (OVMF and AAVMF have `VirtioGpuDxe`), its framebuffer is
  *guest RAM* in boot-services data, which the frame allocator hands out
  again, not a BAR or a reserved region. AAVMF does exactly that on AArch64
  (§3). A panic would then draw over frames that belong to someone else, and
  once the driver resets the device nobody would see it.
* **The rule (os-f6, 2026-09-16).** The loader looks at every graphics
  output, not only the first, and prefers one whose framebuffer the
  allocator will not own (`ramfb`'s reserved pages, the VGA BAR) over one it
  will (`VirtioGpuDxe`'s), and its boot line says which it took. It writes
  `BootInfo.framebuffer.reclaimable` (BootInfo version 4) from the final
  memory map with `ferrix_bootinfo::allocator_owns`. That one field has two
  readers: the panic screen draws only when it is 0, and the display core
  refuses to publish `card0` over a framebuffer where it is 1. Nothing is
  reserved to protect a picture nobody would see.
* **Real hardware with one display controller** (the DK1's LTDC, §6): the
  firmware's framebuffer is the one that controller scans out, so once a
  driver takes it over **a panic's picture is lost and serial is not**; where
  that framebuffer is reclaimable RAM, the flag is 1 and the panic goes to
  serial only. The alternative, a driver-independent "restore scanout" in the
  kernel, is drawing by another name, and ARCHITECTURE §1 rules it out.

## 3. QEMU and xtask

* **Devices.** `-device virtio-gpu-pci,id=gpu0,disable-legacy=on,iommu_platform=on,xres=1024,yres=768`
  on x86-64 and AArch64, only under `--display` and `test-display`, so no
  existing gate changes. The size differs from the firmware's usual 1280×800,
  so the kernel's `display` boot line says which device the firmware drew
  on. ARMv7-A takes part too, since 2026-09-23: its `virt` machine has the
  same generic PCI host the kernel enumerates for its disks, so the card is
  the same `virtio-gpu-pci`, with only `disable-legacy=on`. It leaves off
  `iommu_platform=on` for the reason every PCI virtio device on that machine
  does: U-Boot 2025.10's virtio-pci driver resets when a device offers
  `VIRTIO_F_ACCESS_PLATFORM`, so the driver runs in degraded trusted mode
  there, as the disk drivers do. The compositor's programs are built for
  `armv7-unknown-linux-musleabihf`, hard float.
* **`test-display`.** Builds `src/user/system/linux/compositor/blank` as init and boots it with the
  device, waits for a line starting `compositor: `, then asks QEMU over QMP
  (TCP on localhost on every host) for `screendump device=gpu0 head=0` in
  QEMU's default PPM, and compares every pixel. A negative control, the
  program built with `negative-control`, fills pixel (0, 0) differently, and
  the check must fail on exactly that pixel. If the program prints
  `compositor: failed`, the test stops at once and reports what QEMU's first
  console showed.
* **What the first run showed (2026-09-16, before L5 and L6).** On both
  architectures the program ran as init and failed as it should, with no
  `/dev/dri/card0`, and the QMP screendump and PPM parse worked. On x86-64 the
  firmware framebuffer is q35's VGA (`display 1280x800`). **On AArch64 it is
  the virtio-gpu** (`display 1024x768`): AAVMF's `VirtioGpuDxe` took the boot
  framebuffer over `ramfb`, which is §2.4's hazard. The fix is decided before
  L5.
* **`run --display` and `run-compositor`** are the two boots a person
  watches, and `tools/common/xtask/src/window.rs` decides where their screen goes.
  `run-compositor` builds the same image `test-compositor` boots -- the
  compositor as init, its clients, `hyprctl` and a `hyprland.conf` -- and
  gives it a screen, this terminal as its serial port, and the host's own
  accelerator, which is what `run` does too. `--config <PATH>` carries a real
  configuration instead of the small one it writes; zinc is carried at
  `/bin/zinc`, so `SUPER+RETURN` opens a shell on a pseudoterminal.
* **A watched desktop has a wallpaper, and a run never goes looking for
  one (2026-09-18).** Ferrix has no JPEG or video decoder, so a picture is
  converted where one can be and kept: `cargo xtask wallpapers --from
  <directory>`, or `--from <host>:<directory>` to have that host's `ffmpeg`
  do it over `ssh`, writes raw `XRGB8888` pictures cut for a 1920x1080
  screen (`--size` for another; the first frame, for a video) into
  `~/.local/share/ferrix/wallpapers`, or `$FERRIX_WALLPAPERS`.
  `run-compositor` reads that directory and nothing else -- it names no
  host and opens no connection, so it is the same run offline -- carries one
  picture at `/etc/wallpaper.fxwall`, another each run or `--wallpaper
  <name>`, and starts `/bin/pattern --wallpaper` on it before anything the
  configuration starts: a `background` layer surface anchored to all four
  edges, which scales a picture cut for another screen until it covers this
  one. `--wallpaper none` is the plain background. An empty directory is
  *ember* (2026-09-27), a picture xtask draws itself in the brand's colours
  (`wallpaper::ember`), so a first desktop is not a grey one and nothing
  shown in public is somebody else's art; a line says which. A gate boot
  carries none and its archive is what it was.
* **A wallpaper that moves, which is what `mpvpaper` is for (2026-09-18).**
  Hyprland has no video wallpaper of its own: its wiki sends a person to
  `mpvpaper`, started from `exec-once`, which puts mpv on the same
  `background` layer surface `hyprpaper` and `swaybg` use. Ferrix cannot put
  mpv there, so the decoding moves to the side of the conversion that has a
  decoder. An `mp4`, `webm`, `mkv` or `gif` in the pictures directory is
  transcoded by `ffmpeg` to 8-bit 4:2:0 AV1 in IVF, and `run-compositor`
  carries it at `/etc/wallpaper.ivf` and starts `/bin/pattern --video` on it
  instead of `--wallpaper`. `compositor_pattern::Movie` demuxes `DKIF`, feeds
  its AV1 temporal units to rav1d, and converts I420 to XRGB8888 only as each
  frame is due. The image retains compressed packets rather than raw frames:
  a raw 1920x1080 second is 250 MB and the initramfs is built into the
  kernel.
* **What is kept, and where that is decided (2026-09-19).** Frames were kept
  a quarter of the screen each way, ten a second, for four seconds, and the
  client scaled them up to cover the screen. That was the raw-frame budget
  outliving raw frames: measured against this tree's encoder, ten seconds of
  1920x1080 at thirty costs 1.1 MB of initramfs, where the 480x270 four
  seconds it replaces cost 36 KB. The default is now the screen's own size at
  thirty for ten seconds, and the remaining argument for keeping less is the
  **guest's** decode, not the image -- sixteen times the pixels through rav1d
  on an emulated CPU -- which is what `scale` is for. None of it is a
  compositor option: Hyprland has none for a wallpaper, this machine's
  `hyprland.conf` delegates to `booru-wallpaper daemon` and that daemon's own
  commented `config.toml`, and `tools/common/xtask/src/wallpaper/config.rs` is the same
  file for the same job at `~/.config/ferrix/wallpaper.toml`. It could not be
  a compositor option anyway: every setting in it is spent by `ffmpeg` on the
  host when the image is built, and the guest has no encoder to change its
  mind with. `src/user/system/linux/compositor/config`'s table stays exactly Hyprland 0.56's, which
  is what lets the differential harness compare it. The client keeps **two** buffers
  where a still wallpaper keeps one, because a compositor releases a buffer
  when a later commit replaces it: a client that drew once and waited for the
  release would wait for ever.
* **The client damages the rows that changed, not the screen (2026-09-18).**
  `Movie::damage` compares decoded source rows, maps changed rows through the
  same scale the pixels went through — one arithmetic in `Picture::cut`,
  because a row said to have changed whose pixels the scale took from
  somewhere else is a row the compositor would not redraw — and sends a
  `damage_buffer` band for each run of them, up to sixty-four, after which
  one rectangle is cheaper than the list. A buffer not yet drawn into is
  damaged whole whatever the video says, since it holds nothing.
* **And it paces on frame callbacks, which is what stops it (2026-09-18).**
  The next frame waits for `wl_surface.frame` on the last, so a wallpaper
  under a full-screen opaque window is not drawn, is told nothing, and stops
  playing — `mpvpaper-stop`'s job, done by the protocol rather than by a
  person. Five seconds without an answer draws anyway, so a compositor that
  fires no callbacks is a slow wallpaper rather than a stopped one. It also
  keeps **two** buffers where a still wallpaper keeps one: a compositor
  releases a buffer when a later commit replaces it, so a client that drew
  once and waited for the release would wait for ever. That one was found by
  waiting for ever.
* **`cargo xtask test-video` is the gate (2026-09-18).** A four-frame AV1
  test pattern is checked in as IVF, so the gate needs no `ffmpeg` or AV1
  encoder on the machine that runs it. It boots the compositor with that
  wallpaper and **nothing else started**, then takes QMP screendumps until it
  sees two distinct non-flat frames. What it proves is the whole path at
  once: the format, the client that decodes and plays it, the layer surface,
  and the compositor drawing one frame after another. CI runs `test-boot` and
  no display gate, so this one is run by hand, as `test-display` and
  `test-compositor` are.
* **What it costs, measured (2026-09-18), and the honest answer.** About one
  frame a second in `run-compositor` on this Windows machine — and **that is
  not the video path**. The guest runs under `tcg`: WHPX is compiled into
  this QEMU and initialises, but Ferrix panics under it at stage 3 ("the
  timer and the counter disagree about how long a second is"), which is the
  same fragility `tools/common/xtask/src/qemu.rs` records as the reason `tcg` is the
  default. Under full emulation the compositor is 1.1 to 2.1 seconds a
  frame with the video playing — and 2.5 seconds a frame with a **still**
  wallpaper and no video at all. The ceiling is a software compositor
  drawing 1920x1080 through an interpreter, not the frames reaching it.
  `docs/COMPOSITOR-DAMAGE-HANDOFF.md` §2.6 measures the same frame at 22 ms
  on a host that is not emulating. What is still owed on the client's own
  side: `Picture::cover` allocates and scales a fresh 1920x1080 buffer every
  frame, 8.3 MB of it, which the damage does not save.
* **A watched desktop is 1920x1080, and a `monitor =` line is how
  (2026-09-18).** Under a window the card prefers the *window's* size --
  the next item but one found 640x480 on Windows whatever `xres` said --
  and until now the connector listed that one mode and the compositor set
  it. A virtio-gpu shows whatever size it is handed a scanout of, so the
  kernel's connector lists the standard sizes beside the preferred one, as
  Linux's `virtio_gpu` does; `compositor_drm::Plan::take` picks a listed
  mode by size and nearest refresh, and leaves the plan alone for a size
  nobody listed; and `hyprix` reads its `monitor =` lines before it opens
  the screens, so `monitor = , 1920x1080@60, auto, 1` sets that mode.
  `run-compositor` puts that line ahead of whatever configuration it
  carries -- a configuration's own line for the monitor comes later and
  wins -- and gives QEMU the same size as `xres` and `yres`, which is what a
  screen served over VNC is; `--size <W>x<H>` says another. Judged boots
  stay 1024x768, which is what their pictures are of, except the one that
  holds this: `test-compositor --boot mode` asks a card that prefers
  1024x768 for 1920x1080 and requires the picture of two windows tiled on a
  screen that size, all 2,073,600 pixels of it.
* **Where the screen goes is asked, not assumed (2026-09-17).**
  `-display default` is no good: QEMU's `default` is whichever local backend
  was compiled in, and a build with none fails at startup rather than falling
  back. So `window.rs` asks QEMU (`-display help`) and the host, in this
  order:
  1. `gtk`, then `sdl`, when QEMU has one *and* this host can open a window:
     always on Windows and macOS, and on a POSIX host when `DISPLAY` or
     `WAYLAND_DISPLAY` is set.
  2. VNC otherwise, at `127.0.0.1:0` -- the loopback, because `-vnc` without
     `password=on` lets any client in -- with the port printed and the `ssh
     -L` line that reaches it from another machine. `--vnc <display>` asks for
     VNC anyway, and is the only way anything here binds a wider address.
  3. Neither, which is an error naming what QEMU did offer and what to
     install.
  The two hosts this is developed on are exactly the two cases: the Windows
  QEMU 11.1 offers `gtk` and `sdl`; the Linux box the gates run on builds QEMU
  9.2.4 headless, offers `none`, `spice-app` and `dbus`, is reached over
  `ssh`, and shows its screen over VNC.
* **A watched boot has two heads, and the card is the second one.** Firmware
  keeps the machine's own display device -- q35's VGA, `virt`'s `ramfb` -- and
  the kernel's driver drives the virtio-gpu, so QEMU has two consoles and
  shows the first. VNC is told which console to serve
  (`display=gpu0,head=0`), so a viewer sees the compositor and nothing else.
  GTK cannot be told which tab to open on, only to show the tab bar
  (`show-tabs=on`), so the run prints which tab the compositor is and that
  `Ctrl-Alt-2` is its shortcut. Taking VGA away instead (`-vga none`) would
  leave the loader's framebuffer on the card the driver later takes over,
  which is §2.4's hazard and not something a convenience should decide.
* **What a watched boot showed (2026-09-17).** On Windows, `cargo xtask
  run-compositor` opens a GTK window and the guest reaches `hyprix: card0
  Virtual-1 640x480`, with `seat 2 devices [event0 QEMU Virtio Keyboard,
  event1 QEMU Virtio Tablet], 15 binds` and both `/bin/pattern` clients
  started: the binds are pressed by pressing them. The card is 640×480 rather
  than the `xres=1024,yres=768` a headless boot reports, because a console
  with a UI attached takes its size from the window; the compositor follows
  whatever the card says, which is why the picture is right either way.
  Input injection over QMP (`input-send-event`) is `test-input` and
  `test-seat`, and needs no window.

## 4. Landings and points

Each is a small landing on main, gated on example. The first four touch no
kernel code.

| # | Landing | Kernel? | Points |
|---|---|---|---|
| L1 | `src/lib/proto/linux-abi::drm`: ioctl numbers, `drm_mode_*` and `drm_event_vblank` layouts at both widths, from a committed probe | no | 3 |
| L2 | `src/lib/drivers/virtio::gpu`: 2D control commands and responses encoded and decoded, config, features, hostile-device tests, fuzz target; checked against QEMU 9.2.4's `virtio_gpu.h` | no | 5 |
| L3 | `src/lib/proto/displayctl`: §2.2's messages and validation, host-tested, fuzzed | no | 3 |
| L4 | `src/lib/drivers/display/virtio-gpu`: driver logic over `src/lib/drivers/block/virtio-blk`'s traits (bring-up, the command queue, resource lifecycle), tested against a simulated device | no | 5 |
| L5 | `src/user/system/native/drivers/display/virtio-gpu` driver, devmgr's table entry, `DISPLAY_CONTROL_CREATE`, the core's control task and the §2.4 check; exit: the boot line names the mode the driver reported | yes | 8 |
| L6 | devfs `/dev/dri/card0`: subdirectory, exclusive open, the ioctl branch and §2.3's subset, `mapping()` to the card VMO, event `read` | yes | 8 |
| L7 | `src/user/system/linux/compositor/` first binary (open, mode, dumb buffer, fill, `SETCRTC`), built into the initramfs; `xtask test-display` with QMP screendump and its negative control; `xtask run --display` | no | 5 |
|  | **Iteration 1** |  | **37** |

That is 3 more than the 34 first given. The difference is devfs
subdirectories, the exclusive open and §2.4's check, none of which were
counted before. After L7, the next iterations each end in a screendump the
customer can look at: two pattern clients tiled (the layout core, the protocol
server), then input.

## 5. Decisions and open questions

**Decided by os-f6, 2026-09-16:**

1. **The card VMO's budget is 256 MiB** for iteration 1, as one named
   constant with its arithmetic (§2.1). Ranges are decommitted and reused
   (added in os-02's review round, which found the budget could be spent for
   good).
2. **The exclusive open stands in for DRM master**, as a written deviation
   from Linux: a second `open` of `card0` answers `EBUSY`, and `SET_MASTER`
   and `DROP_MASTER` answer for the one client. Linux's many opens with one
   master come with the render node in stage 19, and `docs/BACKLOG.md` has a
   row saying so.
3. **Legacy-only DRM** in iteration 1: atomic is refused through
   `SET_CLIENT_CAP` so Smithay falls back, and Linux keeps legacy.
4. **`card0` reports the card VMO's size** as `st_size` (§2.1).
5. **§2.4's rule** — refuse to publish `card0` when the firmware framebuffer
   lies in allocator-owned memory, and say so on the boot line — is right.

**Still open, for the kernel reader of L5 and L6:** whether the ioctl branch
is acceptable or `Inode::ioctl` should come first; and who reserves the
firmware framebuffer's frames, mm or the reader, if §2.4's check ever fires.

## 6. A board's HDMI output: the DK1's LTDC and its SiI9022

Written by os-d1, 2026-09-23, at the customer's request to run the Wayland
stack on the STM32MP157D-DK1 over HDMI. What was the P3 hardware row of
stage 17 is now a second kind of card beside virtio-gpu, served by the same
core and the same control protocol: nothing above `/dev/dri/card0` knows the
difference, and `hyprix` names the connector `HDMI-A-1`.

**The hardware.** The DK boards scan out from the SoC's LTDC, a display
controller with two layers and no scatter-gather, through a 24-bit parallel
RGB bus into a Silicon Image SiI9022 HDMI transmitter on I2C1 at 0x39
(`stm32mp15xx-dkx.dtsi`). The LTDC's pixel clock is PLL4's Q output, which
the board's firmware (TF-A) leaves at exactly 74.25 MHz — 24 MHz from the
HSE, divided by 4, times 99, divided by 8, read back from the RCC on the
board — and that is CEA-861's 1280x720 at 60 Hz. Until 2026-09-24 the card
ran that one mode; it now runs the largest mode the monitor offers that
the board can make a pixel clock for (**Modes**, below), by changing Q's
divider and nothing else of PLL4.

**Who does what.**

* **The kernel, once, at boot** (`src/kernel/src/platform/st/stm32mp1.rs`): the parts every
  peripheral on the chip shares, which no driver may hold. It checks PLL4's Q
  output is 74.25 MHz within half a percent and the HSI is running undivided;
  turns on the LTDC's clock and I2C1's, and puts I2C1's kernel clock on the
  64 MHz HSI; muxes the LTDC's and I2C1's pins from their `pinctrl-0`
  groups; pulses the bridge's `reset-gpios`. Then it publishes one device
  node, `Location::Tree`, with two apertures (the LTDC's page and I2C1's,
  each the whole page RM0436 gives the peripheral) and the LTDC's interrupt,
  and says so: `display  LTDC at 0x5a001000, HDMI bridge at 0x39 on I2C
  0x40012000, pixel clock 74.250 MHz (at most 74.250), 30 pins muxed`.
  Anything it cannot check leaves the display alone with a line saying why.
  Later, on the driver's request (`device_clock`, native call 0x104F,
  `MANAGE` on the device), it rounds and sets the pixel clock: PLL4's Q
  divider, gated while it changes, and only while no other kernel clock
  runs from Q — it reads the thirteen muxes that can pick `pll4_q` (Linux's
  `clk-stm32mp1.c`) and DSI's gate each time. It prints `display  pixel
  clock 74.250 MHz: PLL4's VCO over 8`, or why it would not.
* **devmgr** matches the node by `device_info`'s new `DEVICE_TREE_BLOCKS`
  kind and the binding number `TREE_STM32_HDMI`, and starts `/lib/drivers/ltdc`
  with blk's START shape, the two register windows where virtio's blocks
  would be.
* **The driver** (`src/user/system/native/drivers/display/stm32-ltdc`, logic in `src/lib/drivers/display/stm32-display`, host-tested
  against models of the LTDC, the I2C controller, the bridge and an EDID
  EEPROM, and with real monitors' EDIDs): finds the bridge in TPI mode and
  checks its chip id, reads the monitor's EDID (base block and first
  extension) through the bridge's DDC pass-through, says what it found on
  the console, chooses HDMI (with an AVI infoframe naming the mode's VIC) or
  DVI, asks the kernel for the chosen mode's clock, starts the LTDC with its
  layer off, tells the bridge the mode and turns TMDS on. HELLO offers one
  scanout at the chosen size and lists every mode the board can run on the
  monitor (display protocol version 5: up to sixteen timings after the
  scanouts).

**Buffers are one run of memory.** The LTDC has one address register per
layer and nothing to translate through, so a buffer it shows must be
physically contiguous. The core fills such a card's ranges at ATTACH with
one block from the frame allocator — 720p's 3.6 MB fits the largest block,
4 MiB — or, for a larger buffer, with that many largest blocks lying back
to back (`Frames::allocate_run`: 1920x1080's 8.3 MB and 1920x1200's 9.2 MB
take three), which `src/lib/kernel/frame`'s `split` turns into single frames, so the
card VMO owns, maps, decommits and gives back each page exactly as it does
any other, and the pages past the buffer go back at once. A run exists only
where that much memory is free and aligned to 4 MiB, which after boot is
most of the board's 512 MiB; a compositor makes its buffers at start. The
driver pins the range and refuses one that is not a single run below 4 GiB.

**The LTDC does not snoop the caches.** The compositor draws through a
normal cacheable mapping, as on every other card, and the core writes the
buffer's lines back to the point of coherency (`arch::clean_for_device`,
`DCCMVAC` on ARMv7-A) before it tells the driver: the whole buffer before
SCANOUT, the damaged rows before FLUSH. Nothing about the mapping changes,
so the compositor's reads of its own buffers stay fast. On x86-64 the clean
is a no-op; virtio cards are coherent and skip it.

**Flips are paced by the screen.** SCANOUT points the layer at the buffer,
FLUSH asks for a shadow reload at the next vertical blanking, and FLIPPED goes
out on the reload's interrupt: a compositor's page flips come at 60 Hz at
most, never mid-screen. DETACH of a buffer on screen takes the layer off and
waits for that reload before it unpins.

**The DRM surface.** The connector is `DRM_MODE_CONNECTOR_HDMIA` and lists
the modes the driver listed, each with its own timing and refresh, the one
running first and marked preferred. `SETCRTC` with another listed size
sends a SCANOUT of that size, and the driver switches mode on it (TMDS off,
LTDC stopped, clock set, both started again); `SETCRTC` with a size not
listed is `EINVAL`, as Linux's `mode_valid` refuses it. So `monitor =
HDMI-A-1, preferred, ...` (and `highres`, which hyprix reads the same way)
runs the largest mode, and `monitor = HDMI-A-1, 1280x720@60, 0x0, 1,
transform, 3` still runs 720p: `compositor_drm::Plan::take` finds 1280x720
in the list — the monitor's VIC 4, or the 720p60 the board always offers —
and the switch happens at the first modeset. A size the connector does not
list is still left alone by `Plan::take`. It reports connected whatever the
bridge's hotplug line says: there is no hotplug path to tell a compositor
later, and a card that said disconnected at boot would stay dark until the
next one.

**Modes (2026-09-24).** What limits them, with sources:

* **The LTDC's pixel clock tops out at 90 MHz**: DS12504 Rev 4 (the
  STM32MP157A/D datasheet), table 94, `fCLK` max 90 MHz at 2.7 to 3.6 V
  with the pins at high or very high speed; Linux's `stm_drm_plat_data`
  gives the same `pad_max_freq_hz`. 1920x1200 at 60 Hz needs 154 MHz with
  CVT reduced blanking and 1920x1080 at 60 Hz 148.5 MHz: neither is in
  reach on any STM32MP15. The monitor's native 1920x1200 is therefore
  possible only at about 29 Hz, and only on a monitor that takes timings
  it does not list.
* **The DK boards' LTDC pins are at medium speed** (`ltdc_pins_a`,
  `slew-rate = <1>` in Linux's `stm32mp15-pinctrl.dtsi`), for which the
  datasheet gives no rate at all. The board runs 74.25 MHz there, so the
  kernel holds the clock to the rate firmware left (74.25 MHz) unless the
  tree asks for high speed; raising the pins' speed would open 84.857 MHz
  (594/7) and is untested.
* **Only Q's divider moves.** TF-A's `pll4_cfg1` runs PLL4's VCO at
  594 MHz with P at 99 MHz for SDMMC1 (the SD card) and R at 74.25 MHz;
  moving the VCO would move those. So the rates are 594 MHz / k: 74.25,
  66, 59.4, 54, 49.5, ... 27 MHz. A mode the monitor lists runs if one of
  them is within 0.5 % of its clock (CEA-861's tolerance, the one the boot
  check uses): every 74.25 MHz mode — 720p60 and 50, and 1920x1080 at 24,
  25 and 30 Hz (VICs 32 to 34) — 720x480 and 720x576 at 27 MHz, 800x600 at
  75 Hz (49.5 MHz) and IBM's 720x400 at 70 Hz. VGA's 25.175 MHz, 800x600
  at 60 (40 MHz) and 1024x768 at 60 (65 MHz) are 1 to 2.6 % off and do not.
* **A monitor whose EDID says it takes any timing inside its range limits**
  (feature byte bit 0: continuous frequency in EDID 1.4, default GTF in
  1.3) also gets each size it lists retimed to a rate the board makes —
  its own blanking or CVT's reduced blanking, whichever refreshes faster —
  when that lands inside the vertical and horizontal ranges it gives.
  Neither Dell 1920x1200 monitor in the tests sets that bit.
* **720p60 is always offered**, listed or not, as the board ran it on every
  HDMI sink before it read EDIDs; no monitor gets less than it had.

The driver runs the largest, by pixels and then refresh. Two real monitors,
from their EDIDs (`src/lib/drivers/display/stm32-display/src/edid_tests.rs`): a DELL U2415
(1920x1200, HDMI) lists VICs 32 to 34 and gets **1920x1080 at 30 Hz**; a
DELL U2412M (1920x1200, DVI) lists nothing between 720x400 and 154 MHz and
stays at **1280x720 at 60 Hz**. Scanout bandwidth does not change with the
mode — it is the pixel clock times four bytes, 297 MB/s at 74.25 MHz, as
before — but a frame does: 1080p is 2.25 times 720p's pixels, so a full
software redraw on the two 650 MHz Cortex-A7s costs 2.25 times as long
(about 100 ms a frame at 720p once warm, so about 225 ms at 1080p), against
a refresh that is half as fast. A `monitor =` line naming 720p keeps the
cheaper mode. The driver's console lines at start, for the board test: the
monitor's name and EDID version, the EDID in hex (`ltdc: edid 000 ...`, 32
bytes a line), its range limits, one `ltdc: offered ...` line per mode with
its pixel clock and whether the board makes it, one `ltdc: can run ...`
line per mode listed to DRM, and `ltdc: running ...`.

**The pointer on the second layer (2026-09-24).** The LTDC blends a
second layer over the first, in a window that can be anywhere on the
screen, from a buffer of its own: a cursor plane. HELLO says the card has
one (`cursor`, as virtio-gpu's does) and the driver says so on the
console: `ltdc: a 64x64 cursor plane on the second layer`. Without it the
pointer was drawn into the frame, and on the board a frame is composed,
turned for the portrait monitor and flipped in 25 ms on average and 100 to
150 ms at worst, so the pointer moved only as often as frames came and
lagged the hand by that much. With it a pointer moving is a MOVE: the
driver rewrites the layer's window position and asks for the shadow
reload at the next vertical blanking, and no frame is drawn.

* CURSOR points the layer at the attached 64 × 64 buffer itself -- one run
  of memory like every buffer on this card, which the core cleans from the
  caches before it sends CURSOR -- and is answered at the reload, after
  which the buffer shown before is no longer read. Nothing is copied: a
  ring-3 driver on ARMv7-A cannot clean the caches, and a copy would have
  needed contiguous memory of its own. `hyprix` draws each new image into
  the other of two cursor buffers, so an image is never shown half drawn
  and it and its hotspot's place change at the same blanking.
* The layer is ARGB8888 with the blending factors for premultiplied
  colour, which is what a Wayland client's pixels and the plane's image
  are: `BF1` the constant alpha (one), `BF2` one less pixel alpha times
  constant alpha (RM0436's `100` and `111`). Its default colour, blended
  outside the window, is transparent black.
* A MOVE and a flip in the same frame land at the same reload, and neither
  waits for the other: both are shadow registers under the one
  `SRCR.VBR`. A MOVE is answered by nobody, and a FLIPPED goes out at the
  first reload after its request: a request read in the same batch as a
  reload that has already happened settles that reload's answers first,
  so it is not answered a frame early.
* An image across the screen's left or top edge is shown from its first
  column or row on the screen, the layer's start address moved on by the
  pixels cut off and its window narrower; across the right or bottom edge
  the window is only narrower. Wholly off the screen, or hidden (buffer
  0), the layer is off. `ferrix_stm32_display::ltdc::Clip` is host-tested
  at each edge and corner.
* A mode switch stops the LTDC, which takes both layers off; the pointer
  is put back where it was, clipped to the new screen, when the
  controller starts again.
* The monitor is turned (`transform, 3`), and the card knows nothing of
  it. `hyprix` turns the plane's image, its hotspot and its place as it
  turns the frame (`src/user/system/linux/compositor/hyprix/src/plane.rs`, `turned`): the
  pointer lands on the buffer pixel the frame would have drawn it on, for
  all eight transforms, which virtio-gpu's plane gets as well -- before
  this a turned monitor drew its pointer into the frame on every card.

One reload serves both layers, and it cannot be taken back once asked
for: a move written in the very instant a reload happens may land half
in this frame and half in the next, which is one frame of a pointer one
step off.

**What ran on the board (2026-09-23).** At `FERRIX-BOOT-OK stages 1-12` the
node was published, `devmgr   1 devices, 6 drivers, 1 started, 0 failed`,
`display  card0 scanout 0: 1280x720`. `src/user/system/linux/compositor/blank` as init printed
`compositor: scanout 1280x720 1280x720 colour 0x1e1e2e plane 4 Primary` and
the customer saw that colour on the monitor. `hyprix` as init reported `1
monitor [card0 HDMI-A-1 1280x720 1280x720]`, started a terminal running
zinc, and the customer saw the tiled window. A frame took about 100 ms in
software on the 650 MHz Cortex-A7 once warm; since 2026-09-24's work on the
board (`docs/ROADMAP.md`, the desktop at the speed of a hand) a focus change
between two terminals is a frame of 26 to 30 ms, most of it the flip's wait.

**Not done.** Input is done beside it: USB keyboards and mice on the board's
USB host, 2026-09-23 (`docs/INPUT.md` §7). EDID blocks past the second (the
E-DDC segment pointer). Rates above 74.25 MHz (the LTDC pins' speed, above).
The connector's `EDID` property, which `compositor_drm::Plan` would read.
Hotplug. The panic screen:
the firmware left no framebuffer on this board, so a panic is serial only,
as §2.4 says.

## 7. A connector's EDID: `drm.edid_firmware=` (2026-09-26)

**Why.** A `hyprland.conf` names its monitors by description --
`monitor = desc:Lenovo Group Limited R27qe Gen2 UTP03KBB, preferred, 2560x0, 1`
-- and so does waybar's `"output"`. Under QEMU `card0`'s connector had no
EDID, so hyprix gave the screen no description, its `wl_output` and
`xdg_output` description was only `Virtual-1`, and neither the monitor rule
nor the bar found it. The customer chose to give QEMU's screen the real
monitor's EDID the Linux way, `drm.edid_firmware=[<connector>:]<file>`, and
to leave their configuration as it is.

**Decided: the kernel, not the driver.** Where the blob is handed out is
the DRM uAPI, which the display core owns (§2.3): a connector's `EDID`
property and `DRM_IOCTL_MODE_GETPROPBLOB`. Where the bytes come from was
the question, and the core reads the file itself:

* It is what Linux does. `drm_edid_load.c` is the DRM core's, not a
  driver's: `drm_load_edid_firmware` reads the option and `request_firmware`
  reads `/lib/firmware/<file>` for whatever connector is being probed,
  whichever driver it belongs to.
* It is the smaller change. The core already has the command line
  (`/proc/cmdline`'s copy) and reads files whole (`fs::read_file`). The
  ring-3 drivers are native programs with handles and no filesystem: the
  driver-side design would have been a new displayctl message (protocol
  version 7), a way for a native driver to be handed `/proc/cmdline` and a
  firmware file, and the same parse in `src/user/system/native/drivers/display/virtio-gpu` and `src/user/system/native/drivers/display/stm32-ltdc` both.
* One place means every client sees the same bytes: hyprix through the
  property, anything else through the same ioctls, and
  `/sys/class/drm/card0-Virtual-1/edid`, which the core now has too.

The grammar and the checks are `src/lib/proto/displayctl/src/edid.rs`'s, host-tested
(`edid_tests.rs`): the comma-separated entries, the first `<connector>:`
entry whose connector the name *starts with* (Linux's `strncmp` over the
entry's length, so `DP-1:` is `DP-10`'s too), else the last entry with no
connector; a file whose size is not what its base block's extension count
says is refused, a base block with a bad checksum, fewer than six header
bytes right or a version other than 1 is refused, two wrong header bytes
are put right, and an extension block that is not valid is dropped and the
base block's count and checksum made to agree -- all as `edid_load` and
`drm_edid_block_valid` do. The name must stay beneath `/lib/firmware`:
one that is empty, absolute, holds a NUL or has a `..` component
(`../etc/shadow`, `/etc/shadow`) is refused before anything is read, and
the read walks from `/lib/firmware` one component at a time following no
symbolic link (`fs::read_file_beneath`, `openat2`'s `RESOLVE_NO_SYMLINKS`),
so a link planted there is `ELOOP`. Linux's loader is laxer on both; the
certification consultant asked for it (2026-09-26), since the bytes are
handed to every program that opens the card. Stage 8's boot check holds
the kernel half. `src/kernel/src/interfaces/display/edid.rs` reads the option
from the loader's command line when a driver's HELLO is accepted, before
READY, once per card: that is Linux's connector probe. The connector is
named as Linux names one, `Virtual-1` or `HDMI-A-1`, numbered per card
(`card1`'s first connector is `Virtual-1` to the kernel, where hyprix
renames it `Virtual-2`; an entry with no connector covers both). One boot
line per file named:
`display  card0 Virtual-1: EDID from "edid/LEN-R27qe-Gen2.bin": LEN R27qe Gen2 UTP03KBB, 2 extensions`,
or why not, and the connector keeps no EDID. A card is never refused over
it. Two things are not Linux's: the six built-in EDIDs
(`edid/1024x768.bin` and the rest, which later Linux kernels removed
too; 6.1 still has them), and the search path, which is `/lib/firmware`
alone. And the option is the first of its key on the command line, as every Ferrix option is (`CMDLINE.TXT` before
the image's `DEFAULTS.TXT`), where a Linux module parameter's last wins.

**The uAPI.** A connector given an EDID has one property, `EDID` (id 66,
after the plane's `type`), an immutable blob, `count_values` and
`count_enum_blobs` 0, listed by `OBJ_GETPROPERTIES` and by `GETCONNECTOR`'s
`props_ptr`. Its value is the blob's id (67 + the head) while a display is
connected and 0 while none is, as Linux clears the property on a
disconnect. `GETPROPBLOB` copies the bytes only when `length` is exactly
the blob's, and writes the length back either way; an id that names no
blob is `ENOENT`. A connector with no EDID has no property at all, as
before: Linux attaches `EDID` to every connector that is not virtual,
virtio-gpu's only when the device has `VIRTIO_GPU_F_EDID`, which ours
declines, so a virtual connector without one listing nothing is Linux's
answer.

**Identity, not modes.** Linux's `virtio_gpu_conn_get_modes` would take
the EDID's modes and none of its own. Ferrix takes the monitor's name for
itself and nothing else: the connector keeps the device's preferred mode
and the standard sizes (§2.3), and `mm_width` and `mm_height` stay 0. That
is what `run-compositor` needs. The R27qe prefers 2560x1440, while the
screen `run-compositor` watches is the size of its window and follows it
when it is resized (§3): the user's line says `preferred`, the card's
preferred mode is the window's, so the window wins, and nothing can ask the
card for a mode QEMU does not scan out. (A virtio-gpu scans out any size up
to `MAX_DIMENSION`, and 2560x1440 is among the standard sizes listed
anyway, so a `monitor = …, 2560x1440, …` line still picks it.)

**xtask.** `run-compositor` looks for the monitor on the host when it
builds the image: every `/sys/class/drm/card*-*/edid` is read, checked with
the same `check`, described as hyprix describes one (the PNP registry's
make, the `0xFC` name, the `0xFF` serial), and the first whose description
starts with `--edid <DESCRIPTION>` is taken -- by default
`Lenovo Group Limited R27qe Gen2`, the customer's monitor under waybar's
bar. Never by connector: `card2-DP-1` today is another connector after the
host's next boot. The EDID goes to `/lib/firmware/edid/LEN-R27qe-Gen2.bin`
in the initramfs, `drm.edid_firmware=edid/LEN-R27qe-Gen2.bin` into the
image's `DEFAULTS.TXT`, and the host's `/usr/share/hwdata/pnp.ids` to the
same path in the guest, where `src/user/system/linux/compositor/drm`'s `registered` turns `LEN`
into `Lenovo Group Limited`. Neither file is ever committed: both are read
from the machine the image is built on. On a host without `pnp.ids` the
make is the code and the description `LEN R27qe Gen2 UTP03KBB`, which the
user's `desc:Lenovo…` line does not match; xtask says so. On a host without
the monitor xtask says `edid: no monitor of this machine describes itself
as …; the screen has no EDID and is only Virtual-1`, carries nothing and
adds no argument, and the desktop is what it was before. `--edid none`
asks for that.

**What the user's configuration then does.** Hyprland picks a monitor's
rule as the last line that *names* it, by connector or `desc:`, and only
when none does the catch-all `monitor = , …`
(`CMonitorRuleManager::get`, 0.56.2). hyprix took the last line that
matched at all, so the customer's `monitor = , preferred, auto, 1` below
their three `desc:` lines won over all three; it now picks as Hyprland
does (`MonitorRule::for_monitor`). With the EDID the one QEMU screen is the
R27qe, alone at `2560x0`, at the window's size. What that did, looked for:

* **The pointer.** hyprix's pointer moved over the box from the layout's
  corner, 0, 0, to the far edge of the rightmost screen. A lone screen at
  `2560x0` left 2560 empty pixels on the left: the pointer could go there,
  and QEMU's tablet, whose range is the whole window, landed there for
  most of it. The box now starts at the leftmost and topmost screen, as
  Hyprland's layout box does (`Screen::desktop`, `Seat::place_at`).
  `test-compositor --boot edid` is the check, and its negative control
  (the fix's `place_at` taken out) failed it: 178 pixels of the arrow in
  the wrong place.
* **Windows, layer surfaces, screenshots.** Each is placed and drawn
  relative to its monitor's rectangle, and `wl_output.geometry` says
  `2560, 0`, which a client places against. The EDID boot's first picture,
  the tiled pair drawn at `2560x0`, is `dwindle-two-clients.xrle` to the
  pixel. A screenshot of an output is of that output
  (`zwlr_screencopy_v1` names the output, not a region of the layout); a
  region given in layout coordinates, as `grim -g` gives one, has to say
  2560 and not 0, as it does on the customer's Hyprland.
* **waybar** matches its `"output"` against `wl_output.description` with
  ` (Virtual-1)` cut off, which is now the monitor's description, so the
  bar goes on this screen.

**Tests.** `src/lib/proto/displayctl` (13: the grammar, the name rule, the checks, the identity,
the property and blob answers); `xtask` (3: finding a monitor among
connector directories, describing one, what an image carries);
`src/user/system/linux/compositor/config` (rule precedence) and `hyprix` (the pointer over a
screen at `2560x0`). `test-compositor --boot edid` boots with the host's
monitor, or on a host without it a stand-in EDID xtask makes (`FRX Ferrix
Test EDID0001`), and requires the kernel's line, `hyprctl monitors` saying
`description: Lenovo Group Limited R27qe Gen2 UTP03KBB` and `at 2560x0`,
and the pointer boot's two pictures at that place.

**Not done.** The board: the LTDC driver reads its monitor's EDID and does
not hand it to the core, so `HDMI-A-1` has no `EDID` property and no
description unless the command line names a file (`docs/BACKLOG.md`).
The EDID's physical size in `GETCONNECTOR`'s `mm_width` and `mm_height`.
QEMU's own EDID (`VIRTIO_GPU_F_EDID`, `GET_EDID`) when nothing is named.
