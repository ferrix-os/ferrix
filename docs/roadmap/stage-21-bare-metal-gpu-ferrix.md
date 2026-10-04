# Stage 21 — Bare metal, and a GPU of Ferrix's own  ·  *unsized, over 100 points*

Ferrix on a machine rather than in one, with the NVIDIA card in it driving
the screen: the second half of the decision of 2026-09-18, opened when the
customer wants Ferrix on bare metal and not before. `docs/GPU.md` §4 says
what it takes and why it is not sized: NVIDIA's open kernel modules as a
ring-3 driver process behind an OS interface layer written for Ferrix, the
GSP firmware they load, and a userspace that today means glibc-built closed
libraries -- or Mesa's NVK and Linux's Rust driver Nova, weighed when the
stage opens. What stage 19's Path A leaves ready for it: the render node,
the renderer trait and the GPU renderer's shaders, and gates that judge a
GPU's picture; `zwp_linux_dmabuf` too, once it lands after Path A. It depends on the dynamic linking stage,
whichever userspace is taken.

The customer decided on 2026-10-02 to use NVIDIA's own driver and
userspace, first on nazuna's RTX 3060 through libvirt. `docs/NVIDIA.md` is
the feasibility pass and the design, and puts N0–N4 at 111 points.

**Where it stands (2026-10-04).**

* **On `main`.** N0, the kernel prerequisites, and N1: `nvrm` boots the
  3060's GSP, and `nvidia-smi` lists the card through the chardev core.
* **Done but unlanded.**
  * N2–N4: `vulkaninfo` names the RTX 3060, and Chrome's WebGL renders on
    it through ANGLE on NVIDIA's Vulkan.
  * N6 for the screen: NVKMS runs inside `nvrm` and drives the customer's
    LG TV on the 3060's HDMI port at 1920x1080@60. `nvrm` serves that TV
    as a card of the kernel's display core, copying frames into VRAM.
  * `cargo xtask run-compositor --nvidia` boots the Chrome desktop on that
    monitor.
  * Since 2026-10-04, `FERRIX_NVIDIA_INPUT` passes dedicated host keyboards
    and mice into it. The customer used the desktop with them.
  * What it took in the kernel:
    * `PIN_CONTIGUOUS`, which RM's channel buffers need, or
      `vkCreateDevice` fails under the desktop's memory load;
    * displayctl v8's copying driver;
    * devfs inotify events and init's `.device` units: hyprix now requires
      `dev-dri-card0.device`.
* **Speed.** WebGL runs at 44–53 fps. Chrome's frames are read back and
  composited in software, because hyprix has no dmabuf yet. The customer
  finds the screen's frame rate and page loads not good enough.
* **Branches.**
  * `land-n6` (pushed, 3d6621a64) is the working state as one commit.
    `cargo xtask check` passed on it. The certification consultant allows it
    to land ahead of its conditions (ledger 318). A batch dropped it on a
    docs/generated conflict; it needs a rebase, `cargo xtask model-doc`, and
    the batch's full row plus the desktop and nvrm gates. The conditions it
    leaves owed are BACKLOG rows O1–O6.
  * `nvidia-n2` (pushed) is the development branch. Its tip does not build:
    N3b is half written.
* **Next, in the customer's order.**
  1. N3b: Chrome's GPU compositing over dmabufs, zero-copy. The design is
     approved with conditions (ledger 316). Done so far: `nvrm`'s
     nvidia-drm subset. Half written: the kernel's render node and dmabuf
     object, and hyprix's `zwp_linux_dmabuf_v1`.
  2. A hardware cursor through NVKMS.
  3. Measuring page loads, to tell the network from rendering.

**Exit:** the stage 19 exit on real hardware, drawn by the card.

---

