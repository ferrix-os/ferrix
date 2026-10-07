# The GPU: the host's driver first, a card of Ferrix's own later

Decided by the customer on 2026-09-18. `docs/ROADMAP.md` stage 19 left the
GPU path as the customer's choice and said where it would be recorded; this
is the reasoning, the order and the sizes, and `docs/BACKLOG.md` carries the
decision and the rows.

**Path A first**: GPU acceleration for Ferrix *as a guest*, through
virtio-gpu's 3D commands, which puts the host's own GPU driver -- NVIDIA's, on
both machines this is developed on -- behind the guest's rendering without a
line of it being ported. **Path B later**, in a stage of its own
(`docs/ROADMAP.md` stage 21), when the customer wants Ferrix on bare metal
with an NVIDIA card: a driver for the card itself.

The two are not alternatives and A is not a stopgap. Everything A builds
above the driver -- the render node, the GPU renderer in the compositor,
dmabuf -- is what B needs too. B replaces what is *under* it.

---

## 1. Why this is wanted

Every pixel is drawn on the CPU today. 2026-09-17 and -18 went into making
that bearable (`docs/COMPOSITOR-DAMAGE-HANDOFF.md`): a frame that changes
little costs little. But a frame that changes *everything* -- a video
wallpaper behind a translucent terminal, which is the customer's desktop --
is a dual-Kawase blur of two million pixels every frame, 20 ms across eight
cores on the host and far worse in a guest with one processor. Those are
shaders. Hyprland is fast on the same desktop because a GPU runs them.

## 2. What was checked before deciding

* Both development machines have an NVIDIA GeForce RTX 3090.
* The Windows machine's QEMU, 11.1.0, offers `virtio-gpu-gl-pci` and
  `virtio-vga-gl` and the `gtk`, `sdl` and `egl-headless` displays. That is
  the machine `cargo xtask run-compositor` is watched on, so **the host half
  of Path A is already installed where it matters**.
* The Linux gate host's QEMU, 9.2.4, was built headless and offered no
  `-gl` device, though `libvirglrenderer` was installed there. It has been
  rebuilt since, and does now: §3.1.
* Ferrix drives virtio-gpu in 2D only: `src/lib/drivers/virtio::gpu` has the 2D control
  commands, `/dev/dri/card0` has dumb buffers, `SETCRTC`, `PAGE_FLIP` and
  `DIRTYFB`, and there is no render node (`docs/DISPLAY.md`).

---

## 3. Path A: virtio-gpu 3D

A guest never sees the NVIDIA card. It sees virtio-gpu, and with the `-gl`
device QEMU replays the guest's GL command stream into the host's driver
through virglrenderer. Nothing of NVIDIA's is ported; all of it is used.

In dependency order, with sizes in story points:

| # | what | points |
|---|---|---|
| 1 | **virtio-gpu 3D in the ring-3 driver.** `VIRTIO_GPU_F_VIRGL` and `CONTEXT_INIT` negotiated, `GET_CAPSET_INFO`/`GET_CAPSET`, `CTX_CREATE`/`CTX_DESTROY`/`CTX_ATTACH_RESOURCE`, `RESOURCE_CREATE_3D`, `SUBMIT_3D`, `TRANSFER_TO_HOST_3D`/`FROM_HOST_3D`, and fences. `src/lib/drivers/virtio::gpu` already has the 2D commands and their fuzz target, and this extends both. **Mostly done -- §3.2.** | 13 |
| 2 | **A render node and the `virtgpu` ioctls.** *Begun: `src/lib/proto/renderctl`, `kernel::render`, the ABI table and the node itself are written, a program opens it, and `RESOURCE_CREATE`/`RESOURCE_INFO` make a real resource through the open's handle table -- §3.4. `MAP`, the calls that need a context, and scanout are what is left.* `/dev/dri/renderD128`, GEM handles, `DRM_IOCTL_VIRTGPU_GETPARAM`, `GET_CAPS`, `CONTEXT_INIT`, `RESOURCE_CREATE`, `RESOURCE_INFO`, `MAP`, `EXECBUFFER`, `TRANSFER_TO_HOST`/`FROM_HOST` and `WAIT`, in `src/lib/proto/linux-abi` from a committed probe as every other ABI table is. And **scanout of a 3D resource**, so the finished frame never leaves the GPU: no per-frame transfer at all, where today's best is the damaged rectangles. | 13 |
| 5 | **The host half, in xtask.** `-device virtio-gpu-gl` and a GL display where QEMU has them, asked for as `window.rs` asks for a display today, with the 2D device otherwise. The gate host's QEMU rebuilt with OpenGL and virglrenderer, and `egl-headless` for judged boots. GPU output is not byte-exact across drivers, so a judged GPU boot compares within a stated tolerance, or against a software GL pinned on the gate host; the software renderer's images stay byte-exact. **Judging one needs a way to read its pixels that is not `screendump` -- see §3.1.** | 5 |
| 3b | **A GPU renderer for the compositor, in Rust.** A `src/user/system/linux/compositor/virgl` crate that encodes virgl's command stream -- object creation, state, `draw_vbo`, resource transfers -- with shaders as the TGSI text virgl takes. Hyprland's effects are about eight shaders: the two blur kernels, `blurprepare`, `blurFinish`, the rounded texture, the border gradient, the shadow. `src/user/system/linux/compositor/render` gains a renderer trait with the software renderer as the fallback the roadmap already requires -- the compositor is never GPU-only. Clients stay `wl_shm`; their damaged rectangles are uploaded as textures. | 21 |
| 4 | **`zwp_linux_dmabuf` and a GBM-shaped allocator.** Only once clients render on the GPU themselves. Not needed for 3b, and deferred with 3a. | 8 |

**52 points to a GPU-composited desktop** (1, 2, 5, 3b), in that order: the
driver and the node first because nothing above can be tested without them,
the host plumbing third so that the renderer is gated from its first commit.

**Built, 2026-09-19 (§3.7 and §3.8).** Steps 1, 2, 3b and 5 are done and the
desktop composites on the GPU: the compositor draws its frame there and the
screen is shown that very texture. 1920x1080, a video wallpaper behind a
blurred translucent terminal, under KVM on the gate host: **39 ms a frame in
software, 12 ms on the GPU**, where 60 fps is 16.7 -- on the scene and the
quiet host §3.8 describes, which is worth reading before quoting the number.
Step 4 is still only for clients that render for themselves.

### 3.1 What was found when the gate host was given the device (2026-09-18)

The Linux gate host's QEMU was rebuilt as step 5 asks: the 9.2.4 tree at
`~/Documents/qemu/qemu`, reconfigured with `--enable-virglrenderer
--enable-opengl`, which gives it `virtio-gpu-gl-pci`, `virtio-vga-gl`,
`egl-headless` and `gtk`. Two things had to be worked around and one is a
real constraint on the plan.

* **`--disable-werror` is needed on this host.** QEMU 9.2.4 does not
  compile with the distribution's current GCC: `util/log.c` trips
  `-Werror=discarded-qualifiers`. Nothing to do with the GPU, and no reason
  to patch someone else's tree over it.

* **`xtask --gl` boots.** Ferrix comes up on `virtio-gpu-gl-pci` exactly as
  on the 2D device -- the 3D card is a superset, and the 2D driver drives it
  unchanged. That is the whole of the host half working.

* **`FERRIX_QEMU` points at the rebuilt one.** A hand-built QEMU does not
  have to be installed over the machine's, which needs root and replaces
  something a person may be relying on. `FERRIX_QEMU=~/Documents/qemu/qemu/build
  cargo xtask test-display --gl` is the whole invocation on this host.

* **`screendump` cannot read a GL console, and this is not a setting.**
  QEMU 9.2.4's `qmp_screendump` (`ui/ui-qmp-cmds.c`) asks for a
  `DisplaySurface` and gives up with *"no surface"* when there is none;
  there is no GL path in it. With `egl-headless` the console holds a scanout
  texture on the host GPU and no surface, so every judged boot's way of
  reading pixels stops working the moment the card is the 3D one. `cargo
  xtask test-display --gl` shows it: the guest boots, the compositor sets
  its scanout, and the dump fails.

  So step 5 owes a second way to read a frame, and the cheapest one is
  probably from *inside* the guest -- the compositor already knows how to
  write its own frame out (`src/user/system/linux/compositor/shot`), and a picture judged there
  needs no host console at all. That also sidesteps the tolerance question
  for the 2D path, where the guest's bytes are still the renderer's own.
  Whether a newer QEMU grew a GL screendump is worth checking before
  writing anything.

### 3.2 Where step 1 stands (2026-09-18)

**The wire format is complete** and every command is unit-tested against
`virtio_gpu.h` by hand and fuzzed: the capability sets, the four context
commands, `RESOURCE_CREATE_3D`, both 3D transfers and `SUBMIT_3D`, with the
control header's context, fence and ring (`gpu::Context`), which a 2D
driver had been writing as zeros.

**Four of them are proved against real virglrenderer**, not just against
the header. `cargo xtask test-display --gl` prints

    display  card0 is a 3D card: virgl, 2 capability sets,
             the first #1 of 308 bytes

which is feature negotiation granting `VIRTIO_GPU_F_VIRGL`,
`num_capsets` read from the configuration block, `GET_CAPSET_INFO`
answering, and `GET_CAPSET` returning `virgl_caps_v1`'s 308 bytes. Without
`--gl` the same boot says `card0 is a scanout: no 3D`.

Two things a person picking this up should know:

* **QEMU offers two capability sets and index 0 is `VIRGL` (#1), not
  `VIRGL2` (#2).** A renderer that wants VIRGL2 walks the indices; taking
  index 0 gets the older set.
* **The driver's response buffer is a page now** (`RESPONSE_BYTES`), up
  from 512 bytes, because a capability set is longer than anything the 2D
  half ever read. `CAPSET_ROOM` is what that leaves, and the driver
  declines to ask for a set larger than it rather than have the device
  write past the end.

**A context and a resource are made on the device** through the seam §3.3
describes:

    render   renderD128 is `virtio_gpu`, version 1, capset 1, objects to 64 MiB
    render   renderD128 made context 1 on the device
    render   renderD128 made object 1 of 4096 bytes in context 1
    render   renderD128 gave object 1 back

`kernel::render` names no virtio type. What turns `MAKE_CTX` into
`CTX_CREATE`, `MAKE_OBJ` into `RESOURCE_CREATE_3D` with
`CTX_ATTACH_RESOURCE` behind it, and `DROP_OBJ` into `RESOURCE_UNREF`, is a
hundred lines in `src/user/system/native/drivers/display/virtio-gpu`: the adapter a second GPU replaces.

**READY carries the work VMO**, as `Ready::HANDLE_RIGHTS` always said it
would: a megabyte the core owns, handed over `READ | TRANSFER` with the
core's port beside it. An object's description and a command buffer are
ranges of it, and the core writes neither.

**The description the core sent was empty, and that is the seam working.**
`MAKE_OBJ` said four thousand and ninety-six bytes and nothing about what a
resource is; the driver chose `PIPE_BUFFER`, `VIRGL_FORMAT_R8_UNORM` and
`VIRGL_BIND_VERTEX_BUFFER` itself. Those numbers come from Mesa's
`p_defines.h` and virglrenderer's `virgl_hw.h`, because virgl validates them
and `src/lib/drivers/virtio` defines none of them -- they are the renderer's language,
not virtio's. A core that wrote them would be a core the NVIDIA path could
not reuse.

**What is left of step 1** is `SUBMIT_3D` against the device. It needs a
command buffer, and a command buffer is virgl's own language: the encoder is
step 3b's. A program can open the render node now (§3.4), so what is missing
is no longer somewhere to write one from -- it is the encoder itself, and the
`EXECBUFFER` path under it. The core will not invent a command buffer, for
the reason §3.3 gives.

### 3.3 The render node's seam, and what Path B inherits (2026-09-18)

Decided when step 2 was started, because where the seam goes is the whole
of whether Path B reuses this or rewrites it. The customer asked for a
driver adapter that an NVIDIA driver can be loaded behind later; this is
what that means concretely, and what it honestly cannot mean.

**The seam is the control protocol, not the ioctls.** Three layers, and
only the middle one is new work per device:

1. **`/dev/dri/renderD<N>`, the node.** The kernel owns it, as it owns
   `/dev/dri/card<N>`. What lives here is device-*independent*: the inode,
   the GEM handle table, an object's lifetime and its reference counts,
   mapping an object into a process, and the generic ioctls (`VERSION`,
   `GEM_CLOSE`, and the `PRIME` pair when dmabuf lands). Linux keeps the
   same things in `drm_gem.c` for the same reason.

2. **`src/lib/proto/renderctl`, the control protocol.** The sibling of
   `src/lib/proto/displayctl`: a fixed-size little-endian message on one `Channel`
   per device, `HELLO`/`READY`/`REFUSED`, a doorbell each way, validation
   in the order fields are read, host-tested and fuzzed. Its messages are
   what *every* GPU can do -- make a context, allocate an object of so many
   bytes, give an object to a context, map it, submit a command buffer,
   move bytes in or out, wait for a fence -- and nothing narrower.

3. **The ring-3 driver**, which maps those onto its device. virtio-gpu maps
   `SUBMIT` onto `SUBMIT_3D`, `CREATE_OBJECT` onto `RESOURCE_CREATE_3D`,
   `WAIT` onto a fenced header. An NVIDIA driver would map the same
   messages onto a channel and a pushbuffer. Drivers stay in ring 3, which
   the decision of 2026-09-13 requires of this one as of any other.

**What is deliberately opaque, and why pretending otherwise would be
worse.** A command buffer's *contents* and an object's format and binding
words are the renderer's own language: virgl's TGSI and `PIPE_*`
enumerations here, NVIDIA's classes and methods there. There is no honest
portable abstraction over them -- Linux does not attempt one either, which
is why `DRM_IOCTL_VIRTGPU_*` and `DRM_IOCTL_NOUVEAU_*` are different
ioctls and why Mesa has a back end per driver. So they pass through as
bytes, and userspace learns which language to speak from the node's driver
*name*, exactly as it does on Linux. An abstraction that claimed to hide
this would be a lie that costs a rewrite the first time it is tested.

**What Path B therefore inherits, unmodified:** the node, the handle table
and object lifetime, the protocol's shape and its refusal rules -- including
"pages the device may still hold are never unpinned" (§2.2 of
`docs/DISPLAY.md`), which is a property of the *core*, not of virtio --
the fence and wait model, devmgr bring-up and the `START` message, the
ring-3 placement, and the gates. What it adds is one more implementation of
`src/lib/proto/renderctl` and one more driver-specific ioctl range.

**What it does not inherit** is the command encoder: `src/user/system/linux/compositor/virgl`
(step 3b) speaks virgl, and an NVIDIA path needs its own, or Mesa (§3a).
That is the same boundary Linux draws and it is drawn here on purpose.

### 3.4 Where step 2 stands (2026-09-18)

**The ABI table is written down**, in `src/lib/proto/linux-abi/src/virtgpu.rs`, from
`probe/virtgpu.c` as every other table in that crate is: every `virtgpu`
ioctl, parameter, context and execbuffer flag, and twelve structure layouts,
printed from `<drm/virtgpu_drm.h>` at both widths and pinned by tests. It is
probed apart from `drm.c` because each test module requires every line of
the probe file it reads to be claimed by that module, and because a driver's
header is not DRM's. Nothing here has a width: these structures carry their
user pointers as `__u64`, so both probe files are identical and a test says
so, rather than listing exceptions the way `drm::Version` must.

**The renderer outlives its proof.** `kernel::render` used to prove a
context and an object and let its task return; a node's ioctls arrive later,
from whichever process opened it, so a `Renderer` is now published and
served -- replies taken off the channel, judged by the session, left where
the request waiting for one will find them. The proof runs before it is
published, so nothing can open the node while the core is still asking its
own questions.

**That cost the display, and the fix is worth writing down.** A card has two
conversations on one thread and the driver served the render one in a loop
that ended only when the core closed the channel -- which the core used to
do, by returning. With the renderer kept alive the display was never served
again and the compositor timed out. The driver now takes render requests
from its own event loop, between the display's: the device runs one command
at a time, so a render command goes only when the pipeline is idle and
nothing is in flight. Underneath was an older bug: a command waiting on the
device dropped every packet that was not its interrupt, while `wait_async`
delivers one once, so a display message arriving during the render proof was
a wakeup nobody would ever hear again. Such packets are kept and given back.

**The node is in `/dev/dri`**, and a program opens it:

    render: renderD128 virtio_gpu 3d 1 capsets 0x2

It takes any number of opens, unlike `card<N>`, which takes one: a render
node exists so that a client can render without being the display's master,
which is why §3.3 puts the handle table at the open. `DRM_IOCTL_VERSION`
reports the driver's own name, from its HELLO, because that is what
userspace picks a back end by; `VIRTGPU_GETPARAM` is answered from the HELLO
too, so neither call wakes the driver. The two display boots are a pair and
each is the other's control: with `--gl` the node must be there and name its
driver, without it there is none and the program must say so.

**What is left, and what is in the way of each.**

* **The handle table, `RESOURCE_CREATE` and `RESOURCE_INFO`.** *Done.* The
  open holds the table, which is why a render node takes any number of opens:
  two programs' `bo_handle` 1 are different objects. The core holds the
  object-id counter and hands ids out in turn, so a driver's late reply names
  an object that is gone rather than one just made; an id the device would
  not let go of is stepped over for good. A resource's `res_handle` is the
  core's object id, because the driver names the device's resource by it.
  The judged `--gl` boot now makes one and reads it back, which is the first
  thing in this path that costs the device a message rather than being
  answered from the driver's HELLO.

  Two things it does not do yet, neither of them in the way of step 3b:
  **a handle released on purpose**, because `DRM_IOCTL_GEM_CLOSE` is not in
  `src/lib/proto/linux-abi`, which takes every number from a committed probe -- adding
  it means running `probe/drm.sh` on a Linux host, and until then an open's
  objects go when the open does; and **a resource's shape**, because
  `MAKE_OBJ`'s description carries target, format and bind and nothing else,
  so every resource is a buffer. Carrying width, height and stride is a
  change to the description both sides read, and belongs with the transfers
  that would be the first to need it.
* **`MAP`, `GET_CAPS`, the transfers and `EXECBUFFER`.** *Done, 2026-09-19;
  §3.7 says how.*
* **`WAIT` that waits, and `GEM_CLOSE`.** Neither is in the way of a frame;
  §3.7 says what each is waiting for.
* **Scanout of a 3D resource**, which is the other half of this row.

### 3.5 The build plan for steps 2 and 3b, researched (2026-09-19)

The customer chose Path A first, then the AV1 wallpaper (§3.6). What the
software renderer can give was reached on 2026-09-19: a video frame in the
guest is 46 ms, of which 37 is the renderer's own arithmetic and the rest a
serial copy (`docs/COMPOSITOR-DAMAGE-HANDOFF.md` §2.7). 60 fps (16.7 ms) is
below that floor; only the GPU drawing the pixels reaches it. This is the
path, in the order each piece can be *tested*, with the wire details already
looked up so the next session does not repeat the reading. Reference sources
are cloned at `~/.local/share/ferrix/virgl-ref`: virglrenderer 1.2.0
(`src/virgl_protocol.h`, `src/virgl_hw.h`) and Mesa's virgl driver
(`src/gallium/drivers/virgl/virgl_encode.c`, the encoder to copy). QEMU's
`hw/display/virtio-gpu-virgl.c` at `~/Documents/qemu/qemu` shows the host
side of each command.

**rav1d builds for the target already** (proven 2026-09-19): a static musl
binary against the repo toolchain, C-shaped API at `rav1d::src::lib::dav1d_*`,
`default-features=false, features=["bitdepth_8"]`, BSD-2-Clause (already on
deny.toml's allow-list). That is what §3.6 stands on and is why AV1 waits
without risk.

The pieces, each landable on its own:

1. **`CONTEXT_INIT` and `GET_CAPS` through the node** (step 2 tail, ~3 pts).
   `context_init` in `src/user/system/native/drivers/display/virtio-gpu` maps onto `MAKE_CTX` with a capset; the node
   answers `VIRTGPU_GETPARAM` from the HELLO already. `GET_CAPS` needs the
   core to carry the capset *bytes*: add a `Capset` message to
   `src/lib/proto/renderctl` (core asks, driver runs `GetCapset`, bytes ride a work
   VMO range as a description does). Testable by the `--gl` display gate
   reading a capset back, as it reads a resource now.

2. **`MAP`, and an object's backing** (step 2, ~5 pts). This is the seam
   question §3.4 flags: `MakeObject` carries a size and no backing. Decision
   to make: the object's guest pages come from the *work VMO's* allocator
   extended, or a VMO per object. Recommend a VMO per mappable object owned
   by the core (like the card's one big VMO but per resource), attached to
   the device with `ResourceAttachBacking` (the driver already pins for the
   display's `attach`). The node's `MAP` returns an mmap offset into that
   VMO exactly as `map_dumb` does (`src/kernel/src/interfaces/display/drm.rs:789`, and
   `devfs::mapping` at `src/kernel/src/fs/devfs.rs:938` is the hook — the render
   inode needs its own `mapping()` returning the object's VMO). Testable:
   a program creates a resource, maps it, writes a byte, reads it back.

3. **`src/user/system/linux/compositor/virgl`, the command encoder** (step 3b core, ~8 pts). A new
   crate, pure Rust, encoding virgl's command stream into a byte buffer the
   compositor hands to `EXECBUFFER`. The header is
   `VIRGL_CMD0(cmd,obj,len) = cmd | obj<<8 | len<<16`, len in dwords
   (`virgl_protocol.h:141`). The commands the compositor needs, with their
   dword layouts already in `virgl_protocol.h` and Mesa's writer in
   `virgl_encode.c`:
   * `CREATE_OBJECT`/`BIND_OBJECT`/`DESTROY_OBJECT` for blend, rasterizer,
     DSA, vertex-elements, sampler-state, sampler-view, surface, shader
     (object types in `enum virgl_object_type`).
   * `SET_FRAMEBUFFER_STATE`, `SET_VIEWPORT_STATE`, `SET_VERTEX_BUFFERS`,
     `SET_SAMPLER_VIEWS`, `BIND_SAMPLER_STATES`, `BIND_SHADER`,
     `SET_CONSTANT_BUFFER`, `CLEAR`, `DRAW_VBO` (layouts at the matching
     `VIRGL_*` defines; Mesa's `virgl_encoder_draw_vbo` etc. are the model).
   * `TRANSFER3D`/`RESOURCE_INLINE_WRITE` to upload a client's `wl_shm`
     damaged rectangles as textures.
   Shaders are TGSI *text* (Mesa's `virgl_encode_shader_state` dumps
   `tgsi_dump_str`), uploaded in a `CREATE_OBJECT VIRGL_OBJECT_SHADER`.
   Hyprland's effects are about eight shaders: the two blur kernels,
   `blurprepare`, `blurFinish`, the rounded-texture sampler, the border
   gradient, the shadow. Write them as TGSI by hand or translate the GLSL.
   Everything here is host-testable without a device by asserting the byte
   stream, the way the software renderer's images are golden.

4. **`EXECBUFFER`, transfers and `WAIT` through the node** (step 2 + 3b glue,
   ~4 pts). The node's `EXECBUFFER` copies the command bytes into a work VMO
   range and sends `SUBMIT`; `SUBMIT`/`WAIT` are already in `src/lib/proto/renderctl`
   and the session. The driver's `run_command`/`serve_render` loop already
   runs one device command at a time between the display's — `Submit3d`
   carries the bytes (`src/lib/drivers/virtio::gpu::Command::Submit3d`). `WAIT` maps
   onto a fenced header (FLAG_FENCE). Fences are encoded but the driver does
   not offer the `FENCES` feature yet; offer it and wire `on_interrupt`'s
   fence to `WAITED`.

5. **A renderer trait in `src/user/system/linux/compositor/render`** (step 3b integration, ~5 pts).
   `render_onto` and the `Canvas` operations become a trait with two impls:
   the software one that exists, and a `virgl` one that emits commands. The
   fallback is mandatory (roadmap) — the compositor is never GPU-only, and
   the software images stay the byte-exact reference. `hyprix` opens
   `/dev/dri/renderD128` when it is there and falls back when it is not.

6. **Scanout of a 3D resource** (step 2, ~4 pts). `SET_SCANOUT` on the 3D
   resource the compositor drew into, so the finished frame never leaves the
   GPU — no per-frame `TRANSFER_TO_HOST` at all, which is the 5.5 ms the
   software path spends in `DIRTYFB`. QEMU's `virgl_cmd_set_scanout`
   (`dpy_gl_scanout_texture`) is the host side; the display core's `scanout`
   path takes a buffer id, and a 3D resource id has to reach it. Needs the
   card and render conversations, today separate, to name one resource.

7. **The host half** (step 5, ~5 pts, mostly done). `--gl` boots; what is
   left is judging a GPU frame — `screendump` cannot read a GL console
   (§3.1), so judge from inside the guest with `src/user/system/linux/compositor/shot`, or check
   whether a newer QEMU grew a GL screendump.

Order 1→2→3→4→6→5, with 5's renderer trait landing beside 3. Each of 1, 2,
4, 6 is a display-gate boot that proves the new call; 3 and 5 are
host-tested byte-for-byte. The whole is ~30 points and cannot be verified
piecemeal below the level of these seven — a virgl command means nothing
until `EXECBUFFER` carries it and a scanout shows it, so land 1–2 first to
have somewhere to submit from.

### 3.6 The AV1 wallpaper, after the GPU (2026-09-19)

The customer asked (2026-09-19) that the wallpaper load a real container
rather than run-length `.fxvid` frames, which for a long clip run to
gigabytes. This is now **AV1 in IVF, decoded by rav1d in the guest**:

* **Host side** (`tools/common/xtask/src/wallpaper.rs`): `cargo xtask wallpapers` asks
  `ffmpeg` for 8-bit 4:2:0 AV1 in an IVF container (`libaom-av1`, a bounded
  CRF and fast encoding). The image carries the resulting `.ivf`, which is
  megabytes rather than hundreds of megabytes of raw frames.
* **Guest side** (`src/user/system/linux/compositor/pattern`): a small IVF demuxer feeds temporal
  units to rav1d, converts the decoder's 8-bit I420 pictures to XRGB8888,
  and retains compressed packets plus one decoded frame. It validates the
  stream before startup, reopens the decoder at a loop boundary, and damages
  only decoded rows whose pixels changed.
* `cargo xtask test-video` carries a checked-in four-frame IVF test pattern,
  so its QEMU gate requires neither `ffmpeg` nor an AV1 encoder. rav1d is
  BSD-2-Clause, pure Rust with assembly disabled, and builds for the static
  target. Decode cost remains CPU work, but a wallpaper under an opaque
  window pauses with its frame callbacks as before.

### 3.7 Where steps 2 and 3b stand (2026-09-19)

Pieces 1, 2 and 4 of §3.5 are built, and the first of piece 3. A program in
the image now does on the GPU what the whole path exists for, and the judged
`--gl` boot holds it to the answer:

    render: renderD128 virtio_gpu 3d 1 capsets 0x4 object 1/1073741824 of
    4096 bytes caps 1405 bytes v2 moved 4096 bytes drew 0xff0000 0x00ff00
    0xff0000

**An object's backing is a VMO of its own** (`src/lib/proto/renderctl` version 2). The
core makes it, keeps it -- it is what a program maps -- and hands it to the
driver with `MAKE_OBJ`; the driver pins it, attaches it, and keeps the pin
until the device has said the resource is gone. An object the device would
not let go of keeps its pages through that pin, so the rule that a device's
pages are never unpinned needs nothing remembered in the core. `mmap` reaches
it through a new `Inode::mapping_at`: a render node's offsets are *names* --
`VIRTGPU_MAP` answers `handle << 32` -- where every other file's are places,
and the default keeps every other file as it was.

**The two conversations of a card no longer share numbers.** The display
core counts buffers from 1 and the render core counted objects from 1, and
the device names both from one set: render object 1 *was* display buffer 1.
Nothing had collided only because nothing had lived long enough. Render
objects now count from 2^30. The number cannot be translated on the way down
instead, because it is the `res_handle` a program writes into its command
streams.

**An open has a context of its own**, made the first time it is needed, as
Linux makes one for a device without `CONTEXT_INIT`; it goes after the
open's objects have, and what the driver's channel has no room for is asked
for as replies make room, so closing a program with many textures leaks
none of them. `GET_CAPS` is answered from bytes the core fetched once, before
publishing the renderer, so it wakes nobody; the driver now names
`CAPSET_VIRGL2` when the device has it.

**`TRANSFER` names a box, a level and two strides**, which is as much as any
GPU's texture has. `SUBMIT` was already in the protocol: the node copies a
stream into a 64 KiB slot of the work VMO and the driver copies it on into
the command area. The driver runs each to completion in the device's one
command slot, between the display's commands, as it did a `MAKE_OBJ`.

**What `WAIT` does not do.** The driver offers no fences, so nothing says
when the GPU has *finished* a stream rather than taken it. A transfer from
the device is ordered after every stream before it, which is the one place
this path needs to know, so `WAIT` answers at once and says why. QEMU 9.2
polls fences on a 10 ms timer, so a frame that waited on one would lose more
than it gained; a frame is paced by the display's flip, not by a fence.

**`src/user/system/linux/compositor/virgl` writes the streams**, host-tested word for word against
`virgl_protocol.h`, and its shaders are TGSI text. The text is what no
word-for-word test can judge, and a guest boot is a minute and a black
screen per mistake, so the crate also speaks to `virgl_test_server` --
virglrenderer 1.2.0 with a socket where QEMU would be, installed on the gate
host as `virgl-server` -- and `tests/host.rs` runs the same streams on the
host's GL in under a second. A host without it skips them and says so.

**`test-display --gl` passes**, judged by what the render node did; the
screen's pixels are the 2D boot's to judge, because `screendump` cannot read
a GL console (§3.1).

**The shaders are GLSL, and Mesa compiles them -- on the gate host**
(pieces 3 and 5, 2026-09-19). virgl carries a shader as TGSI assembly, and
there is no Mesa on Ferrix to make it. So `src/user/system/linux/compositor/virgl/shaders/` is
GLSL, `tools/regenerate.sh` compiles each with Mesa's own virgl driver run
against the test server (`GALLIUM_DRIVER=virpipe`, `VIRGL_DEBUG=tgsi`), and
the TGSI text that driver sends is what is committed under `src/tgsi/`: the
text a Linux guest's Mesa would have sent for the same shader. One vertex
shader serves every fragment shader, which needed one thing a person would
not guess: a linker packs two varyings to suit whichever of them a fragment
shader reads, so the texture coordinate and the pixel's place travel in one
`vec4`, packed by hand. Eight fragment shaders: a solid colour and a surface,
each inside a rounded rectangle cut by the superellipse the software
renderer cuts; a gradient read from the ramp `src/user/system/linux/compositor/render` builds; the
shadow's falloff; and Hyprland's four blur passes.

**The frame is drawn through a trait, `compositor_render::Painter`**, which
is the ten operations `render_onto` calls. `Canvas` implements it by calling
itself, so not one byte of any expected image moved. `gpu::Canvas` implements
it by writing streams for a `compositor_virgl::Device`, which is the render
node in a guest and the test server on a host. Damage is geometry, not a
scissor: a quad is an operation's rectangle cut by each rectangle of the
damage, and a shape is cut in the fragment shader from the pixel's own
place. A surface is a texture kept by the surface's *name* -- a client that
draws into two buffers in turn is one surface and two addresses -- and what
is moved each frame is the part under the damage. The blur is a pyramid of
textures anchored to the canvas, so a level's pixel is always the same four
of the level above whatever region is blurred, which is what the software
blur snaps its region to a lattice to get.

**It is the same picture.** `render/src/gpu/tests.rs` draws five scenes with
both painters -- plain, decorated, a blur of the backdrop, a blur of the
frame so far, and a second frame drawn only inside its damage -- and on the
gate host no channel of any of them is more than two steps from the software
frame, corners and blur included. The last also holds the GPU's frame to
being byte-identical outside the damage.

**`hyprix` draws on the GPU when there is one** (piece 5's other half,
2026-09-19). `--renderer auto`, the default, takes the card's render node
when its driver's name is `virtio_gpu` -- the name is what a back end is
picked by, here as on Linux, which is also what keeps a development host's
own render node from being taken -- and the software renderer otherwise,
saying which and why. `software` and `gpu` force either, and `vtest` starts
virglrenderer's test server on the host, which is how the whole compositor
is tested on a GPU without a guest: `hyprix/tests/two_clients.rs` runs real
Wayland clients against it and holds the frame to the expected image the
software path is held to. A surface's texture is kept by the *connection's
serial* and the surface's id, since a slot's place changes when another
client goes. A GPU that fails is not a screen that fails: the screen's
watch forgets what it saw, the next frame is a whole one, and it is drawn
in software from then on.

**In the guest it is the same picture.** `cargo xtask test-compositor --gl`
boots the decorated pair on `virtio-gpu-gl`, requires the compositor to say
it draws on the GPU and never that it gave it up, and has the guest judge
its own frame, because QEMU cannot (§3.1): `/bin/shot 0 <image>` takes a
screenshot through `zwlr_screencopy_v1`, reads the expected image off the
guest's filesystem, and says over the serial port how far apart they are:

    shot: 1024x768 against /etc/expected.xrle: 0 channels more than 3
    apart, the furthest 2

### 3.8 The screen shows what was drawn, where it was drawn (2026-09-19)

Piece 6, and with it §3.5 is built. The frame was being *fetched*: read out
of the device into the dumb buffer and handed straight back to it by
`DIRTYFB`, a megabyte or eight crossing between host and guest twice a
frame for pixels the host already had.

**The seam is a second way of making a display buffer.** `src/lib/proto/displayctl`
is version 3, with `ATTACH_OBJ` beside ATTACH: a buffer whose pixels are a
resource the *render* conversation made, named by its object id, with no
range of the card VMO, nothing pinned, and a flush that sends nothing --
QEMU's `virgl_cmd_resource_flush` tells the display what changed and
transfers not one pixel. The driver's pipeline had been naming the device's
resource by the buffer id; it now keeps what the device calls each buffer
beside its shape, which is what lets the two kinds live side by side.

**The two nodes meet through PRIME, as they do on Linux.**
`DRM_IOCTL_PRIME_HANDLE_TO_FD` on the render node answers a descriptor
holding the object -- a dmabuf in every way that matters here, though it is
not one a second process or a second device could read -- and
`DRM_IOCTL_PRIME_FD_TO_HANDLE` on the card takes it, `ATTACH_OBJ`s it and
gives it a handle. `ADDFB2`, `SETCRTC`, the page flip and `DIRTYFB` know
nothing about any of it. A render object is refcounted now, so the
descriptor and the card's handle each hold it: a program may close the
handle it drew through, or the render node itself, while the screen shows
what it drew.

**A dumb buffer's offset is an `Option` for the reason that matters**: an
imported object has no range in the card VMO, and `MODE_MAP_DUMB` of one
answers nothing rather than an offset, which would have been some other
buffer's pixels.

**`hyprix` adopts it when both ends can.** The renderer exports the texture
it draws into, the backend imports it and sets the mode to it, and after
that a frame is `DIRTYFB` and nothing else. A backend that cannot, and a
renderer with nothing to export -- the test server, which has no card --
leave the frame to be fetched, which is right and only slower. The two
readers that want the pixels on this side, a screenshot and a dumped PPM,
fetch the frame when they ask rather than every frame for a reader who is
usually not there; a night-light still fetches every frame, because its
ramps are applied to pixels on the way out and the fetched ones are the only
pixels this compositor can reach.

**`VIRGL_RESOURCE_Y_0_TOP` is not set, and this is worth writing down.** It
says which way up a texture's rows are, and *both* the transfers and the
scanout honour it, while what the renderer draws is unaffected by it -- so
setting it turned the screen and the readback upside down together. The
guest's own screenshot caught the readback; nothing in the gate could have
caught the screen, because a GL console cannot be dumped. What caught it was
looking: `src/user/system/linux/compositor/hyprix/probe/vncshot.py` grabs a frame from the VNC
server beside `egl-headless`, which is the screen as a person sees it.

**What it is worth**, 1920x1080 under KVM on the gate host, behind a blurred
translucent terminal:

| | a frame, mean | slowest of ten |
|---|---|---|
| the software renderer | 39 ms | 46 ms |
| the GPU, frame fetched | 22 ms | 28 ms |
| the GPU, screen shown it | **12 ms** | 17 ms |

60 fps is 16.7 ms. The goal of `docs/GPU.md` is met on this host.

**These three cannot be reproduced as they stand, and what they measured is
worth naming.** The wallpaper was the *run-length* `.fxvid` one -- a whole
1920x1080 frame decoded by the client and handed over every frame -- which
§3.6's AV1 work has since replaced with `.ivf`, so `--wallpaper` no longer
matches the file these were taken on. Under AV1 the client decodes with
rav1d instead, which changes the CPU side of the scene; the upload is the
same size, so the shape holds -- the batching table below is on the AV1
scene and lands in the same place -- but this row is not that run. Three of
the four numbers a later run of this comparison produced were also thrown
away because another session was booting throughout: on a contended host the
same code measured 18 ms and 22 ms in the same hour, which is several times
the effect being looked for. A frame time from this scene is worth having
only from an unloaded machine.

**Where a GPU frame's time actually goes**, measured in the guest with
temporary counters on the scene above, is the more useful number, because it
is the one that says what to fix next:

| | a frame |
|---|---|
| moving client pixels into textures | 5.4--6.3 ms, 2 uploads, **13.6 MB** |
| submitting commands | 2.8--3.5 ms, 3 submissions, **6 KB** of commands |
| reading the frame back | 0 ms -- the screen is shown it |

Six kilobytes of drawing a frame: the GPU itself is idle nearly all of it.
What the frame costs is *moving client pixels* -- every `wl_shm` surface
memcpy'd into a pinned backing and then transferred, at roughly 800 MB/s for
this scene -- and round trips, each submission about 0.95 ms through the
render core, the driver, the virtio queue and virglrenderer, with the driver
running one device command at a time. The first row is what step 4 is for: a
client rendering into a GPU buffer the compositor samples costs nothing to
move at all.

**Batching a frame's drawing into one submission** is the second row's
answer, and it was measured rather than reasoned about. The scene above with
the AV1 wallpaper, 1920x1080 under KVM, each run two and a half minutes on
an otherwise idle host, the first twenty reports dropped as warm-up:

| | run 1 | run 2 |
|---|---|---|
| a frame before, three submissions | 12.35 ms mean, 12.18 median | 12.03 ms mean, 11.65 median |
| a frame after, one submission | **10.73 ms** mean, 9.88 median | **10.94 ms** mean, 10.58 median |

About 1.3 ms a frame, repeatably, which is close to the two round trips the
change removes at roughly 0.95 ms each. It is a small number honestly come
by: what is left is the first row, and no amount of batching touches that.

**A handle can be let go of now** (2026-09-19). `probe/drm.sh` was run on
this host -- it needs a Linux host with the UAPI headers, an ARM cross
compiler and `qemu-arm`, all of which the gate host has -- and
`DRM_IOCTL_GEM_CLOSE` and the two PRIME calls came out of it at both widths,
identical, as structures carrying no pointer are. The render node answers
`GEM_CLOSE`, and the renderer keeps at most a handful of textures for the
next surface of their size and lets the rest go, so a session of windows
resized by hand no longer ends in a renderer with no objects left.

### 3.9 What a person watching the desktop gets, and the default (2026-09-23)

The customer called the desktop's performance abysmal. The desktop they
watch is `cargo xtask remote-desktop` with `run-compositor --clipboard`: a
served screen, and no `--gl` -- so every frame above was the software
renderer's, on a GPU that was there all along. Measured as they see it,
with `src/user/system/linux/compositor/hyprix/probe/vncbench.py`: a viewer that keeps one update
request outstanding, as TigerVNC does, while it moves the pointer in a
circle sixty times a second, and reports what arrived. KVM, four
processors, 1920x1080, the configuration `run-compositor` writes (a
translucent terminal), twenty seconds a run, on the gate host:

| scene | the guest's frames | slowest, per second | viewer's updates | pointer lag, median / p95 |
|---|---|---|---|---|
| still wallpaper, software | 61 a second, 1-2 ms | 1.5-2.7 ms | 33 a second | 10 / 17 ms |
| video wallpaper, software | 38 a second, 600-700 ms of drawing a second | 60-95 ms | 21-23 a second | 20 / 46 ms |
| video wallpaper, GPU | 61 a second, 340-500 ms of drawing a second | 7-17 ms | 24-33 a second | 14-21 / 25-53 ms |

The pointer lag is on the loopback, before any network; the viewer's
update rate stops at 33 because QEMU's VNC server looks for changes every
30 ms. In software the four processors were 78-91% busy each through the
video run -- the decoder and the blur between them -- where on the GPU the
blur is the host's.

**So a served screen gets the 3D card without being asked**
(`tools/common/xtask/src/window.rs`, `watched_gl`). Where it can be had: a QEMU on
`PATH` with `virtio-gpu-gl-pci` -- the first that has it, which on the gate
host is the distribution's `/usr/bin` behind a source build without it --
and a render node for `egl-headless`, the first whose driver is not
NVIDIA's proprietary one. A window on the host keeps the 2D card unless
`--gl` asks, because GL in a local window is a backend nobody has proven
here. `--no-gl` is the software renderer.

What is left, in the order a person feels it:

* **The pointer is part of the frame.** Every movement is a frame drawn, a
  flush waited for, and an update the viewer can have only when VNC next
  looks. virtio-gpu has a queue for exactly this -- `UPDATE_CURSOR` and
  `MOVE_CURSOR`, a cursor plane -- and QEMU hands such a cursor to a VNC
  viewer as a shape the viewer draws where its own mouse is. *Done, §3.10.*
* **One command at a time, each waited for.** An upload, a submission and
  a flush are each a round trip through the render or display core, the
  driver, the device and back. seL4's device driver framework (sDDF) and
  Genode's GPU session are the reference for the other way: descriptors in
  shared rings, a whole queue drained into the virtqueue behind one
  doorbell, completions taken in batches, and a client that says "run the
  buffer at this offset" rather than handing the bytes over. *Done,
  §3.11.*
* **Client pixels are copied in the guest** into a texture's backing before
  the device moves them -- the 13.6 MB a frame of §3.8's table. sDDF's GPU
  class makes the client's own memory the resource's backing instead.

### 3.10 The pointer on a plane of its own (2026-09-23)

The first item §3.9 left. A screen whose card has a cursor plane shows the
pointer on it, and its frames neither draw the pointer nor count its moves as
damage (`src/user/system/linux/compositor/hyprix/src/plane.rs`). Top to bottom:

* **`DRM_IOCTL_MODE_CURSOR` and `CURSOR2`** on `card<N>`, as Linux's
  `drm_mode_cursor_universal` answers them, and `DRM_CAP_CURSOR_WIDTH` and
  `HEIGHT` of 64, which is the one size QEMU's host shows. The image is an
  ordinary dumb buffer, as it is on Linux's `virtio_gpu`, and the numbers
  come from `src/lib/proto/linux-abi`'s probe like every other.
* **`CURSOR` and `MOVE`** in `src/lib/proto/displayctl`, version 4 (`docs/DISPLAY.md`
  §2.2). An image waits for its pixels to reach the host, in the flushes'
  line; a move waits for nothing, and a core whose channel is full keeps
  only a scanout's newest place for when there is room. A driver says in
  its HELLO whether its card has a plane: the DK1's LTDC says no, is sent
  neither, and its card answers the cursor calls with `ENXIO` and has no
  cursor size, so a compositor there draws the pointer itself.
* **The cursor queue** in `src/lib/drivers/display/virtio-gpu`, run as seL4's device driver
  framework runs its queues -- commands in slots of a page of their own,
  what is owed posted behind one doorbell, completions taken back when the
  next command is posted, no interrupt at all -- and the newest place of a
  scanout, never a stale one, when every slot is taken. It was the first
  part of the driver to run that way; the control queue followed in §3.11.

Measured the way §3.9 measures, on the 3D card under KVM at 1920x1080, the
viewer circling the pointer sixty times a second for 25 seconds: the
compositor drew **4 frames** where it drew 61 a second, the viewer was sent
**no framebuffer updates at all** where it was sent 33 a second, and it was
sent the pointer's shape -- after which the pointer a person sees is drawn by
their own viewer, where their own mouse is, with no lag but the one between
the mouse and the viewer. In the guest, every process sat at 0% of a
processor through the sweep.

`cargo xtask test-compositor --boot cursor` holds it to that: the frame is
the two windows and nothing else before and after the pointer is swept
through four hundred places, the sweep costs at most ten frames (it cost 0),
and a VNC viewer -- `tools/common/xtask/src/vnc.rs`, which speaks as much of RFB as asking
for `RichCursor` takes -- is handed exactly the arrow the pointer boot's
picture has drawn into it, pixel for pixel, mask and hotspot. Both halves
were shown to fire: with every pointer move owing a frame the boot fails on
"sweeping the pointer cost 63 frames", and with the plane's image moved one
pixel it fails on "the viewer's pointer shows nothing at (0, 0)". The pointer
boot keeps judging the arrow drawn into the frame, with Hyprland's
`cursor:no_hardware_cursors = 1`, which is also how a person asks for it.

A client's own cursor larger than the plane, a screen with no plane, and the
headless backend draw the pointer into the frame as before, and a drag's icon
is still drawn: it follows the pointer, so a drag owes frames.

### 3.11 Commands in flight, and a frame that waits once (2026-09-24)

§3.9's second item. A frame on the GPU was an upload or two, a command
stream and a flush, and each was a round trip of its own: the program's
ioctl, the render or display core, a channel message to the driver, the
virtqueue, virglrenderer, the interrupt, the reply, the core, the program --
about 0.95 ms each, with the driver taking one command at a time and the
program waiting for each. Now only the flush is waited for.

**The driver keeps eight commands in flight** (`src/lib/drivers/display/virtio-gpu`). The
command area starts with slots, each a request and its response in a
quarter of a page; a command too long for a slot -- a backing list of many
pages, a capability set -- takes the one large place after them, as every
command did when there was only one. `Driver::post` writes a command into a
free slot and publishes it without ringing the doorbell, `Driver::kick`
rings it once for everything posted since, and `Driver::take_done` hands the
completions back in whatever order the device made them, each with the tag
it was posted with. That is seL4's device driver framework's shape again,
the cursor queue's of §3.10, on the queue that carries the frames.

**A command stream is run where the core wrote it**, Genode's "execute the
buffer at this offset". The driver pins the render core's work VMO
read-only for the device when READY hands it over, and a `SUBMIT_3D` is its
header in a slot followed, in the same chain, by the addresses of the pages
the stream lies in. The stream used to be read into this process and copied
a byte at a time into the command area; now neither happens. The core holds
a stream's slot until the driver answers, which is after the device is done
with it, so the device never reads a slot that is being rewritten. Shown to
fire: with the first page's address shifted by four bytes, virglrenderer
reports an illegal command buffer and the render node's probe draws black
where it draws red.

**A request's order is kept across the two conversations.** `src/user/system/native/drivers/display/virtio-gpu`
reads the render core's channel into a backlog and puts it on the device in
order, uploads and streams at once -- one slot is always left for the
display -- and anything else (a context, an object, a capability set) only
once nothing is in flight, as before, since those are rare and several are
more than one command. A display request is given to the pipeline only
when every render request read before it is on the device, and the render
channel is read again after each display request is taken: a stream written
before a flush is in its channel by the time the flush can be read, so the
device draws before it shows.

**An upload and a stream return when they are sent**, as they do on Linux
(`src/kernel/src/interfaces/render`). `VIRTGPU_TRANSFER_TO_HOST` and `VIRTGPU_EXECBUFFER`
send their request and return; the reply is taken by the renderer's task,
which gives the stream's slot back, and `VIRTGPU_WAIT` -- which answered at
once, since nothing was ever outstanding -- now waits until everything its
open sent before it has been answered. A program writes a backing again only
after a wait, which is Linux's contract; `src/user/system/linux/compositor/drm`'s render device
keeps it, and waits only when an upload from that texture may still be on
its way. A transfer *from* the device still waits for its bytes, since its
caller asked in order to read them, and is ordered behind the object's
upload. A refused upload or stream goes unheard, as on Linux, and the core
says so once on the console.

What it is worth is not measured yet: the gate host was never idle the
evening this landed, and a frame time from a contended host is noise
(§3.8). It is owed as §3.9 measures it -- the AV1 wallpaper at 1920x1080
on the 3D card under KVM, main and this change run alternately.

What is left of §3.9 is the third item: a client's pixels are still copied
into a texture's backing in the guest before the device moves them -- planned
in §3.13.

### 3.12 The 3D card on Windows needs a patched QEMU (2026-09-27)

Every `--gl` boot on Windows -- `run-compositor --everything` among them --
ended as the loader handed over, with QEMU exiting `0xC0000005`. The
released Windows build (Stefan Weil's 11.1.0) is stripped, so the crash was
found in a build of the same release with symbols, under gdb:

```
#0  0x0000000000000000 in ?? ()
#1  surface_gl_destroy_texture ()      ui/console-gl.c:187
#2  gd_gl_area_switch ()               ui/gtk-gl-area.c:196
#4  qemu_console_set_surface ()        ui/console.c:795
#5  virtio_gpu_disable_scanout ()      hw/display/virtio-gpu.c:440
#7  virtio_gpu_gl_reset ()             hw/display/virtio-gpu-gl.c:102
#10 virtio_pci_reset ()                a status write of 0, in a vCPU thread
```

A reset of `virtio-gpu-gl-pci` -- OVMF's, at `ExitBootServices` -- disables
the scanouts in the vCPU thread that wrote the status register. The display
then deletes a texture with its GL context, which only the main thread can
make current: WGL refuses it in another thread, no context is current, and
epoxy's dispatch for `glDeleteTextures` is NULL. On Linux the same call
fails quietly, which is why no Linux host ever saw it. SDL's GL window
crashes the same way. The bug is QEMU's since bd9258917f ("virtio-gpu:
Destroy virgl resources on virtio-gpu reset"), in 11.1.0.

The fix is `tools/common/data/qemu/0001-*.patch`: the reset's bottom half, which
already runs in the main thread, disables the GL scanouts. On Windows,
`tools/common/fetch/fetch-qemu-windows.sh` builds 11.1.0 with it into
`~/.local/share/ferrix/qemu`, which xtask takes before `PATH`. With it the
`--everything` desktop draws on the GPU on Windows. Current QEMU master
(v11.1.0-1860) is not the base: built there, devmgr never reported, with
or without the 3D card.

"Fails quietly" was wrong for Linux (2026-10-03). The vCPU thread's
`gtk_gl_area_make_current` binds the GL area's EGL context there, and the
main thread can never make it current again: GDK warns `eglMakeCurrent
failed` four times a frame (`tools/common/xtask/src/noise.rs` hides those),
and on a Wayland host the window's menu bar and tab bar turn transparent or
fill with stale pixels from the moment the kernel's driver resets the card.
`0003-*.patch` is 0001 carried to 10.2.1, in `series-10.2.1`, so the
Linux build `tools/common/fetch/fetch-qemu-linux.sh` makes has it too: 47
warnings in a two-minute boot without it, none with it. `0004-*.patch`
fixes the other thing that window got wrong with `gl=on`: a GtkGLArea has
no window of its own, so the blank cursor QEMU sets over the guest was set
on the whole window, and the host pointer vanished over the menu bar.

### 3.13 Client pages as texture backing: the plan (2026-10-07)

The third item of §3.9, and the first row of §3.8's table: a `wl_shm`
surface on the GPU path is copied twice in the guest before the device
moves it -- hyprix copies the client's rows into a texture's backing
(`src/user/system/linux/compositor/drm/src/device.rs`, `upload`), and the
device then reads that backing. The backing exists only to be copied into.
The client's own pages can be the backing instead, as sDDF's GPU class
makes a client's memory the resource's backing, and the transfer then
reads the pixels where the client drew them.

**The ABI is Linux's, in two steps it already has.** A client's pool is a
`memfd`, and Linux turns a `memfd`'s pages into a buffer a device can be
given with `/dev/udmabuf` (`UDMABUF_CREATE`: the memfd, an offset and a
size, page-aligned, the memfd sealed against shrinking) and imports such a
buffer into a DRM node with `DRM_IOCTL_PRIME_FD_TO_HANDLE`. So:

1. **`/dev/udmabuf`** (kernel, `src/kernel/src/fs/devfs.rs` and a new
   `interfaces/udmabuf`): `UDMABUF_CREATE` answers a descriptor that holds
   the memfd's VMO and the range, refusing a memfd without `F_SEAL_SHRINK`
   (`EINVAL`, as Linux does) -- the seal is what keeps a client from
   truncating pages the device is reading. `UDMABUF_CREATE_LIST` is not
   needed and is not answered.
2. **`PRIME_FD_TO_HANDLE` on the render node** takes such a descriptor and
   gives a handle to an object whose backing is that range: the render core
   hands the driver the memfd's VMO and the range where it hands an
   anonymous VMO today (`Renderer::make_object`), and the driver pins those
   pages for the device. `RESOURCE_CREATE` with that handle as `bo_handle`
   makes the texture on it -- the field Linux has for this, which the node
   refuses today because nothing made such an object.
3. **hyprix** makes one such texture per buffer, keyed by the pool and the
   buffer's place in it, and an upload becomes a `TRANSFER_TO_HOST` of the
   damaged rows at the buffer's offset and stride, with no copy. A
   buffer's `wl_buffer.release` waits until the last transfer from it has
   been answered (`VIRTGPU_WAIT`), since the device reads the client's
   memory now and not a copy. A pool that is not sealed, not page-aligned
   or not a memfd keeps today's copy, which stays the fallback.

**What it changes for the certification argument.** Step 1 and step 2 are
kernel code: a new device file, and the device reading a *client's* pages,
where it read only pages the render core allocated. The pages are the
client's own and already mapped by the compositor; the device reads them
and never writes (`TO_DEVICE` only), and the seal is what keeps them alive
for as long as the backing holds them. Both go to the certification
consultant before they land (`AGENTS.md`), with negative controls: an
unsealed memfd refused, a range past the memfd's end refused, and a client
that drops its pool while a transfer is in flight leaving the pages held
until the transfer is answered.

**Sizing.** 8, as counted on 2026-09-23: about 3 for `/dev/udmabuf` and the
import, 2 for the driver's backing of a foreign VMO and the consultant's
review, 3 for hyprix's textures, the deferred release and the measurement
§3.8 asks for (the AV1 wallpaper behind a translucent terminal, 1920x1080
on the 3D card under KVM, main and the change run alternately on an idle
host).

### 3a, which was not chosen for the compositor

Mesa's virgl driver built on ferrousli would give *every client* OpenGL ES
as well as the compositor. It is a large C and C++ port -- libdrm, a C++
standard library, EGL and GBM -- it loads its drivers with `dlopen`, which
waits on the dynamic linking stage, and it is the C device stack
`src/user/system/linux/compositor/README.md` says the compositor never takes. 40 points or more,
most of it unknown. It stays the way clients get GL, later, beside step 4;
the compositor does not wait for it.

The customer's word was for Path A and its order. Taking 3b rather than 3a
for the compositor's own renderer is the recommendation that word was given
on, recorded here so that it can be overruled by name.

### What Path A does not give

* **Vulkan on the Windows host.** Venus, Vulkan over virtio-gpu, needs a
  Linux host with KVM. Under `whpx` it is virgl, which is OpenGL.
* **More processors under `whpx`.** The one-processor limit is QEMU 11.1's
  own fault (`docs/BACKLOG.md`) and has nothing to do with the GPU. It
  matters less once the GPU draws the pixels.
* **Anything on real hardware.** That is Path B.

---

## 4. Path B: an NVIDIA card under Ferrix itself

The customer chose this path on 2026-10-02, with NVIDIA's own driver and
userspace. Its feasibility pass and design are `docs/NVIDIA.md`, which
supersedes the sketch below.

For the day Ferrix runs on bare metal with an NVIDIA card in it. Not sized:
well over a hundred points, most of them unknowns, and it accelerates nothing
that runs today -- the one real board in this tree, the DK1, has no NVIDIA
GPU. What it would take:

* **The kernel side.** NVIDIA's open kernel modules (MIT and GPLv2, Turing
  and newer) are an OS-agnostic core over an OS interface layer, which is
  how a FreeBSD driver exists. What it plugs into on this side is
  `src/lib/proto/renderctl` and the render node above it, which §3.3 built to take a
  second implementation. Reusing them means writing that layer for
  Ferrix: PCI configuration and BARs, MSI-X, DMA mappings under the IOMMU,
  threads, timers, locks, allocation and firmware loading, and then the
  modesetting and DRM halves. On Ferrix that is a very large C program in a
  ring-3 driver process. The device objects stage 10 built -- apertures,
  vectors, IOMMU domains -- are the right shape for it, and the decision of
  2026-09-13 that drivers stay in ring 3 applies to it as to any other.
* **The firmware.** Tens of megabytes of GSP firmware, loaded by the driver,
  which then mostly speaks RPC to it.
* **The userspace.** NVIDIA's GL, Vulkan and CUDA libraries are closed
  shared objects built against glibc. They need a dynamic linker and
  `dlopen` (the dynamic linking stage), glibc's versioned ABI, and the
  `/dev/nvidia*`, `/proc` and `/sys` surface they probe. ferrousli is a
  static, musl-shaped libc that must never copy glibc. This is a project of
  its own and the part most likely to decide the whole path.
* **The open alternative**, to be weighed when the stage opens rather than
  now: Mesa's NVK over the nouveau interface, or upstream Linux's Rust
  driver for the same GPUs, Nova. Nova is the nearest in spirit -- Rust, and
  most of the logic is in the GSP firmware, so the driver is largely RPC --
  but both are written against Linux's DRM internals, and both need Mesa in
  userspace, which is §3's 3a again.

What Path A leaves in place for it: the render node, the renderer trait and
the GPU renderer's shaders, dmabuf, and gates that already know how to judge
a GPU's picture. What B adds is a second thing under the render node.

---

## 5. What does not change

* The software renderer stays, tested byte for byte, as the fallback and as
  the reference a GPU's picture is judged against.
* Drivers stay in ring 3.
* No C in the compositor. Path A's 3b keeps that; 3a and Path B's userspace
  are where it would be argued again, by name, when they come up.

---

## 6. Gears: Vulkan through Venus, and the board's own GPU (decided 2026-09-24)

The customer asked for vkgears on Ferrix, as a 3D demo that tests Vulkan and
GPU acceleration, and then for the same on the DK1. Two facts shaped the
answer, and the customer chose on them:

* **Ferrix has no Vulkan driver**, and there is no Mesa on ferrousli (§3a).
  Under virtio-gpu, a Vulkan that the host's GPU executes is **Venus**:
  Mesa's `virtio` Vulkan driver in the guest serialises Vulkan calls, and
  virglrenderer's render server replays them into the host's own Vulkan
  driver. That needs a Linux host with KVM. The Windows machine's `whpx`
  QEMU offers virgl, which is OpenGL (§3, *What Path A does not give*).
* **The DK1's GPU cannot do Vulkan at all.** The STM32MP157 carries a
  Vivante GC400T, an OpenGL ES 2.0 core. No Vulkan exists for it, in its
  hardware or in any driver, Linux's included. vkgears there would have to
  run on a CPU Vulkan (Mesa's lavapipe) and would test nothing about the GPU.

**Decided:** real vkgears through Venus in QEMU on the Linux host, and
**GLES2-level gears on the GC400** on the board, drawn by the GPU itself.
Both are first guesses in points, recorded here the way §3 was.

### 6.1 Venus, in QEMU on the Linux host

**The host is ready.** Checked on 2026-09-24: the distribution's QEMU
(10.2.1, `/usr/bin`) has `venus=`, `blob=` and `hostmem=` on
`virtio-gpu-gl-pci`, its virglrenderer was built with Venus and ships
`virgl_render_server`, and RADV drives the AMD node (`renderD128`) that
`--gl` already uses. The source-built QEMU 9.2.4 first on `PATH` has none of
it, so `FERRIX_QEMU=/usr/bin` again.

**What Mesa's Venus driver asks of the kernel**, read from
`src/virtio/vulkan/vn_renderer_virtgpu.c` of Mesa 26.2.3:

* `VIRTGPU_GETPARAM` answering `3D_FEATURES`, `CAPSET_QUERY_FIX`,
  `RESOURCE_BLOB`, `CONTEXT_INIT` and `HOST_VISIBLE` with non-zero, and
  `SUPPORTED_CAPSET_IDS` with bit 4 (`VIRTIO_GPU_CAPSET_VENUS`);
* `GET_CAPS` for the Venus capset, whose `wire_format_version` must not be 0
  and whose `supports_blob_id_0` must be set;
* `CONTEXT_INIT` with a capset id, 64 rings and no polled rings;
* `RESOURCE_CREATE_BLOB` of `BLOB_MEM_HOST3D`, `USE_MAPPABLE`: the rings and
  every host-visible Vulkan allocation are host memory, and `MAP` + `mmap`
  must put it in the program's address space. That memory is QEMU's
  `hostmem` region, a PCI BAR the driver maps blob resources into with
  `RESOURCE_MAP_BLOB`;
* `EXECBUFFER` with `RING_IDX` and `FENCE_FD_OUT`, returning a descriptor
  that `poll` reports readable once the ring's fence has signalled. Venus
  then simulates its sync objects on those descriptors
  (`vn_renderer_sim_syncobj.c`) and needs **no DRM syncobjs**, provided
  `DRM_CAP_SYNCOBJ_TIMELINE` is answered 0;
* `DRM_IOCTL_VERSION` naming `virtio_gpu` 0.x, which the node does, and
  libdrm's `drmGetDevices2` finding the node, which reads `/dev/dri` and
  `/sys/dev/char/226:<minor>/device`;
* `GEM_CLOSE`, `RESOURCE_INFO` and the `PRIME` pair.

**Presentation needs no dmabuf yet.** Mesa's Wayland WSI takes the CPU path
when a device is a "software" one, which `MESA_VK_WSI_DEBUG=sw` forces: the
frame is rendered by the host GPU, copied into host-visible memory, and
handed to the compositor as `wl_shm`, which `hyprix` shows today. So step 4
(`zwp_linux_dmabuf`) is where zero-copy comes from later, not a
prerequisite.

**The guest userspace is static.** Mesa loads a Vulkan driver as a shared
object through the Khronos loader and `dlopen`. ferrousli's ports are all
static, and its `dlopen` refuses a library with its own TLS, which Mesa has.
So the Venus driver is built as a static archive, and a small static loader
of Ferrix's own hands vkgears its `vkGetInstanceProcAddr` through the
driver's `vk_icdGetInstanceProcAddr`. vkgears is built from its three source
files (`vkgears.c`, `wsi/wsi.c`, `wsi/wayland.c`) rather than through
mesa-demos' meson, which requires desktop GL. It needs `libwayland-client`,
`libxkbcommon` and `libdecor`: the first two come with the foot port.

| # | what | points |
|---|---|---|
| V1 | **Blob resources and Venus in `src/lib/drivers/virtio::gpu`**: `RESOURCE_CREATE_BLOB`, `RESOURCE_MAP_BLOB`/`UNMAP_BLOB`, a context's capset in `CTX_CREATE`, a fence's ring index, the PCI shared-memory capability. Host-tested and fuzzed as the rest | 5 |
| V2 | **`src/user/system/native/drivers/display/virtio-gpu`**: negotiate `RESOURCE_BLOB` and `CONTEXT_INIT`, map the `hostmem` BAR, carry the Venus capset, and complete fences per ring. `src/lib/proto/renderctl` gains blob objects and ring fences | 8 |
| V3 | **The render node**: the ioctls and parameters above, a blob's host pages mapped into the program, fence descriptors that `poll`, and the `/sys` entries libdrm reads | 8 |
| V4 | **The ports**: libdrm, Mesa's Venus driver as a static archive, the static loader, libdecor and vkgears, against ferrousli, with vkgears' shaders compiled to SPIR-V on the host | 13 |
| V5 | **`cargo xtask test-vkgears`**: a `venus=on,blob=on,hostmem=` boot on the Linux host, judged from inside the guest: the device vkgears names is the host's GPU through Venus, frames are counted, and `/bin/shot` finds gears in the frame | 5 |

**39 points.** V1 to V3 are proved by a boot that makes a Venus context and
maps a blob before any Mesa exists; V4 and V5 are the demo.

**Built, 2026-09-24: vkgears draws on the host's GPU.** `cargo xtask
test-vkgears` on the gate host:

    deviceName    = Virtio-GPU Venus (AMD Ryzen 9 9900X 12-Core Processor
                    (RADV RAPHAEL_MENDOCINO))
    212 frames in 5.0 seconds = 42.353 FPS

That is Vulkan 1.4 in the guest, executed by RADV on the host: an
instance, a device, a pipeline whose SPIR-V the host compiles, command
buffers on a Venus ring in host memory, and a fence on every frame that
the guest polls. The frame is copied into shared memory for the compositor
(`MESA_VK_WSI_DEBUG=sw`), so the 42 frames a second are the copy's as much
as the GPU's; the same binary on the host against a headless hyprix draws
over 3,000 a second. Each step, and what differed from the plan:

* **V1**, the wire format: the blob commands and `OK_MAP_INFO` in
  `src/lib/drivers/virtio::gpu`, and a shared memory capability found by its id in
  `src/lib/platform/pci`, with the high halves a window larger than 4 GiB needs. QEMU
  with `hostmem` moves the registers to BAR 2 and makes BAR 4 the window.
* **V2 and V3**, `src/lib/proto/renderctl` version 3 and the node. HELLO names
  every capability set rather than one; `MAKE_BLOB` places a blob in the
  window as it is made, as Linux does, and the core gives a place back only
  when the device has said the blob is gone. The window is the kernel's,
  found at enumeration: the driver never maps it. `mmap` of a blob is a new
  kind of region, device memory with an id, whose keeper is the node's
  object -- so the device is not told to let a blob go, nor its place given
  to another, while any program still maps it. A ringed `EXECBUFFER` is
  fenced on its ring; the driver answers it when the host has signalled
  the fence, and the node's fence descriptor polls readable then. Venus
  needs no DRM sync objects when `GET_CAP` says there are none. Neither the
  `/sys` entries nor syncobjs the plan feared were needed: when libdrm finds
  no device, Venus opens the render nodes by name (a patch of the port's).
  `/sys` landed the same day; whether libdrm now finds the node through it,
  which would retire the patch, has not been tried.
* **Mesa's header is newer than the kernel's.** Mesa 26.2 carries a
  `drm_virtgpu_resource_create_blob` eight bytes longer than linux-libc-dev
  7.0's, so its ioctl number differs. Linux's `drm_ioctl` matches a driver's
  call by number and takes any size; the node does the same for this one.
* **V4**, the port: see the `vkgears` app's `build.sh` and its
  four patches. The driver is linked into the program rather than loaded,
  as one relocatable object with Mesa's hidden symbols made local -- Mesa
  builds its own C11 threads, which would collide with ferrousli's -- and
  `vkshim.py` writes the loader's part from the Khronos registry. Mesa
  needs ferrousli's `sincos` and `program_invocation_name`, which its glibc
  names landing of the same day gives.
* **V5**: judged from what vkgears says, the device and a count of frames.
  The plan's `/bin/shot` of the gears is not done: the frame is a client's
  in a GL-composited desktop, and counting frames drawn on the named device
  is the claim. With the node's fences made never to signal, the boot fails
  "found `Virtio-GPU Venus (...)` and drew no frames".
* **Two things found on the way that were not Ferrix's.** vkgears polled its
  display and then read it with `wl_display_dispatch`, which waits for an
  event another thread -- Mesa's -- may already have taken; patched. And
  hyprix offers `wp_fifo_v1` and `wp_commit_timing_v1` without holding a
  commit at a barrier, sends no `wp_presentation.clock_id`, and stamps
  presentation with wall-clock time; a FIFO client is unthrottled there.
  hyprix's owner has them (`docs/BACKLOG.md`), and `hyprland.conf`'s `env`
  now reaches what hyprix starts.

**What is left:** zero-copy presentation, which is step 4 of §3 --
`zwp_linux_dmabuf` in the compositor and the Venus blob imported into its
virgl context -- after which `MESA_VK_WSI_DEBUG=sw` goes; and the Khronos
loader, for a program that loads its driver rather than linking it, which
waits on `dlopen` of a library with thread-local storage.

### 6.2 The GC400 on the DK1

The core is at `0x5900_0000` (0x800 bytes of registers), interrupt SPI 109,
with a bus and a core clock and a reset line in the RCC (Linux's
`stm32mp157.dtsi`, `gpu@59000000`, `compatible = "vivante,gc"`). Nothing in
Ferrix touches it yet: the board's display is the LTDC alone
(`docs/DISPLAY.md` §6).

Vivante's command stream and state registers are documented by the etnaviv
project's reverse-engineered register database, which Mesa carries under the
MIT licence in `src/etnaviv/hw/`. Linux's etnaviv driver is GPL and is read,
never copied, the way glibc is for ferrousli. A shader is Vivante machine
code, and the gears need only a fixed few, so they are compiled on the host
by Mesa's etnaviv compiler and checked in, as `src/user/system/linux/compositor/virgl/shaders/`
compiles its GLSL with Mesa's virgl driver (§3.8).

| # | what | points |
|---|---|---|
| G1 | **The kernel's part**: the GPU's clocks and reset in `src/kernel/src/platform/st/stm32mp1.rs`, and a device node with its registers and interrupt, as the LTDC has | 3 |
| G2 | **`src/user/system/native/drivers/gpu/gc400`, a ring-3 driver**: identify the core (model, revision, features), power it, run a command buffer through the front end, take its completion by interrupt. Proved on the board by a `WAIT`/`LINK` loop and an event | 8 |
| G3 | **Pixels**: a render target cleared and resolved by the GPU into a buffer the LTDC shows | 8 |
| G4 | **Drawing**: vertex streams, a depth buffer, the host-compiled shaders, and draws | 8 |
| G5 | **`gears` on the board**: the three gears lit and turning, drawn by the GC400, with frames per second on the serial console | 5 |

**32 points.** None of it can be gated in QEMU, which emulates no Vivante
core: G2 to G5 are judged on the board by hand, and what can be
host-tested (the command stream's words, the register layout) is.

### 6.3 Where the GC400 stands (2026-09-24)

G1 and G2 are done: on the board on 2026-09-24 the kernel clocked the core
and took it out of reset, and `src/user/system/native/drivers/gpu/gc400` identified it, started its front
end and ran two blocks through it, each ending in an event taken by
interrupt. Nothing of either can run in QEMU: the three QEMU machines boot
exactly as before (no `vivante,gc` node, so no line and no device), and
devmgr now counts 8 drivers.

**What was built.**

* `src/kernel/src/platform/st/stm32mp1/gpu.rs` (G1), a sibling of `platform/st/stm32mp1/usb.rs` rather
  than a part of `platform/st/stm32mp1.rs`, which the display's session is changing. It
  finds the enabled `vivante,gc` node at `0x5900_0000`, reads its interrupt
  through `ferrix_fdt`, checks that PLL2 is on and locked and that its Q
  output (`DIVQEN`), the GPU's core clock, is enabled, and computes its rate
  as `stm32mp1`'s `pll4_q` does for PLL4, from `RCC_PLL2CFGR1`/`CFGR2`/
  `FRACR` and the reference `RCC_RCK12SELR` selects. Only then does it write
  the RCC: `GPUEN`, bit 5 of `RCC_MP_AHB6ENSETR` (0x218), which gates the bus
  and the core clock alike, then `GPURST`, bit 5, set in `RCC_AHB6RSTSETR`
  (0x198) and cleared in `RCC_AHB6RSTCLRR` (0x19C) 10 µs later. Every offset
  and bit is Linux's `clk-stm32mp1.c` (`K_MGATE(G_GPU, RCC_AHB6ENSETR, 5, 0)`,
  `pll2_q` gated by `RCC_PLL2CR` bit 5) and `stm32mp1-resets.h` (`GPU_R` =
  3269 = 0x198 × 8 + 5). `platform/st/stm32mp1/gpu.rs` registers the GPU with the device
  registry at bring-up, and `device.rs` publishes the node as binding
  `TREE_STM32_GPU` (3): the registers a page, the interrupt as vector
  0, and a `DmaShape` that is contiguous and not coherent, as the LTDC's is.
  Anything off -- no PLL2, its Q output off, a node elsewhere -- is said on
  a `gpu` line and no node is published.
* `src/lib/drivers/gpu/gc400`, host-tested (28 tests): the registers and bitfields used, the
  command encoders (`LOAD_STATE`, `END`, `NOP`, `WAIT`, `LINK`, `STALL`, the
  semaphore, the pipe select and the event), the identity, the reset and
  initialisation, the addressing, and the ring. Its tests pin every command
  word against `cmdstream.xml.h`, pin the reset's register writes in order,
  and run the ring through a model of the front end that parses slots,
  follows `LINK`s and records events. That model found a bug before the
  board could: a block queued before the front end started was never run,
  because the start address followed the loop instead of staying at the
  ring's first slot.
* `src/user/system/native/drivers/gpu/gc400`, the driver, started by devmgr as a new kind, an *engine*:
  handed its device and START as a port's driver is, publishing to no
  subsystem, and taken at its word. It stays alive afterwards, answering any
  interrupt by saying what it was: a driver that exited would have devmgr
  quiesce the device and report it dead, and after the front end has been
  started the command page must not go back to the allocator while the core
  might still read it.

**Where the numbers come from.** The register offsets, bitfields and command
encodings are the etnaviv project's register database under the MIT licence:
`cmdstream.xml.h`, `common.xml.h` and `state.xml.h` as Mesa 26.2.3 carries
them, and `state_hi.xml.h` -- the host interface, power management and
memory controller, which Mesa does not carry -- from Linux's
`drivers/gpu/drm/etnaviv/`, where it is the same generated, MIT-licensed
header. The sequence was learned by reading Linux's GPL etnaviv driver; none
of its code was copied, and each step in `src/lib/drivers/gpu/gc400` names the function it
follows.

**The identity** is read as `etnaviv_hw_identify` reads it: `HI_CHIP_IDENTITY`
first (a family of `0x01` is a core too old for the rest), then model,
revision, date, time, customer, product and ECO (the last two not on a GC600
of revision `0x19`, which faults), the major feature word, minor word 0, and
words 1 to 5 only when word 0's `MORE_MINOR_FEATURES` says they exist. All
fifteen values are printed raw, on one line. Which features drive the core
follows etnaviv too: its hardware database (`etnaviv_hwdb.c`, numbers from
Vivante's own feature database) has an entry for exactly this core -- model
`0x400`, revision `0x4652`, product `0x70001`, customer `0x100`, ECO 0 --
and when the registers match it, its feature words are used instead of the
registers', which Vivante cores are known to get wrong. The second line
says which were used.

**The register sequence**, in order:

1. *Soft reset*, as `etnaviv_hw_reset`, tried again until it takes or a
   second has passed: `PM_POWER_CONTROLS` = 0 and read back (module clock
   gating off: a gated module does not reset); `PM_PULSE_EATER` =
   `0x01590880 | bit 17`, then `| bit 0`, read back (the frequency scaler
   off); `HI_CLOCK_CONTROL` = `FSCALE_VAL(64)` with and then without
   `FSCALE_CMD_LOAD` (full speed, latched); `| ISOLATE_GPU`; `| SOFT_RESET`;
   20 µs; `SOFT_RESET` cleared; `ISOLATE_GPU` cleared. Then the checks: every
   bit of `HI_IDLE_STATE` but `AXI_LP` set, both `IDLE_3D` and `IDLE_2D` set
   in `HI_CLOCK_CONTROL`, and -- on a core with a version-2 MMU --
   `MMUv2_CONTROL`'s enable clear. Then `DISABLE_DEBUG_REGISTERS` cleared, so
   the `FE_DMA_*` registers read for a diagnosis are real.
2. *Clock*, as `etnaviv_gpu_update_clock` for a core without dynamic
   frequency scaling: `FSCALE_VAL(64)` loaded again into what the reset left.
3. *Initialisation*, as `etnaviv_gpu_hw_init` for a GC400: `HI_AXI_CONFIG` =
   `AWCACHE(2) | ARCACHE(2)`; `PM_PULSE_EATER` = `0x01590880`, the value
   etnaviv gives every core that is not one of its listed exceptions;
   `HI_INTR_ENBL` = all ones. **Module-level clock gating is left off**,
   where etnaviv turns it on with per-revision exceptions that are fixes for
   hangs: it saves power and nothing else, and it can be turned on once the
   core has been seen running without it. The GC320, GC2000 and
   security-block steps are other cores'.
4. *Addressing*, below.
5. `HI_INTR_ACKNOWLEDGE` read once, which clears anything left pending, and
   printed as the stale interrupts.
6. *The ring*, one page pinned `PIN_COHERENT`: a `WAIT(200)` and a
   `LINK(2)` back to it at the page's start, and the front end started there
   (`FE_COMMAND_ADDRESS`, then `FE_COMMAND_CONTROL` = `ENABLE | 2`, as
   `etnaviv_gpu_start_fe`). Two blocks are spliced in one after the other,
   each by overwriting the `WAIT` the front end spins on with a `LINK` to
   the block -- argument word, barrier, header word, as
   `etnaviv_buffer_replace_wait` orders it -- and each ending in a
   `WAIT`/`LINK` of its own: the first is `PIPE_SELECT(3D)`, `GL_EVENT` =
   `1 | FROM_PE`; the second `GL_EVENT` = `2 | FROM_PE`. Each event is taken
   as the interrupt on a port: `HI_INTR_ACKNOWLEDGE` read (which acknowledges
   the core), then the interrupt acknowledged to the kernel, and the time
   from the splice printed. Then the last `WAIT` is overwritten with `END`,
   as `etnaviv_buffer_end` does, and `HI_IDLE_STATE`'s front-end bit is
   waited for.

**A loop and not a stream that ends.** A stream of an event and an `END`
would show the front end fetches and the interrupt arrives, and nothing
more: every later step needs the front end started once and fed while it
runs, which is what etnaviv's ring does and what G3 builds on. So the first
run on the board is the loop, the splice and the event, together.

**Addressing: physical, not a window at `0xC000_0000`.** The plan assumed a
version-1 MMU with the linear window at the start of the DK board's memory.
Reading the database and the driver says otherwise, twice. The database's
GC400T has `chipMinorFeatures1_MMU_VERSION` set: its MMU is version 2, which
comes out of reset disabled and passes every address through untranslated,
and the first stream etnaviv runs on such a core, the one that turns the MMU
on (`etnaviv_iommuv2_restore_nonsec`), is fetched from its *physical*
address, with no memory base written. And for version-1 cores etnaviv today
puts the window at 2 GiB whenever the command buffer is above 2 GiB, as all
of this board's memory is, not at the memory's start. So the driver takes
`Addressing::Physical` on a core with a version-2 MMU -- the command page's
address is its physical one and no `MC_MEMORY_BASE_ADDR_*` is written -- and
on a core with version 1 it writes 2 GiB to all five of those registers and
gives the core the physical address less 2 GiB. The MMU itself is not
programmed either way: nothing here uses an address it would translate. With
it off the front end reads one run of physical addresses, which is why the
node says contiguous and why G2 uses a single page; a buffer larger than a
page, from G3 on, has to be physically contiguous or the MMU has to be
turned on.

**What the board printed** (2026-09-24, the DK1 with its firmware's PLL2:
24 MHz HSE, M 2, N 65, fraction 0x1400, Q 0). The kernel, among the device
lines:

    gpu      GC400 at 0x59000000, interrupt 141, core clock 533.000 MHz from PLL2 Q, out of reset

`devmgr` one more driver started, and the driver, the same in all five runs:

    gc400: model 0x400 revision 0x4652 date 0x20160522 time 0x20594600 product 0x70001 customer 0x100 eco 0x0 identity 0x04010000 features 0xa0e9e004 minor 0xe1299fff 0xbe13b219 0xce110010 0x08000001 0x00020102 0x00020000
    gc400: features from etnaviv's database for this core, 3D pipe, memory controller 1.0, addressing physical, MMUv2 off
    gc400: reset in 1 attempt(s), clock control 0x00070100, idle 0x7fffffff, stale interrupts 0x00000000
    gc400: command page at 0xd69ae000 (GPU 0xd69ae000); front end started there, now at 0xd69ae000 (WAIT)
    gc400: event 1: interrupt with 0x00000002 after 133 us
    gc400: event 2: interrupt with 0x00000004 after 105 us
    gc400: front end ended at 0xd69ae040, idle 0x7fffffff: command buffer ran, events 1 and 2 by interrupt

The last line is G2 done. A second line saying *features from the
registers* would have meant the core is not the one in etnaviv's database.

**What the run settled**, in the order it was doubted:

* The features are the database's: model, revision, product `0x70001`,
  customer `0x100` and ECO 0 match its entry exactly.
* A version-2 MMU left off passes addresses above 2 GiB through: the front
  end fetched the command page at `0xd69ae000` physical, and ended 64 bytes
  on, after both blocks.
* The pixel engine's event arrives with only a pipe select before it; no
  `SEM`/`STALL` pair was needed while nothing is drawn.
* The reset's idle mask is right: `HI_IDLE_STATE` read `0x7fffffff`, every
  bit but `AXI_LP`, on the first attempt.
* The board's firmware enables PLL2's Q output, and the interrupt is 141
  (SPI 109), as the tree says.

What is still unproven is everything after an event: that the core writes
memory (G3's resolve), and that it draws (G4).

**On a timeout** the driver prints, in one line, `HI_IDLE_STATE` with the
front end's bit spelled out, `HI_INTR_ACKNOWLEDGE` (an event bit set there
means the core raised it and no interrupt reached the program),
`HI_AXI_STATUS`, `FE_DMA_STATUS`, `FE_DMA_DEBUG_STATE` with the parser
state's database name (`WAIT`, `LINK`, `END` ...), `FE_DMA_ADDRESS`, and
`FE_DMA_LOW`/`HIGH`, the command last fetched. It keeps the page and stays
up either way.
