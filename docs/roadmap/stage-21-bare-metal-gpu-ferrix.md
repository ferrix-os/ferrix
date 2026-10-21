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

**Where it stands (2026-10-05).**

* **On `main`.**
  * N0, the kernel prerequisites, and N1: `nvrm` boots the 3060's GSP, and
    `nvidia-smi` lists the card through the chardev core (2026-10-03).
  * N2–N4 and the screen, landed 2026-10-05 as `land-n6` (b136ac7d4, in batch
    20261005T190105Z, tip a84992dc5): `vulkaninfo` names the RTX 3060, and
    Chrome's WebGL renders on it through ANGLE on NVIDIA's Vulkan. NVKMS runs
    inside `nvrm` and drives the customer's LG TV on the 3060's HDMI port at
    1920x1080@60; `nvrm` serves that TV as a card of the kernel's display
    core, copying frames into VRAM. `cargo xtask run-compositor --nvidia`
    boots the Chrome desktop on that monitor, and `FERRIX_NVIDIA_INPUT`
    passes dedicated host keyboards and mice into it (the customer used the
    desktop with them).
  * The chardev core's queue to `nvrm` is held to its room (F-63, closed),
    with stage 10's chardev self-check and a fake driver (N10's code half,
    N12, N13; FX-1013), since 2026-10-05.
  * What the landing took in the kernel: `PIN_CONTIGUOUS`, which RM's channel
    buffers need, or `vkCreateDevice` fails under the desktop's memory load;
    displayctl v8's copying driver; devfs inotify events and init's
    `.device` units (hyprix requires `dev-dri-card0.device`).
* **Owed.** The conditions the landing left are BACKLOG rows O1–O6 (consultant
  ledger 318); N5's switch, D1 to D10 and `test-nvidia-smi` stay owed.
* **Speed.** WebGL runs at 44–53 fps. Chrome's frames are read back and
  composited in software, because hyprix has no dmabuf yet. The customer
  finds the screen's frame rate and page loads not good enough.
* **Branches.** `nvidia-n2` (pushed) is the development branch, and its tip
  does not build: N3b is half written.
* **Next, in the customer's order.**
  1. N3b: Chrome's GPU compositing over dmabufs, zero-copy. The design is
     approved with conditions (ledger 316). Done so far: `nvrm`'s
     nvidia-drm subset. Half written: the kernel's render node and dmabuf
     object, and hyprix's `zwp_linux_dmabuf_v1`.
  2. A hardware cursor through NVKMS.
  3. Measuring page loads, to tell the network from rendering.

**Where it stands (2026-10-21).** Two lines of work have joined NVIDIA's.

* **NVIDIA, N3b's kernel half is on `main`** (2026-10-11). The chardev core
  has a render node and VMO dmabufs (`chardev_dmabuf_install` and `_resolve`,
  1320c4345), `sync_file` fences (`chardev_sync_install`, `_signal`, `_resolve`,
  74ba209a3) and name-only dmabufs of video memory that `nvrm` exports, which
  no processor mapping can read and only NVIDIA's userspace imports again
  (b05d045c9, `vk-dmabuf` plays an export and an import between two
  processes). hyprix has `zwp_linux_dmabuf_v1` with a GBM-shaped allocator
  since 2026-10-08, and an empty dmabuf slot's fence resolves as signalled
  since 2026-10-21 (8777e90f9). Still on branches: hyprix drawing on the card
  through virglrenderer's test server (N3c, `nv-zerocopy`) and an X program's
  window drawn by the card, DRI3 and Present over `nvrm` (`nv-next`, whose
  tip does not pass `cargo xtask check`); the hardware cursor and the
  measurement of page loads are not started.
* **The customer's Intel N150 box** (`docs/N150.md`, the machine on which
  stage 23's anti-cheat is to be tried, 2026-10-13). A PC with no NVIDIA card,
  so stage 21's bare-metal half is run there first. Ferrix booted on it on
  2026-10-15, built from the branch `ahci`, to a desktop on the firmware
  framebuffer with `/` on its SATA drive and the network up. On `main` are the
  drivers and kernel changes that boot needed and those it showed missing: AHCI
  (`docs/AHCI.md`), the Motorcomm YT6801's network driver, `efifb` (H6: hyprix on
  the firmware's framebuffer with no GPU driver, write-combining from its
  first map), a PC's xHCI controller under `usbhid`, an NVMe driver, x2APIC
  mode and a clock with bounded PIT and HPET waits and a CPUID fallback, `CR0.NE` and `CR4.MCE`,
  HWP, the SMBIOS tables in sysfs, a kernel log kept across a warm reset and
  an event-log partition for panic reports, VT-d on its two units, and the
  defences for RFDS and BHI. The kernel bring-up is evidenced on QEMU only and
  has not run on the box with checks on; its NIC and USB still poll
  (`docs/BACKLOG.md`).
* **An Intel GPU driver for the box** (`docs/INTEL-GPU.md`, ring 3, following
  i915 file by file; design OK IF, ledger 2352). On `main`: the kernel's
  render-only chardev node, the screen lent by identity, write-combining
  mappings, sync-file import and export on a dmabuf's slot (K3 to K6), and
  `intel-xelp`, the i915 uAPI front end over a back end, host-tested. On the box
  on 2026-10-20: M1, the device answers, and M2, the GT's bring-up and the
  first GPU commands (a blit exact by hash); M3, the render node served over the
  GT back end, is wired and not shown on the box. The box hung hard under
  `ferrix.intelgpu` on 2026-10-18 and -19; the first cause is found. Mesa's
  anv and DXVK present through the front end over a fake back end in QEMU; next
  are M3 on the box and Endfield's first iGPU run, which is prepared (the
  launcher on lavapipe, only the game on anv).

**Exit:** the stage 19 exit on real hardware, drawn by the card.

---

