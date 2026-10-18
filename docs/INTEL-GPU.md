# The N150's Intel GPU: a render driver so Mesa's anv and iris run on Ferrix

Version 3, 2026-10-18: the consultant's design review folded in (§4.4, §5.1).
Version 2, 2026-10-18: version 1 plus the customer's D1 and D2 and §3.6.
Version 1, 2026-10-18, a design with no code. Asked for by the customer on
2026-10-18. The N150 box (`docs/N150.md`) draws every frame with Mesa's
lavapipe today. That keeps its four cores busy (load about 10) and may be
what breaks Endfield's Qt user interface. On Linux the same box uses
Intel's i915 driver with Mesa's anv (Vulkan) and iris (GL). This document
decides how Ferrix gets the same, and sizes the work. `docs/N150.md` §7 had
deferred it as "i915/xe uAPI for Mesa ANV, ~150-250 points render-only,
first guess". This design replaces that guess.

**What the design recommends, in one paragraph.** Write a **new ring-3
driver in Rust** for the Xe-LP (Gen12.0) GT. Render only: execlist
submission on the render engine, with **no GuC and no firmware**. The
driver serves the **i915 uAPI**, not xe, as a forwarded render node through
the chardev core that NVIDIA's `nvrm` already uses (`docs/NVIDIA.md` §4.4).
It serves only the subset that Mesa 25/26's anv and iris call on Gen12:
softpin, no relocations, a VM per context, timeline fences and syncobjs.
Debian 13's Mesa 25.0.7 already carries the anv and iris binaries in the
packages the volumes pin. The kernel needs five changes in the certified
item, each small and each reviewed. About **82 points** to Endfield's
renderer on anv, 93 with the screen (§7), against 200 or more for porting i915.

**No certification claim.** The N150 box lies outside the reference
configuration (`docs/N150.md`, "No certification claim"). The driver is a
ring-3 program and outside the item, as every driver is. The kernel changes
of §5 are inside the item and go to the consultant one by one.

---

## 1. The device

| What | Value | From |
|---|---|---|
| Function | `00:02.0`, `8086:46d4`, "Alder Lake-N [Intel Graphics]", class 0300 | the box's `lspci -nn`, 2026-10-16 (`~/.local/share/ferrix/n150-linux/box-2026-10-16/lspci.txt`) |
| BAR0 (GTTMMADR) | 16 MiB at `0x6000000000`, 64-bit, non-prefetchable: registers in the lower half, the GGTT's page table entries in the upper 8 MiB | same, `lspci-vvv.txt`; i915 `gt/intel_ggtt.c` |
| BAR2 (GMADR) | 256 MiB at `0x4000000000`, prefetchable: the aperture through the GGTT. **The firmware's framebuffer, which `ferrix.efifb` lends to hyprix, is in it** | same; `docs/N150.md` §1 |
| Interrupts | one MSI, maskable, **32-bit address**, no MSI-X | `lspci-vvv.txt` (`MSI: Count=1/1 Maskable+ 64bit-`) |
| IOMMU | behind `dmar0` (0xfed90000, VER 4.0), the unit that serves only the iGPU. An RMRR covers `0x7c000000-0x803fffff` (stolen memory and the GGTT's backing) | `vtd-design.md` §1, §3.9 |
| GT | Xe-LP, Gen12.0 (IP 12.0), 24 EUs. One render engine (RCS0), one copy engine (BCS0), video engines. No compute engine, no local memory, LLC shared with the cores | Intel's specification; i915 `i915_pci.c` |
| Linux's view | i915 binds the device as **Alder Lake-P, subplatform N** (`INTEL_ADLN_IDS` → `adl_p_info`, `i915_pci.c:862`). xe lists it (`adl_n_desc`, `xe_pci.c:238`) but with `require_force_probe = true`, so xe does not bind it by default | Linux 7.0 source |

What i915 does on this device by default (Linux 7.0, `gt/uc/intel_uc.c`
`uc_expand_default_options`): it turns on **GuC submission and HuC**, since
only Tiger Lake, Rocket Lake and Alder Lake-S are excluded. ADL-N loads
ADL-S's GuC blob, `tgl_guc_70.bin`, because "ADL-N does not support
HWConfig" (`intel_uc_fw.c:290-300`), and HuC `tgl_huc.bin`. The display
needs DMC firmware (`adlp_dmc.bin`). With `i915.enable_guc=0`, i915 runs
ADL-P/N on **execlists**. That path still exists in i915 for every Gen12.0
part, and Genode runs Alder Lake on it (§3.3). Only Gen12.55 and later
need GuC.

## 2. Which uAPI: i915, not xe

Mesa chooses its back end from the name that `DRM_IOCTL_VERSION` returns:
`"i915"` gives `INTEL_KMD_TYPE_I915` and `"xe"` gives `INTEL_KMD_TYPE_XE`
(`src/intel/dev/intel_kmd.c:44-47`). anv and iris each carry both back ends
(`src/intel/vulkan/{i915,xe}/`, `src/gallium/drivers/iris/{i915,xe}/`). So
Ferrix's driver picks the uAPI, and Mesa follows it. (Checked in Mesa
26.1.0-devel, `~/aosp17/external/mesa3d`. Debian's 25.0.7 has the same
split.)

| | i915 uAPI | xe uAPI |
|---|---|---|
| What Mesa thinks of it on ADL-N | the supported path | `intel_device_info.c`: for `verx10 < 200`, "Support for this platform is experimental with Xe KMD, bug reports may be ignored." |
| What Linux runs on this box | i915 | xe only with `force_probe` |
| GPU address space | userspace picks addresses (softpin). The kernel binds the objects that each execbuffer lists | userspace binds explicitly (`VM_BIND`) |
| Sync | syncobj fence arrays, plus implicit sync through objects' write flags and dma-buf sync files | syncobjs only, explicit |
| Ioctls Mesa uses | 25 i915 ioctls plus the common ones (§2.1) | about 10 xe ioctls plus the common ones |
| Sparse resources | through TR-TT on Gen12 (`anv_physical_device.c:2813`), which DXVK at feature level 11 does not need | through `VM_BIND` |

xe is the cleaner interface. It is still the wrong choice here. Mesa
upstream calls xe on this generation experimental and may ignore bugs found
on it, and Linux does not run it on this hardware. Every DXVK, Unity or Qt
failure would then be on a path nobody else runs. i915's larger surface
costs about 4 points more in the front end (§7, M3), and it is the path
that Mesa's CI, Debian and every ADL-N laptop exercise. If the front end is
kept apart from the GT code (§4), an xe front end could be added later at
the same cost.

**crocus is not involved.** It is Mesa's GL driver for Gen4 to Gen7.5. GL
on Gen12 is iris, and Vulkan is anv (not hasvk, which is Gen7/8).

### 2.1 The surface anv and iris need

The ioctls below are those that `src/intel/{vulkan,common,dev}` and
`src/gallium/drivers/iris` call, from a grep of Mesa 26.1-devel. The
numbers were printed from Linux 7.0's UAPI headers
(`/usr/src/linux-headers-7.0.0-29/include/uapi/drm/`, byte-identical to the
v7.0 tag's `include/uapi/drm/i915_drm.h` and `xe_drm.h`) by a throwaway
probe (Appendix A). They are not written from memory.

**Common DRM**, served by the driver through the forwarded node:

| Ioctl | Number | What the driver does |
|---|---|---|
| `VERSION` | `0xc0406400` | name `i915`. This is what selects Mesa's back end |
| `GET_CAP` | `0xc010640c` | `SYNCOBJ`, `SYNCOBJ_TIMELINE`, `PRIME` (import and export) |
| `SET_CLIENT_CAP` | `0x4010640d` | accepted, no effect on a render node |
| `GEM_CLOSE` | `0x40086409` | drops a handle |
| `PRIME_HANDLE_TO_FD` / `PRIME_FD_TO_HANDLE` | `0xc00c642d` / `0xc00c642e` | a VMO dmabuf through `chardev_dmabuf_install` and its resolve (`docs/NVIDIA.md` §4.4) |
| `SYNCOBJ_CREATE`, `DESTROY`, `HANDLE_TO_FD`, `FD_TO_HANDLE`, `WAIT`, `RESET`, `SIGNAL`, `TIMELINE_WAIT`, `QUERY`, `TRANSFER`, `TIMELINE_SIGNAL` | `0xc00864bf` … `0xc01864cd` | syncobjs are the driver's own objects, per file. A sync-file export or import goes through `chardev_sync_install` and `resolve` (`chardev/sync.rs`). A wait is a request answered when its fence signals or its deadline passes |
| `GEM_FLINK`, `GEM_OPEN` | `0xc008640a`, `0xc010640b` | `EPERM` on a render node, as on Linux |

**i915**:

| Ioctl | Number | Subset served |
|---|---|---|
| `I915_GETPARAM` | `0xc0106446` | the parameters anv refuses to start without (`vulkan/i915/anv_device.c:69-128`): `HAS_WAIT_TIMEOUT`, `HAS_EXECBUF2`, `MMAP_VERSION` ≥ 1, `HAS_EXEC_SOFTPIN`, `HAS_EXEC_FENCE_ARRAY`, `HAS_EXEC_ASYNC`, `HAS_EXEC_CAPTURE`, `HAS_EXEC_TIMELINE_FENCES`. Plus `CHIPSET_ID`, `REVISION`, `CS_TIMESTAMP_FREQUENCY`, `EU_TOTAL`, `SUBSLICE_TOTAL`, `SLICE_MASK`, `SUBSLICE_MASK`, `HAS_CONTEXT_ISOLATION`, `HAS_EXEC_NO_RELOC`, `HAS_EXEC_HANDLE_LUT`, `HAS_EXEC_BATCH_FIRST`, `HAS_EXEC_FENCE`, `HAS_LLC` (1), `HAS_USERPTR_PROBE` (0 until K7), `MMAP_GTT_VERSION`, `CMD_PARSER_VERSION` (0), `PXP_STATUS` (`ENODEV`), `PERF_REVISION` (`EINVAL`). Anything else is `EINVAL`, as for an unknown parameter |
| `I915_QUERY` | `0xc0106479` | `TOPOLOGY_INFO`, `GEOMETRY_SUBSLICES`, `ENGINE_INFO` (RCS0, BCS0 later), `MEMORY_REGIONS` (system memory only). `HWCONFIG_BLOB` returns `ENODEV` (ADL-N has no HWConfig, §1), as do `GUC_SUBMISSION_VERSION` and `PERF_CONFIG` |
| `I915_GEM_CREATE` / `_CREATE_EXT` | `0xc010645b` / `0xc018647c` | a VMO, pinned into the device's domain at first use. The extensions are memory regions (system only) and `SET_PAT` (Gen12 PAT index, checked against the table of §3.4). Protected content is refused |
| `I915_GEM_MMAP_OFFSET` | `0xc0206464` | `WB` and `WC` (WC needs K6) by offset, then `mmap` answered with `MapReply::Vmo`. `GTT` and `UC` modes are `ENODEV`, which anv and iris handle |
| `I915_GEM_MMAP` | — | legacy. `ENODEV` makes Mesa use `MMAP_OFFSET` |
| `I915_GEM_EXECBUFFER2_WR` | `0xc0406469` | **softpin only**: every object `EXEC_OBJECT_PINNED` with `SUPPORTS_48B_ADDRESS`, `NO_RELOC` and `HANDLE_LUT` required, a relocation count of 0 required (anything else is `EINVAL`). `BATCH_FIRST`, `FENCE_ARRAY` and the timeline-fences extension, `FENCE_OUT`, and `EXEC_OBJECT_WRITE` / `ASYNC` for implicit sync on shared objects. `CAPTURE` is accepted and ignored. Engines by the context's engine map |
| `I915_GEM_WAIT`, `I915_GEM_BUSY` | `0xc010646c`, `0xc0086457` | from each object's last fences |
| `I915_GEM_SET_TILING`, `GET_TILING` | `0xc0106461`, `0xc0106462` | recorded and answered. Tiling has no kernel meaning on Gen12 with softpin, since there are no fences |
| `I915_GEM_SET_CACHING`, `SET_DOMAIN` | `0x4008646f`, `0x400c645f` | recorded. On an LLC part, domains mean nothing for a WB mapping |
| `I915_GEM_MADVISE` | `0xc00c6466` | recorded. Nothing is purged (no shrinker) |
| `I915_GEM_USERPTR` | `0xc0186473` | `ENODEV` until K7. anv uses it only for `VK_EXT_external_memory_host` |
| `I915_GEM_GET_APERTURE` | `0x80106463` | GGTT and VM sizes |
| `I915_GEM_CONTEXT_CREATE_EXT` (and the legacy `CONTEXT_CREATE`, as one with no extensions), `DESTROY`, `GETPARAM`, `SETPARAM` | `0xc010646d`, `0x4008646e`, `0xc0186474`, `0xc0186475` | parameters: `ENGINES`, `VM`, `GTT_SIZE`, `RECOVERABLE`, `PRIORITY` (low and normal only; higher needs privilege, as on Linux), `BANNABLE`, `PERSISTENCE` (0 only), `NO_ERROR_CAPTURE`, `LOW_LATENCY` (accepted). `SSEU`, `CONTEXT_IMAGE` and `PROTECTED_CONTENT` return `EINVAL`/`ENODEV` |
| `I915_GEM_VM_CREATE`, `VM_DESTROY` | `0xc010647a`, `0x4010647b` | a 48-bit, 4-level PPGTT per VM |
| `I915_GET_RESET_STATS` | `0xc0186472` | per context: hung batches and pending batches. anv turns them into `VK_ERROR_DEVICE_LOST` |
| `I915_REG_READ` | `0xc0106471` | the render ring's `TIMESTAMP` only, as i915's whitelist allows |

**And on the dmabuf**, which the kernel serves (§5, K5):
`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` `0xc0086202` and `IMPORT_SYNC_FILE`
`0x40086203`. **anv refuses the device without the export**
(`i915_gem_supports_dma_buf_sync_file`, `common/i915/intel_gem.c:317`, the
"kernel missing dma-buf sync file import/export" error). Ferrix's dmabuf
today answers `ENOTTY` to every ioctl except `DMA_BUF_IOCTL_SYNC`
(`docs/NVIDIA.md` §4.4, N3b), so this is on M3's path.

That is 25 i915 ioctls, 11 syncobj ioctls, 8 other common ones and 2 on
the dmabuf. The front end is a parser and a table per ioctl, host-tested
against the struct layouts that a probe prints, as
`src/lib/proto/linux-abi` does for virtio-gpu.

## 3. The hardware side, render only

### 3.1 What render-only leaves out

* **The display engine.** The firmware lit the panel, and the plane scans
  out of stolen memory through GGTT entries the firmware wrote. The driver
  never touches the display registers, the DMC, the power wells of the
  display, or the GGTT entries behind the lit plane (§3.3). The screen stays
  the firmware's mode, as with `ferrix.efifb` today (§4.3).
* **GuC, HuC, GSC.** No firmware is loaded (§3.2).
* **Video, compute, OA, PXP.** Not exposed. VDBOX and VEBOX are left in
  reset and their power gates on.

### 3.2 Submission: execlists, not GuC

Two choices exist on this GT. Execlists: the driver writes context
descriptors into the engine's submit queue (`RING_EXECLIST_SQ_CONTENTS`,
`base + 0x510`), loads them with `RING_EXECLIST_CONTROL` (`base + 0x550`),
and reads completions from the context status buffer. GuC: the driver loads
GuC firmware, speaks its CT protocol, and lets the GuC schedule.

Execlists, because:

* **No firmware.** GuC means loading `tgl_guc_70.bin` by DMA through the
  GGTT. It also means the ADS, the CT buffers, and the GuC's ABI, which
  changes with each 70.x release (`intel_guc_submission.c` alone is 6,014
  lines). Execlists need nothing from the volume.
* **It runs on this hardware.** Linux 7.0's i915 keeps execlists for all
  of Gen12.0 (`intel_execlists_submission.c`, 4,176 lines, of which a
  render-only driver needs the core: ELSQ, CSB, preemption off at first).
  Genode's driver submits through `GEN12_EXECLIST_SQ_CONTENTS` on Alder Lake
  (§3.3). M0 shows it on this box under Linux with `i915.enable_guc=0`
  before any Ferrix code is written.
* **Less hidden state.** A hang under execlists is in registers the driver
  can read. A hang under GuC is a firmware log.

What execlists cost: GuC's SLPC frequency control is gone. The driver
requests frequencies itself through `GEN6_RPNSWREQ` (`0xa008`), as i915's
host RPS does, and turns on RC6 (`GEN6_RC_CONTROL`, `0xa090`) itself.
Without RPS the GT stays near its lowest frequency, and M4 is where that is
fixed. If execlists ever turn out not to be enough, GuC submission can be a
later slice behind the same front end.

### 3.3 What a non-Linux kernel does with this GPU

| OS | How | What it shows here |
|---|---|---|
| **FreeBSD** (drm-kmod) | Linux's i915 compiled against `linuxkpi`, an emulation of Linux's kernel interfaces, in ring 0, re-based on Linux releases | a port runs, but the compatibility layer grows with every Linux release, and it is ring 0 |
| **OpenBSD** (inteldrm) | Linux's i915 with OpenBSD's own `drm_linux.c` shims, in ring 0 | as FreeBSD's: a port tracks Linux by hand |
| **Haiku** | `intel_extreme`: modesetting only. 3D is llvmpipe | nothing for render |
| **Fuchsia** | `msd-intel-gen`, a new C++ driver under the Magma interface, written for Gen9.5 and **Gen12**; Mesa's anv on a Magma back end ([source](https://fuchsia.googlesource.com/fuchsia/+/main/src/graphics/drivers/msd-intel-gen/)) | a new driver outside a monolithic kernel carries anv on Gen12 |
| **Genode** | `repos/os/src/driver/gpu/intel`, a **new user-level C++ driver** of about 7,500 lines (`main.cc` 2,638, `mmio.h` 2,167, `context.h` 971, plus GGTT, PPGTT, RC6, reset and workarounds headers). It submits by execlists on Gen8 to Gen12 including Alder Lake (`_submit_execlist_gen12`), and it serves Mesa's iris through a client-side libdrm that maps i915 ioctls onto its session (`repos/libports/src/lib/libdrm/ioctl_iris.cc`). It has done Gen12 since 22.11 ([release notes](https://genode.org/documentation/release-notes/22.11)) | **the nearest precedent**: a microkernel system, the driver in user space, execlists, Mesa unchanged above an i915-shaped interface |

### 3.4 The pieces, and where Linux has each

| Piece | What the driver does | Linux reference (7.0) |
|---|---|---|
| **Forcewake** | Takes the render and GT domains before touching their registers: `FORCEWAKE_RENDER_GEN9` `0xa278` with its ack `0xd84`, and `FORCEWAKE_GT_GEN9` `0xa188` with its ack `0x130044`. Masked writes, waits on the acks with a bound, and the shadowed-register list (writes that need no wake) | `intel_uncore.c` (Gen12 ranges), `gt/intel_gt_regs.h` |
| **Identity** | Device and revision from config space. Slices and DSS from `GEN12_GT_GEOMETRY_DSS_ENABLE` `0x913c`, EUs from `GEN11_EU_DISABLE` `0x9134`. CS timestamp frequency from `RPM_CONFIG0`, and RP0/RP1/RPn from `GEN6_RP_STATE_CAP` | `gt/intel_sseu.c`, `gt/intel_gt_clock_utils.c`, `gt/intel_rps.c` |
| **GGTT** | The 8 MiB of PTEs in BAR0's upper half: 4 GiB of global space. The driver owns one range for its rings, context images and status pages. **It never rewrites the PTEs that back the lit plane or the firmware framebuffer's aperture window** (found at M1 by reading the plane's surface address and walking the PTEs read-only) | `gt/intel_ggtt.c` (gen8 PTE format, `GEN8_GTT_ADDR`), stolen-memory base from config `0x5c` (`INTEL_BSM`) |
| **PPGTT** | A 4-level, 48-bit table per VM. PTEs carry the pinned pages' device addresses and a PAT index | `gt/gen8_ppgtt.c` (1,085 lines) |
| **PAT and MOCS** | The Gen12 PAT table (`GEN12_PAT_INDEX(i)` at `0x4800 + 4i`). MOCS is the TGL/ADL table, written into every context's image and the global registers. anv's `SET_PAT` indices are checked against it | `gt/intel_gtt.c` (`tgl_setup_private_ppat`), `gt/intel_mocs.c` |
| **Workarounds** | Gen12 and ADL-P's GT, context, engine and whitelist lists: about 50 entries (`gen12_ctx_workarounds_init` 17 `Wa_` lines, plus the GT, engine and `tgl_whitelist_build` lists that cover ADL-P). Each is applied and read back. The whitelist (`RING_FORCE_TO_NONPRIV`) is what user batches may write, and anv relies on it | `gt/intel_workarounds.c` (3,129 lines in all) |
| **Rings and LRC** | One ring and one context image per context per engine. The image's register state comes from the Gen12 offsets table, and its PDPs point at the context's VM. The **default image** is recorded as i915 records it: the first context is run, then its saved image is copied for every new one (`intel_gt.c:603`). Gen12 needs no golden render-state batch (`intel_renderstate.c` stops at Gen9) | `gt/intel_lrc.c` (1,978 lines), `gt/gen8_engine_cs.c` (flushes; on Gen12 including the **AUX-table invalidation** anv's compression needs, `gen12_get_aux_inv_reg`) |
| **Submission** | Execlist submit queue, context status buffer, the per-context seqno in the hardware status page. A user interrupt on each batch's end | `gt/intel_execlists_submission.c` |
| **Interrupts** | Gen11+ layout: `GEN11_GFX_MSTR_IRQ` `0x190010`, `GEN11_GT_INTR_DW(x)` `0x190018`, then the identity registers (`GEN11_IIR_REG_SELECTOR` `0x190070`, `GEN11_INTR_IDENTITY_REG` `0x190060`). One MSI. **MSI never arrived on this box under Ferrix so far** (`docs/N150.md` §13; the cause is in the drivers, `vtd-design.md` §0), so the driver also polls the status page at a bounded rate until a first interrupt is seen, as `usbhid` does | `gt/intel_gt_irq.c`, `i915_irq.c` (`gen11_irq_handler`) |
| **Hang and reset** | A per-engine heartbeat. On a hang the engine is reset (`GEN11_GRDOM` domains, not a full-GT or function-level reset, §4.3), the guilty context is banned, and its fences complete with an error. Never an FLR while the firmware's plane is the screen | `gt/intel_reset.c` |
| **Power** | RC6 and RPS by host requests. RC6 needs no DMC (DMC is the display's) | `gt/intel_rc6.c`, `gt/intel_rps.c` |

Linux's sizes, to calibrate: i915 without display, GVT, selftests, PXP and
perf is 142,421 lines of C. Its display alone is 198,986. xe without
display and tests is 123,363. The pieces above come to roughly 15,000 lines
of i915's code, and a Rust driver that leaves out what Ferrix does not need
(shmem backing, the shrinker, relocations, the command parser, GVT, PMU,
sysfs, debugfs) should come to about 12,000 to 18,000 lines, the uAPI front
end included.

### 3.5 Port i915, or write a driver: write one

**Porting** means running i915's GT and GEM code, about 140,000 lines, in a
ring-3 process over a compatibility layer. The host's `i915.ko` (Linux 7.0)
imports **1,252 kernel symbols**, 546 of them from DRM, dma-fence,
dma-resv and TTM. It depends on `drm_display_helper`, `ttm`, `drm_buddy`,
`cec` and `i2c-algo-bit`. xe imports the same count, and needs `drm_gpuvm`,
`drm_exec`, `gpu-sched` and `drm_gpusvm_helper` on top, plus GuC (xe has
execlists only as a debugging mode, `force_execlist`). Three things make
this worse than NVIDIA's port, which worked:

1. **No OS seam.** NVIDIA's RM is written OS-agnostic over about 260
   functions (`docs/NVIDIA.md` §2.2), so `nvrm` implemented those. i915 is
   written against Linux's internals: `shmem` page-cache backing for GEM
   objects, page-fault handlers for its mmaps, MMU notifiers for userptr,
   the shrinker, RCU, `ww_mutex`, `dma_resv`, and runtime PM. FreeBSD's
   linuxkpi, which hosts it, has grown over a decade.
2. **Faults a ring-3 driver cannot serve.** i915's `mmap_offset` fills
   pages from a fault handler. In Ferrix no page fault is served by a
   ring-3 driver (`interfaces/chardev/mod.rs`, N5: the fault windows of
   `docs/NVIDIA.md` §11.4 are not built). A port would need them, which is
   a large item change.
3. **It tracks Linux.** Every i915 fix comes with Linux's internal
   interfaces as they change. A Rust driver that answers a stable uAPI
   changes only when Mesa's use of it does.

A port's size: the compatibility layer, plus the DRM core pieces (GEM,
syncobj, prime, dma-fence, dma-resv, `drm_mm`), plus bring-up. **200 to
300 points**, most of them in code that is neither Ferrix's nor the
driver's.

**A new driver** does the twelve pieces of §3.4 and the front end of §2.1.
Genode shows the GT part at about 7,500 lines of C++. Fuchsia shows a new
driver carrying anv on Gen12. The new driver follows i915 file by file
(§3.6). Intel's public PRMs for Tiger Lake and Alder Lake (volumes 2 and 8)
are the second source. **About 82 points** (§7). This is the
recommendation.

### 3.6 Following i915 file by file (the customer, 2026-10-18)

The customer decided on 2026-10-18 to write the new Rust driver with the
i915 uAPI, staying as close to Linux's i915 as possible to save time. That
means mirroring its structure, its init sequence, its register definitions,
its workaround lists, its PAT and MOCS tables, its LRC layout and its
execlist submission. Each is translated file by file, with the source cited
for every function. Nothing that i915 already gets right is redesigned.
What Ferrix changes is only the layer underneath i915: how memory, MMIO,
interrupts and requests reach the driver.

**The license.** Every i915 file this driver translates is MIT, by SPDX tag
(`gt/*`, `gem/*`, `i915_query.c`) or by the full MIT text (`intel_uncore.c`,
`i915_reg.h`, `uapi/drm/i915_drm.h`). This was checked file by file in the
v7.0 tree, and the only GPL headers are in selftests. Ferrix is MIT too.
The terms are the MIT notice's:

* each translated Rust file starts with the i915 file's copyright lines
  (`Copyright © 2014 Intel Corporation`, and so on) and the MIT permission
  notice as that file carries it, then `Translated from Linux v7.0,
  drivers/gpu/drm/i915/<path>`;
* each function, table and register block names its origin
  (`/// i915: gt/intel_workarounds.c, gen12_ctx_workarounds_init`);
* tables are carried as data with i915's names and order, so a diff
  against a later Linux release is readable;
* the crate's README lists every file translated with its copyright
  holders. The crate is `license = "MIT"`, which `deny.toml` already
  permits.

The mapping. Here `xelp` is `src/lib/drivers/gpu/intel-xelp/src` (pure,
host-tested, no I/O) and `drv` is `src/user/system/native/drivers/gpu/intel/src`
(the process):

| i915 (v7.0) | Rust module | What is kept |
|---|---|---|
| `include/drm/intel/pciids.h` | `xelp::pciids` | `INTEL_ADLN_IDS`, and the ADL-P/RPL ids for later |
| `i915_pci.c`, `intel_device_info.c` | `xelp::device_info` | `adl_p_info` and the N subplatform as data |
| `i915_reg.h` (subset), `gt/intel_gt_regs.h`, `gt/intel_engine_regs.h`, `gt/intel_lrc_reg.h` | `xelp::regs::{i915_reg, gt, engine, lrc}` | names, offsets and bitfields as i915 spells them (`_MMIO`, `REG_BIT`, `REG_GENMASK`, masked-bit helpers) |
| `gt/intel_gpu_commands.h` | `xelp::commands` | the `MI_*`, `PIPE_CONTROL` and `XY_*` encoders |
| `intel_uncore.c` | `xelp::uncore` (tables) + `drv::uncore` (MMIO) | Gen12's forcewake domain ranges and shadowed list, `fw_domain_get`/`put` with acks, unclaimed-access checks |
| `gt/intel_sseu.c`, `gt/intel_gt_clock_utils.c` | `xelp::sseu`, `xelp::clock` | `gen12_sseu_info_init`, the topology query encoding, `RPM_CONFIG0` decoding |
| `gt/intel_ggtt.c`, `gt/intel_gtt.c` | `xelp::ggtt`, `xelp::gtt` | `gen8_ggtt_pte_encode`, the GGMS and stolen-memory probe, `tgl_setup_private_ppat` and the PAT table |
| `gt/gen8_ppgtt.c` | `xelp::ppgtt` | `gen8_pte_encode`, the 4-level walk, the scratch page |
| `gt/intel_mocs.c` | `xelp::mocs` | `tgl_mocs_table`, `init_global_mocs`, the per-context MOCS |
| `gt/intel_workarounds.c` | `xelp::workarounds` | the `i915_wa_list` builder, `wa_masked_en`/`wa_write_clr_set` and the rest, the Gen12 and ADL-P lists (GT, context, engine, whitelist), and the verify pass |
| `gt/intel_lrc.c` | `xelp::lrc` | `gen12_rcs_offsets`/`gen12_xcs_offsets`, `set_offsets`, `init_common_regs`, the PPGTT and ring state in the image |
| `gt/gen8_engine_cs.c` | `xelp::engine_cs` | `gen12_emit_flush_rcs`/`xcs` with the AUX invalidation, `gen8_emit_bb_start`, the fini breadcrumbs |
| `gt/intel_engine_cs.c`, `gt/intel_ring.c` | `xelp::engine` + `drv::engine` | engine table, `RING_MODE`, the status page, the ring |
| `gt/intel_execlists_submission.c` | `xelp::execlists` (descriptor, CSB parse) + `drv::execlists` | `lrc_descriptor`, `execlists_submit_ports` for the Gen12 ELSQ, `process_csb` and its Gen12 decoding. Its virtual engines and preemption come later |
| `gt/intel_timeline.c`, `gt/intel_breadcrumbs.c` | `drv::timeline` | per-context seqno in the status page, completion by the user interrupt |
| `gt/intel_gt_irq.c`, `i915_irq.c` (Gen11 part) | `xelp::irq` + `drv::irq` | `gen11_gt_irq_handler`, the identity-register dance, the master control |
| `gt/intel_reset.c`, `gt/intel_engine_heartbeat.c` | `drv::reset` | engine reset by `GEN11_GRDOM` domains, the heartbeat |
| `gt/intel_rps.c`, `gt/intel_rc6.c` | `drv::rps`, `drv::rc6` | host RPS (`gen6_rps_set`), RC6 enable for Gen11+ |
| `include/uapi/drm/i915_drm.h` | `xelp::uapi` | every struct of §2.1 with its layout pinned by the probe |
| `i915_getparam.c`, `i915_query.c` | `xelp::getparam`, `xelp::query` | the answers, in i915's order and with its errors |
| `gem/i915_gem_create.c`, `gem/i915_gem_mman.c`, `gem/i915_gem_context.c`, `gem/i915_gem_execbuffer.c`, `gem/i915_gem_wait.c`, `gem/i915_gem_tiling.c` | `xelp::gem::*` (parsing, checking) + `drv::gem` | argument checks and errors as i915 gives them. execbuffer is the softpin path of `eb_lookup_vmas`/`eb_validate_vma` with relocations refused |
| `drm_syncobj.c`, `drm_prime.c`, `drm_ioctl.c` (in `drivers/gpu/drm`, MIT) | `xelp::drm` + `drv::syncobj` | the common ioctls' checks and errors |

**Left out**, each by name:

* `display/` (199k lines), the DMC, the power wells;
* `gt/uc/` (GuC, HuC, GSC, SLPC) until §8 R1 needs it;
* `gvt/`, `pxp/`, `i915_perf.c` and OA, `i915_pmu.c`, `i915_hwmon.c`;
* `i915_cmd_parser.c` (Gen12 has none);
* the GEM backing stores (`i915_gem_shmem.c`, `_userptr.c`, `_ttm*.c`,
  `_lmem.c`, `_stolen.c`, `_region.c`) and the shrinker. A GEM object is a
  pinned VMO;
* relocations, the GTT mmap and its fence registers (`i915_gem_fence_reg.c`);
* `i915_request.c`, `i915_scheduler.c`, `i915_active.c` and `i915_sw_fence.c`,
  the dma-fence machinery. Their semantics (in-order per timeline,
  completion by seqno, a hang that marks the guilty request) are kept in
  `drv::timeline`, which is smaller and has no Linux fence callbacks;
* runtime PM, suspend and resume, sysfs, debugfs, the error-capture dumps
  (`i915_gpu_error.c`; a hang prints the ring and engine registers
  instead);
* every platform but ADL-P/N. The tables are translated with their other
  platforms' branches dropped and the condition named, so that adding TGL
  or RPL later is a lookup.

Kept differences, the only redesigns: memory is VMOs and pins rather than
`sg_table`; MMIO is an `IoMapping`; the interrupt is a port packet;
requests come from the chardev core; and waits are deferred replies rather
than sleeping threads (§4.1).

## 4. Where it lives in Ferrix

### 4.1 The driver

* `src/lib/drivers/gpu/intel-xelp`: the host-tested logic, its modules
  named after the i915 files they translate (§3.6). It holds the register
  maps and masked-write helpers, the GGTT and PPGTT entry encoders, the LRC
  image builder from the Gen12 offset table, the ELSQ and CSB state
  machine, the workaround lists with their verify pass, the PAT and MOCS
  tables, and the uAPI front end (one parser per ioctl, its layouts pinned
  against a probe's output). A model of the submission port, like the
  GC400's front-end model (`docs/GPU.md` §6.3), runs execlist traces
  through it.
* `src/user/system/native/drivers/gpu/intel`: the process. It is a
  **native** program on `ferrix-rt` with one event loop on a port:
  chardev requests, the interrupt packet, and deadlines through
  `port_wait`'s deadline. It does not need `nvrm`'s Linux personality,
  because nothing in it sleeps inside foreign code. A request that must
  wait (`GEM_WAIT`, `SYNCOBJ_WAIT`, a full ring) is answered later, when
  its fence signals or its deadline passes. Whether the chardev core takes
  replies out of order is to be confirmed in its code before M3 (its table
  is keyed by request id).
* **devmgr**: a match for `8086:46d4` (the ADL-N ids, `INTEL_ADLN_IDS`)
  with class 03 to the `Gpu` kind, under a kernel option of its own
  (`ferrix.intelgpu`) at first. The kind brings the pin budget of N0f, set
  much larger for an integrated GPU that has no memory of its own (§8, R5),
  and the isolated-interrupts mark (§4.4).

### 4.2 The node, and how Mesa finds it

The driver asks for a chardev control and lists only `RENDER_MINOR`. It
then serves `/dev/dri/renderD<N>`, numbered by the kernel, every request
forwarded undecoded as `nvrm`'s are (`interfaces/chardev`,
`ferrix_chardevctl::node`). The core today names major 195's minors after
NVIDIA's nodes, and a HELLO needs only to be non-empty, so a render-only
HELLO fits as built. That is checked in code under K3 before it is relied
on. libdrm finds the PCI identity through `/sys/dev/char/226:<N>/device`
(`docs/SYSFS.md`), which the render node already gets. Mesa's userspace
is Debian 13's `mesa-vulkan-drivers` (anv: `libvulkan_intel.so`,
`intel_icd.x86_64.json`) and `mesa-libgallium` (iris) at 25.0.7, which
`fetch-yserver.sh` already pins. Today only lavapipe is extracted from
them, so no Mesa build is needed. GEM objects are VMOs, and their mmaps are
`MapReply::Vmo`. Dmabufs are VMO dmabufs, so hyprix's
`zwp_linux_dmabuf_v1` path (`compositor/hyprix/src/dmabuf.rs`) can map
them.

### 4.3 The screen

`ferrix.efifb` **yields** the function that holds the framebuffer: every
call needing `MANAGE` on `00:02.0` answers `BAD_STATE`, and devmgr cannot
start a driver for it (`docs/DISPLAY.md` §8, step 4). That rule is right,
since a driver could reset or remap the memory under the screen, and this
design keeps its intent. Under `ferrix.intelgpu`:

* the framebuffer is not lent to efifb; the Intel driver holds the
  function;
* the Intel driver serves `card0` itself, as a **copying card**: the same
  `copies` contract efifb and `nvrm` have (`docs/DISPLAY.md` §2.1). It
  copies flushed rectangles into the lit plane's surface through a
  write-combining BAR2 window, and later with a BCS blit;
* it keeps the GGTT entries behind that surface and never resets the
  display or the function (engine resets only, §3.4). The kernel's panic
  screen keeps its write-combining hold on the same pages (`DISPLAY.md` §8
  step 3).

Before K4 lands, M1 to M3 run with neither option. The boot console stays
the screen's (`ferrix.fbcon`), the driver touches no GGTT entry it did not
allocate, and the box is reached over ssh (`fx.sh`).

### 4.4 Isolation and DMA

* **Device to memory.** Every GEM object is a VMO pinned into the device's
  IOMMU domain (`vmo_pin`, addresses identity-mapped, `iommu.rs`), so the
  GPU reaches only pinned pages. That holds only when `dmar0` translates.
  On the box today neither unit is driven (degraded trusted mode,
  `n150-linux/FINDINGS.md` item 3). `vtd-n150` (unlanded) brings `dmar0` up
  for interrupt remapping with translation **off**, because of the RMRR
  (`vtd-design.md` §3.9(a)). Its slice 2, an identity-mapped RMRR domain
  for `00:02.0` with the driver's pins added to it, is this design's K2.
  The rule, as the consultant approved it (ledger 2352, D4 OK IF (a) to
  (d)): **M1 and M2 may run on the box untranslated; no client reaches the
  GPU until `dmar0` translates.** The GPU executes what programs hand it,
  and without translation one bad PPGTT entry is DMA anywhere. The rule is
  the kernel's, not the driver's promise:
  * (a) while the unit behind a function does not translate, the chardev
    core **refuses a HELLO listing any client minor** (`RENDER_MINOR`
    included) for that function, so no `renderD<N>` exists before K2. This
    is a chardev check with a negative control (with the refusal removed,
    the check fires);
  * (b) M1 and M2 run only under `ferrix.intelgpu`, which is off by default
    and refused in a release image's default command line. DEGRADED is
    announced at boot as today, and the boot line names `00:02.0` as
    untranslated DMA;
  * (c) `VULNERABILITY-ANALYSIS.md` V-03 states that under
    `ferrix.intelgpu` without K2, a ring-3 driver with `MANAGE` on
    `00:02.0` can DMA anywhere. The driver is trusted like the kernel in
    that mode, and no isolation claim covers the box;
  * (d) "the driver's own command streams only" is a property of the
    driver, recorded as such, and never cited as evidence.
* **Client to client.** Each VM is its own PPGTT, and batches run
  non-privileged from it (`MI_BATCH_BUFFER_START` with the PPGTT address
  space). The hardware itself refuses privileged commands and register
  writes outside the whitelist in such batches. This is the driver's
  property, and the hardware's, outside the item, as it is i915's on Linux.
  Gen12 has no command parser (`CMD_PARSER_VERSION` 0), and Linux relies on
  the same hardware rule.
* **Driver to clients.** As for `nvrm`: copies only within a waiting
  request, dmabufs and sync files through the core's calls, no reach into a
  process that is not waiting (V-10 in `VULNERABILITY-ANALYSIS.md`). The
  node is 0666 like NVIDIA's, so V-11's parser-surface argument applies to
  this driver's front end too, and its parsers are fuzzed.
* **Interrupts.** The `Gpu` kind's isolated-interrupts mark requires every
  unit to remap (`L.device.27`). That needs `vtd-n150` slice 1 on the box,
  so the driver's first boots poll (§3.4) until it lands.

## 5. What the certified item must review

| # | Change | Ring | Points |
|---|---|---|---|
| K1 | `vtd-n150` on the box (designed, reviewed OK IF, branch unlanded) | core | not counted: its own row |
| K2 | `dmar0` translating: an RMRR identity domain for `00:02.0`, refused when it overlaps usable RAM, never detached; the driver's pins added to it (`vtd-design.md` §3.9(b)) | core | 5 |
| K3 | A render-only chardev driver: devmgr's `Gpu` kind for Intel, the ADL-N match under `ferrix.intelgpu`, a HELLO of `RENDER_MINOR` alone, the budget sized for an iGPU | load (chardev), devmgr outside | 3 |
| K4 | The screen's hand-over: under `ferrix.intelgpu`, the function is not yielded, the Intel node may be a copying card (`display::check`'s rule), and the lent range's GGTT entries and the panic screen's hold are argued | load (display) and item (device yield) | 5 |
| K5 | `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` and `IMPORT_SYNC_FILE` on chardev dmabufs: one fence slot per dmabuf that the serving driver sets with a sync file of its own, and an export that hands out a `sync_file` | load (chardev dmabuf, sync) | 5 |
| K6 | Write-combining VMO mappings for a chardev mmap reply (`MAP_VMO` with `MAP_WRITE_COMBINING`, refused today) | item (user memory types) | 2 |
| K7 | Client-page pins for `I915_GEM_USERPTR` (`request_pin`, not built; designed in `docs/NVIDIA.md` §4.3) | load | 3, only if a gate needs it |

The driver itself, Mesa, and the uAPI front end are ring 3 and outside the
item. `check-item-boundary.py` gains a line refusing any path under
`src/kernel` that names `i915`/`xe`/`intel-xelp`, as it does for `nvidia`.
It lands with K3 or before. Their review asks for what CONVENTIONS.md
lists: negative controls that fire, traced `unsafe`, coverage carried.

### 5.1 The consultant's design review (2026-10-18, ledger line 2352)

The certification consultant reviewed version 2's K2 to K7 and D4. The
verdicts, with their conditions, are binding on the code that follows:

* **D4, the isolation rule: OK IF (a) to (d)**, folded into §4.4. K2
  before M2 is not required.
* **K2 (core): OK IF (1) to (7), to be built only after K1 (`vtd-n150`)
  lands.**
  1. Its own design note in `vtd-design.md` §3.9(b), reviewed at code
     review, with ids reserved first.
  2. The identity domain maps exactly the RMRR's pages. It is refused
     (function left untranslated, DEGRADED, said) when the RMRR overlaps
     usable RAM by the memory map or is not page-aligned. Host-tested on
     `box-2026-10-17/DMAR.bin` and on malformed tables.
  3. The RMRR's frames are reserved from the frame allocator for the
     boot's life.
  4. A driver pin that overlaps the RMRR is refused, and an unpin can never
     remove an RMRR entry. Each is a check with a control.
  5. Never detached means across driver restarts too. The pin quarantine
     (a3f23da4) applies to the driver's pins in this domain, and a restart
     re-attaches nothing.
  6. A boot check under QEMU's intel-iommu with an RMRR (by the patch route
     if needed). If that cannot be built, host tests plus a BACKLOG row for
     the box run.
  7. Coverage carried, and `unsafe` traced.
* **K3 (load: chardev; devmgr outside): OK IF**
  1. A render-only HELLO is checked in the chardev core's code, with minor
     numbering not keyed to NVIDIA's 195 layout (render minors as Linux's
     128 and up), and a check that no `card0` is made.
  2. The budget may vary with the GPU kind, provided the kind is decided
     by the kernel-visible match (vendor and class of the published node),
     never by what the driver says.
  3. The bound is kernel-side and does not rely on devmgr: a stated
     fraction of usable RAM with an absolute cap, at most one half of
     usable RAM above the watermark. Pinned frames are charged to the
     driver's job, refusal fails closed (`EDQUOT`), there is a check at
     the cap with a control, and MEMORY-AND-TIMING states the formula.
  4. The `check-item-boundary.py` line lands with K3 or before.
* **K4 (load: display; item: the device yield): OK IF**
  1. The yield exception is keyed to the kernel option and the node's
     identity (Intel, class 03, the function holding the framebuffer),
     never to a driver's request.
  2. The kernel **refuses FLR and any device-reset call** on that function
     while it holds the framebuffer: enforced, with a check and a control,
     not only the driver's promise of engine resets.
  3. `display::check`'s copying-card rule widens to that node alone, by
     identity, with a control.
  4. This document and `DISPLAY.md` state that the panic screen's
     visibility under `ferrix.intelgpu` depends on the driver keeping the
     lit plane's GGTT entries, so the kernel does not claim it. The
     kernel's write-combining hold on the framebuffer pages is unchanged.
  5. Under K2 the framebuffer's pages are inside the RMRR identity domain,
     or the design says how the plane's scan-out is reached.
* **K5 (load: chardev dmabuf and sync): OK IF**
  1. Ledger 647's S-rules apply unchanged: only the serving driver
     signals, a dead driver's fences signal with an error, and nothing
     reaches a process that is not waiting.
  2. Only the dmabuf's serving driver sets its fence slot. A client's
     `IMPORT_SYNC_FILE` is passed to the serving driver as data, and never
     makes the kernel or another client wait on it.
  3. `EXPORT` installs a `sync_file` that snapshots the slot at the call,
     as Linux does.
  4. The slot's lock is a stated leaf, bounded, in MEMORY-AND-TIMING.
  5. chardev checks for export, import, a dead driver, and a foreign
     driver's attempt to set the slot, each with a control.
* **K6 (item: user memory types): OK IF**
  1. WC only for a VMO in the serving driver's own table, and only on
     x86-64 at first. Other architectures refuse, as today, until each
     one's attribute (Normal-NC) is argued.
  2. The memory-type aliasing argument is written into the requirement's
     rationale. The frames stay write-back in the direct map while a user
     mapping is WC (the SDM's aliasing rules and i915's own practice on LLC
     parts). The kernel never relies on a coherent read through the alias.
     WC buffers are drained (an `sfence` or a named serialising step)
     before frames of a WC-mapped VMO are unmapped and reused.
  3. A check that a non-driver VMO, or a non-x86 boot, refuses
     `MAP_WRITE_COMBINING`, with a control.
* **K7: NOT YET, by design.** Deferred until a gate needs it, and then
  reviewed on its own against `NVIDIA.md` §4.3.
* **Ids.** Reserved per K item (K2 core `L.iommu`; K3 and K5 `L.object` or
  chardev rows as each requirement lands; K4 item; K6 item) in **one
  docs-only reservation landing before any code**. None for K7 now.
* **C1, answered (the kernel side, K3, landed caf25c0d5).** The chardev
  core takes replies in any order: `chardev_reply` looks a request up by
  its id, so any outstanding request may be answered at any time, once.
  A second reply to an id, or a reply to an abandoned request, is
  `BAD_STATE`. A signal abandons the client's request without telling the
  driver: the client sees `EINTR`, not a restart, and the driver's later
  reply is `BAD_STATE`, which the driver must tolerate. At most 256
  requests are outstanding per control, deferred `GEM_WAIT` and
  `SYNCOBJ_WAIT` replies included, and the 257th is `EBUSY` to the client.
  So the driver's waits are deferred replies within that bound (§4.1), and
  a long wait on a busy node costs other clients room.
* **C2.** The fake back end of `test-intel-uapi` exercises the core
  changes but not the GPU's DMA, so no K2 claim may cite it.

**The order of work** (the coordinator, 2026-10-18): the `intel-xelp` lib
crate and its host tests now; then the reservation landing; then K3, which
M1 needs under `ferrix.intelgpu`; then M0 and M1 on the box.

## 6. What can be tested without the hardware

QEMU emulates no Intel GPU, and nazuna has none (two NVIDIA cards and an
AMD iGPU). So:

* **Host tests** (`cargo test`, CI): every encoder and table above,
  pinned against i915's numbers. The LRC image's offsets, the PTE formats,
  the workaround list with masks, MOCS and PAT. The ELSQ/CSB state machine
  through the submission model, with traces of the CSB events that M2
  records on the box replayed. Every uAPI parser, its layout pinned against
  Appendix A's probe, fuzzed.
* **A fake back end in QEMU** (`test-intel-uapi`, x86-64 KVM): the same
  driver with its GT replaced by a back end that completes every batch at
  once and runs nothing. It answers as `8086:46d4` on a fake node and
  publishes `renderD<N>`. Mesa's anv runs against it with `INTEL_NO_HW=1`,
  so `vulkaninfo` and an offscreen `vkcube` exercise every ioctl of M3:
  objects, mmaps, contexts, VMs, execbuffer, syncobjs, sync files and
  dmabufs, with garbage pixels. That is the gate for the front end, K3, K5
  and K6 on every architecture's runner that can boot it.
* **The kernel changes**: K3 to K6 have QEMU checks as the chardev core's
  have now (`chardev/check.rs`). K2's RMRR domain is host-tested on the
  box's DMAR (`box-2026-10-17/DMAR.bin`). Its boot check needs QEMU's
  intel-iommu with an RMRR, which may need the patch route of
  `vtd-design.md`.
* **On the box only**: M0 to M5's exits. Forcewake, the fuses, the GGTT,
  every command executed, interrupts, resets, RPS, and every picture.

## 7. Milestones and points

| Slice | What | Points |
|---|---|---|
| **M0** | Linux reference on the box (needs a BOX-HOLDER slot): the inspection Linux with `i915.ko` and `tgl_guc_70`/`tgl_huc`/`adlp_dmc`. Record dmesg, `i915_engine_info`, `i915_gpu_info`, the fuses, RP0/RPn, and the plane's surface and its GGTT entries. Then boot with `i915.enable_guc=0` and run `vulkaninfo` and `vkcube` on anv, to show execlists work on ADL-N | 2 |
| **M1** | **The device answers.** devmgr starts the driver (K3's match). BAR0 mapped, forcewake taken with acks, the identity read: 24 EUs and the DSS mask, CS timestamp frequency, RP0/RPn. The GGTT walked read-only, with the plane's entries found. One `intel:` line on the box's log per fact, compared with M0 | 5 |
| **M2** | **A GPU command executes.** The driver's GGTT range, a 4-level PPGTT, PAT and MOCS, the workaround lists read back, RCS0's ring and LRC with the recorded default image, an execlist submission and its CSB. Then `MI_STORE_DWORD_IMM` into a pinned page, a `XY_FAST_COPY_BLT` on BCS0, and a batch from a PPGTT. Completion by MSI, or by the bounded poll, both counted. A deliberately hung batch is reset and banned | 16 |
| **M3** | **anv's `vulkaninfo`.** The front end of §2.1, syncobjs and sync files, `renderD<N>`, the fake back end and `test-intel-uapi` (§6), anv and iris extracted from the pinned debs. Exit: on the box, `vulkaninfo` names "Intel(R) Graphics (ADL-N)" through anv, and `eglinfo` names iris. Needs K2, K3, K5 (K6 if anv maps WC) | 18 |
| **M4** | **`vkcube`.** Several contexts and VMs, implicit sync on shared objects, dmabufs to hyprix (`zwp_linux_dmabuf_v1`) or the WSI's copy path (`MESA_VK_WSI_DEBUG=sw`), RPS and RC6, recovery under `GET_RESET_STATS`. Exit: `vkcube` turning in a hyprix window on the box, `glmark2-es2-wayland` on iris, and an offscreen frame's hash within range | 12 |
| **M5** | **Endfield's renderer.** DXVK on anv in Wine, presenting through yserver by the WSI's copy path (no DRI3 yet, `docs/STEAM.md`), the pin budget for a game's working set, and Mesa's shader cache on `/data`. Unity's *Spaceship Demo* (`test-unity`) first, then the GRYPHLINK launcher's Qt UI and the game. Exit: frames per second against lavapipe's on the same scene, and the load average | 14 |
| **M6** | **The screen.** K4: `card0` from the Intel driver over the lit plane, efifb's role taken, flushes by BCS blit | 6 |

* **Kernel (§5):** K2 5, K3 3, K4 5, K5 5, K6 2: **20 points**, plus K7's
  3 if a gate needs userptr. K4 belongs to M6; the rest are on M3's path.
* **Driver and gates:** M0 to M5 come to **67 points**, and M6 to 6.
* **In all, to Endfield on anv: about 82 points** (M0 to M5, 67, and K2,
  K3, K5 and K6, 15). With the screen (M6 and K4) about 93, and about 96
  with K7. That is against
  the 150 to 250 first guessed, and 200 to 300 for a port (§3.5).

M2 is the widest estimate. A wrong context image or a missing workaround
shows up as a GPU that does nothing, and the first signal is an empty CSB.
M0's register dumps are what M2 is compared against. M5 is the next widest:
it is where Mesa's real use of the uAPI meets corners the fake back end
never reached.

**Order.** M0 first, because it decides execlists on real hardware for 2
points. M1 and M2 can run on the box before VT-d translates (§4.4). M3
waits on K2, so `vtd-n150` and its slice 2 are on this path's critical line
and should be started with M1. K5 and the fake back end can be built beside
M1 and M2, without the box.

## 8. Risks

1. **Execlists on ADL-N** have a defect i915 hides because it uses GuC by
   default. M0 tests that first. If it holds, GuC submission becomes a
   slice of about 10 points (firmware load through the GGTT, CT, ADS).
2. **VT-d on the box** (K1, K2). Translation for `00:02.0` is the item's
   hardest change here. If the RMRR identity domain breaks the firmware's
   scan-out, the box's screen goes dark. `ferrix.vtd=off` (`vtd-design.md`
   §3.10) is the way back.
3. **MSI on the box** has never arrived for any driver (`docs/N150.md`
   §13). The driver polls, at a bounded rate, until it does. Frames still
   work, at some cost in latency and power.
4. **Mesa's corners.** anv on i915 may use an ioctl path no grep found:
   `CONTEXT_IMAGE`, TR-TT register writes for sparse, OA. Each one meets a
   loud refusal that names the ioctl and parameter (`intel: unserved …`).
   Missing ones are added as M3 to M5 find them. `ANV_DEBUG=no-sparse` and
   `INTEL_DEBUG=noccs` are the fallbacks when sparse or compression turns
   out to be the cause.
5. **Memory.** The iGPU has no memory of its own: every buffer is pinned
   RAM, with no shrinker. N0f's budget (an eighth of RAM, at least 1 GiB,
   sized for `nvrm`) is too small for a game on 16 GB. The `Gpu` kind
   needs a larger budget for an integrated part, and the 16 GB box has no
   swap (`docs/N150.md` §6).
6. **Speed.** 24 EUs at up to about 1 GHz are well under Endfield's
   minimum (a GTX 1060). The goal is to take the load off the cores and
   keep Qt's UI responsive, not to make the game playable. M5 records
   frame rates against lavapipe's.
7. **The anti-cheat.** A GPU driver changes nothing ACE is shown beyond a
   real Intel device's identity through Vulkan. Nothing in this design is
   built to disguise anything (`docs/N150.md` §8).

## 9. Decisions for the customer

* **D1**: a new Rust driver, not a port of i915 (§3.5). **Decided** by the
  customer on 2026-10-18, with the i915 uAPI (D2), following i915 file by
  file (§3.6).
* **D2**: the i915 uAPI, not xe (§2). **Decided** 2026-10-18.
* **D3**: execlists, no GuC, decided again after M0 (§3.2).
* **D4**: the order M0, then M1 and M2 on the box untranslated, with M3
  waiting on `dmar0` translating (§4.4). This is subject to the
  consultant's verdict.

## 10. Where it stands

**M3's hardware-independent part (branch `intel-uapi`, 2026-10-18).**

* **The front end** is `intel-xelp`'s `frontend` module: every ioctl of
  §2.1, the opens, mmaps and releases, and the chardev core's dmabuf events,
  answered as Linux v7.0's DRM core and i915 answer them on Gen12 (each
  function names the one it follows). Waits are deferred replies, at most 64
  at once (C1). An execbuffer that reads or writes a shared object sets its
  dmabuf's slot (ledger 2360). No memory is allocated, so every table has a
  fixed size (`frontend::limits`). The uAPI's numbers and layouts are now
  printed by `probe/i915.c` from the v7.0 headers, and two tests hold
  `uapi.rs` to every line, both ways.
* **The back end interface** (`frontend::Backend`) was agreed with M2
  through main on 2026-10-18: Ok, Hung and Cancelled completions;
  `service`, `next_deadline` and `completed`; `ContextParams`; `bind`
  refusing a PAT index past 3. The front end submits a request only once its
  fences have signalled, in order per hardware context, and unbinds only
  after completion. The serving loop is
  `src/user/system/native/drivers/gpu/intel/src/serve.rs`, which M2's
  `intel` adds to its module tree. Its interrupt goes to the loop's port
  under `KEY_INTERRUPT`.
* **Departures from i915**, each said where it is made: relocations and
  unpinned objects are refused (i915 refuses relocations on Gen12 past TGL
  too). A syncobj cannot be exported as an opaque descriptor, because the
  chardev core has no object for one; that would be a kernel interface, so
  it answers `EOPNOTSUPP` and says so once. A binding over one still in use
  answers `EAGAIN` instead of sleeping. Userptr (K7), the legacy mmap,
  pread, pwrite and perf answer `ENODEV`. Every unserved call is said once a
  boot, by name.
* **`cargo xtask test-intel-uapi`** boots `intel-fake`
  (`drivers/gpu/intel-fake`) as `intel`, on a QEMU `pci-testdev` that answers
  as 8086:46d4, class 030000, at 00:02.0 (QEMU patch 0006), under
  `ferrix.intelgpu` with VT-d translating. The volume is
  `fetch-intel-uapi.sh`'s: Debian 13's Mesa 25.0.7 with anv and iris kept.
  In its first passing run the kernel published `renderD128` (K3).
  `vulkaninfo --summary` named "Intel(R) Graphics (ADL-N)" with
  `DRIVER_ID_INTEL_OPEN_SOURCE_MESA` and exited 0, after anv's
  dmabuf-sync-file probe (K5). `vk-offscreen` made a device, an image and a
  mapped buffer, submitted a clear, waited on its fence and read back. Its
  pixels were wrong, as they must be on a back end that runs nothing.
  `eglinfo -B -p surfaceless` named "Mesa Intel(R) Graphics (ADL-N)" for GL
  core, compatibility and ES, and exited 0. The run said no `unserved`
  line. Only that gate's image may carry the fake: xtask refuses an `intel`
  that is `intel-fake` in every other image. It is never cited for K2 (C2).
* **GBM** on the render node fails, because GBM wants a card node. That is
  M4's business, through the WSI.
* **The box checkpoint** is `tools/common/n150/intel-checkpoint.sh
  [BOX]`. It runs anv's `vulkaninfo` and iris's `eglinfo` on the box's
  running Ferrix, from the same Mesa copied to `/tmp`, and keeps the output in
  `~/.local/share/ferrix/n150-linux/evidence/`. Until K2 translates 00:02.0
  and M2's `intel` serves the node, it records the kernel's refusal.
* **Left for M3's exit**: M2's back end behind `serve.rs`, K2, and a box run.

2026-10-18, later: the customer decided D1 and D2, and §3.6 was added (the
i915 file to Rust module mapping and what is left out). The consultant's
design review is in (§5.1, ledger 2352). The lib crate `intel-xelp`
(tables, encoders, parsers and their host tests, no I/O, no item change) is
started on branch `intel-xelp`. Its tables are generated from i915 files
vendored under the crate's `i915/` by `tools/common/gen/gen-intel-xelp.py`.

**M0 is done (2026-10-18, on the box's inspection Linux, Linux 7.0.0-29 and
Mesa 26.0.8; raw output in `~/.local/share/ferrix/n150-linux/intelgpu-m0/results/`).**

* **Default i915 on ADL-N is GuC.** GuC 70.49.4 (`tgl_guc_70.bin`), HuC 7.9.3
  authenticated, "GUC: submission enabled", SLPC and RC by GuC. Linux drives
  the iGPU's VT-d unit ("VT-d active for gfx access").
* **Execlists work.** After an FLR, i915 with `enable_guc=0` initialised with
  "GuC disabled" and no hang. anv's `vulkaninfo` named "Intel(R) Graphics
  (ADL-N)", and `vkcube --wsi display` drew 600 frames in 13.6 s, the same as
  under GuC. Both are bound by the panel, not the GPU. D3 stands.
* **The WOPCM lock.** Once GuC has run, the GuC WOPCM registers stay locked
  until the device resets. i915 then refuses execlists (`__uc_check_hw`,
  "Enabling uc failed (-5)", the GT wedged), and a PCI FLR clears it. So
  the driver checks `uc_is_wopcm_locked` first ([`uc`](../src/lib/drivers/gpu/intel-xelp/src/uc.rs)).
  On a warm boot after an OS that ran GuC (the inspection Linux does), it
  needs an FLR. That is fine for M1 to M3, which run without `ferrix.efifb`.
  Under K4, where the kernel refuses FLR while the plane is the screen, a
  locked WOPCM means the driver must say so and stop, and the screen stays
  the firmware's. The consultant's K4 (2) is to be read with this case in
  mind.
* **The GT.** 1 slice and 2 dual-subslices (mask 0x3), each with 12 EUs
  (mask 0x3f3f, EU pairs 3 and 7 fused off), so 24 EUs. Slice power gating
  only. RPn 200 MHz, RP1/RPe 300 MHz, RP0 1000 MHz. The engines are rcs0,
  bcs0, vcs0 and vecs0: **vcs2 is fused off** on ADL-N, so the driver
  applies `engine_mask_apply_media_fuses` and does not assume `adl_p_info`'s
  list.

**M2's code is built, not yet run on the box (2026-10-18, branch `xelp-m2`).**
`intel-xelp` holds the 4-level PPGTT (`ppgtt`, `gen8_ppgtt.c`), binds into
the driver's own GGTT window with each entry read back, the PAT and MOCS
read back, execlists' ports and CSB (`process_csb`, `wa_csb_read`), contexts
from a recorded default image (`submission`, `lrc_init_state`), the context
workarounds read back by SRM, and one engine's reset with the hung context
skipped, banned and replayed (`reset`, `intel_reset.c`). They run on a model
of the engine's execlist port in host tests (51). The driver goes on from
M1's lines to M2 under `ferrix.intelgpu`, in four checkpoints the box runs
one at a time (`m2::CHECKPOINT`, built by
`~/.local/share/ferrix/n150-linux/intelgpu-m2-build.sh` and run by
`intelgpu-m2.sh BOX_ADDR cpN`, which prints what each proves): 1 the PAT,
MOCS, workarounds, engines and a PPGTT read back; 2 the default images and
`MI_STORE_DWORD_IMM`; 3 the blits (one onto the lit plane, hashed, left on
the screen for a photograph) and a PPGTT batch; 4 a hung batch reset and
banned. Every completion is counted as seen by interrupt or by the 1 ms poll.

**The kernel items (intel-gpu-kernel).** K3 landed in caf25c0d5 (ledger
2356): `ferrix.intelgpu`, the render-only HELLO, the refusal of every node
while 00:02.0's DMA is untranslated, and the iGPU's own pin budget. Its box
evidence is a `docs/BACKLOG.md` row, owed with M1. K5 is on branch
`intel-k5` and awaits the consultant (its request is
`~/.local/share/ferrix/n150-linux/intel-gpu-k5-review-request.md`). It adds
one fence slot per dmabuf, set only by its own driver with
`chardev_dmabuf_fence` (0x1065). `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`
snapshots the slot. `IMPORT_SYNC_FILE` reaches the driver as
`Op::DmabufImport` (dmabuf cookie in `arg`, flags in `cmd`, the fence's
kind in `pages`, its cookie or status in `file`), and the driver answers it
as an ioctl. **The driver's contract (ledger 2360).** The slot stands for
the driver's latest queued GPU access of any kind to the buffer, reads
included. So the driver sets it with `chardev_dmabuf_fence` for every
execbuffer that reads a shared object, as well as for every one that writes
it. A fence exported for READ or for WRITE then waits for both, which is at
least what Linux's reservation object gives. K6 is on branch `intel-k6`: an
mmap answer of `MAP_VMO | MAP_WRITE_COMBINING` makes the driver's own
anonymous VMO write-combining for every user mapping of it from then on.
This works only on x86-64, only before anything maps the VMO, and only for
a VMO the driver never copies with `vmo_read`/`vmo_write` (both are
`BAD_STATE` for it). Elsewhere the program's mmap is `EIO` (L.user.132-133).
K4 is on branch `intel-k4`. Under `ferrix.efifb ferrix.intelgpu` the
framebuffer is lent to 00:02.0's own node: an aperture of the visible
lines, which the driver maps with `io_mapping_map_combining`. The function
is not yielded, the driver may be given a copying card, and every
configuration write to the function is refused while it holds the
framebuffer (`DISPLAY.md` §8.1, L.device.46-48). Since the kernel refuses
the FLR that M0 found the WOPCM lock needs after a warm boot from a
GuC-running OS, a driver finding the WOPCM locked under K4 must say so and
stop, and the screen stays the firmware's. **Under K2** the lit plane
scans out of stolen memory, `0x7c000000`-`0x803fffff` on the box, which is
the RMRR. The aperture the kernel lends is a GGTT window onto it. So K2's
identity domain covers the plane's own reads, and the lent aperture is a
CPU mapping that no domain needs to cover (the consultant's K4 (5)). K2
waits for vtd-n150.


2026-10-18: this design, on branch `intel-gpu-design`, docs only. Nothing
is built. Nothing has run on the box: it was held by `fxvalidate`, and the
inspection Linux of 2026-10-16 had no `i915.ko` loaded (simpledrm only),
so its dmesg holds no GPU facts beyond `lspci`. The design goes to the
certification consultant for a design review of K2 to K7
(`~/.local/share/ferrix/n150-linux/intel-gpu-review-request.md`). The
references are fetched to `~/.local/share/ferrix/intel-gpu-ref/linux` (the
v7.0 tag, sparse: `drivers/gpu/drm/{i915,xe}`, `include/uapi/drm`,
`include/drm/intel`, 37 MB). Mesa 26.1.0-devel is in
`~/aosp17/external/mesa3d`.

## Appendix A: the probe that printed the numbers

Compiled on nazuna against Linux 7.0's UAPI headers, run once, and kept out
of the tree:

```
gcc -w -D__user= -I/usr/src/linux-headers-7.0.0-29/include/uapi \
    -I/usr/src/linux-headers-7.0.0-29-generic/arch/x86/include/generated/uapi \
    -I/usr/src/linux-headers-7.0.0-29/arch/x86/include/uapi -o ioctls ioctls.c
```

`ioctls.c` includes `<drm/drm.h>`, `<drm/i915_drm.h>`, `<drm/xe_drm.h>`,
`<linux/dma-buf.h>` and `<linux/sync_file.h>`, and prints each
`DRM_IOCTL_*` value of §2.1. The struct sizes it printed:
`drm_i915_gem_execbuffer2` 64 bytes, `drm_i915_gem_exec_object2` 56. For
comparison, the xe ioctls: `XE_DEVICE_QUERY` `0xc0286440`, `XE_GEM_CREATE`
`0xc0386441`, `XE_GEM_MMAP_OFFSET` `0xc0286442`, `XE_VM_CREATE`
`0xc0206443`, `XE_VM_DESTROY` `0x40186444`, `XE_VM_BIND` `0x40886445`,
`XE_EXEC_QUEUE_CREATE` `0xc0306446`, `XE_EXEC_QUEUE_DESTROY` `0x40186447`,
`XE_EXEC_QUEUE_GET_PROPERTY` `0xc0286448`, `XE_EXEC` `0x40386449`,
`XE_WAIT_USER_FENCE` `0xc048644a`. Before the front end is written, the
probe becomes `src/lib/proto/linux-abi`'s `probe/i915.c`, claimed line by
line as `virtgpu.c` is.
