# NVIDIA's own driver on Ferrix

Version 2, 2026-10-02. Version 1 was the draft for the customer, who
answered §9 the same day; the design below follows those answers.
The customer chose the path that day (`docs/BACKLOG.md` Decisions): a real
NVIDIA driver, not nouveau and not NVK. NVIDIA's
[open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules)
is ported so that its OS-agnostic core runs as a Ferrix driver. NVIDIA's own
userspace (`libnvidia-*`, the Vulkan and GL ICD, and later CUDA) then runs
unmodified under the Linux personality. It talks to `/dev/nvidiactl`,
`/dev/nvidia0`, `/dev/nvidia-modeset` and `/dev/nvidia-uvm` as it does on
Linux. This document is the feasibility pass (§2) and the design that comes
out of it. Nothing in it is built yet beyond the probe in §2.3.

It replaces the sketch in `docs/GPU.md` §4 ("Path B") and gives
`docs/roadmap/stage-21-bare-metal-gpu-ferrix.md` its first sizing. One of that
sketch's premises no longer holds: it called glibc-built closed libraries
"the part most likely to decide the whole path". Since then Chrome and
yserver run from data volumes through Debian's `ld-linux`, and NVIDIA's
libraries load the same way.

## 1. What this is, and what it is not

This is:

* **the driver**: NVIDIA's resource manager (`src/nvidia`, "RM") and its
  display half (`src/nvidia-modeset`, "NVKMS"), built from NVIDIA's own
  makefiles. They run in a ring-3 process, `nvrm`, under an OS layer written
  for Ferrix (§4.1–4.3);
* **the device files**: a kernel core that serves NVIDIA's character
  devices, `/proc/driver/nvidia` and the rest of what the userspace probes,
  by forwarding each request to `nvrm` (§4.4);
* **how frames reach hyprix, yserver, Chrome and Steam** (§4.6);
* **the milestones N0–N6 with points** (§7).

It is not nouveau, NVK or Nova. Those were weighed in `docs/GPU.md` §4, and
the customer has now decided against them.

It is not a GPU of Ferrix's own design. Everything above the OS layer is
NVIDIA's code, at a pinned release, unmodified.

It is not something every Ferrix machine gets. It needs an NVIDIA GPU that
Ferrix owns, either on bare metal or passed through by KVM. The WHPX world
on the customer's Windows machine has no passthrough, so this path does not
exist there (§8, R6).

**Exit of this design (N1):** on nazuna, in the libvirt domain `ferrix-3060`,
NVIDIA's GSP firmware boots on the RTX 3060. NVIDIA's unmodified
`nvidia-smi`, run from a data volume, lists `NVIDIA GeForce RTX 3060` with
its memory and its PCI address. N2–N4 then put pixels and programs on it.

## 2. What the feasibility pass found (2026-10-02)

### 2.1 The release, and what is already on nazuna

* **Release**: open-gpu-kernel-modules **580.173.02**. Its tag exists
  upstream, and it is exactly the version of the NVIDIA userspace installed
  on nazuna (Ubuntu's `nvidia-driver-580` 580.173.02).
* **Firmware**: nazuna also has the matching GSP firmware in
  `/lib/firmware/nvidia/580.173.02/`. `gsp_ga10x.bin`, for Ampere, is
  75,012,080 bytes; `gsp_tu10x.bin`, for Turing, is 30,471,256 bytes.
* **Newer releases**: the newest upstream tag is 615.71.09. Pinning to the
  host's version means the userspace and firmware can be read locally
  without downloading anything.
* **Download size**: the matching `.run` for a fetch script is 398 MB
  (`NVIDIA-Linux-x86_64-580.173.02.run`). The `-no-compat32` one, without
  the 32-bit libraries, is 326 MB. Neither was downloaded: the analysis
  used the installed copies.
* **The tree**: a shallow clone is 152 MB, in
  `~/.local/share/ferrix/nvidia-ref/ogkm-580.173.02` (not committed).

### 2.2 The core builds as freestanding objects

`make` in `src/nvidia` and in `src/nvidia-modeset`, with NVIDIA's own
makefiles and the host's gcc 15, built both objects on the first try:

| Object | Text | Defined globals | Undefined |
|---|---|---|---|
| `nv-kernel.o` (RM) | 12.2 MB | 13,372 | 406 |
| `nv-modeset-kernel.o` (NVKMS) | 1.5 MB | 2,694 | 69 |

What RM needs from the OS (the full lists are in
`~/.local/share/ferrix/nvidia-ref/*.undef`):

* **150 `os_*` functions.** By area:
  * memory: `os_alloc_mem`, `os_alloc_pages_node`, `os_get_page`;
  * locks: mutex, rwlock, semaphore, spinlock;
  * waits: wait queues, `os_wait_*`, `os_wake_up`;
  * PCI: config access through `os_pci_read_*` and `os_pci_write_*`,
    port I/O through `os_io_*`;
  * time: `os_get_monotonic_time_ns`, `os_delay_us`;
  * work queues: `os_queue_work_item`, `os_flush_work_queue`;
  * user memory: `os_memcpy_from_user` and `os_memcpy_to_user`;
    `os_lock_user_pages` to pin;
  * mappings: `os_map_kernel_space`;
  * the registry, files, process identity (`os_get_current_process`,
    `os_get_euid`, `os_is_administrator`), random bytes, and SMBIOS/ACPI
    tables.
* **117 `nv_*` functions.** By area:
  * page allocation: `nv_alloc_pages`, `nv_free_pages`;
  * DMA mapping: `nv_dma_map_alloc`, `nv_dma_map_mmio`, `nv_dma_map_peer`;
  * mappings: `nv_alloc_user_mapping`, `nv_alloc_kernel_mapping`,
    `nv_add_mapping_context_to_file`;
  * events: `nv_post_event`, `nv_get_event`;
  * firmware: `nv_get_firmware`;
  * timers: `nv_create_nano_timer`, `nv_start_rc_timer`;
  * ACPI: `nv_acpi_*`;
  * I²C;
  * Tegra and SoC functions: 26 of the 117, which a discrete GPU never
    calls.
* **71 `libspdm_*` functions**: crypto for Confidential Computing, which
  stubs out on a GeForce.
* **53 `nvswitch_*` and `nvlink_*` functions**: also stubbed.
* **13 retpoline thunks.**

NVKMS needs 55 `nvkms_*` functions (allocation, timers, semaphores,
`nvkms_call_rm`, `nvkms_copyin` and `nvkms_copyout`), `memcpy`, and the
same 13 thunks. This said 56 until N1a counted again.

In the other direction, Linux's glue calls 161 distinct `rm_*` entry points
into the core.

**Both objects link into an ordinary user program.** The test was a static
non-PIE `ET_EXEC` linked at the usual 4 MiB address, with every import
stubbed. It linked into a 15 MB program, with one clash to rename
(`nvstatusToString`, defined in both objects). The objects are built
`-mcmodel=kernel -fno-pic`, and their 58,445 `R_X86_64_32S` relocations
resolve in the low 2 GiB as well as the top. So the core needs no rebuild
to leave the kernel. It does need the kernel's codegen restrictions on its
own floating-point state: `-mno-sse` and `-mgeneral-regs-only`, which are
harmless in a user process.

**The Linux glue, by size:**

| Part | Lines | What it is |
|---|---|---|
| `kernel-open/nvidia`, all | 47,812 | Linux glue for RM |
| … of which a discrete GPU uses | ≈ 24,900 | `nv.c` 6.3k, `os-interface.c` 2.7k, `nv-pci.c` 2.2k, `nv-dmabuf.c` 1.9k, `nv-procfs.c` 1.5k, `nv-acpi.c` 1.6k, `nv-mmap.c` 1.0k, `nv-dma.c` 1.0k, … |
| `kernel-open/nvidia-modeset` | 3,143 | Linux glue for NVKMS |
| `kernel-open/nvidia-drm` | 15,321 | a Linux DRM driver over NVKMS; no OS-agnostic core |
| `kernel-open/nvidia-uvm` | 146,210 | the unified memory driver, written against Linux's mm; no OS-agnostic core |
| `src/nvidia` + `src/common` + `src/nvidia-modeset` | ≈ 1,980,000 | the OS-agnostic cores, MIT |

The core is portable as NVIDIA says. The two pieces that are not are UVM,
which CUDA needs (§11), and nvidia-drm, which NVIDIA's Wayland WSI needs
(§4.6).

**What the userspace needs besides ioctls.** This was read from the strings
of the 580.173.02 libraries on nazuna (`libnvidia-glcore`, `libnvidia-eglcore`,
`libnvidia-glsi`, `libcuda`, `libnvidia-ml`):

* **Character devices, numbered as Linux numbers them**:
  * `/dev/nvidiactl` is 195:255, `/dev/nvidia0` is 195:0, and
    `/dev/nvidia-modeset` is 195:254;
  * `/dev/nvidia-uvm`, `/dev/nvidia-uvm-tools` and `/dev/nvidia-caps/*`
    have dynamic majors, which the libraries look up by name in
    `/proc/devices`;
  * `/dev/char/<maj>:<min>` links.
  If a node is missing, the libraries run `/usr/bin/nvidia-modprobe` to
  make it, so Ferrix should simply have the nodes.
* **`/proc/driver/nvidia/`**: `params` (DeviceFileUID/GID/Mode and
  ModifyDeviceFiles are read before any open), `gpus/<bdf>/…` (`information`,
  `numa_status`), `capabilities/…` (MIG only), and `version`.
* **`/sys`**: `/sys/bus/pci/devices` (which Ferrix has) and
  `…/<bdf>/rescan`. Also `/sys/devices/system/memory/…` (NUMA onlining, for
  coherent platforms only), `/sys/module/<name>/initstate`, and CPU
  topology under `/sys/devices/system/cpu`.
* **`/proc/self/maps`, `/proc/<pid>/exe`, `/proc/modules`,
  `/proc/sys/kernel/modprobe`**.
* **The ioctl ABI.** There are three families:
  * RM's: magic `'F'`, base 200. `NV_ESC_CARD_INFO`,
    `NV_ESC_REGISTER_FD`, `NV_ESC_ALLOC_OS_EVENT` … `NV_ESC_WAIT_OPEN_COMPLETE`;
    plus the RM API escapes 0x27–0x5F (`NV_ESC_RM_ALLOC`, `…_CONTROL`,
    `…_MAP_MEMORY`, …), all `_IOWR('F', nr, size)`;
  * NVKMS's single `_IOWR('m', 0, struct NvKmsIoctlParams)`;
  * UVM's **raw numbers** (`UVM_INITIALIZE` is `0x30000001`, and
    `UVM_IOCTL_BASE(i)` is `i`), which encode no size at all.
  
  The arguments carry **user pointers that RM follows itself**. For
  example, `NV_ESC_RM_CONTROL`'s `params` points to a command-specific
  struct that may point further, and RM copies through
  `portMemExCopyFromUser` → `os_memcpy_from_user`
  (`src/nvidia/src/kernel/rmapi/param_copy.c`). No kernel table can know
  those layouts.
* **mmap.** An `NV_ESC_RM_MAP_MEMORY` records a mapping context on the
  file. A later `mmap(fd, offset)` on `/dev/nvidia0` or `/dev/nvidiactl`
  then maps one of three things, cached as RM says (write-combined for BAR1
  and most system memory):
  * BAR0 registers (USERD and doorbells);
  * BAR1 video memory;
  * system memory that RM allocated or pinned.
  
  RM can later revoke the mappings (`nv_revoke_gpu_mappings`).
* **fd identity.** Several calls hand RM a file descriptor number and
  expect it to resolve to an NVIDIA file of the same process:
  `NV_ESC_REGISTER_FD`, `NV_ESC_RM_EXPORT_OBJECT_TO_FD` and
  `…_IMPORT_OBJECT_FROM_FD`, `nvkms_fd_is_nvidia_chardev`.
* **Presentation.** On Wayland, NVIDIA's Vulkan WSI and EGL
  (`libnvidia-egl-wayland`) use the DRM render node of nvidia-drm.
  `libnvidia-glcore` references `renderD`, `drmPrimeHandleToFD`,
  `drmSyncobj*`, `zwp_linux_dmabuf_v1` and `wp_linux_drm_syncobj_*`. On
  X11, `libGLX_nvidia` wants either NVIDIA's own `NV-GLX` server extension
  or DRI3. Its Vulkan WSI says
  "Failed to create DRI3 Pixmap, using fallback presentation path", so a
  copy path exists for X11 Vulkan.

### 2.3 The bring-up probe on the RTX 3060

**Setup.**

* **Domain**: `ferrix-3060` in `qemu:///system`. Its XML is
  `~/ferrix-nvidia-vm/ferrix-3060.xml`, and Appendix A has a copy.
* **Image**: the `nvidia` branch's x86-64 image, copied to
  `~/ferrix-nvidia-vm/ferrix.img`.
* **Machine**:
  * q35 with OVMF (`OVMF_CODE_4M.fd`, no Secure Boot) and KVM;
  * 2 vCPUs (`host-passthrough`, `maxphysaddr` passthrough) and 4 GiB;
  * a VT-d unit (`intremap='off' caching_mode='on' aw_bits='48'`), with
    `caching_mode` because VFIO requires it behind a vIOMMU;
  * a virtio-rng that goes through the IOMMU (`<driver iommu='on'/>`);
  * no video;
  * serial over TCP to a capture script.
* **The GPU**: the 3060's function 0, `0000:01:00.0`, as an unmanaged
  hostdev (`managed='no'`), so libvirt never rebinds a host driver.

Before every start, the customer's domains that share the card (GameLab,
win11, manjaro, manjaro-kde-test, manjaro-xfce-test, manjaro-sway-test) were
checked to be shut off. They were each time. The domain was shut off after
each boot. Six boots were made; the logs are in `~/ferrix-nvidia-vm/serial*.log`.

**What Ferrix saw:**

```
  nvidia   0000:00:05.0: 10de:2504 rev a1 class 030000, memory decoding on
  nvidia   0000:00:05.0: BAR0 memory 32-bit 16384 KiB at 0x80000000
  nvidia   0000:00:05.0: BAR1 memory 64-bit prefetchable 16777216 KiB at 0x381000000000
  nvidia   0000:00:05.0: BAR3 memory 64-bit prefetchable 32768 KiB at 0x381400000000
  nvidia   0000:00:05.0: BAR5 I/O 128 bytes at port 0x6000
  nvidia   0000:00:05.0: capabilities 01@0x60 05@0x68 09@0xb4; extended 0002@0x100 0018@0x250 0004@0x128 0001@0x420 000b@0x600 0015@0xbb0
  nvidia   0000:00:05.0: BAR0 read: NV_PMC_BOOT_0 0xb76000a1, NV_PMC_BOOT_42 0x176a1000
  nvidia   0000:00:05.0: resizable BAR1: now 16384 MiB, sizes 0x40000 (bit n+4 = 2^n MiB), more 0x0
  iommu    pci 0000:00:05.0 behind the VtD unit at 0xfed90000 as stream 0x28
FERRIX-BOOT-OK stages 1-12
```

What these lines show:

* **Enumeration finds the card.** `10de:2504` (GA106, GeForce RTX 3060)
  is found, and every BAR is sized correctly:
  * BAR0 is 16 MiB of registers;
  * BAR1 is **16 GiB, 64-bit, prefetchable**, which is the whole 12 GB of
    video memory, since Resizable BAR is on in the host's firmware;
  * BAR3 is 32 MiB;
  * BAR5 is I/O.
  
  OVMF placed the 64-bit BARs at about 56 TiB. Ferrix's 64-bit sizing
  (`src/lib/platform/pci/src/bar.rs`) needed no change.
* **The CPU reaches the chip's registers.** `NV_PMC_BOOT_42` `0x176a1000`
  is architecture 0x17 (Ampere), implementation 6 (GA106), revision A1.
* **Resizable BAR** shows one size per BAR. QEMU passes only the current
  size, so the guest cannot change BAR1's size, and does not need to.
* **There is no MSI-X capability, only MSI** (`05@0x68`). The same holds
  behind a root port, where the PCIe capability (`10@0x78`) appears too.
  Ferrix delivers only MSI-X (`src/kernel/src/discovery/pci.rs`,
  `src/kernel/src/arch/x86_64/msi.rs`). So **today the 3060 gets 0
  interrupt vectors**, and the boot's `devices` line says
  `0 vectors`. This is prerequisite N0b.
* **The IOMMU path works when the card is on the root bus.** Placed at
  `00:05.0`, the card gets a translated VT-d domain (stream 0x28). The
  same unit's out-of-domain probe on the virtio-rng faulted as it must,
  and the boot ended `FERRIX-BOOT-OK stages 1-12`. The card made no DMA,
  because there was no driver for it yet. The first real test of its DMA
  is GSP boot itself, which fetches its firmware from system memory (N1d).
* **Behind a PCIe root port, where libvirt puts a hostdev by default**, the
  card is one of 5 `unresolved` functions. QEMU's DMAR describes the root
  ports as *sub-hierarchy* scopes. `place_dmar` (`src/kernel/src/iommu.rs`)
  does not follow those scopes, so it reports them unresolved rather than
  guessing. The card then gets no domain at all. This is prerequisite N0a.
  NVIDIA's driver prefers a slot behind a root port, because it reads the
  PCIe capability for link state, and QEMU hides that capability on the root
  bus.

**What failed on the way**, all in the domain rather than in Ferrix, and
written down so that the `run-nvidia` command in N0e avoids each one:

1. **Both functions of the card in one domain behind a vIOMMU** were
   refused: `vfio 0000:01:00.1: group 14 used in multiple address spaces`.
   QEMU's VT-d gives each function its own address space unless both sit
   behind one conventional PCI bridge. The HDMI audio function is not
   needed, so only function 0 is passed. VFIO accepts this, because all of
   group 14 stays bound to `vfio-pci` on the host.
2. **libvirt's virtio devices bypass the vIOMMU** unless they are given
   `<driver iommu='on'/>`, which xtask's `iommu_platform=on` gives them.
   Without it, Ferrix's stage 10 out-of-domain probe sees an unmapped write
   complete with no fault, and panics (FX-1001). That is correct: in that
   configuration it is unsafe.
3. **libvirt's QEMU cannot traverse `~/.local/share`** (mode 0700), and
   virtlogd's serial log files are root's. So the image lives in
   `~/ferrix-nvidia-vm/` (0755), and the serial port is a TCP socket on
   127.0.0.1:47060 read by `capture.py`.
4. **libvirt picks `OVMF.amdsev.fd`** when `firmware='efi'` is left to it.
   The loader is named explicitly.

## 3. The build and the sources

Nothing NVIDIA ships is committed here: not the kernel modules, not the
firmware, and not the userspace. Instead, `tools/common/fetch/fetch-nvidia.sh`
does the following, as `fetch-steam.sh` and `fetch-yserver.sh` do:

Everything it writes is under `~/.local/share/ferrix/nvidia/580.173.02/`
(`$FERRIX_NVIDIA` moves the root), and it refuses a directory inside a git
checkout:

* **The sources.** It fetches GitHub's tarball of open-gpu-kernel-modules'
  580.173.02 tag. It checks a pinned sha256 and that the tarball's header
  names the tag's commit, `20e4e6e1`. It unpacks the tarball into `src/`.
  `cargo xtask test-uvm` and `uvm-kpi`'s Makefile build UVM from there,
  unless `FERRIX_NVIDIA_SRC` names another tree.
* **The build.** It builds `nv-kernel.o` and `nv-modeset-kernel.o` with
  NVIDIA's own makefiles, unmodified, in `src/`, and copies them to
  `objects/` with lists of what each defines and imports. The summary
  compares those counts with §2.2's. Ferrix needs no patch to the core;
  if one is ever needed, it is carried as a numbered patch in
  `tools/common/fetch/nvidia/` and justified there.
* **The userspace.** It downloads the matching `.run`, pinned by the sha256
  that NVIDIA publishes beside it, and extracts it with `--extract-only`
  into `run/`. It then checks pinned sha256s of `nvidia-smi` and both GSP
  images. Where the host has the same release installed, it also compares
  them with the installed files; on nazuna they are byte-identical.
* **The volume.** `nvidia.img` is a btrfs image of `tree/`, built like
  Chrome's and yserver's, which Ferrix mounts at `/data`. It holds:
  * the x86-64 libraries and the links NVIDIA's installer would make, read
    from the `.run`'s manifest, in `usr/lib/x86_64-linux-gnu`;
  * Debian 13's glibc, the build `fetch-chrome.sh` pins, so that
    `nvidia-smi` runs from this volume alone and the volume merges with
    the others;
  * `nvidia-smi`, `nvidia-debugdump`, the CUDA MPS pair and
    `nvidia-persistenced`, in `usr/bin`;
  * the Vulkan ICD and layer, GLVND EGL and EGL platform manifests;
  * the OpenCL ICD;
  * NVIDIA's licence;
  * the firmware, at `lib/firmware/nvidia/580.173.02/gsp_{ga10x,tu10x}.bin`,
    where `nvrm` reads it.
  
  It leaves out the 32-bit libraries, the X driver and its GLX module,
  `nvidia-settings` and its GTK libraries, `nvidia-modprobe`, the
  installer and the Windows DLLs. `nvidia.version` beside the image says
  what the image was made from. No gate attaches it yet: N1f's
  `test-nvidia-smi` will, as `test-yserver` attaches yserver's.

The license allows vendoring: the core is MIT file by file (2,751 `SPDX: MIT`
headers, and nothing else in `src/`). The GPLv2 half of NVIDIA's dual
license applies only "when linked together to form a Linux kernel module",
which Ferrix never does. Vendoring is still not recommended, because it
would put about 2 million lines and 150 MB into this repository for code that
is never edited (decision D2). The firmware and the userspace are under
NVIDIA's proprietary licenses, which allow redistributing them unmodified
with a driver. They are fetched by each user and never committed.

Ferrix's own code is the OS layer and the shim around NVIDIA's objects. It
lives in `src/user/system/linux/drivers/nvrm/`, and `xtask` links it with
the fetched objects into `/lib/drivers/nvrm`. A machine with no fetch has
no `nvrm`, and devmgr leaves an NVIDIA function undriven, as it does today.

## 4. The design

### 4.1 Where the core runs: `nvrm`, a ring-3 process that hosts it

The decision of 2026-09-13 keeps drivers out of the kernel, and it applies
here too. Because the core links as an ordinary user program (§2.2),
running it in ring 3 costs nothing at link time.

**Which runtime.** Ferrix's native programs (`ferrix-rt`) have no threads
and no clock (`docs/DEVMGR.md` §3). RM needs both:

* a thread that takes the interrupt and runs `rm_isr`;
* the bottom half `rm_isr_bh` and the work items `os_queue_work_item`
  queues;
* the 1 Hz RC timer and the nanosecond timers;
* a thread per blocked client ioctl, since RM sleeps in `os_wait_*`.

A process may make both Linux and native calls (`docs/ARCHITECTURE.md` §2;
`syscall/mod.rs` dispatches by number from the same entry). So **`nvrm` is
a static Linux-personality program built against ferrousli** and written in
Rust and C:

* threads are `clone`, locks are futexes, and time comes from
  `clock_gettime` and `timerfd`;
* device access uses the native calls a ring-3 driver already uses:
  `io_mapping_create`/`_map` for BAR0, `interrupt_create`/`_bind`/`_ack`,
  and `vmo_create` + `vmo_pin` for DMA.

Rejected alternatives:

* threads in the native runtime: a runtime of its own for one driver;
* a kernel driver: refused by 2026-09-13, and 13 MB of C in ring 0.

**How it starts.** devmgr gains a match for vendor `0x10de`, class `03`.
This is a new driver kind, `Gpu`. Like `Engine` (gc400), it is not
restarted at first. Restart comes at N2, once `nvrm` can tear down a GSP
that it did not boot (R2). devmgr hands the device over only after its
isolated-interrupts mark, its isolation and its pin budget
(`docs/DEVMGR.md` §3.2), then gives it the device handle on its bootstrap
channel, as START gives it to the other drivers; the control handle of the
forwarding core comes with N1e.

This design first said devmgr would start `nvrm` through the personality's
exec path rather than `process_create`, because a ferrousli program needs
`argv` and `auxv` on its stack. N1b built it the other way round (§10,
2026-10-03): devmgr starts `nvrm` with `process_create` like every other
driver, which keeps its job, its process handle, its death watch and the
image-as-memory rule (`docs/DEVMGR.md` §5), and `nvrm`'s own entry builds
the stack ferrousli expects.

**The core.** RM's core, `nv-kernel.o`, is 13 MB linked. That is more than
the kernel reads into a driver's image, which is one heap block of 4 MiB
(`docs/DEVMGR.md` §5). The customer chose on 2026-10-03 to keep `nvrm`
small and load the core at run time (§10, N1c):

* **Two links.** `nvrm` is linked first: static, not PIE, at 4 MiB, with a
  build-id and a zeroed 32-byte `.nvrm_core_sha256` section. Then
  `nvrm-core` is linked on the host from `nv-kernel.o` alone at
  0x4000_0000 (`core/nvrm-core.ld`). `core/core-link.py` binds each of its
  imports, code and data, to `nvrm`'s address for it. Both images lie in
  the low 2 GiB, so RM's 32-bit relocations reach.
* **The export header.** The core's first bytes are the magic `NVRMCORE`,
  version 1, a count, `nvrm`'s build-id, and the addresses of what `nvrm`
  uses in the core: its data first, then its functions. The count is what
  `nvrm`'s objects import, computed per build, 17 today. Finally the core's
  sha256 is written into `nvrm` with `objcopy --update-section`.
* **Where it comes from.** The core is read from the NVIDIA volume at
  `/data/usr/lib/ferrix/nvrm-core`, and never from the initramfs. `read(2)`
  copies it into anonymous memory of `nvrm`'s own, so nothing maps the file
  and no page of it is filled from the disk afterwards. The disk's own
  driver serves the volume. `nvrm` is never a disk's driver.
* **The wait.** The volume is mounted after devmgr's REPORT, so `nvrm`
  waits for the file: polling every 250 ms, for at most 60 s. It then
  stops with its own line (`nvrm: stopped: no NVIDIA volume within 60 s`,
  exit 12). Neither the boot nor devmgr's REPORT waits on `nvrm`.
  `device_isolation` is printed before the wait, so F-57's first-boot
  record is had even when the core never loads.
* **The checks** (`os/nvos/src/rmcore.rs`, each refusal its own
  `nvos: core refused: …` line, then exit with no fallback):
  * the pin is not all zero, and the file's sha256 is the pin;
  * the file is an x86-64 `ET_EXEC` with only `PT_LOAD` and `PT_GNU_STACK`
    headers;
  * every segment is page-aligned, does not overlap another, lies in
    [0x4000_0000, 2 GiB), and is not both writable and executable;
  * the header's magic, version and count match, its build-id is `nvrm`'s
    own, and every export lies in the right segment;
  * each segment is mapped with `MAP_FIXED_NOREPLACE`, and `EEXIST` is a
    refusal, never a retry elsewhere.

  On success `nvrm` writes one line: `nvos: core loaded: sha256 …, N
  bytes, base 0x40000000, N exports, build-id …`.
* **W^X is `nvrm`'s own discipline.** The kernel does not refuse a mapping
  that is both writable and executable; Linux does not either, and
  Chrome's JIT relies on that. Only the ELF loader refuses a W+X image. So
  after its `mprotect`s, and before the first call into RM, `nvrm` reads
  `/proc/self/maps` back. It refuses if anything in the core's range is
  writable and executable, if text is not `r-x`, or if read-only data is
  not `r--`.
* **Where the heap is.** `nvrm`'s `brk` heap starts above its image at
  4 MiB and grows upward. The personality's `mmap` searches top-down from
  the top of user space. Neither may reach the core's range,
  [0x4000_0000, 2 GiB), before the core is mapped. The core is mapped
  early: before any thread starts and before RM runs, when `nvrm` has made
  only a handful of small allocations and one file-sized buffer. If something is already in the core's range,
  `MAP_FIXED_NOREPLACE` turns that into a refusal, not an overlap.
* **RM's one privileged instruction.** Across all of RM's code, only
  `osNv_rdcr4` (`mov %cr4,%rax`) faults in ring 3. A linker-script
  assignment overrides the object's own definition, so its three call
  sites reach `nvrm`'s `nvos_rdcr4` (`os/nvos/src/cpu.rs`). That function
  returns OSFXSR, and OSXSAVE as CPUID.1:ECX bit 27 reports it, which are
  the bits RM reads. `core-link.py` scans `nv-kernel.o` and refuses the
  build on any other privileged instruction.
* **The build refuses** with named lines:
  * an `nvrm` at or over the driver read limit, less half a mebibyte
    (3670016 bytes), so that FX-1006's misleading "not in the image" is
    never the symptom (its row in `docs/BACKLOG.md`);
  * an unresolved symbol or a relocation overflow in the core link;
  * any section header or symbol of `nvrm` that moved across the
    `objcopy`;
  * a core program header other than `PT_LOAD` and `PT_GNU_STACK`, or a
    W+X `PT_LOAD`.

The hash pin protects the product: it makes sure `nvrm` runs the core it
was built with. It is **not** an argument for the certified item, and is
never to be cited as one. The item's arguments about `nvrm` (§12.1's
window, §12.2's pin budget, §12.3's isolation mark) already bound a
compromised `nvrm` running any code at all.

### 4.2 The OS layer

The ≈ 260 functions of §2.2 come in four groups. The estimates are lines of
Ferrix code.

1. **C kept from `kernel-open/nvidia` (MIT), with its Linux calls
   replaced.** About 9k of the 25k lines survive. Most of it is the
   ioctl dispatcher and per-file state in `nv.c`, the mapping-context logic
   in `nv-mmap.c`, the registry parser in `os-registry.c`, and the
   `/proc/driver/nvidia` text. This logic is OS-neutral in all but its
   calls, and rewriting it would only add risk. FreeBSD's NVIDIA driver is
   built the same way.
2. **A Rust OS library under it, `ferrix-nvos`, about 5k lines.** It
   provides:
   * memory: `os_alloc_mem` from the process heap, and `nv_alloc_pages` as
     VMOs pinned into the device's domain;
   * locks, semaphores and wait queues: futex-based, with RM's spinlocks as
     spin-then-futex mutexes;
   * threads: a work-queue thread pool and timers;
   * PCI configuration through the native window of N0d;
   * MMIO through `IoMapping`s;
   * firmware: read from the volume;
   * random bytes, from `getrandom`;
   * logging, to the kernel log through the driver log channel.
3. **Stubs**: Tegra, NVSwitch, NVLink, libspdm, IMEX, vGPU, NUMA onlining,
   and ACPI. A desktop GPU in a VM has no `_DSM` worth calling; real ACPI
   for laptop muxes and power can come later.
4. **The request bridge** to the device core of §4.4: client copies, client
   pins, mapping replies and fd identity.

### 4.3 Memory, DMA and interrupts

* **DMA.** RM allocates system memory through `nv_alloc_pages`, maps it
  for the device with `nv_dma_map_alloc`, and uses the returned addresses.
  On Ferrix:
  * each allocation is a VMO pinned with `vmo_pin` into the card's IOMMU
    domain;
  * `VMO_PIN_ADDRESSES` gives the device addresses. These are
    identity-mapped, each page at its own physical address
    (`src/kernel/src/iommu.rs`);
  * RM needs no physically contiguous memory on Ampere. GSP's firmware is
    reached through a radix-3 page table of 4 KiB pages.
  
  `nv_dma_map_mmio` and `nv_dma_map_peer` (peer-to-peer) return
  unsupported.
* **How much is pinned.** GSP boot pins the firmware image and its logs,
  some tens of MiB. After that, every Vulkan or CUDA allocation in system
  memory is pinned too, which can reach gigabytes. Since N0f the pins are
  bounded by a per-device budget devmgr sets (`src/kernel/src/object/pin.rs`,
  §12.2): an eighth of RAM and at least 1 GiB for `nvrm`, in place of the
  quarantine's fixed cap of about 520 MiB, which a crash of a busy `nvrm`
  would have filled before its successor could pin GSP's firmware.
* **Client pages.** `os_lock_user_pages` pins a *client's* pages. This
  happens for `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, which GL and Vulkan use
  for imported host memory, and CUDA for `cudaHostRegister`. It becomes a
  native call scoped to an in-flight request (§4.4): the kernel pins the
  calling process's range into the card's domain and returns the
  addresses. The pin belongs to `nvrm` and is quarantined like `nvrm`'s
  own pins.
* **Interrupts.** The 3060 has MSI and no MSI-X (§2.3). N0b adds MSI: one
  vector, 64-bit address, not maskable per vector. It is minted from the
  same 64-vector pool and delivered as `PACKET_INTERRUPT`. The interrupt
  thread in `nvrm` calls `rm_isr`. If that asks for the bottom half, the
  thread runs `rm_isr_bh` on the work queue and then acknowledges. Ferrix's
  storm bound stays in force. N0g puts every message through VT-d's
  interrupt remapping, so the card can raise only its own vector (§12.3).
* **Write-combining.** BAR1, and most of RM's system memory mapped into
  clients, must be write-combined. Uncached BAR1 makes every vertex upload
  a series of single PCIe writes. x86-64 Ferrix does not program the PAT
  (`src/kernel/src/arch/x86_64/mod.rs`: "Write-combining needs the PAT, which
  is not programmed"). N0c programs it and adds a `write_combining` flag to
  `IoMapping` and to client mappings.
* **Apertures.** BAR1 is 16 GiB at about 56 TiB. `nvrm` maps only what it
  touches: BAR0 whole, and BAR1 windows on demand. Clients' BAR1 windows
  are mapped by the kernel from the aperture (§4.4). `DeviceInfo` reports
  aperture *counts*, not addresses, and its `DeviceBlock.length` is a
  `u32`. N0d adds a call that reports each aperture's 64-bit address and
  length, and leaves `DeviceInfo` as it is (§12.1).

### 4.4 The device files: a forwarding core

The existing cores for `/dev/dri`, `/dev/snd` and `/dev/input` decode every
ioctl in the kernel, copy fixed sizes, and send typed messages to their
driver. RM's ioctls cannot be decoded that way (§2.2: pointers RM follows
itself, and UVM's raw numbers). So a new kernel core,
`src/kernel/src/interfaces/chardev/`, forwards requests **undecoded**. It
sits in the `load` ring beside display and render (§6).

* **Registration.** `nvrm` registers device numbers with the core:
  * 195:0, 195:255, 195:254;
  * `nvidia-uvm`'s and `nvidia-uvm-tools`' dynamic majors (CUDA, D7), whose
    raw ioctl numbers the core forwards as it forwards the others;
  * a name for `/proc/devices` for each.
  
  The core makes the devfs nodes and `/dev/char` links, and lists the
  names in `/proc/devices`, which Ferrix does not have yet.
* **Requests.** `open`, `ioctl(cmd, arg)`, `mmap(offset, len, prot)`,
  `poll` and `release` each become a message on `nvrm`'s port. A message
  carries:
  * a request handle;
  * the client's file identity and its credentials (euid, pid, namespace);
  * the raw `cmd` and `arg`.
  
  The client's thread sleeps in the core until `nvrm` replies.
* **Client memory.** The request handle authorizes three native calls,
  and only while that request is outstanding:
  * `request_copy_in(request, address, buffer, len)` and
    `request_copy_out(…)`: the kernel copies to or from the waiting
    client's address space, with the same checks as `copy_from_user`.
    These back `os_memcpy_from_user`, `os_memcpy_to_user` and
    `nvkms_copyin`;
  * `request_pin(request, address, len, device)`: the client-page pin of
    §4.3;
  * `request_file(request, fd)`: resolves one of the client's descriptors
    to a file identity of this core, or fails. This backs
    `NV_ESC_REGISTER_FD`, export and import to fd, and
    `nvkms_fd_is_nvidia_chardev`.
  
  Once the reply is sent, the handle is dead. `nvrm` can never reach a
  process that is not, at that moment, waiting in a call to it.
* **mmap.** For `mmap(fd, offset)`, `nvrm` looks up the mapping context
  that `NV_ESC_RM_MAP_MEMORY` left on that file. It replies with one of
  two things, plus the caching (UC or WC):
  * an aperture range of its device: BAR0 doorbell pages, or a BAR1
    window;
  * a VMO range: system memory.
  
  The kernel maps that into the client: `map_window` for apertures,
  checked against the device's apertures; a shared VMO mapping for
  memory. Revocation is a later message that makes the kernel unmap every
  client mapping of a file.
* **Events.** `NV_ESC_ALLOC_OS_EVENT` and `nv_post_event` become readiness
  on the file. `nvrm` sends `EVENT(file)`, and the core wakes the file's
  `poll` and `epoll` waiters.
* **Cost.** Every RM ioctl is a round trip to `nvrm`. Ioctls are used for
  allocation, mapping and control, not for submitting work: Vulkan and GL
  ring doorbells through the USERD and doorbell pages they have mapped.
  The round trip therefore stays off the per-frame path. The measured
  native round trip is 2.6–6.5 µs (`docs/BACKLOG.md`, 2026-10-01).

**As built in N1e (2026-10-03), and what is deferred** (the consultant's
N11 and N15, ledger 294 and 297). Registration is the driver's HELLO on its
control channel, judged by `ferrix_chardevctl::session`, not a native call
(N1), and the kernel names the nodes (N3); `nvidia-uvm`'s majors are not
registered yet. The core forwards `open`, `ioctl` and `release`, nothing
else:

* `mmap` of a node is `ENODEV` and never reaches `nvrm` (the file has no
  pages to map). Forwarding it is N2's, designed under ledger 300's M1-M12,
  and its code goes to the consultant before it lands.
* `poll` never reports a node readable or writable, only hangup and error
  once its driver is gone; events (`EVENT(file)`) come with mmap.
* `read` and `write` are `EINVAL`, as on NVIDIA's own nodes.
* `request_file` is built, as `chardev_file` (`0x105E`): scoped to the
  outstanding request's client and to this control's own files.
* `request_pin`, the client-page pin of §4.3, is not. `nvrm`'s
  `os_lock_user_pages` and `os_lookup_user_io_memory` are loud stubs
  (`os/glue/stubs.c`: a line naming the stub, then
  `NV_ERR_NOT_SUPPORTED`) until a gate needs them, and the kernel call goes
  to the consultant for review before any code is written.

`/proc/driver/nvidia/{params,version,gpus/<bdf>/information}` are text
files that the core asks `nvrm` for, under a `procfs` hook. A `/sys/module/
nvidia/initstate` reading `live` is added to sysfs, which already serves
`/sys/bus/pci/devices`.

### 4.5 NVKMS and the display

NVKMS (`nv-modeset-kernel.o`) links into the same `nvrm` process, so
`nvkms_call_rm` is a direct call. `/dev/nvidia-modeset` is served by the
same forwarding core. That is enough for:

* the userspace's own use of `nvidia-modeset`, since EGL and GLX open it
  for display queries and surface memory;
* the later choice of scanning out on the 3060's own outputs (N6).

Without N6, the card drives no monitor, and Ferrix's screen stays
virtio-gpu, seen over VNC or SPICE.

### 4.6 How frames reach hyprix, yserver, Chrome and Steam

The card renders. The problem is getting its pixels to a compositor that
today takes only `wl_shm` (`docs/YSERVER.md` §1: no `zwp_linux_dmabuf_v1`).
There are three steps, and each one stands on its own:

1. **Copy presentation (N3a).** A Vulkan layer of Ferrix's, `VK_LAYER_FERRIX_wsi`,
   implements `VK_KHR_wayland_surface` and the swapchain over NVIDIA's
   driver:
   * the swapchain's images are ordinary device images;
   * at present, a `vkCmdCopyImageToBuffer` copies the image into a
     host-visible, linear buffer in system memory;
   * the frame is then attached as a `wl_shm` buffer.
   
   This needs nothing from nvidia-drm. At 1080p it moves 8 MB per frame
   over PCIe, which is cheap. The CPU copy into hyprix's shm and hyprix's
   CPU compositing are not cheap (39 ms at 1080p, `docs/GPU.md`). Under
   libvirt, hyprix has no virgl, because virgl would render on the host's
   3090, which this work must not touch.
2. **dmabuf (N3b).** This step makes NVIDIA's own WSI, EGL and GBM work
   unmodified, which Chrome and every EGL program need:
   * Ferrix serves `/dev/dri/renderD129` with nvidia-drm's subset over RM
     and NVKMS. That subset is GEM handles, PRIME export and import,
     `drm_syncobj`, and NVIDIA's private ioctls `0x00`–`0x18` (Appendix B).
     It is written in `nvrm`, not ported from nvidia-drm's 15k Linux-DRM
     lines, and served through the forwarding core;
   * hyprix gains `zwp_linux_dmabuf_v1` and `wp_linux_drm_syncobj_v1`. It
     advertises the linear modifier only, so that it can map a buffer from
     the CPU while it still composites on the CPU.
3. **The compositor on the card (N3c).** hyprix's renderer, which already
   draws through a render node (`docs/GPU.md` §3.3), runs on NVIDIA's
   Vulkan and imports clients' dmabufs without a copy. Only the finished
   frame is copied, into virtio-gpu's scanout, or not at all with N6.

**yserver.** Its X clients get Vulkan through NVIDIA's X11 fallback
presentation (§2.2), which works with no change to yserver if `MIT-SHM` and
`PutImage` are enough. GL clients need GLX. NVIDIA's `libGLX_nvidia` needs
`NV-GLX`, which only NVIDIA's X driver has, or DRI3 with dmabufs from
nvidia-drm. That leaves two options:

* **(a)** DRI3 and Present in yserver over N3b;
* **(b)** Mesa's GLX over zink on NVIDIA's Vulkan, presenting through the
  same X11 path. That costs one Mesa build in the volume and no NVIDIA
  GLX.

(b) needs less from Ferrix. (a) is what NVIDIA supports, and the customer
chose it (D4, 2026-10-02): yserver gains DRI3 and Present over N3b.

**Chrome.** Chrome runs `--enable-gpu` over EGL or ANGLE-Vulkan, with
dmabuf presentation to Wayland. It needs N3b.

**Steam.** The client's web helper keeps its software path. Games need
either GL or Vulkan through yserver, as above.

## 5. What other parts of Ferrix need

* **The kernel**, as prerequisites N0a–N0d, N0f and N0g (§7, §12):
  * DMAR scopes through bridges;
  * MSI;
  * the PAT and write-combining;
  * a driver's configuration-space window and 64-bit aperture addresses;
  * a per-device pin budget;
  * interrupt remapping on VT-d.
  
  All of them are inside the certified item's `core` and `item` rings, so
  each needs the consultant's OK.
* **devmgr**: the `0x10de` match, the `Gpu` kind, and starting a
  Linux-personality driver.
* **procfs**: `/proc/devices`, and a hook for `/proc/driver/<name>`.
* **sysfs**: `/sys/module/<name>/initstate`.
* **hyprix**: `zwp_linux_dmabuf_v1` and the syncobj protocol (N3b).
* **xtask**:
  * `run-nvidia`, which checks that the shared domains are shut off,
    defines `ferrix-3060` from a template, copies the image, captures
    serial, and shuts the domain down at the end;
  * the gates that follow `test-yserver`'s shape, but run only on nazuna
    and only when the card is free (§6).

## 6. Certification, and the tests

**The certified item stays free of NVIDIA code**:

* `nvrm` is a ring-3 process, so none of NVIDIA's C, its headers, the
  OS layer or the firmware is in `src/kernel` at all. It is outside the
  item as every ring-3 driver is (`docs/certification/ITEM.md` §2: "Device
  enumeration is inside; device drivers are not").
* The forwarding core `interfaces/chardev` is a GPU driver's kernel half.
  It goes in the `load` ring with `interfaces/display` and `render`, in
  `tools/common/data/certification-item.json`.
* Its native calls (`request_copy_*`, `request_pin`, `request_file`, mmap
  and event replies) are registered through `syscall::native::serve` and
  answered above the item, as the `*_control_create` calls are.
* `check-item-boundary.py` already enforces the rings. A new line in it
  refuses any path under `src/kernel` that names `nvidia` or includes a
  header from the fetched tree.
* RM's core is loaded at run time from the NVIDIA volume (§4.1, "The
  core"), not from the initramfs. That changes nothing in the item.
  * W^X for the core is `nvrm`'s own discipline, checked against
    `/proc/self/maps`. The kernel's `mmap` and `mprotect` refuse no W+X
    mapping, and asking them to would be an item change.
  * `nvrm`'s wait for the volume is bounded (60 s). It does not hold up
    devmgr's REPORT, since the volume mounts after REPORT.
  * `nvrm`'s heap and `mmap` area stay out of [0x4000_0000, 2 GiB) until
    the core is mapped, and the core is mapped before any thread starts.
  * The `osNv_rdcr4` binding replaces RM's one privileged instruction.
  * The sha256 pin is product integrity. It is never an argument for the
    item.

**What enters the item** is only the platform work of N0: MSI, DMAR
bridge scopes, the PAT, the configuration window, the pin budget and
interrupt remapping. These
are generic. A future AMD or Intel driver needs the same, and the
certification consultant reviews each one before landing, as usual.

`request_copy_*` and `request_pin` widen what a driver can do. A driver can
now read and write a client's memory and pin it, but only for a request
that client made to that driver's own device file, and only while the
client waits. This is a designed exposure with a stated bound, not a defect,
so it is argued in `docs/certification/VULNERABILITY-ANALYSIS.md` (V-10, and
V-11 for the 0666 nodes' parser surface), not in FINDINGS: the threat is a
compromised `nvrm` reading a client of its own. Linux has the same
exposure, because the RM there runs in ring 0.

**Tests.** The GPU is shared and is not in CI, so the gates are xtask
commands run on nazuna:

* `test-nvidia-probe` checks the §2.3 lines;
* `test-nvidia-smi` is N1's exit;
* `test-nvidia-vulkan` renders `vkcube`'s offscreen frames, reads them
  back and checks their hash range, for N2;
* the presentation gates reuse `test-display`'s picture judge.

Every gate first checks that the shared domains are shut off, and refuses
to start otherwise. CI keeps what it can run anywhere: the kernel
prerequisites' boot checks under QEMU, where an emulated MSI device and a
root port in `xtask`'s machine cover N0a and N0b, and `nvrm`'s OS-layer
unit tests.

## 7. Milestones and points

| Slice | What | Points |
|---|---|---|
| N0a | DMAR sub-hierarchy scopes followed through bridges; `xtask`'s q35 gets a root port to prove it | 2 |
| N0b | MSI next to MSI-X: enumeration, vectors, delivery, and a boot check on an emulated MSI-only device | 3 |
| N0c | PAT programmed; write-combining `IoMapping`s and client windows | 3 |
| N0d | 64-bit aperture addresses and lengths, by a new `device_aperture` call beside an unchanged `DeviceInfo`; a driver's configuration-space window, writable only in vendor-capability bodies, every configuration write under one lock per node, and a read-back that refuses a node whose kernel-owned registers were rewritten (§12.1) | 5 |
| N0e | `run-nvidia` and `test-nvidia-probe` (libvirt, the shared-domain guard, capture) | 2 |
| N0f | A per-device pin budget, set by devmgr through `SET_LIMIT`, in place of the quarantine's fixed cap, with `2B` a hard bound per device (§12.2) | 3 |
| F-58 | Not NVIDIA's: VT-d table-walk coherency, a pre-existing defect in the item, fixed on its own by another agent before N0g (§12.3). Not counted in N0 | 2 |
| N0g | Interrupt remapping on VT-d, closing F-57: queued invalidation for everything, remappable-format MSI, MSI-X and I/O APIC messages, an interrupt remapping table with one validated entry per minted vector and source-ID checks, compatibility format blocked, a patched QEMU that implements that block for the gates and the `ferrix-3060` domain, and xtask's q35 with `intremap=on` on a split interrupt controller; before N1 hands the 3060 and its GSP firmware to `nvrm` (§12.3). Design reviewed twice, OK IF; its code goes back to the consultant | 10 |
| N1a | `fetch-nvidia.sh`: sources, objects, `.run` extraction, the volume | 3 |
| N1b | `nvrm` skeleton: a ferrousli static program started by devmgr, with handles over bootstrap | 4 |
| N1c | The OS layer (§4.2), with the 9k lines of kept C and `ferrix-nvos` | 10 |
| N1d | GSP boots on the 3060: firmware load, booter, WPR, the first RPCs | 6 |
| N1e | The forwarding core, `/dev/nvidia{ctl,0}`, `/proc/driver/nvidia`, `/proc/devices` | 6 |
| N1f | `nvidia-smi` from the volume lists the GPU (`test-nvidia-smi`) | 3 |
| N2 | Vulkan offscreen: mmap contexts, client pins, events and `poll`, fd identity; `vulkaninfo` and `vkcube` offscreen (`test-nvidia-vulkan`) | 14 |
| N3a | Copy presentation layer: `vkcube` in a hyprix window | 6 |
| N3b | nvidia-drm subset as `renderD129`; hyprix `zwp_linux_dmabuf_v1` and syncobj; NVIDIA's own WSI and EGL | 16 |
| N3c | hyprix composites on the 3060 | 8 |
| N4 | Chrome `--enable-gpu` on the 3060; GLX for yserver (D4); a Steam game on the 3060 | 20 |
| N5 | CUDA: NVIDIA's `nvidia-uvm` rebuilt in `nvrm` against a Linux-compatible header set, fault windows in the kernel for managed memory, the CUDA samples (§11.5: C0–C4) | 52 |
| N6 | Scan-out on the 3060's own outputs through NVKMS | 15 |

Each milestone's total, and what it shows:

* **N0**: 28 points. N0g was added at 6 after the consultant's review of
  N0a and N0b. Its first design review raised it to 8, and the second to
  10, for the patched QEMU and for the console's conversion, per-processor
  x2APIC and the set-once mark. N0d went from 3 to 5 and N0f from 2 to 3
  once designed and reviewed (§12). The F-58 fix (2) is the item's own
  defect and is not counted. The order is F-58, N0d, N0f, N0g, then N1.
* **N1**: 32 points, the exit of §1.
* **N2**: 14 points. It is where "real driver" becomes "renders".
* **N3**: 30 points.
* **N4**: 20 points.

That is **124 points from here to Chrome and Steam on the card**. N5 (CUDA)
and N6 (its own monitor) are sized apart. CUDA is wanted now, alongside the
graphics (D7), and has its own feasibility pass and design by a separate
session; this design only keeps `/dev/nvidia-uvm` servable by the same
forwarding core (§4.4). N6 waits for a monitor on the 3060 (D6).

`docs/GPU.md` §4 said "well over a hundred points". It still is. The
difference is that every part is now named.

N1d is the widest estimate. GSP boot is where RM's assumptions meet a new
OS for the first time, and NVIDIA's firmware tells you very little when it
refuses (R1).

## 8. Risks

* **R1 — GSP boot.** On Ampere, RM must do all of the following before
  anything else works:
  * read the VBIOS through BAR0's PROM window;
  * run FWSEC from it, then the booter;
  * load a 75 MB firmware file whose RM image goes into WPR in video
    memory;
  * exchange RPCs over a message queue in system memory.
  
  Any of the DMA, timing or interrupt plumbing being wrong shows up as a
  GSP that never answers. Mitigations:
  * the probe's VT-d domain and VFIO path already work (§2.3);
  * RM's own logging (`NVreg_RmMsg`) goes to the kernel log;
  * the same release runs on the host's 3090. Comparing register traces is
    possible in principle, but **not done**: the 3090 is off limits to this
    work.
* **R2 — the card's state across guests.** The 3060 is also GameLab's and
  win11's. VFIO resets the card when the domain starts, and Ampere resets
  cleanly on a secondary bus reset. A `nvrm` restart *within* one boot
  needs RM's own unload path (`rm_shutdown_adapter`) and a GSP reset,
  which is why restart waits.
* **R3 — RM's Linux assumptions.**
  * Process identity: `os_get_current_process`, `os_get_euid`, pid
    namespaces through `os_find_ns_pid`. These come from the request's
    credentials.
  * `os_is_administrator`: root in the client's user namespace.
  * Per-file private data, and mapping contexts that outlive the ioctl
    that made them.
  * `os_get_max_user_va`: RM wants the client's address-space limits.
  
  Each has a Ferrix answer in §4.4. The risk is the ones not found
  until N2.
* **R4 — mapping volume.** Vulkan maps many small BAR1 and system-memory
  ranges into clients, and RM may revoke them. The kernel's client-window
  bookkeeping was sized for one virtio-gpu window (`docs/GPU.md` §6.1).
* **R5 — the userspace's other probes.** NVML reads PCI configuration
  through sysfs (`config`, `resource`), which Ferrix lacks
  (`docs/SYSFS.md`), and it may refuse a GPU without them. `nvidia-smi` at
  N1f is where this is found out.
* **R6 — where it runs.** Only on nazuna under libvirt with KVM and VFIO,
  or on bare metal with an NVIDIA card. Under WHPX there is no
  passthrough, the machine has one vCPU, and the path does not exist.
  Under KVM, the frames are only seen through VNC or SPICE of a virtio-gpu
  screen, with a copy, unless N6 drives a monitor attached to the 3060.
* **R7 — the card is shared.** Every boot needs the customer's five
  domains shut off. No gate can be scheduled; it runs when the card is
  free.
* **R8 — pinned version.** RM and the userspace must match exactly
  (`NV_ESC_CHECK_VERSION_STR`). Moving the pin means a new fetch, rebuilt
  objects, and N1f to N2 run again. The host's own driver updates are
  independent of Ferrix.
* **R9 — CUDA.** UVM is 146k lines written against Linux's memory manager:
  `mmu_notifier`, HMM, `migrate_vma`, and GPU fault replay. libcuda opens
  `/dev/nvidia-uvm` and manages GPU virtual address space through it even
  for plain `cudaMalloc`. N5 is a port of a Linux subsystem's worth of
  assumptions, not glue. §11 sizes it and gives its own risks (§11.6).

## 9. Decisions for the customer

All taken on 2026-10-02. The customer answered D1, D4, D6, D7, D8 and D9
and took the recommended answer for D2, D3 and D5.

1. **A real NVIDIA driver**: NVIDIA's open modules, their GSP firmware and
   their unmodified userspace. Not nouveau or NVK.
2. **D1 — where the core runs: `nvrm`**, a ring-3 Linux-personality
   program built against ferrousli, using native calls for the device
   (§4.1). Not native threads, not the kernel.
3. **D2 — the sources are fetched, never committed**: a pinned tag built
   out of tree by `fetch-nvidia.sh`; only Ferrix's OS layer is in the
   repository (§3).
4. **D3 — the release is 580.173.02**, nazuna's own userspace and
   firmware.
5. **D4 — GL for X clients is DRI3 and Present in yserver**, over the
   nvidia-drm subset of N3b. Not zink.
6. **D5 — the copy layer first** (N3a), then dmabuf (N3b). N3c, hyprix on
   the card, stays in the plan after them.
7. **D6 — a monitor on the 3060 will be added later.** N6 waits for it;
   until then the screen is virtio-gpu over VNC.
8. **D7 — CUDA now, alongside the graphics.** `nvidia-uvm` gets its own
   feasibility pass and design from a separate session. This design keeps
   the forwarding core and `nvrm`'s request bridge able to serve
   `/dev/nvidia-uvm` (raw ioctl numbers, its dynamic major, its own
   mmap and fault paths) without a second mechanism. §11 is that design,
   52 points. Its own five decisions, D-C1 to D-C5, were answered the same
   day (§11.8): managed memory as small as possible, NVIDIA's own UVM,
   beside graphics after N1, no profilers, NVIDIA's samples.
9. **D8 — the platform changes inside the item** (N0a–d, f) go one by one
   through the certification consultant, each as its own commit with its
   tests and negative controls.
10. **D9 — the card's time.** An agent may start `ferrix-3060` whenever
    GameLab, win11 and the manjaro domains are shut off, checked before
    every start, and shuts it down when done. The RTX 3090 is never
    touched.

## 10. Where it stands

* **2026-10-02 — feasibility pass.**
  * Sources at 580.173.02 fetched outside the repository. The two objects
    were built, their imports listed and counted, and they link into a
    user program.
  * The `ferrix-3060` domain is defined and shut off.
  * Ferrix enumerates the 3060, sizes its 16 GiB BAR1, reads its
    registers, and gives it a VT-d domain on the root bus. It does not
    give it one behind a root port (N0a), and it gives it no interrupt
    (N0b).
  * The probe is a never-land commit on branch `nvidia-probe-wip`.
  * The customer answered §9 the same day. N0, the kernel prerequisites,
    starts next on branch `nvidia-n0`.
* **2026-10-02 — N0a and N0b reviewed: OK IF.** The certification
  consultant reviewed DMAR scopes through bridges (N0a) and MSI (N0b) on
  `nvidia-n0` 0a619332e, with their gate and controls, and accepted them
  with nine conditions (its ledger, 2026-10-02):
  * the alias rule on every placement, not only sub-hierarchies;
  * the command register written under one lock per node;
  * L.device.22 proven whole: test-boot requires the MSI check's mint and
    both deliveries, INTx is read back off, and the mask registers move to
    `ferrix_pci::msi` with host tests (L.device.23);
  * L.iommu.45 split into the topology rule and the kernel's placement
    (L.iommu.46);
  * `ferrix-pci` and `ferrix-acpi` named in ITEM.md §2, with a row to
    classify them;
  * coverage re-argued, the documents updated, a new AoU-19;
  * no MSI vector allocated before a 32-bit capability is refused;
  * a full gate on the final head.
  It proposed F-57, pre-existing: x86-64 has no interrupt remapping. N0g
  closes it before N1.
* **2026-10-02 — N0's owed conditions, fixed forward** (branch
  `nvidia-n0-conds`, for the consultant). N0 landed ahead of its
  re-review's conditions; these meet A2, A3 and B1-B3:
  * A2: `ferrix_pci::topology::claims` decides every placement and is
    never `Own` where `behind` says aliased; host-tested (L.iommu.45).
  * B1: one memory type per device page across every user mapping, both
    directions, render-node windows included (L.user.109,
    `user::memory_type`), with a stage-9 refusal check.
  * B2: `io_mapping_map_combining` is refused until every processor
    programmed its PAT; B3: L.user.108 names where it runs (all three
    ISAs: ARMv7-A's virtio-pci has a prefetchable BAR too).
  * A3: what N0 left unmeasured is a row of `coverage-owed.json`, shown
    in COVERAGE-WORKLIST, until the next F-10 run.
* **2026-10-02 — CUDA feasibility and design (§11).** On that day the
  customer asked for CUDA now, alongside graphics.
  * `nvidia-uvm` was built twice against the host kernel's headers in a
    scratch copy: whole, and in the profile §11.3 proposes. The imports
    were counted and sorted by subsystem.
  * The UVM ioctls that libcuda 580.173.02 issues were read from its
    code.
  * The GPU was not booted for this pass.
* **2026-10-02 — K1 design reviewed: OK IF.** The certification
  consultant reviewed §11.4's fault windows before any code, and accepted
  them with conditions:
  * the hazards H1–H8 and the conditions are folded into §11.4;
  * the boot checks A–L, each with a negative control, are in §11.4;
  * the obligations (AoU-17, AoU-18, FM-12, V-10, V-11) are in §11.7;
  * `L.user.111`–`124`, `L.object.121`–`125` and `H.MEM.20`–`21` are
    reserved on `main`, for C3-K's landing to release;
  * C3-K's code, QEMU only, follows C0a and goes to the consultant before
    it lands.
* **2026-10-02 — C0a: UVM runs its own tests on `uvm-kpi`.** On branch
  `nvidia-cuda-c0`, in `src/user/system/linux/drivers/nvrm/uvm-kpi/`
  (5.6k lines of C, all Ferrix's):
  * **The header set.** It has 127 empty `<linux/…>`, `<asm/…>` and
    `<generated/…>` names over four headers (`include/kpi/`), conftest answers, and a 67-line
    `nv-linux.h` of its own, because UVM uses only seven names from
    NVIDIA's 1,852-line one.
  * **The runtime.** It has futex locks (including `downgrade_write`),
    wait queues, kthreads, one timer and work thread, a page pool on a
    memfd that `vmap` re-maps, and its own red-black tree, sorted map,
    heapsort and bitmaps.
  * **`uvm_common.c`** is written anew from its MIT header, so the
    GPL-2.0-or-later original is never linked.
  * **The build.** NVIDIA's unmodified UVM sources from the fetched tree
    (`NVSRC`) build with no errors: 126 files, the tests included. The 76
    `nvUvmInterface` calls go to a generated stand-in for RM with no GPU.
    The result links into `uvm-selftest` with nothing undefined: 1.19 MB
    of text, 9.8 MB with debug information.
  * **The run** on the host: `uvm-selftest` loads UVM as the module
    loader would, opens its device and calls `UVM_INITIALIZE` as a client
    would. It then passes all 15 of UVM's GPU-free tests through
    `UVM_RUN_TEST`'s ioctl path: RNG, range tree, lock, perf utils,
    kvmalloc, perf events, `nv_kthread_q`, red-black tree directed and
    random (100k iterations), CPU chunk API, range allocator, range
    groups, thread context sanity and perf, and CPU chunk sizes.
  * **Two shim bugs** were found by those tests and fixed: `krealloc` to
    size 0, and an inode without a mapping.
  * **Not done then.** The run under Ferrix waited for disk space: the
    host had 5.8 GB free when C0a's build finished, under the 6 GB stop
    line, so no Ferrix image was built. It is the next entry. Linking into
    `nvrm` waits for N1b; `uvm-selftest` stands in for it.
* **2026-10-02 — C0a: the same tests pass under Ferrix.** On branch
  `nvidia-c0b`:
  * **The build.** `uvm-selftest` is linked statically against ferrousli,
    Ferrix's own C library, and nothing else: `crt1.o`, the objects above,
    `libferrousli.a` and `libgcc`. No object changed, because no UVM or
    `uvm-kpi` unit includes a C library header (`kpi/libc.h` declares the
    27 calls `uvm-kpi` makes). It linked with nothing undefined at the
    first try. `uvm-kpi`'s Makefile builds it as `make ferrix`.
  * **The run.** Booted as init on x86-64, it passes all 15 GPU-free tests
    and its four range-group setups, as on the host, and exits 0. This
    held under KVM and under TCG, at four processors. The times under KVM
    match the host's: `UVM_TEST_RB_TREE_RANDOM` took 6.3 s against 5.8 s,
    and `UVM_TEST_NV_KTHREAD_Q` 0.5–0.6 s against 0.3 s. Under TCG,
    `RB_TREE_RANDOM` took 90–137 s.
  * **Nothing was missing.** Every call `uvm-kpi` makes behaved as on
    Linux: futex waits and wakes, `memfd_create` with `ftruncate` and a
    shared `mmap` that `vmap` maps again, `getrandom`, `sched_getcpu`,
    `clock_nanosleep` and threads. The serial log shows no `ENOSYS` from
    the program. No bug was found in `uvm-kpi`, and no gap in Ferrix.
  * **The gate.** `cargo xtask test-uvm` builds the program with `make`
    from the tree `FERRIX_NVIDIA_SRC` names (by default the fetched one
    above) and boots it. It requires every test line to say PASS, in order,
    the summary line, and exit 0. Then it boots a negative control: the
    same program with `uvm-kpi`'s `krealloc` keeping a block that it is
    asked to shrink to nothing (`-DKPI_NEGATIVE_CONTROL`, `kpi/mm.c` only).
    That build must fail `UVM_TEST_KVMALLOC` alone, at UVM's own check
    `uvm_kvrealloc(new_p, 0) == ZERO_SIZE_PTR`, and exit 1, and it does.
    The gate waits 400 s for a boot unless `--timeout` says otherwise,
    because of TCG. It is x86-64 only, like `uvm-kpi`. Its unit tests
    judge the two recorded runs.
* **2026-10-02 — N0d built: 64-bit apertures and a driver's configuration
  window** (branch `nvidia-n0d`, for the consultant; the design and its
  conditions D1–D4 are `nvidia-n0-designs` §12.1). QEMU only:
  * **`device_aperture`** (0x1054) writes a 32-byte `ApertureInfo`: the
    minted address and length whole, the BAR and the offset in it, and the
    prefetchable, whole-pages and 64-bit flags. `DeviceInfo` keeps its 96
    bytes. xtask's x86-64 machine carries `pci-testdev,membar=8G`, and its
    8 GiB BAR is reported whole above 4 GiB (W1).
  * **The window**: `device_config_read` (0x1055) and
    `device_config_write` (0x1056), both needing `MANAGE`. Every byte reads.
    A write is made only inside a vendor capability's body
    (`ferrix_pci::window`, host-tested), virtio's `pci_cfg_data` excepted
    (D1), and any other is refused whole. The first refusal at each dword
    prints one line naming the register, after the lock is dropped, from a
    bitmap made at stage 10 (D3).
  * **One lock per node, as a type**: every write of a published node's
    configuration space goes through `ConfigWrites`, which only taking the
    node's `IrqSpinLock` makes. `MsixTable::open` has lost its own mapping
    (D4). The node maps its configuration space once.
  * **The breach detector** (D2): `verify_config` reads the command bits,
    the BARs and the MSI and MSI-X registers back at each accepted HELLO
    (`DeviceNode::hello_accepted`, beside the quarantine release) and at
    each quiesce. On a mismatch it turns bus mastering off and refuses the
    node until reboot, and prints the register. AoU-22 is in the
    SAFETY-MANUAL.
  * **Two clarifications of the design**, both small:
    * `device_config_read` writes its value to a `u32` in the caller's
      memory. It does not return it, because on ARMv7-A a 32-bit read of
      all ones would be decoded as an error.
    * A function behind a CAM window (crosvm) has 256 bytes, and reads
      above them answer all ones.
  * **Two departures from §12.1's check table**:
    * W3 runs on ARMv7-A too, since its virtio-rng has the PCI
      configuration capability.
    * W5's control reduces the whole-range test to the *last* byte. No
      QEMU vendor capability has an aligned pair whose first byte is
      writable and second kernel-owned.
  * Requirements `L.device.24`–`26` and `L.object.117`, released from the
    reservation.
  * **The consultant's code review: OK IF**, on 1f95d8813 (its ledger,
    2026-10-02). D1–D4 are met and the deviations accepted. Its two
    conditions are folded in on the rebased branch:
    * A refused node never gets bus mastering back. `refuse` sets the flag
      and turns bus mastering off in one hold of the configuration lock, and
      `set_bus_master(true)` tests the flag under it. W7 now also asks for
      bus mastering after the refusal and requires it to stay off.
    * The walker fails closed on its own limits. More capabilities than it
      records (304), or more vendor bodies (64), leave the function nothing
      writable. Two host tests hold it.
    * As its advisory asked, `hello_accepted` reads the configuration back
      before it releases the quarantine.
* **2026-10-02 — N0f built: a pin budget per device** (branch
  `nvidia-n0f`, for the consultant; the design and its conditions F1–F5
  are `nvidia-n0-designs` §12.2). QEMU only, rebased on N0d:
  * **The budget and its counts.** Each device has a budget `B`, 66560
    pages by default, and `live`, `quarantined` and `kept` counts, kept on
    its domain and changed only under the quarantine's lock. A pin is
    refused `LIMIT_REACHED` (a new status, `ENOSPC`) when `live + n > B`,
    and `QUARANTINE_FULL` when `quarantined + kept + live + n > 2B` (F1).
    `live` is reserved in the same step, before `Domain::pin`, and given
    back if the domain refuses (F2). Kept pages stay counted, and a release
    takes off only what it gave back (F3). The pin path no longer walks the
    quarantine list.
  * **The calls**: `device_set_limit` (0x1057), needing `SET_LIMIT`, and
    `device_get_limit` (0x1058), through N0d's `device_call`. The kernel
    hands devmgr both device handles with `SET_LIMIT`, and every launch
    path narrows it away. A set is `BAD_STATE` under live pins and
    `NO_MEMORY` past the ceiling, a quarter of RAM counted at stage 10;
    the default is not counted (F5).
  * **devmgr's rule for `nvrm`** (F4) is `ferrix_devmgr_proto::budget`,
    with its four lines. devmgr has no `Gpu` kind until N1b, so the rule is
    host-tested at 16, 8, 4 and 1 GiB and with the ceiling partly taken,
    and not yet called.
  * **Checks P1–P5** run at stage 10 on x86-64 and AArch64, at a budget of
    two pages, in place of `check_quarantine`. P6 is two devmgr-proto host
    tests. Each check's negative control fired: P1 `>` as `>=`, P2 the old
    `quarantined >= 2B` rule, P3 the give-back dropped, P4 a refused unpin
    counted as freed, P5 the `SET_LIMIT` check dropped, P6 one launch path
    at `SAME_RIGHTS` and the floor not yielding.
  * **Departures from §12.2 and §12.4**, all small:
    * `DEVICE_LIMIT_PIN_ROOM` leaves out the asking device's own raised
      budget, so devmgr setting a budget again after its own restart is not
      cut by itself.
    * The requirements are split so that each check verifies a whole one:
      `L.object.48` is the `2B` rule over quarantined and live pages (P2),
      `L.object.49` the release and the kept count (P4), and `L.object.120`
      the reservation and its give-back (P3). `L.object.118` (P5) and
      `L.object.119` (P1) are as §12.4 says. H.DMA.4's criterion names the
      budget's line.
    * `device_set_limit` writes no audit record. job_set_limit does, and the
      design did not ask for one.
  * **The consultant's code review: OK IF**, on 253bff4b5 (its ledger,
    2026-10-02), which landed as is. Its three conditions are fixed forward
    on `nvidia-n0f-fix`:
    * `device_set_limit` is audited: `DEVICE_LIMIT_SET` for a set and
      `DEVICE_LIMIT` (refusal ring) for a refusal, `ACCESS_DENIED` included,
      each with the device, the limit and the old and new values. P5 reads
      the four records back; its control drops the record.
    * AoU-12 and T.EXHAUST path 7 state the untranslated-domain bound.
      Every closed pin there is kept for good and counts against `2B`. The
      review's premise that drivers pin only at start does not hold:
      `virtio-gpu` and `ltdc` pin each attached buffer and `virtio-snd` each
      stream buffer, so the budget is spent per attach as well as per
      restart. The documents say so.
    * N0d's duplicate MEMORY-AND-TIMING §2.2f is §2.2h, after N0f's §2.2g.
    * The stage-10 line now says "4 pins refused, and device_set_limit
      refused 3 times".
* **2026-10-02 — N0a, N0b and N0c on `main`, ahead of their evidence.**
  On the customer's word they were merged before the consultant's
  conditions were met. The consultant's verdicts on them (A, B) and on the
  first N0g design (C) were all OK IF. A and B are owed on `main`, fixed
  forward (`docs/handover/2026-10-02-bridge.md`).
* **2026-10-02 — N0d, N0f and N0g designed for review (§12).** Branch
  `nvidia-n0-designs`, documents only:
  * N0g is revised to meet the consultant's C1–C6 and points (i)–(xi).
    Reading for it found two things the first text did not know:
    * QEMU's VT-d model reports `ECAP.C`=0, and `vtd.rs` writes its tables
      without flushing them. That is a pre-existing gap on real
      non-coherent units, proposed as a finding.
    * QEMU 9.2 never blocks compatibility-format messages, so two of the
      seven checks cannot fire on it without a patch.
  * N0d keeps `DeviceInfo` unchanged and adds `device_aperture`, because
    `device_info` takes no length and a longer struct would overrun every
    existing caller. RM 580.173.02's own config writes were read: it
    rewrites the BARs and command register after a reset, and it reaches
    the BAR0 mirror of config space, which becomes AoU-22.
  * N0f's budget is set by devmgr through `SET_LIMIT`.
  * The ids are reserved in this branch's first commit (§12.4).
  * N0 is now 26 points, and the road 122.
* **2026-10-02 — N0d, N0f and N0g reviewed: OK IF; the customer's
  answers.** The consultant reviewed `nvidia-n0-designs` adb49fdfe
  (its ledger, 2026-10-02):
  * the reservation 82d0aae59 is OK to land as docs-only;
  * N0d is OK IF D1–D4: a smaller allowlist (vendor-capability bodies
    only, virtio's `pci_cfg_data` refused), AoU-22 widened to every BAR
    path with a read-back that refuses a rewritten node, the refusal line
    outside the lock, and `MsixTable::open` through the guard;
  * N0f is OK IF F1–F5. The first bound leaked to `3B − 1`; it is now a
    hard `2B`, with `live` reserved under the lock, pages kept for good
    still counted, and devmgr's handling of a refused budget named;
  * N0g is OK IF G1–G9. The decisive one: QEMU implements no
    compatibility-format blocking at all, in 10.2.1 as in 9.2, so a
    patched QEMU is needed for F-57 to close on any QEMU configuration.
    The rest: the console's live line converted at bring-up, isolation
    judged over every unit, a set-once mark, x2APIC per processor, Arm
    files untouched, and F-58's flushes;
  * it confirmed **F-58**, VT-d table-walk coherency (Moderate,
    pre-existing), which lands as its own fix before N0g, by another
    agent.
  
  The customer answered the same day:
  * `nvrm`'s budget is an eighth of RAM, at least 1 GiB, and the floor
    yields to the ceiling, with named lines;
  * the patched QEMU is accepted, for the gates and as the `ferrix-3060`
    domain's emulator;
  * no accepting boot option for now;
  * a firmware-locked x2APIC stops the boot by name.
  
  §12 now folds in every condition, with a table of where each is met
  per slice; the checks are renamed W, P and R. N0 is 28 points, and the
  road 124.
* **2026-10-03 — N0g's patched QEMU.** Branch `nvidia-n0g-qemu`:
  * `0002-intel_iommu-honour-CFI-and-block-compatibility-format.patch`
    against v10.2.1, with a qtest that fails with either half removed;
  * `fetch-qemu-linux.sh` builds it, x86-64 only, as
    `QEMU emulator version 10.2.1 (ferrix-cfi)` into
    `~/.local/share/ferrix/qemu`, and prints the root line that installs
    it for the `ferrix-3060` domain;
  * xtask takes it for x86-64 and refuses any other QEMU on Linux hosts;
    CI builds and caches it. test-boot passes on it under KVM and TCG.
  * Found while reading QEMU: under the split irqchip a refused route
    update leaves KVM's old route delivering (§12.3, "A refused route
    keeps the old one"), so the console's I/O APIC entry must be masked
    before it is rewritten.
  
  The kernel half of N0g is separate.
* **2026-10-03 — N0g step 1 built: every VT-d invalidation through the
  queue** (landed as slice 1, 68c49655a). The first of §12.3's steps, as its own slice:
  * register-based invalidation is gone; each invalidation is one
    descriptor and a fenced, sequence-numbered wait through F-58's
    helpers (`ferrix_paging::vtd::queue`, six host tests);
  * a unit without `ECAP.QI` is refused; firmware's `IRE` and `QIE` are
    turned off and read back; `CFI` is no longer a standing bit;
  * a failed invalidation releases nothing, and `IQE` or `ITE` marks
    the unit failed (`Cause::Queue`, FX-1007);
  * checks R6 and R7 run, and a check that firmware's queue is turned
    off (`L.iommu.51`), which the design did not list; each control
    fired. R7 waits 2 ms rather than the unit's 100 ms. R6's control is
    caught first by the PCI check's pin refusal ("virtio-rng: the
    check's own domain refused its pin"), the first unpin to flush.
  * the consultant's review (OK IF): a completion error (`ICE`) is now
    cleared and counted as it is taken, never left to fail every later
    wait, and a check plants one on every unit; queue errors firmware
    left in `FSTS` are cleared at bring-up, or the unit is refused;
    `L.iommu.49` is released to F-58's `L.iommu.56`/`57`.

* **2026-10-03 — N0g's interrupt remapping built** (branch `nvidia-n0g`,
  for the consultant before `land.sh take`, C6). §12.3's steps 2 to 8 on
  slice 1:
  * the IRT per unit that can remap, `IRE` without `CFI` and `CFIS` read
    back clear, remappable MSI and MSI-X with `msi_allocate`'s shared
    signature (no Arm file changes: x86-64 registers its hooks with
    `iommu::remapping` from `console_receive_irq`), the check vector 0xFC,
    interrupt faults 0x20 to 0x26 as `Cause::Interrupt` and
    `INTERRUPT_FAULT` records;
  * the console's I/O APIC line converted to a fresh vector at bring-up
    (amended above), masked before it is rewritten, the port serviced
    once, and check R9 over every vector handed out;
  * firmware's x2APIC on every processor; `device_isolation`, the
    set-once isolated-interrupts mark and its refusals; devmgr-proto's
    launch rule, which N1b's `Gpu` kind calls;
  * checks R1 to R10 under KVM (split irqchip) and TCG on the patched
    QEMU, and test-boot requires every line. F-57 closes with it.

  What differs from §12.3: R3 names the first MSI-X table's entry, a
  virtio function at 00:02.0, rather than the rng's; `L.device.28` is
  shown on a requester no unit places, since no QEMU function is aliased;
  a check that a present entry is never rewritten (`L.iommu.52`) was
  added; R8 puts the first application processor to arrive in x2APIC
  mode, and TCG's CPU gains `+x2apic`; R1 to R3 wait for the fault and 2 ms
  more, not 50 ms per message; and `device_isolation`'s bit 1 is not yet
  set for an AArch64 device behind a GICv3 ITS, which would change Arm
  files (G7). R5 loops a byte back through COM1, which QEMU delivers; a
  16550 on real hardware may hold its interrupt in loopback, so the check
  is the reference machine's.

* **2026-10-03 — N1a: `fetch-nvidia.sh`** (branch `nvidia-n1a`). §3 is now
  what the script does. It writes everything under
  `~/.local/share/ferrix/nvidia/580.173.02/`. No GPU was used.
  * **What it pins**:
    * GitHub's tarball of the 580.173.02 tag: sha256 `a2cd41cf…`, whose
      header names commit `20e4e6e1`;
    * `NVIDIA-Linux-x86_64-580.173.02.run`: sha256 `8d8eb900…`, as
      NVIDIA's own `.sha256sum` gives it;
    * `nvidia-smi`, `gsp_ga10x.bin` and `gsp_tu10x.bin` inside the `.run`;
    * Debian's `libc6` 2.41-12+deb13u4, Chrome's pin.
    
    The extracted `nvidia-smi`, `libnvidia-ml`, `libcuda`,
    `libnvidia-glcore` and both GSP images are byte-identical to nazuna's
    installed packages.
  * **The objects match §2.2 exactly**, rebuilt with gcc 15.2.0:
    * `nv-kernel.o` has 12,169,725 bytes of text, 13,372 defined globals
      and 406 imports;
    * `nv-modeset-kernel.o` has 1,502,689, 2,694 and 69;
    * both import lists are the feasibility pass's, line for line.
    
    One count in §2.2's prose was off by one: NVKMS imports 55 `nvkms_*`
    functions, not 56. The 69 are those, `memcpy` and 13 thunks.
  * **The volume**: `nvidia.img` is 1,253 MiB (956 MiB of files), and
    `btrfs check` passes on it. On the host, `nvidia-smi` resolves every
    library from the tree, through the volume's own loader.
  * **Disk use**: the downloads take 406 MB, `src/` with its build 220 MB,
    `run/` 1.5 GB, and the image 1.3 GB. The tree is hard links into
    `run/`.
  * **`test-uvm`** now defaults to the tree in `src/`, and passes on it.
    `FERRIX_NVIDIA_SRC` still names another tree. The feasibility pass's
    `~/.local/share/ferrix/nvidia-ref/` is no longer read by anything.
  * **Not done.** No gate attaches the volume yet; N1f's
    `test-nvidia-smi` will. The volume has no 32-bit libraries, which
    N4's 32-bit games may want.
* **2026-10-03 — N1b: the `nvrm` skeleton, and devmgr's `Gpu` kind**
  (branch `nvidia-n1b`). Outside the certified item: devmgr, its proto
  crate, `nvrm` and xtask only, no kernel file.
  * **devmgr** matches vendor `0x10de` class `03` to `nvrm`, and QEMU's
    `pci-testdev` (`1b36:0005`) to `nvrm-test`, an image only the gate's
    driver directory carries. QEMU emulates no NVIDIA function and lets no
    device's vendor be overridden (`pci-testdev` and `edu` have no such
    property; `vfio-pci`'s needs a host device), so the test device stands
    in, and is matched only by that image's presence. `edu` was not used:
    stage 10's check W7 leaves it refused until reboot.
  * **The hand-over**, `ferrix_devmgr_proto::gpu::hand_over`, host-tested
    with a fake device for every outcome and its order: the mark first
    (obligation b: `isolation::launch`, a refused mark fails the launch),
    then `device_isolation` bit 1 (obligation c: the launch is refused
    without it, after the mark and before any budget is raised), then
    `GpuBudget::plan` and `device_set_limit` before the start (obligation
    a: Whole or Cut starts; TooSmall, Refused (`NO_MEMORY`) and any other
    refusal fail the launch, each with its line, with no fallback).
  * **devmgr's lines reach the boot log** through standard error, which
    every process has open on the console from its start, as `gc400`'s do.
    No kernel change.
  * **`nvrm`** is C against ferrousli (`src/user/system/linux/drivers/nvrm/`),
    1.4 MB static. devmgr starts it with `process_create`, not the
    personality's exec path that §4.1 first said: a native start gives the
    bootstrap handle and an empty stack, and `nvrm`'s entry (`src/start.c`)
    builds `argc`, `argv`, an empty environment and the auxiliary vector
    (program headers, `AT_RANDOM` from `getrandom`, page size, name), then
    calls ferrousli's `__libc_start_main`. The job, the process handle, the
    death watch and the image-as-memory rule stay devmgr's as for every
    driver; no vDSO is mapped, so ferrousli makes those calls itself.
    `nvrm` reads START and its device from bootstrap, prints the device,
    `device_isolation` and refuses the device itself without bit 1 (the
    second guard), its budget and mark, its apertures whole, the vendor and
    device through the configuration window, BAR0's first register
    (`NV_PMC_BOOT_0` on an NVIDIA device), runs and joins a thread, and
    sleeps.
  * **The gate**, `cargo xtask test-nvrm`, boots three outcomes on the
    patched QEMU, each judged by its lines (recorded transcripts in
    `tools/common/xtask/src/nvrm/tests.rs`):
    * handed over, 4 GiB: `devmgr   gpu 00:04.0: marked for isolated
      interrupts, device_isolation 0x2 …`, `… pin budget 488 MiB, cut from
      1024 MiB …`, and `nvrm: device_isolation 0x3: interrupts isolated
      (bit 1), DMA translated`, `nvrm: pin budget 124928 pages (488 MiB)`,
      … `nvrm: skeleton up on 00:04.0; idle`;
    * refused, 512 MiB: `devmgr   gpu 00:04.0 not started: its pin budget
      of 42 MiB is under the 256 MiB nvrm needs`, no `nvrm` line, REPORT
      `1 failed`;
    * refused, `intel-iommu,intremap=off`: `devmgr   gpu 00:04.0 not
      started: its interrupts are not isolated (device_isolation 0x0)`, no
      `nvrm` line, REPORT `1 failed`. AArch64 and ARMv7-A would refuse the
      same way (bit 1 is never set there), but `nvrm` is x86-64 only, so
      their boots carry no image to match.
    It passes under KVM and TCG. A host test holds `nvrm`'s C header to the
    native ABI's numbers.
  * **Found for N1c.** An image whose `nvrm` was linked with its debug
    information (10 MB, twice in the driver directory) stopped stage 10:
    "a driver the manifest names is not in the image" (FX-1006), with every
    file unpacked. The cause was not traced: the kernel reads every driver
    whole into its own memory before devmgr starts, and maps any failure of
    that read to this sentence, and `process_create` takes images up to
    16 MiB. `nvrm` is now linked with `--strip-debug` and boots, but with
    RM's 13 MB core linked in it will be near those limits; N1c must find
    which one it was (the log is kept in
    `~/.local/share/ferrix/nvidia-n1b-logs/`).
  * **Left for N1c**: the OS layer (§4.2) and the kept C, threads and
    timers on ferrousli (one thread ran here), `interrupt_create` and
    `vmo_pin` under the mark, and `nvrm` in `run-nvidia`'s image for the
    `ferrix-3060` domain, where its first boot shows bit 1 for real.

* **2026-10-03 — N1c: RM runs in `nvrm`, its core loaded from the NVIDIA
  volume** (branch `nvidia-n1c`). This is outside the certified item:
  `nvrm`, its OS layer, xtask and docs only, with no kernel file changed.
  * **FX-1006 traced.** The kernel reads a driver's image into one heap
    block, and the largest block is 4 MiB (`MAX_ORDER` 10). Any failure of
    that read is reported as "not in the image." The customer chose option
    (b): `nvrm` stays 1.5 MB, and it loads RM's 13 MB core at run time
    (§4.1, "The core"). The consultant gave OK IF with nine conditions
    (ledger line 289), all addressed in this branch. Its BACKLOG row
    records FX-1006's misleading cause. Fixing `panic/catalog.rs` would be
    an item change, so it is not done here.
  * **The OS layer** (`os/nvos`, Rust, host-tested) and the kept MIT C are
    linked into `nvrm`. `glue/stubs.c` holds the 70 imports nothing else
    defines, each a loud stub. The `nv_dma_*` and `nv_acpi_*` stubs are
    N1d's to make real.
  * **`cargo xtask test-nvrm-link`** (on demand, as it needs the fetch)
    runs `nvrm-link-test` on the host and on Ferrix under KVM. RM
    initialises, allocates a root client, and refuses `NV01_DEVICE_0` with
    status 0x40, since there is no GPU, then exits 0. The eight negative
    controls each refuse with their own line and exit status: a flipped
    byte 25, unpinned 20, another build 35, a segment past 2 GiB 30, a W+X
    segment 29, a bad magic 32, an rwx mapping 41, and the core's base
    already mapped 37 (`MAP_FIXED_NOREPLACE`'s EEXIST, on Ferrix as on the
    host).
  * **`cargo xtask test-nvrm`** now attaches a volume carrying the core.
    In the handed-over boot, `nvrm` waits for the volume after devmgr's
    REPORT, then prints `nvos: core loaded: sha256 be83e087…, 13445824
    bytes, base 0x40000000, 17 exports, build-id …`. It passes all three
    boots under KVM and TCG, with transcripts re-recorded. A host test
    checks that a `nvos: core refused:` line fails the hand-over.
  * **The code review: OK IF** (ledger line 291). Conditions 1-9 of line
    289 were found met, and four more were set and are met in this branch:
    * C1: `core-link.py`'s privileged scan also refuses the UMIP
      instructions (Ferrix sets CR4.UMIP), `clac`/`stac`,
      `monitor`/`mwait`, `xsaves`/`xrstors`, `sysexit`, and the VMX and
      SVM instructions by name. It was shown firing on scratch objects with
      `sgdt (%rax)` and `mov %cr0,%rax`.
    * C2: the pin's same-layout check refuses when readelf or nm fails or
      prints nothing.
    * C3: `cargo xtask check` runs ferrix-nvos's host tests, its formatting
      and its clippy (step `nvos`).
    * C4: this entry.

    The consultant ruled that:
    * the `osNv_rdcr4` binding is sound: three PLT32 calls, all against the
      symbol;
    * a count computed per build replaces the design's estimate of 161;
    * the negative controls belong in `test-nvrm-link` itself, judged by
      their own line and status on the host and on Ferrix, in place of
      `gate.sh control`. A PASSED row of that gate on the landing hash is
      their evidence.

    The eighth control, base taken, was its advisory A1. It found Ferrix answering
    `MAP_FIXED_NOREPLACE` over a mapped page with EINVAL, where Linux answers
    EEXIST. `mmap` now answers EEXIST, checked by the kernel's
    `check_noreplace_refuses_a_taken_place` and its control
    `n1c-ctl-noreplace2`. The call was refused either way, so there is no
    finding (ledger line 292).
  * **Left for N1d**: the real `nvidia.img` must carry
    `usr/lib/ferrix/nvrm-core`, which xtask writes because it depends on
    `nvrm`'s build. After that comes the first boot on the 3060 and GSP
    boot.

* **2026-10-03 — N1d: GSP boot on the RTX 3060** (branch `nvidia-n1d`).
  Outside the item.
  * **`cargo xtask run-nvidia`** (N0e) boots Ferrix in libvirt's
    `ferrix-3060`. It refuses to start while a domain sharing the card runs
    or while `01:00.0` is not on `vfio-pci`, and it needs the patched QEMU
    at `/usr/local/lib/ferrix/qemu` (installed as root on 2026-10-03, with
    three local AppArmor rules for that directory). The domain has VT-d
    with `intremap='on'` on a split interrupt controller, the card behind a
    root port, the three fixture disks and the NVIDIA volume as `vdd`, the
    serial port over TCP, and the card's option ROM off: with a monitor on
    the card, OVMF's GOP put a boot framebuffer in BAR1, which the kernel
    keeps for its console and so withheld from `nvrm`. The boot's
    self-checks are skipped there (`ferrix.checks=skip`; they are
    `test-boot`'s evidence), since a timing check flakes at two vCPUs on a
    loaded host. Each boot ends with the domain destroyed.
  * **First boot**: `nvrm: device_isolation 0x3: interrupts isolated (bit 1),
    DMA translated` for the card at guest `02:00.0` (F-57's first-boot
    record), RM's core loaded from the volume, and `NV_PMC_BOOT_0`
    `0xb76000a1`.
  * **`os/kept/nv-pci.c`** keeps the subset of NVIDIA's `nv_pci_probe` and
    `nv_start_device` that applies: the legacy check, BAR0, the state's
    identity, the locks, `rm_is_supported_device`, `rm_init_private_state`,
    the BARs placed by number, the firmware request, the device list, the
    one MSI vector through `nvos_interrupt_start`, then `rm_init_adapter`.
    `glue/dma.c` maps DMA as identity, since nvos pins each page at its own
    physical address. **`rm_init_adapter` succeeds**: FWSEC, the booter and
    GSP-RM run on the card.
  * **Contiguous memory.** RM asks for physically contiguous system memory
    (the booter's ucode, among others). Ferrix commits a VMO's pages one at
    a time, so nvos retries a contiguous request until its pins follow each
    other (interim; up to 8 attempts were seen). The kernel's
    `PIN_CONTIGUOUS` replaces it (design OK IF, ledger 293, D1–D10 owed).

* **2026-10-03 — N1e and N1f: `/dev/nvidia*`, and `nvidia-smi`** (branch
  `nvidia-n1d`).
  * **The chardev core** (`src/kernel/src/interfaces/chardev`, load ring;
    design OK IF, ledger 294) forwards open, ioctl and release on major
    195's nodes to `nvrm` undecoded, as §4.4 designs it.
    `src/lib/proto/chardevctl` carries HELLO, READY and REQUEST, and the
    kernel names the nodes. Five native calls, `0x105A` to `0x105E`: the
    control, the reply, the two copies and `chardev_file`, the §4.4
    `request_file`, which NVML's `NV_ESC_REGISTER_FD` needs. mmap and poll
    are not forwarded yet.
  * **`nvrm`** says HELLO with `nvidiactl` and `nvidia0` once the GPU has
    started, and serves each ioctl on a thread of its own as the client
    the request names (`glue/chardev.c`, `nvos/src/chardev.rs`).
  * **`nvidia-smi` from the volume exits 0** and lists "NVIDIA GeForce RTX
    3060", 12288 MiB, driver 580.173.02, CUDA 13.0; `nvidia-smi -L` gives
    its UUID. This is N1's exit.
  * **Landed ahead of its conditions**, as N0 did, at the customer's word
    (the consultant's verdict, ledger 297): the item gains only the five
    call numbers and their `SERVED` rows, and the core does nothing on any
    image but `run-nvidia`'s. A reply arriving while a copy is in flight
    was found in that review and fixed before landing: the program's call
    returns only once no copy for it is running (L1). **Owed** before N2
    lands, or before `nvrm` goes into any other image (BACKLOG): ledger
    294's N10's code half (F-63), N12 and N13 (plus a control for L1), N5's
    switch when fault windows land, ledger 293's D1–D10, and a
    `test-nvidia-smi` gate. N10's text, the core's bounds, is in
    `docs/certification/MEMORY-AND-TIMING.md` §2.2k, and N15's remainder,
    what is deferred, in §4.4 (2026-10-05). Writing N10 found the queue to
    `nvrm` unbounded (F-63, reserved, ledger 361): an abandoned request
    stays queued while a new one is admitted. N10 stays owed until the
    code holds the bound.

## 11. CUDA (N5)

Written on 2026-10-02, when the customer asked for CUDA now, alongside
graphics. It replaces N5's old "40+" estimate with a sized design. The
aim is that NVIDIA's own unmodified CUDA samples run on the 3060:
`deviceQuery`, `vectorAdd`, `bandwidthTest` and a managed-memory sample.
Peer access and multiple GPUs are out of scope.

### 11.1 What CUDA needs beyond RM

libcuda talks to RM exactly as Vulkan does (§2.2, §4.4):
`/dev/nvidiactl` and `/dev/nvidia0`, channels, compute objects, memory,
`NV_ESC_RM_MAP_MEMORY` and doorbells. Most of what N2 builds serves both.
Three things are CUDA's own:

* **`/dev/nvidia-uvm`.** It is UVM's device, with a dynamic major that
  libcuda finds in `/proc/devices`. `cuInit` opens it, and CUDA does not
  start without it, even for plain `cudaMalloc`. The reason is that UVM
  owns the GPU page tables of every CUDA context:
  * `UvmRegisterGpuVaSpace` takes RM's page directory over
    (`nvUvmInterfaceSetPageDirectory`);
  * from then on, `cudaMalloc`'s video memory is mapped on the GPU by
    `UVM_CREATE_EXTERNAL_RANGE` plus `UVM_MAP_EXTERNAL_ALLOCATION`.
* **`mmap` of `/dev/nvidia-uvm`**, always `MAP_SHARED`, read-write, at
  `offset == address` (`uvm_mmap` refuses anything else). It is used for
  two things:
  * the semaphore pool (`UVM_ALLOC_SEMAPHORE_POOL`), whose pages are mapped
    once;
  * **managed memory** (`cudaMallocManaged`). Here the CPU's pages appear
    and disappear while the program runs: they appear on a CPU page fault,
    and they are taken away when the GPU's faults migrate the data to video
    memory.
* **Paths and calls** that libcuda reads, from its strings:
  * `/dev/nvidia-uvm-tools`, which only profilers use;
  * `/proc/devices`, `/proc/self/maps`, `/proc/sys/vm/mmap_min_addr`,
    `/proc/driver/nvidia/params`, `/dev/shm` and `memfd_create`;
  * `madvise` and `mremap`, which are in its imports.
  
  Large `PROT_NONE` reservations for CUDA's unified address space cost
  nothing on Ferrix: anonymous VMOs are sparse, overcommit is 1, and user
  space is 128 TiB.

**The UVM ioctls libcuda issues.** These are raw numbers (§2.2). Its code
has call sites for `UVM_INITIALIZE` and 45 of UVM's 80 numbers. The table
says which ones a single Ampere GPU needs:

| Needed by | ioctls |
|---|---|
| C1 (`deviceQuery`) | `INITIALIZE`, `MM_INITIALIZE`, `PAGEABLE_MEM_ACCESS`, `REGISTER_GPU`, `UNREGISTER_GPU` |
| C2 (`vectorAdd`, `bandwidthTest`, streams) | `REGISTER_GPU_VASPACE`, `REGISTER_CHANNEL` and their unregisters, `CREATE_EXTERNAL_RANGE`, `MAP_EXTERNAL_ALLOCATION`, `MAP_EXTERNAL_SPARSE`, `UNMAP_EXTERNAL`, `FREE`, `ALLOC_SEMAPHORE_POOL`, `VALIDATE_VA_RANGE`, `MAP_DYNAMIC_PARALLELISM_REGION`, range groups |
| C3 (managed memory) | the managed `mmap`, `MIGRATE`, `SET_PREFERRED_LOCATION`, `SET_ACCESSED_BY`, `ENABLE_READ_DUPLICATION` and their unsets, `PREVENT`/`ALLOW_MIGRATION_RANGE_GROUPS`, `MIGRATE_RANGE_GROUP`, `DISCARD`, system-wide atomics |
| Refused | `ENABLE_PEER_ACCESS`, `ALLOC_DEVICE_P2P` (peers); `POPULATE_PAGEABLE`, `PAGEABLE_MEM_ACCESS_ON_GPU` (HMM and ATS); every `TOOLS_*` and `/dev/nvidia-uvm-tools` (profilers); `RUN_TEST`; `CLEAR_ALL_ACCESS_COUNTERS` |

Some of these answers come from UVM's own switches rather than from new
code:

* `MM_INITIALIZE` answers `NV_WARN_NOTHING_TO_DO` when
  `uvm_enable_va_space_mm=0`. UVM documents that answer as "no loss of
  functionality".
* `PAGEABLE_MEM_ACCESS` answers "no" when HMM and ATS are off. libcuda
  then keeps ordinary `malloc` memory away from the GPU, as it does on any
  Linux kernel without HMM.

The C1 row is the first thing to confirm on the card. `nvrm` logs every
UVM ioctl and `mmap` it serves. If libcuda turns out to use managed memory
internally at context creation, the fault windows of §11.4 move from C3 into C2.

### 11.2 What UVM is

* **No OS layer.** RM has its `os_*` and `nv_*` seam (§2.2); UVM has
  none.
  * `kernel-open/nvidia-uvm` is 146,210 lines. It includes 117 distinct
    `<linux/…>` and `<asm/…>` headers.
  * It uses the kernel's types directly: `struct page` on 144 lines in
    24 files, `vm_area_struct` on 127 lines in 24 files, `mm_struct` on
    191 lines in 39 files, and the mmap lock on 238 lines in 30 files. All
    of these counts leave out the tests.
  * There is no portable copy in `src/`: UVM exists only as Linux code.
* **Its other half is RM.** UVM drives the GPU through 76
  `nvUvmInterface*` calls: channels, memory, fault buffers, PMA and page
  directories. `kernel-open/nvidia/nv_uvm_interface.c` (1,765 lines, MIT)
  turns them into `rm_gpu_ops_*` calls into `nv-kernel.o`.
  * In `nvrm`, that is an ordinary call within one process.
  * RM's interrupt path already hands UVM its interrupts: `nv.c` calls
    `nv_uvm_event_interrupt`, which runs UVM's top half.
  * So the GPU side of UVM, including replayable GPU faults and their
    servicing, needs nothing from Ferrix beyond what RM needs: the MSI of
    N0b, and DMA through pinned VMOs.
* **License.**
  * 228 of the 229 source files carry the MIT text.
  * `uvm_common.c` (307 lines) is GPL-2.0-or-later. It holds errno
    mapping, debug switches and a spin loop. Ferrix is MIT, so `nvrm`
    replaces it with its own code and never links it.
  * The module is declared "Dual MIT/GPL". As for RM (§3), the GPL half
    binds only a Linux kernel module.
* **What a single Ampere GPU leaves unused:**
  * HMM, ATS and pageable-memory migration (8.5k lines);
  * Confidential Computing and SEC2 (2.1k);
  * the tools interface (3.0k);
  * device P2P (0.6k);
  * the built-in tests (20.9k).
  
  The Hopper and Blackwell HAL files compile but never run. Ampere's HAL
  inherits from Maxwell, Pascal, Volta and Turing, so those files stay.

### 11.3 The build experiment

There is no seam to stub, so the experiment builds UVM against the host
kernel's own headers (7.0.0-29). It uses NVIDIA's Kbuild in a scratch copy
of `kernel-open`, with `nvidia` beside it so that the conftests run. The
imports of the resulting `nvidia-uvm.o` are what a Ferrix shim has to
provide.

| Build | `.c` files | Text | Defined globals | Undefined | Linux symbols among them |
|---|---|---|---|---|---|
| Whole module | 127 | 1.47 MB | 2,420 | 322 | 246 |
| Ferrix profile | 97 | 0.83 MB | 2,196 | 299 | 220 |

The Ferrix profile does four things:

* it turns off HMM, ATS and coherent device memory
  (`UVM_IS_CONFIG_HMM`, `UVM_HMM_RANGE_FAULT_SUPPORTED`,
  `UVM_CDMM_PAGES_SUPPORTED` and `UVM_ATS_SVA_SUPPORTED` all 0);
* it drops the tests;
* it builds with no errors and no warnings;
* the rest of the undefined symbols are the 76 `nvUvmInterface*` calls
  and three test entry points.

Turning off HMM and ATS removes, among others, `hmm_range_fault`, the
`mmu_interval_notifier_*` calls, `__mmu_notifier_register`,
`migrate_device_*`, `make_device_exclusive`, `memremap_pages` and
`iommu_sva_*`.

**The profile's 220 Linux imports, by subsystem:**

| Imports | Area | Examples |
|---|---|---|
| 71 | runtime: string and memory functions, bitmaps, rbtree and radix tree, printk, module parameters, hardening and retpoline thunks | `memcpy`, `__bitmap_*`, `rb_erase`, `radix_tree_*`, `_printk`, `__x86_indirect_thunk_*` |
| 51 | concurrency: locks, waits, threads, work queues, time | `mutex_*`, `down_*`/`up_*`, `downgrade_write`, `_raw_spin_*`, `prepare_to_wait_event`, `kthread_*`, `queue_delayed_work_on`, `ktime_get_raw_ts64` |
| 43 | kernel memory: page, slab and vmalloc allocation, page flags, NUMA | `__alloc_pages_noprof`, `kmem_cache_*`, `vmalloc`, `vmap`, `__folio_lock`, `set_page_dirty`, `node_data` |
| 24 | the process's address space | `vm_insert_page`, `unmap_mapping_range`, `find_vma`, `get_user_pages_remote`, `pin_user_pages`, `handle_mm_fault`, `mmput`, `_copy_from_user`, `_copy_to_user` |
| 16 | device files, file descriptors, procfs | `cdev_*`, `alloc_chrdev_region`, `fget`, `fput`, `proc_*`, `seq_*` |
| 11 | DMA mapping | `dma_map_page_attrs`, `dma_map_sg_attrs`, `dma_alloc_attrs`, `sg_*`, `pci_p2pdma_add_resource` |
| 4 | migration still referenced from `uvm_migrate_pageable.c` | `migrate_vma_setup`/`_pages`/`_finalize`, `devm_memunmap_pages` |

**147 of the 220 are imported by RM's own Linux glue too**
(`kernel-open/nvidia`, built in the same run), so the OS layer of §4.2
already provides them. 73 are new. Most of those are mechanical: bitmaps,
the radix tree, `sort`, delayed work, `downgrade_write` and bit waits.

The address-space imports that matter at run time are only two:
`vm_insert_page` and `unmap_mapping_range`. Of the rest:

* `get_user_pages_remote`, `pin_user_pages*`, `handle_mm_fault` and
  `migrate_vma_*` are reached only through HMM, ATS, pageable migration,
  the tools and the tests. All of these are off in the profile, so they
  become stubs that fail;
* `mmput` and `__mmdrop` are reached only through `va_space_mm`, which
  `uvm_enable_va_space_mm=0` turns off.

Unlike RM, these objects **cannot be linked into `nvrm` as they are**.
They were compiled with the kernel's inline internals: `current` read
through `%gs`, `__preempt_count`, `pv_ops`, and `vmemmap_base` for
`page_to_pfn`. So UVM is rebuilt from source against **`uvm-kpi`**, a
Linux-compatible header set of Ferrix's own, in the way FreeBSD's
LinuxKPI hosts Linux drivers. It covers only the 117 headers UVM includes,
and only what UVM uses from them:

* `struct page` is a descriptor of one page of `nvrm`'s pinned pool
  (§11.4);
* `vm_area_struct` stands for a client mapping, as `nvrm` tracks it;
* `mm_struct` is an empty token;
* `struct file` and `address_space` are the forwarding core's file
  identities.

The scratch build was deleted after counting. The import lists are kept in
`~/.local/share/ferrix/nvidia-ref/`: `uvm-whole.undef`,
`uvm-profile.undef`, `uvm-profile-linux.undef`, and
`nvidia-glue-linux.undef` for RM's glue.

### 11.4 The Linux mm services UVM uses, and where each goes

| Service | What UVM does on Linux | On Ferrix |
|---|---|---|
| Ioctls without encoded sizes | its dispatcher copies a per-command struct | The forwarding core of §4.4 is undecoded, so nothing changes in the kernel. `nvrm` knows each command's size from `uvm_ioctl.h` and copies with `request_copy_in`/`_out` |
| fd identity | `MAP_EXTERNAL_ALLOCATION` and `REGISTER_GPU` name the client's RM file (`rmCtrlFd`, `hClient`) | `request_file` (§4.4) |
| Its own system memory | `alloc_pages` for CPU chunks, page tables and push buffers; `dma_map_page` for the GPU | Pages of `nvrm`'s pool VMOs, pinned into the card's domain with `vmo_pin` as RM's are (§4.3). `uvm_cpu_chunk_allocation_sizes=4K` means no chunk needs physically contiguous memory, at the cost of 4 KiB GPU mappings of system memory |
| Kernel mappings | `vmap`, `kmap`, `page_address` | The pool is mapped in `nvrm`'s own space, so these are table lookups |
| Locks, threads, work queues, time | — | Shared with RM's `ferrix-nvos` (§4.2) |
| The semaphore pool | `vm_insert_page` of its pages at `mmap` | A window of pool pages, inserted when the client calls `mmap` (K1 below). §4.4's VMO-range reply would also do |
| **Managed memory: a CPU fault** | the vma's `->fault` services the page: allocate, copy back from the GPU, then `vm_insert_page` with read or read-write access | **K1**: the kernel forwards the fault to `nvrm`, which runs UVM's own handler and inserts the pages; the faulting thread then retries |
| **Managed memory: migration to the GPU** | `unmap_mapping_range` on the va_space's `address_space`, keyed by `offset == address`, in every process that maps the file | **K1**: `window_revoke`, a shootdown of every mapping of that window |
| `munmap`, a split, `mremap` | `->close` on the vma destroys UVM's range; this is how `cudaFree` frees managed memory, because `UVM_FREE` refuses managed ranges (`uvm_free`). `->open` splits it | K1: when a whole-window `munmap` brings the window's mapping count to zero, the kernel queues the `UNMAPPED(window)` it promised when the window was made. A partial `munmap`, `mprotect` or `mremap` of a window is refused with `EINVAL` |
| `fork` | `VM_DONTCOPY` on managed ranges, `VM_WIPEONFORK` on the semaphore pool | Nothing new: a window is inherited like every shared device mapping today, and the child's faults are served like the parent's (Linux's `MADV_DOFORK` behaviour). `MADV_DONTFORK` stays accepted and ignored |
| Process identity, `current->mm` | `va_space_mm` holds the mm for HMM and ATS | Off (`uvm_enable_va_space_mm=0`). Faults on managed memory need no client mm, because the CPU side is the window |
| Pinning client pages | the tools' `pin_user_pages_remote` | Not needed: the tools are refused. `cudaHostRegister` goes through RM's `os_lock_user_pages` and `request_pin` (§4.3) |
| HMM, `mmu_notifier`, `migrate_vma`, ATS | pageable memory on the GPU | Off; the ioctls answer "not supported" |
| Replayable GPU faults, the fault buffer interrupt | top half in hard IRQ, bottom half on a kthread queue | In `nvrm`: RM's interrupt thread runs UVM's top half, and the queue is a thread. Needs the MSI of N0b, nothing more |
| Access counters | migrations triggered by remote accesses | Off (`uvm_perf_access_counter_migration_enable=0`). Ampere has them, but nothing in the samples needs them |
| `/proc/driver/nvidia-uvm` | procfs | The procfs hook of §4.4 |

**K1, the kernel piece that cannot be avoided: fault windows.** Linux
lets UVM put its own pages into a client's page tables at any address and
take them out again. Ferrix today maps a VMO linearly
(`AddressSpace::map_file`), or maps a device range that can never be
revoked (`map_window`, `Backing::Device`). Its page faults are resolved
in the kernel. The only callout, the page cache's `Filler`
(`user/vmo.rs`, `fs/pages.rs`), is a kernel filesystem hook that may wait
on a ring-3 disk driver.

Neither covers managed memory, and the CPU fault is the part that has to
be in the kernel: no process is running while a client's thread faults.
The customer chose on 2026-10-02 to build it "as small as possible"
(D-C1), so K1 is cut to what one managed-memory sample needs. It adds one
kind of region and three native calls:

* **The window.** Each `mmap` of `/dev/nvidia-uvm` that `nvrm` answers
  with "window" becomes one region of a new kind, backed by its own
  `FaultWindow` object. The object is generic and never names NVIDIA. It
  holds:
  * a sparse table from page offset to a frame and an access level, read
    or read-write;
  * the list of address spaces that map it, as a VMO's mapper list does
    (`user/vmo.rs`);
  * its mapping count;
  * the server handle it was made for.
* **A user fault on a page the table lacks**, or a write to an entry that
  is read-only, queues one `FAULT(window, offset, access)` for the server.
  It is sent from the `Filler` position: after the region is looked up,
  with no space, window, VMO or preempt-disabling lock held. The faulting
  thread then waits, and the wait ends in one of three ways:
  * on the server's answer, after which the access is retried;
  * on `Host::wait_interrupted`, for a kill or a pending signal, after
    which the thread returns to user mode and faults again;
  * on the server's death.
  
  An error answer, or a dead window, gives `SIGBUS` (`BUS_ADRERR`) at the
  fault address. A fault from the server's own process into a window it
  serves gets `SIGBUS` at once. The wait has no timeout: a client trusts
  the server it mapped for the liveness of its faulting threads (AoU-17,
  §11.7).
* **Kernel copies never wait.** `with_page` and `with_present_page`, and
  so `futex`, `process_vm_*` and `/proc/<pid>/mem`, reach the fault path
  in a mode the type system carries, and that mode cannot forward. An
  absent window page is `Refused`, so the copy fails with `EFAULT` and the
  server sees no `FAULT`. A present page copies within its entry's
  access. Device regions stay refused whole, as today.
* **`window_insert(window, [(offset, pool_vmo, pool_offset, access)])`**,
  batched, is `vm_insert_page`. It checks:
  * the window handle's right;
  * each VMO handle's rights: read, plus write for a read-write entry;
  * that the page is committed in an anonymous VMO of the caller, not a
    file's;
  * that the offset is within the window.
  
  Any bad entry refuses the whole batch, and the table room is reserved
  before the first change. The precondition is a hold on the page
  (`Vmo::hold`), not an IOMMU pin: the hold is what keeps a frame at its
  index, so the mechanism is the same on every ISA, including ARMv7-A,
  which has no IOMMU. Holds are taken before the window lock is, and given
  back after it on a refusal. An entry whose frame or access changes is
  taken down and shot down before the new PTE goes in (break-before-make).
  So read-only becomes read-write only through a take-down.
* **The memory type of a window PTE is the pool VMO's**: cacheable,
  coherent (uncached) or write-combining, never chosen per insert, so a
  frame is never mapped with two types. Windows are never executable, at
  `mmap` or at `mprotect`.
* **`window_revoke(window, offset, len)`** is `unmap_mapping_range`:
  1. it takes the entries out under the window lock;
  2. it takes every mapper's PTEs down through the existing `forget_runs`,
     including the pending-count rule;
  3. it runs one shootdown;
  4. only then does it drop the holds.
  
  A fault racing the revoke on another processor never installs a revoked
  frame, because the entries are out before the mappers are visited.
* **The end of a window is its mapping count reaching zero.** It is not
  an object's `Drop`. The server's handle keeps the object alive, and the
  render `Window` keeper is the wrong model: `drop_unnamed` can defer a
  keeper when it has no memory for its list. The count is decremented in
  `take_down`, without allocation. The `UNMAPPED(window)` packet is
  promised on the server's port when the window is made, as a port promise
  charged to the client's job. It is queued exactly once, from whatever
  context the last mapping goes in: `munmap`, `exec`, exit, or the reaper.
  It is queued before the unmap returns, so a later `mmap` at the same
  address arrives behind it, and UVM has destroyed the old range first.
  This is how `cudaFree` of managed memory works: UVM frees a managed
  range only when its mapping goes. The window's holds go only after the
  last mapper's shootdown has returned.
* **Partial operations are refused.** A `munmap`, a `MAP_FIXED` or
  `MAP_FIXED_NOREPLACE` replacement, or an `mremap` that covers part of a
  window gets `EINVAL`, and so does any `mprotect` or `mremap` that
  touches one. The map, the tables and the window are left unchanged.
  `madvise` answers as it does for a device region.
* **`fork` shares the window.** The child is attached to its mapper list,
  no PTE is copied, and the child's faults are served as the parent's.
* **The server's identity is its server handle.** Its rights exclude
  duplicate and transfer, and its close is the server's death. Only
  faults outstanding on windows whose server handle the caller holds can
  be answered; a stale or foreign answer is ignored.
* **When the server dies**, every window is first marked dead under its
  lock, so faults stop waiting and inserts are refused. Then, in task
  context, never from a `Drop`, every window is revoked with its
  shootdown, and only then are the holds given back. The windows are
  revoked before the pool's pins fold into the quarantine (§4.3), so
  client writes cannot show in the quarantine's sums. A frame stays until
  both the quarantine's release and the window's hold let it go. A new
  `nvrm` can never map it into a client again, because dead windows
  refuse inserts and its handles are new.
* **A `FAULT` packet's slot is promised from the faulting job's
  charge.** So a flood of faults is bounded by the job's tasks and memory,
  and it can never take up `nvrm`'s port capacity.
* **Lock order**, written into `space.rs`'s and `vmo.rs`'s module docs:
  space lock, then the window's mapper list, then the window's table, then
  the VMO's pages. No window lock is held while a space lock is taken, and
  no shootdown runs under any of them. The window spin-lock sections and
  their bounds go into MEMORY-AND-TIMING.
* **One walker.** The new region kind is matched in `forget_in`,
  `take_down`, naming, `copyable`, `advise_region`, `shared_object` and
  `Named`, so revoke reuses `forget_runs`. The `ferrix-vma` variant is
  host-tested: a window region is never split, merged, marked
  copy-on-write or moved, and `clone_for_fork` shares it.

**Rings.** The window object, its table and holds, the region kind, the
fault forwarding and its wait, revocation and the refusals are in `core`
(`user/space.rs`, `user/vmo.rs` or a new `object/` file, `trap.rs`'s
`SIGBUS`, `object/oom.rs`'s fault entry). `window_insert`,
`window_revoke` and the fault answer are `item`'s native calls
(`syscall/native.rs`); the answer may ride on `window_insert` as an error
entry. The forwarding core that creates windows stays in `load` and only
calls a core constructor.

**What the certification consultant found in the first K1 text**, on
2026-10-02, and where each is met above:

| Hazard | Met by |
|---|---|
| H1: a render `Window` keeper is no model. `drop_unnamed` can defer it when it has no memory, so `UNMAPPED` would be late; and the server's handle keeps the object alive, so its `Drop` is not the end | The mapping count, and a promised `UNMAPPED` |
| H2: a space's last reference can go where nothing may sleep or shoot down | Revoke-all on death in task context, never in `Drop` |
| H3: kernel copies reach `space.fault` and would wait on `nvrm`, including `nvrm`'s own `request_copy_in` (RC2) | The no-wait mode of kernel copies |
| H4: `resolve()` treats a present page as a spurious fault, so a write to a read-only entry would loop | A write to a read-only window entry forwards |
| H5: "pinned to `nvrm`" ties a core object to IOMMU pins, which ARMv7-A keeps for good | The hold as the precondition |
| H6: a coherent or write-combining pool mapped cacheable is an alias of two memory types | The memory type from the VMO |
| H7: "nvrm's port" is undefined if the server handle can be duplicated or moved | A server handle without duplicate or transfer |
| H8: pages that clients' faults make `nvrm` commit are charged to `nvrm`'s job | Accepted as V-11 (§11.7) |

**The boot checks**, under QEMU with an in-tree test server and no GPU,
on x86-64 (KVM and TCG), AArch64, and ARMv7-A at `--smp 2` and at the
4-core default. Each has a negative control, and the landing message
gives the control's count of fired runs:

| Check | What it shows | Its control |
|---|---|---|
| A | a fault forwarded once and served | no retry |
| B | an error answer gives `SIGBUS` `BUS_ADRERR` at the address | error mapped to success |
| C | a client stuck on a server that never answers ends on `SIGKILL` within a bound | the wait ignores `wait_interrupted` |
| D | the server's death gives the waiter and later faults `SIGBUS`, and leaves the frames reachable by no client | no revoke on death |
| E | a revoke against a reader spinning on another processor: the frame is poisoned the moment its hold drops, and the reader never sees the poison | holds dropped before the shootdown |
| F | `read()`/`write()` on an absent window page fail `EFAULT`, the server's `FAULT` count unchanged; a present page copies | `with_page` forwards |
| G | a partial `munmap`, `MAP_FIXED` over part, `mprotect`, `mremap` give `EINVAL`, the region and tables intact, no `UNMAPPED` | the refusal dropped |
| H | a whole `munmap` makes `UNMAPPED` readable the instant `munmap` returns, also with every allocation in the unmap refused | through `drop_unnamed`'s fallible list |
| I | each insert refusal, and one bad entry in a batch inserting nothing | one per refusal |
| J | a write to a read-only entry makes one `FAULT` with write access, then the write lands | the spurious-fault return |
| K | a coherent pool gives an uncached client PTE | cacheable |
| L | `fork`: the child is served, a revoke reaches the child's PTE, the child's exit sends no `UNMAPPED` while the parent still maps it, and the last unmap does | — |

**What was cut from the first draft, and what each cut costs:**

| Cut | Instead | Cost |
|---|---|---|
| Kernel copies (`uaccess`) waiting on a window fault | A kernel copy that reaches a page the window lacks fails with `EFAULT` | A system call given managed memory the CPU has not touched since the GPU last had it fails: for example a `write` of a result buffer straight after the kernel that filled it. Programs that read the data first, as the samples do, see nothing. It also removes the deadlock RC2 described |
| A separate unmap notice for any range (`UNMAPPED(window, offset, len)`) | Partial `munmap`, `mprotect` and `mremap` of a window are refused with `EINVAL`; the end-of-window notice above covers a whole `munmap` | A program that unmaps part of a managed allocation, or makes it read-only, gets an error. CUDA's own allocator maps and unmaps whole allocations. C1's trace shows whether libcuda ever does otherwise |
| Not inheriting windows on `fork` (`VM_DONTCOPY`) | Windows are inherited like every shared device mapping | A child forked after `cuInit` (`system`, `popen`) shares the managed memory until it `exec`s or exits, and UVM's range stays alive that long. CUDA does not support using it in the child either way |
| `MADV_DONTFORK` honoured, `MADV_DOFORK` refused | `madvise` unchanged | None for CUDA: with windows inherited, the hint changes nothing that matters to it |
| `VM_WIPEONFORK` for the semaphore pool | Inherited too | A child sees the parent's semaphore values; it cannot use them without a CUDA context |

So the first draft's second kernel piece, K2 (the unmap notice, the fork
rule and `madvise`), is gone. Everything left lives in the window object,
in `core`, and in the native calls, in `item`. Each cut can be undone later without
changing K1's interface.

The same object also answers two things §4.4 and §8 left open:

* RM's own mapping revocation (`nv_revoke_gpu_mappings`), which §4.4 left
  as "a later message";
* R4's many small client mappings.

**Rejected alternatives:**

* **A pager VMO keyed by `offset == address`**, Zircon-style, so that
  the existing VMO reverse map does the revocation. UVM allocates a CPU
  page before it knows the address: `uvm_cpu_chunk_alloc` takes no
  address. So this would need either a patched UVM or frames that move
  between VMOs while they are pinned.
* **`SIGSEGV` handling in the client**, through a preloaded library.
  This still needs revocation, and it breaks while a CUDA kernel runs on
  the GPU and the CPU touches the same range. Ampere promises that case
  works: `concurrentManagedAccess` is 1.
* **Stopping at C2.** Without K1, `nvrm` refuses the managed `mmap`, and
  `cudaMallocManaged` fails. Every other sample runs (D-C1).

### 11.5 Milestones and points

| Slice | What | Points |
|---|---|---|
| C0a | `uvm-kpi` headers and the 73 imports that are new; UVM built from the fetched source by `fetch-nvidia.sh` in the §11.3 profile; `uvm_common.c` replaced; links into `nvrm` with nothing undefined; UVM's own built-in tests (`uvm_test.c`, 20.9k lines) runnable inside `nvrm` from a test build, as a check of the shim with no client | 8 |
| C0b | The samples: `cuda-samples` built on nazuna with the installed CUDA 12.9 `nvcc` (`/usr/local/cuda-12.9`, not on `PATH`) for `sm_86`, in a data volume beside the userspace; `test-cuda` in `xtask`, with the card guard of §6 | 2 |
| C1 | `deviceQuery`: `/dev/nvidia-uvm` and `/dev/nvidia-uvm-tools` nodes with their dynamic major in `/proc/devices`; UVM loaded in `nvrm`, its GPU registered through `nv_uvm_interface.c`; the C1 ioctls; the logged trace of every UVM call | 6 |
| C2 | `vectorAdd`, `bandwidthTest` (pinned and pageable), `simpleStreams`: VA-space and channel registration, external ranges, the semaphore pool window, replayable and non-replayable fault interrupts, UVM's CE channels | 10 |
| C3-K | K1 in the kernel (`core` ring, the consultant's conditions): fault windows, insert, revoke, fault forwarding from user faults with a killable wait, kernel copies that never wait, the promised `UNMAPPED`, refused partial operations, server death; the `ferrix-vma` host tests; boot checks A–L with their controls under QEMU on four ISAs with a test server, no GPU; the certification documents of §11.7 | 10 |
| C3-U | Managed memory in `nvrm`: the vma shim (`close` from `UNMAPPED`, no splits), CPU faults through UVM's own handler, GPU fault migration, prefetch and advice; `UnifiedMemoryStreams`, `UnifiedMemoryPerf`, and `cudaMallocManaged` with the CPU and the GPU touching the same pages in turn | 8 |
| C4 | Samples suite: `0_Introduction`, `1_Utilities` and `6_Performance` of `cuda-samples` minus IPC, multi-GPU, graphics interop and MPS; fix what they find | 8 |

**N5 is 52 points**: C0 10, C1 6, C2 10, C3 18, C4 8. C3-K went from 7
to 10 points with the consultant's conditions: four ISAs, twelve checks
with their controls, the host tests and the certification documents.

**What comes first.** CUDA needs N0 and N1. From N2 it needs the RM
half: mapping contexts, events and `poll`, fd identity, and client pins.
It does not need the Vulkan half. So the CUDA track can run beside
graphics once N1 is done:

* **to `deviceQuery`**: N0 15 + N1 32 + RM half of N2 about 8 + C0 10 +
  C1 6 = **71 points**;
* **to managed memory**: + C2 10 + C3 18 = **99 points**;
* **to the samples suite**: + C4 8 = **107 points**.

C0a and C3-K need no card and can start now. C0a is all ring 3, and C3-K
is checked under QEMU.

### 11.6 Risks

* **RC1: libcuda's unwritten expectations.** These include:
  * internal managed memory at context creation, which would move C3
    forward;
  * `mremap`, a partial `munmap` or `mprotect` of a UVM mapping, which
    K1 refuses;
  * `/proc/self/maps` naming `/dev/nvidia-uvm` for its mappings;
  * `MAP_SHARED_VALIDATE`, which Ferrix refuses with `EINVAL`.
  
  C1's trace finds them. The host's 3090 could show the same sequence in
  a minute, but it is off limits.
* **RC2: client memory in kernel copies.** Since kernel copies do not
  wait on window faults (§11.4), an ioctl or system call whose buffer
  lies in managed memory the CPU has not touched fails with `EFAULT`. That
  also rules out the deadlock in which `nvrm`'s own `request_copy_in`
  faults into `nvrm`. If a program needs it, the wait can be added for
  system calls, with window faults served by dedicated `nvrm` threads.
* **RC3: pinned volume.** Every managed page on the CPU side is pinned,
  since Ferrix has no swap and UVM DMA-maps every chunk. A managed
  working set larger than `nvrm`'s pin budget (N0f) fails to allocate
  rather than paging.
* **RC4: the shim's fidelity.** UVM leans on `struct page` reference
  counts, lock-ordering assertions and `current`. C0a's in-process run of
  UVM's own tests is the mitigation, before any client exists.
* **RC5: CPU fault cost.** Each managed CPU fault is a round trip to
  `nvrm` (2.6–6.5 µs measured, §4.4) plus UVM's own service. That service
  maps whole regions per fault, so the trip is paid per region, not per
  page. GPU faults stay inside `nvrm`.
* **RC6: identity IOMMU domains.** A frame can be pinned once
  (`AlreadyPinned`, `iommu.rs`). UVM maps each CPU chunk once per GPU,
  which is once here. A second GPU would collide.
* **R7 and R8 apply unchanged**: the card is shared, and the release is
  pinned.

### 11.7 The certified item

NVIDIA code stays out of the item, as in §6:

* UVM, `uvm-kpi` and `nv_uvm_interface.c` are linked into `nvrm`, in ring
  3;
* `check-item-boundary.py`'s `nvidia` rule covers `uvm` too.

What enters the item is generic:

* **K1, in the `core` ring** (`user/space.rs`, `user/vmo.rs` or a new
  `object/` file, the fault path in `trap.rs` and `object/oom.rs`). Its
  three native calls are in `item`. Kernel copies gain their no-wait mode;
  `fork` and `madvise` keep their behaviour, with the new region kind
  matched where every other kind is.

It does not name NVIDIA, and `check-item-boundary.py`'s `nvidia` rule
keeps it so. K1 is the mechanism any GPU driver with shared virtual memory
needs: AMD's KFD SVM, Intel's SVM, and RM's own revocation. A later
`userfaultfd` reuses this object rather than adding a second fault
callout.

**The requirements**, reserved on `main` before any code
(`tools/common/data/requirement-reservations.json`) and released by
C3-K's landing, each proven whole by one check:

* `L.user.111`–`124`: the window region (shared, never executable), the
  forwarded fault and its wait, `SIGBUS` on error or death, the
  interrupted wait, kernel copies that never wait, the refused partial
  operations, the whole unmap, the memory type, break-before-make,
  `fork`, revoke, the fault racing a revoke, server death, and the
  `FAULT` slot charged to the faulting job;
* `L.object.121`–`125`: the insert's checks, its holds, inserts refused
  into a dead or closed window, the promised `UNMAPPED` queued once and in
  order, and answers only from the server handle;
* `H.MEM.20`: a frame a server inserted into a client window is
  unreachable from every processor before its hold is released;
* `H.MEM.21`: a client's thread waits on a server only for a fault in a
  window of a device the client mapped, and the wait ends on the client's
  kill and on the server's death.

**What C3-K's landing adds to the certification documents.** The design
opens no finding; it creates obligations:

* **SAFETY-MANUAL**:
  * AoU-17: a client that maps a server's fault window trusts that server
    for the liveness of its faulting threads and for the contents of the
    window. The wait is killable and the server's death fails it; there is
    no timeout. Kernel copies of absent window pages fail with `EFAULT`.
  * AoU-18: the server keeps its clients' windows apart. The kernel does
    not stop a server inserting one client's page into another's window,
    nor pages it has not cleared. Residual data is the server's to scrub,
    as UVM's zeroing does.
  * The ASR-1 and FM-1 rows name `H.MEM.20` and check E.
  * FM-12: a partition's thread blocked by a server it mapped. Detection:
    checks C and D. Mitigation: the wait is killable, and the server's
    death fails it.
* **VULNERABILITY-ANALYSIS**, both under T.EXHAUST:
  * V-10: a stuck or malicious server stalls the faulting threads of
    every client that mapped its windows. Accepted: the mapping is opt-in,
    the wait is killable, and death fails it.
  * V-11: pages that a client's faults make the server commit, and window
    holds that keep a server VMO alive after the server unmaps it, are
    charged to the server's job, not the client's. They are bounded by the
    server's job limit, and the server apportions them (AoU-18's sibling).
  * The T.MEMORY and T.RESIDUAL text cites `H.MEM.20`.
* **ITEM.md**: the `core` paragraph ("Nothing here may depend on … a
  device driver") is amended. Isolation never depends on the server. The
  liveness of a client's thread that mapped a server's window does, by
  the client's own choice. The core's line count is re-measured.
* **MEMORY-AND-TIMING**: the window spin-lock sections and their bounds,
  and the new wait, which has no bound.
* **SECURITY-TARGET**: the window as a new TSF-mediated sharing
  (FDP_ACC). The client's `mmap` of the server's device is its consent.

The consultant reviews C3-K's code, with the check logs and the
controls, before it lands.

### 11.8 Decisions for the customer

All five were answered on 2026-10-02:

* **D-C1 — managed memory: build it, as small as possible.** C3 is in,
  with the kernel's fault windows cut to the minimum of §11.4: no waiting
  kernel copies, no partial unmaps, nothing new in `fork` or `madvise`.
  Not stopping at C2.
* **D-C2 — NVIDIA's own UVM**, unmodified, rebuilt against Ferrix's
  `uvm-kpi` header set. Not a UVM of Ferrix's own.
* **D-C3 — order: beside graphics after N1.** C0a starts now, and so does
  C3-K's design, which goes to the certification consultant before any
  `core` code is written. Neither uses the card.
* **D-C4 — profilers: `/dev/nvidia-uvm-tools` exists and refuses.**
  Nsight and CUPTI do not work. Porting the tools interface (about 3k lines
  plus `pin_user_pages_remote`) is not planned.
* **D-C5 — the samples are NVIDIA's `cuda-samples`** (BSD-3), fetched at
  a pinned tag and built on nazuna with its installed CUDA 12.9. CUDA's
  runtime is linked statically into each sample, so nothing from the
  toolkit is committed.

## 12. The kernel prerequisites still to build: N0d, N0f and N0g

N0a, N0b and N0c are on `main` (§10). Three kernel pieces stay before N1
can hand the RTX 3060 to `nvrm`. All three are inside the certified item,
so each design went to the certification consultant before any code. The
consultant reviewed this section on 2026-10-02 (`nvidia-n0-designs`
adb49fdfe): N0d OK IF D1–D4, N0f OK IF F1–F5, N0g OK IF G1–G9. Each
condition is folded in below, where it applies, and each slice's condition
table says where. They land in this order:

1. **The F-58 fix**, VT-d table-walk coherency, which is not NVIDIA's work
   and is done by another agent. It may go in parallel with N0d and N0f,
   but it must land before N0g (ruling 1);
2. **N0d**, the configuration window and 64-bit apertures (§12.1);
3. **N0f**, a pin budget per device (§12.2);
4. **N0g**, interrupt remapping on VT-d, which closes F-57 (§12.3);
5. then **N1**.

N0d and N0f are small, need no card, and do not depend on each other.
N0g is the largest. It goes last so that its code review is not held up by
the other two, and it needs N0f's per-device limit call for its
isolated-interrupts mark (§12.3, "Unisolated interrupts"). All three are
built and gated under QEMU only, with N0g's x86-64 boots on the patched
QEMU of §12.3. The consultant's conditions keep its letters (D, F, G), so
the boot checks are named W (N0d), P (N0f) and R (N0g) to keep the two
apart. The card is used again only at N1. The code of each goes
back to the consultant before `land.sh take`.

Their requirement ids are reserved on this branch in its own commit,
`tools/common/data/requirement-reservations.json`. That commit is to land
on `main` as a docs-only landing before any of the three is written, as
`docs/CONVENTIONS.md` asks. §12.4 lists the ids.

### 12.1 N0d: 64-bit apertures, and a driver's configuration window

**What exists today.**

* `DeviceInfo` (`src/lib/proto/native-abi/src/types.rs`) reports a
  device's apertures as a count. Its four `DeviceBlock`s carry an address
  and a `u32` length, and they describe only virtio's register blocks or a
  device-tree node's first two windows
  (`src/kernel/src/device.rs`: `DeviceNode::describe`, where a tree
  window's length is cut with `unwrap_or(u32::MAX)`).
* `device_info` (`src/kernel/src/syscall/native.rs`: `device_info`,
  `info_bytes`) writes exactly `DEVICE_INFO_BYTES`, 96, and takes no
  length. The struct has no spare byte: 64 bytes of blocks, five `u32`s and
  six `u16`s.
* A driver cannot read or write configuration space at all. The kernel
  writes it in five places:
  * enumeration's BAR sizing (`ferrix_pci::bar`), and the virtio entropy
    check, both at stage 10 before any node is handed out;
  * bus mastering (`DeviceNode::set_bus_master`, under the node's
    `command` lock);
  * an MSI mint's `INTx` off (`MsiFunction::mint`, under the same lock);
  * the MSI capability's mask and enable bits (`MsiFunction::set_masked`,
    no lock: it runs in interrupt handlers);
  * the MSI-X enable and function-mask bits (`MsixTable::open`, no lock).
* `IoMappingSpec` already carries a 64-bit address and length
  (`io_mapping_create`). What a driver lacks is a way to *learn* them.

**The apertures: a new call, not a longer `DeviceInfo`.** `device_info`
has no length argument, so a longer struct would be written past the
96-byte buffer of every driver built today. Those are the static native
drivers in the image, and `nvrm`'s and ferrousli's prebuilt programs. That
is a memory corruption in the caller, not a build error. So `DeviceInfo`
keeps its layout and size, and N0d adds one call:

* `device_aperture(device, index, info)` → 0 writes an `ApertureInfo` of
  32 bytes:
  * `phys: u64` and `len: u64`: exactly the minted `Aperture`'s;
  * `bar: u8`: the BAR it came from, or `0xFF` for a device-tree window;
  * `flags: u8`: `PREFETCHABLE` (what `io_mapping_map_combining` checks),
    `WHOLE_PAGES`, and `BAR_64`;
  * six reserved bytes, written zero;
  * `offset: u64`: where in its BAR the aperture starts.
* `offset` is needed because `DeviceNode::pci` cuts the MSI-X table's
  pages out of a BAR, so one BAR can become two apertures. `bar` is needed
  because an aperture the kernel withheld (`DeviceNode::mint`, overlap
  with `Reserved`) leaves no gap in the index.
* `index` runs from 0 to `DeviceInfo::apertures - 1`. Past it the call
  answers `INVALID_ARGS`. Any device handle will do, as for `device_info`:
  what enumeration found is not a capability.
* `DeviceInfo` and its 96 bytes do not change. Every existing driver is
  untouched. The tree windows' `u32` lengths stay as they are, and are
  documented as "cut at 4 GiB; `device_aperture` has the whole length". No
  tree window today is near that.
* The `DeviceNode` stores `bar` and `offset` with each `Aperture` (two
  fields filled in `DeviceNode::pci` and `mint`).

**The configuration window.** It is two calls on the device handle, both
needing `MANAGE`. It is not a mapping: a page of ECAM mapped into a driver
cannot refuse a write to one register beside another.

* `device_config_read(device, offset, width)` → value.
* `device_config_write(device, offset, width, value)` → 0.
* `width` is 1, 2 or 4, and `offset` is a multiple of `width` and below
  4096. Anything else is `INVALID_ARGS`. A function whose configuration
  space the host window did not place (`PciFunction::config_phys` is
  `None`), or a node that is not PCI, gets `WRONG_TYPE`.
* The node maps its 4 KiB of ECAM once, at the first use, and keeps the
  mapping in a `Once`, as `MsiFunction::config` does today for 256 bytes.
  The same mapping then serves every configuration write the kernel makes
  after stage 10.

**Reads.** Every byte of the function's own 4 KiB, legacy and extended,
is readable, the kernel-owned registers included, the MSI and MSI-X
capabilities among them (ruling 5b: the message is not a secret, and
refusing it buys nothing). A configuration read
has no side effect in PCI, and a driver learning its BARs or its MSI
capability from it learns nothing it could abuse: the addresses are
already its apertures, and the message is the kernel's. Reads of a
function the driver does not hold are impossible, since the call names
the node. The root port's space, which RM reads (below), is not the
driver's to read.

**Writes: what is kernel-owned.** A write is accepted only if every byte
it touches lies in a **driver-writable range** of that function. Any other
write is refused whole: `ACCESS_DENIED`, nothing written, nothing
partially written. Refused, not silently dropped. A dropped write hides a
broken driver, and RM checks its writes by reading back. The writable
ranges are an allowlist, computed once at stage 10 from the function's
two capability lists by a new `ferrix_pci::window::writable` (host-tested,
below):

| Range | Writable? | Why |
|---|---|---|
| The header, 0x00–0x3F: command, status, BARs, expansion ROM, cache line, latency, BIST, interrupt line | no | the kernel's: memory decoding and bus mastering (`enable_dma`), `INTx` off, the BARs the apertures were minted from; status is write-one-to-clear and errors are the kernel's to see; BIST can reset the function |
| MSI capability (0x05), whole | no | the kernel mints the message (N0b), and with N0g the remappable handle |
| MSI-X capability (0x11), whole | no | enable and function mask are the kernel's (`MsixTable::open`) |
| Power management (0x01), whole | no | D3hot and back can reset the BARs (`No_Soft_Reset` clear) behind the kernel |
| PCI Express (0x10), whole | no | Device Control's function-level reset and No Snoop, Link Control's retrain and ASPM |
| Every other standard capability, whole | no | not needed until a driver shows otherwise |
| Vendor-specific capability (0x09), past its first three bytes | **yes**, except virtio's `pci_cfg_data` window | the device's own registers. RM's PBI mailbox is one (`pci_pbi.c`). Within virtio's `VIRTIO_PCI_CAP_PCI_CFG` (vendor 0x1AF4, `cfg_type` 5) the 4-byte `pci_cfg_data` window is refused: it reaches the BARs, including the MSI-X table pages `DeviceNode::pci` withheld. Its `bar`, `offset` and `length` fields stay writable (D1), which check W3 uses |
| Extended: ATS, PRI, PASID, SR-IOV, ACS, resizable BAR, VF resizable BAR, DPC, AER, every other | no | ATS, PRI and PASID would bypass translation (SAFETY-MANUAL AoU-12); SR-IOV makes functions enumeration never saw; resizable BAR changes an aperture's size |
| Extended vendor-specific (VSEC, 0x000B), past its 8-byte header | **yes** | as 0x09 |
| Bytes 0x40–0xFFF that no capability of either list covers | no | device-dependent space. Refused until a driver names a register and a reason; it is then added for that `vendor:device` alone, with review (D1, ruling 5) |

Each refusal is printed once per node and offset, as
`config   pci 01:00.0 refused a 4-byte write at 0x10 (BAR 0)`. N1 then
finds which of RM's writes need a decision, rather than discovering them
one failed boot at a time. The line is printed after the IRQ lock is
dropped, never under it. The once-per-offset record is a bitmap of 1,024
bits per PCI node, one per dword, allocated at stage 10 with the node and
only read and set under the lock (D3).

**What RM writes, read from 580.173.02 before any boot.** Grepping
`osPciWrite*` in `src/nvidia` finds three kinds of write:

* **The PBI mailbox** in a vendor-specific capability. Allowed.
* **The header**, after a reset. `kbifRestoreBarsAndCommand_GA100` and
  `kbifRestoreBar0_GA100` (`kernel_bif_ga100.c`) write the BARs and the
  command/status dword back. `os_init.c` also forwards writes to the
  BAR0 mirror of config space to config cycles when RM runs passed through
  a hypervisor. Refused. `nvrm`'s OS layer answers a header write whose
  value equals what the register holds with success and no call, and any
  other with an error. Restoring a function after a reset is the kernel's
  job, and a later `device_reset` call (§8 R2, not in N0) will do it under
  the node's lock.
* **The root port's Link Control** (`chipset_pcie.c`, through
  `gpuClData.rootPort`). Not the driver's device. The OS layer returns
  failure, which RM treats as "no root port access", as it does on
  hypervisors that hide the port. Whether every such path is non-fatal is
  N1's to confirm.

**A residual the window cannot close: configuration mirrors in a BAR.**
Some devices expose their own configuration space through a BAR. NVIDIA
GPUs mirror it at BAR0 + 0x88000 (`NV_PCFG`; RM uses it as
`DEVICE_BASE(NV_PCFG)`), and virtio's `VIRTIO_PCI_CAP_PCI_CFG` is the
reverse path. A driver holding BAR0 can therefore write the GPU's command
register and BARs without the config window. What this can and cannot
do:

* **Bus mastering on** reaches only the device's domain. The IOMMU, not
  the command register, confines DMA (H.DMA.1–3).
* **A rewritten MSI message** is what N0g's source-ID check stops (§12.3).
  Before N0g it is F-57 again.
* **A moved BAR** is the real residual. The device then decodes an
  address range it was not given, possibly over another device's aperture
  or a hole. The processor reaches it only through the driver's own
  mappings, which still point at the old address. But the device could
  answer another device's driver's accesses, and break that driver.

So this is an obligation, and the kernel cannot prevent it. It goes in
the SAFETY-MANUAL as **AoU-22**, widened by the consultant (D2, ruling 6):
**a driver writes no kernel-owned register or structure through any BAR
path.** That covers configuration mirrors (`NV_PCFG`, and any other mirror
RM uses, which N1 lists from the sources) and the MSI-X table and its
pending-bit array. For NVIDIA it is `nvrm`'s OS layer, which routes
`NV_PCFG` writes to the config window, where they are refused. QEMU's own
`vfio-pci` traps the same mirror for the same reason (its NVIDIA BAR0
quirk). The device's own firmware (GSP) can rewrite its configuration
without the driver, so the AoU rests on the device as much as on the
driver.

**The kernel detects a breach (D2).** It cannot prevent one, so before N1
hands the card over it checks for one. `DeviceNode::verify_config` reads
back what the kernel minted and compares:

* the command register's memory-decoding, bus-master and `INTx`-disable
  bits against the kernel's state (`dma_on`, the mint);
* every BAR against the address its apertures were minted from;
* the MSI capability's control, address and data, or the MSI-X
  capability's control, and the address and data of every minted MSI-X
  table entry.

It runs at each accepted HELLO, beside `quarantine_release`, in every core
that calls it, and in `device_quiesce`, which devmgr calls after every
driver's death (and the kernel's own `quiesce`). On a mismatch:

* bus mastering goes off;
* the node is **refused**: a flag after which every call that needs
  `MANAGE` on it answers `BAD_STATE`, until reboot;
* one line names the register:
  `device   pci 01:00.0 refused: BAR1 reads 0x… where 0x… was minted (a driver or its firmware rewrote a kernel-owned register)`.

Check W7 below shows it, with a control.

**One lock per node, as a type.** The consultant asked for every write of
a function's configuration space under one lock per node. Today's
`command: SpinLock<()>` becomes
`config: IrqSpinLock<(), arch::Irq>`. It is an IRQ lock because
`MsiFunction::set_masked` writes the MSI capability from interrupt
handlers, and a plain lock taken there could deadlock against a holder on
the same processor. The lock holds a `ConfigWrites` guard. That guard is
the only type that implements `ferrix_pci::ConfigSpace`'s writes over the
node's mapping. `MappedConfig` loses its `write16` and `write32`, and
keeps its reads. So a write without the lock does not compile. Every
writer takes it:

* `set_bus_master`;
* `MsiFunction::mint`, for `INTx` off and `Msi::program`;
* `set_masked`;
* `MsixTable::open`, for enable and function mask. It loses its own
  `vmap` of configuration space and goes through the node's mapping and
  the guard like the rest (D4);
* `device_config_write`.

Enumeration's writers, BAR sizing (`ferrix_pci::bar`) and the virtio
entropy check, run at stage 10 before the node is published, through
enumeration's own mapping and outside the type. So `L.object.117` speaks of
a *published* node, and is exact (D4).

It is a leaf lock. Nothing inside it allocates, maps, sleeps or takes
another lock. The mappings are made before it is taken, as
`set_bus_master` does now. The hold is one to three MMIO writes, and its
section goes into MEMORY-AND-TIMING. A driver's write and a kernel write
to neighbouring bytes of one dword cannot tear, because ECAM writes carry
byte enables and each writes only its own bytes. The lock rules out
read-modify-write interleavings in the kernel's own sequences, which is
the race N0b's condition 2 named.

**Rings.** `device.rs`, the two calls and `device_aperture` are `core` and
`item`, as `device_info` is. `ferrix_pci::window` joins
`ferrix_pci::topology` and `ferrix_pci::msi` in ITEM.md §2's unclassified
list. A clippy `disallowed-methods` entry is unnecessary: the guard type
already makes an unlocked write unrepresentable.

**The boot checks**, stage 10, x86-64 (KVM and TCG) and AArch64, which
both have ECAM and an emulated PCIe device; ARMv7-A runs the aperture
check. Each has a negative control, and the landing message gives each
control's FIRED count:

| Check | What it shows | Its control |
|---|---|---|
| W1 | `device_aperture` for every aperture of every node equals what enumeration minted, and one aperture above 4 GiB, longer than 4 GiB, is reported whole. That is a `pci-testdev,membar=8G` on xtask's q35, whose 64-bit prefetchable BAR can only go above 4 GiB (if OVMF's 64-bit window is too small for it, `-fw_cfg opt/ovmf/X-PciMmio64Mb` widens it; the first boot tells) | the length cut to `u32` |
| W2 | a read of every header field equals what enumeration read; reads of the MSI capability and of extended space are answered | — |
| W3 | a write to a driver-writable byte sticks: virtio-rng's `VIRTIO_PCI_CAP_PCI_CFG` `offset` field, which QEMU makes writable, read back; a write to that capability's `pci_cfg_data` is `ACCESS_DENIED` | the allowlist empty; and the `pci_cfg_data` exception dropped |
| W4 | writes to `COMMAND`, `BAR0`, the MSI capability (edu, on x86-64) or the MSI-X capability (virtio-rng), the PCIe capability's Device Control (virtio-rng), a device-dependent byte no capability covers, and the ATS extended capability, if present, are each `ACCESS_DENIED`, and each reads back unchanged | the allowlist taken as "all but the header" |
| W5 | a 2-byte write spanning a writable and a kernel-owned byte is refused whole; an unaligned write and offset 0x1000 are `INVALID_ARGS` | the whole-range test reduced to the first byte |
| W6 | `enable_dma` and `disable_dma` racing `device_config_write`s of a writable byte on another processor, 1,000 rounds (the consultant's advisory, given the test-time priority): bus mastering always reads back as `dma_on` says | — (the type makes the unlocked writer uncompilable; the check shows the locked one works under load) |
| W7 | the breach detector: the check flips edu's `INTx`-disable bit (or virtio-rng's on AArch64) through enumeration's mapping, as firmware could, then calls `verify_config`: bus mastering reads back off, the node answers `BAD_STATE`, and the line names `COMMAND`. The check then restores the bit and clears the refusal through a check-only path | `verify_config` made to return without comparing: the node stays usable and the check fails |

ARMv7-A has ECAM under the device tree too, but no device with a writable
vendor capability. Checks W2 to W5 and W7 run there with W3 skipped, and
the log says so.

**Host tests**, `ferrix_pci::window`, over synthetic configuration
spaces:

* the table above, row by row, virtio's `pci_cfg_data` exception and the
  refused device-dependent space included;
* overlapping or looping capability lists, which are refused, and the
  function then gets no writable range;
* a vendor capability whose length runs past 0xFF, which is cut;
* a capability header itself, which is never writable.

The control is the walker made to skip the extended list, after which the
ATS test fails.

**Documents.**

* SAFETY-MANUAL: AoU-22, as widened.
* MEMORY-AND-TIMING: the `config` lock's section.
* VULNERABILITY-ANALYSIS: T.DMA gains the BAR-path residual, under AoU-22,
  and the breach detector.
* ITEM.md §2: `ferrix_pci::window`.
* `device.rs`'s comments that a driver "cannot walk configuration space"
  (`RegisterBlock`, `PciFunction`) are amended.

**The consultant's conditions, and where each is met:**

| Condition | Met by |
|---|---|
| D1: start smaller: vendor-capability bodies only; device-dependent space refused until a driver names a register; virtio's `pci_cfg_data` refused | the allowlist table; checks W3, W4 |
| D2: AoU-22 widened to every BAR path; the kernel detects a breach at HELLO and at a driver's death | "A residual the window cannot close", "The kernel detects a breach"; check W7 |
| D3: the refusal line outside the IRQ lock; its record allocated at stage 10 | "Writes: what is kernel-owned" |
| D4: `MsixTable::open` through the node's mapping and guard; enumeration's writers stated as before publication | "One lock per node, as a type" |
| Ruling 5b: reads answered in full | "Reads" |

**Points.** 5, up from 3. The ABI call, the guard-type change across five
writers, the allowlist walker with its host tests, the breach detector,
and seven checks are more than the row was sized for.

### 12.2 N0f: a pin budget per device

**What exists today** (`src/kernel/src/object/pin.rs`):

* A pin closed because its process died goes to a quarantine. Its pages
  stay mapped in the device's domain, and its frames are held and charged
  to nobody, until a core accepts the device's next HELLO
  (`quarantine_release`, called by the block, net, audio, display and
  input cores).
* `Pin::with_cap` refuses a new pin with `QUARANTINE_FULL` while the
  device's quarantine holds `QUARANTINE_CAP_PAGES` or more. That is
  2 × (65,536 + 1,024) pages, about 520 MiB, sized for two deaths of the
  largest driver today: the display card's 256 MiB.
* Requirements `L.object.47`–`49` and the boot check
  `object/pin/check.rs`: `check_quarantine`, at a cap of one page.
* Nothing bounds a live driver's pins except its job's memory limit, and
  the pages a death strands are the live pins of the driver that died.

**Why it fails for a GPU.** A busy `nvrm` pins far more than 256 MiB:
GSP's tens of MiB, plus every Vulkan and CUDA allocation in system
memory, plus clients' pages it locks (§4.3). Its death puts all of that in
the quarantine, which is then over the cap. The next `nvrm` must pin
GSP's firmware *before* it can send the HELLO that would release the
quarantine. Every one of those pins is refused, so the driver can never
come back. And a cap raised for the GPU would apply to every device.

**The rule.** Each device node gets a pin budget `B`, in pages, and
three counts, all kept on the domain and read and changed only under the
quarantine lock (`QUARANTINE`'s, a leaf lock):

* `live`: pages of live pins into its domain, **reserved** before the pin
  is made (below);
* `quarantined`: pages its quarantine holds. Today that is found by
  walking the global list (`quarantined_pages`); N0f keeps the count;
* `kept`: pages kept for good because the unit would not give them back.
  That covers `Pin::drop`'s failed unpin, `quarantine()`'s
  could-not-quarantine fallback, and `release()`'s `back.leak()`. These
  stay counted for the life of the machine (F3), or the bound below would
  not be one.

A pin of `n` pages is refused, with nothing held, mapped or charged:

* with **`LIMIT_REACHED`**, a new status, when `live + n > B`;
* with **`QUARANTINE_FULL`**, as today, when
  `quarantined + kept + live + n > 2B` (F1).

**The bound is `2B` per device, hard.** A death moves its pins' pages
from `live` to `quarantined` (or `kept`), so the sum of the three never
grows at a death, and no pin is admitted past `2B`. That is today's
two-driver argument (the last driver that published, and one that died
before publishing, after which devmgr starts no other; `docs/DEVMGR.md`
§4) with the device's own budget in place of the display's. The first
design's "`quarantined ≥ 2B`" let a quarantine at `2B − 1` admit `B` more
live pages, so a death could leave `3B − 1`. The consultant found that
(F1), and the sum above closes it.

**No overshoot (F2).** The check and the count are one step. Under the
quarantine lock, `vmo_pin` tests both rules and adds `n` to `live`, then
drops the lock and calls `Domain::pin`. If the pin fails, `n` is given
back under the lock. A pin can therefore never pass the budget, which
today's check-then-pin could. `device_set_limit`'s "no live pins" test is
made under the same lock, so a budget cannot change between a pin's check
and its count.

**Who sets it: devmgr, by a right the driver never holds.**

* New call: `device_set_limit(device, DEVICE_LIMIT_PIN_PAGES, pages)` →
  0. It needs `SET_LIMIT` on the device handle. That is the right
  `job_set_limit` uses (`rights.rs`), and for the same reason: the limit
  is the delegator's, not the delegatee's.
* New call: `device_get_limit(device, which)` → pages, needing no right.
  It answers for `DEVICE_LIMIT_PIN_PAGES`, the device's `B`;
  `DEVICE_LIMIT_PIN_CEILING`, the machine's ceiling below; and
  `DEVICE_LIMIT_PIN_ROOM`, the ceiling less what raised budgets hold.
  devmgr reads RAM's share from these, so it needs no memory call of its
  own.
* The kernel hands devmgr each device with `DEVICE | SET_LIMIT`. Every
  devmgr launch path already narrows the handle to
  `Requested::Exactly(DEVICE_RIGHTS)` before the hand-off
  (`devmgr/src/main.rs`, e.g. the blk path; the consultant checked every
  path), so the driver never holds `SET_LIMIT`. A devmgr host test asserts
  that for every launch path.
* devmgr sets `B` from its match table. Every kind keeps today's default,
  `LARGEST_DRIVER_PIN_PAGES + 1024`, unless its entry says otherwise. So
  `2B` at the default is `QUARANTINE_CAP_PAGES`, and nothing changes for
  existing drivers.
* `device_set_limit` is `BAD_STATE` while the device has live pins (under
  the lock, F2). A budget is never changed under a running driver, so a
  lowered budget never strands pins already made over it.
* **The kernel's own ceiling.** The sum of `2B` over devices whose budget
  was raised above the default may not exceed a quarter of RAM, counted at
  stage 10. A `device_set_limit` that would pass it is `NO_MEMORY`, with
  nothing changed.
* **The default is not counted** (F5, ruling 7). It is today's accepted
  per-device cap (AoU-12, and devmgr's restart rule). T.EXHAUST states the
  whole formula: the memory the kernel may hold for no job is at most
  (devices at the default) × 2 × the default, plus the ceiling.

**`nvrm`'s budget, and what devmgr does when the ceiling refuses it.**
The customer decided on 2026-10-02 that `B` is one eighth of RAM and at
least 1 GiB. Below 8 GiB of RAM, 1 GiB is more than an eighth, and then
`2B` is more than the ceiling of a quarter of RAM; the consultant asked
what happens then (F4). devmgr does this, for the `Gpu` kind, and every
outcome prints a line:

1. Read `CEILING` and `ROOM`. The budget wanted is
   `max(CEILING / 2, 1 GiB)`: an eighth of RAM, with the floor.
2. **The floor yields to the ceiling.** If `2 × wanted > ROOM`, the
   budget is `ROOM / 2`, and devmgr prints
   `devmgr   gpu 01:00.0: pin budget 512 MiB, cut from 1024 MiB: the 1 GiB floor yields to the kernel's ceiling (4096 MiB of RAM)`.
   Otherwise it prints `devmgr   gpu 01:00.0: pin budget 1024 MiB (an eighth of RAM, at least 1 GiB)`.
3. **Below what `nvrm` can run with, the GPU is not started.**
   `NVRM_MIN_PIN_PAGES` is the least `nvrm` needs to boot GSP and serve
   one client: firmware, logs, rings and the first channel. It is a
   constant in devmgr's `Gpu` entry, set from N1d's measurement, and
   256 MiB until then. If the budget is under it, the launch fails with
   `devmgr   gpu 01:00.0 not started: its pin budget of 96 MiB is under the 256 MiB nvrm needs`.
4. If `device_set_limit` still answers `NO_MEMORY`, because another device
   took room in between, the launch fails with
   `devmgr   gpu 01:00.0 not started: the kernel refused its pin budget of 512 MiB (past the ceiling)`.

There is **no silent fallback to the default**, which the consultant
ruled out. A launch that fails is a named line, and the device stays
unstarted. devmgr's host tests cover all four outcomes, with RAM of 16,
4 and 1 GiB and a ceiling already partly taken.

**Across a restart and the quarantine.**

* The budget belongs to the node, not to a process. It survives every
  driver's death and start, and devmgr's own restart, since the node
  outlives devmgr.
* `quarantine_release` takes off `quarantined` exactly the pages it gave
  back. A pin whose unpin it could not complete moves to `kept` (F3).
  `live` is the new driver's.
* A pin made by `nvrm` over a *client's* pages (`request_pin`, §4.3) is
  `nvrm`'s pin. It counts in `nvrm`'s device's `live`, and is quarantined
  if `nvrm` dies, like `nvrm`'s own.
* On an untranslated domain (ARMv7-A, or a unit that would not come up)
  there is no quarantine. A closed pin's frames are kept for good
  (`Pin::drop`), and they count in `kept`. The same `2B` bound then holds
  what each driver's death keeps.
* A boot check's pins (`Pin::with_cap`, `quiet`) keep their own budget
  argument, so the check can drive a budget of one or two pages, as it
  drives the cap today.

**Failure behaviour.** Both refusals are statuses the driver sees at
`vmo_pin`, with nothing changed. The first `LIMIT_REACHED` and the first
`QUARANTINE_FULL` per device are printed, with the device and its counts.
`quarantine_release`'s line gains the device's `live`, `quarantined`,
`kept` and `B`. A driver refused at its budget fails its own allocation,
as it would on `NO_MEMORY`: RM turns it into `NV_ERR_NO_MEMORY`.

**The boot checks**, stage 10, x86-64 (KVM and TCG) and AArch64. ARMv7-A
has no translated domain, so the quarantine checks are skipped there, as
today. Each has a negative control, and the landing message gives each
control's FIRED count:

| Check | What it shows | Its control |
|---|---|---|
| P1 | with `B` = 2 pages: a 2-page pin is taken; another 1-page pin is `LIMIT_REACHED`, and `live` is unchanged | `>` turned into `>=` (an off-by-one) |
| P2 | the 2-page pin closed by a dead owner: `quarantined` = 2, `live` = 0; a new 2-page pin is taken (2 + 0 + 2 ≤ 4); after its owner dies too (`quarantined` = 4) a 1-page pin is `QUARANTINE_FULL`. Then, from `quarantined` = 3, a 2-page pin is `QUARANTINE_FULL` (3 + 2 > 4), which the first design's rule would have taken | the rule left as `quarantined ≥ 2B` (the F1 hole) |
| P3 | a pin whose `Domain::pin` the check makes fail gives its reserved pages back: `live` is as before | the give-back dropped |
| P4 | after `release`, `quarantined` = 0 and a pin up to `B` is taken again; an entry `release` cannot unpin (a check-only refusal) moves to `kept` and still counts | release clearing the whole count |
| P5 | `device_set_limit` is `ACCESS_DENIED` without `SET_LIMIT`, `BAD_STATE` with live pins, and `NO_MEMORY` past the ceiling with nothing changed; `device_get_limit` answers the three values | the rights check dropped |
| P6 | (devmgr, host tests) every launch path narrows the device handle to `DEVICE_RIGHTS`; the `Gpu` budget's four outcomes print their lines | one path left at `SAME_RIGHTS`; the floor not yielding |

`check_quarantine` becomes P2 and P4. `L.object.47`–`49` are reworded from
"the cap" to "twice the device's budget, counted with live and kept
pages", which changes three existing requirements and is listed so in the
landing.

**Documents.**

* SAFETY-MANUAL AoU-12's sentence on the cap.
* VULNERABILITY-ANALYSIS T.DMA path 5 (the quarantine) and T.EXHAUST: the
  kernel-held memory is `2B` per device, and the whole formula with the
  default and the ceiling (F5).
* MEMORY-AND-TIMING: the counts under the quarantine lock, with no walk
  of the list on the pin path any more.
* `docs/DEVMGR.md`: the budget column, the `Gpu` budget's rule and its
  lines.
* §4.3's sentence becomes "a per-device budget devmgr sets".

**The consultant's conditions, and where each is met:**

| Condition | Met by |
|---|---|
| F1: the bound was `3B − 1`; refuse on `quarantined + live + n > 2B` | "The rule", "The bound"; check P2 |
| F2: `live` reserved under the lock before `Domain::pin`, given back on failure; `BAD_STATE` tested under it | "No overshoot"; checks P3, P5 |
| F3: pages kept for good stay counted; the release clears only what it gave back | `kept`; check P4 |
| F4: the 1 GiB floor against the ceiling below 8 GiB; no silent fallback | "`nvrm`'s budget"; check P6 (with the customer's decision) |
| F5 (ruling 7): the default not counted; T.EXHAUST's whole formula | "Who sets it"; Documents |

**Points.** 3, up from 2: two calls and a right, the counts moved onto the
domain under one lock, devmgr's table and its budget rule, and six checks.

### 12.3 N0g: interrupt remapping on VT-d

**What exists today.**

* `src/kernel/src/iommu/vtd.rs` runs VT-d in legacy mode with
  register-based invalidation (`Unit::invalidate_context`,
  `Unit::invalidate_iotlb`). Its module comment says queued invalidation
  and interrupt remapping are not used.
* `Unit::open` checks the version, three-level tables, `RWBF`, and that
  firmware did not leave translation on. It checks no coherency bit, and
  nothing in the kernel issues `clflush`. That is F-58, fixed on its own
  before N0g (below).
* `Unit::command` repeats the standing enables in `STANDING`, which
  includes `CFI` (bit 23). A `CFI` that firmware left set would be carried
  into every later `GCMD` write.
* `src/kernel/src/arch/x86_64/msi.rs`: `msi_allocate` builds a
  compatibility-format message from a 64-slot vector bitmap (`TAKEN`,
  vectors 0x40–0x7F). It already takes the requester ID, and ignores it.
* `src/kernel/src/arch/x86_64/apic.rs`: `IoApicInput::route` writes a
  compatibility RTE. Its one user is the console's receive line. That line
  is live from stage 3 (`main.rs`: `start_console_input`, before
  `iommu::bring_up`). The kernel's own vectors are `IPI_VECTOR` 0xFD,
  `TIMER_VECTOR` 0xFE and `SPURIOUS_VECTOR` 0xFF.
* `Unit::take_fault` reads `F`, the source ID and the page, and ignores
  the fault-reason byte. An interrupt-remapping fault would be counted
  today as a DMA access fault on page 0.
* xtask's q35 has `intel-iommu,intremap=off` and an in-kernel irqchip
  (`tools/common/xtask/src/qemu.rs`).
* **QEMU implements no compatibility-format blocking at all.** The
  consultant read both the gates' 10.2.1 (upstream tag; Ubuntu's package
  patches nothing in `intel_iommu`) and 9.2.4.
  * `vtd_handle_gcmd_write` ignores `GCMD` bit 23, and nothing sets
    `GSTS.CFIS`. So "`CFIS`=0 read back" is constant on QEMU and proves
    nothing.
  * `vtd_interrupt_remap_msi` passes a compatibility-format message
    through ("This is compatible mode.") and never raises fault 0x25.
  * The KVM route fix-up (`kvm_arch_fixup_msi_route` to `int_remap`) goes
    through the same function and passes such messages too.

**What the consultant required, and where each is met below.** It gave
two verdicts on 2026-10-02: C on the first N0g text (C1–C6, points
(i)–(xi)), and G on this revision (G1–G9, rulings 1–4 and 8).

| Condition | Met by |
|---|---|
| C1: ids reserved first | §12.4, and this branch's reservation commit (the consultant: OK to land as docs-only) |
| (i) every register invalidation to the queue, including domain teardown, unpin and the quarantine | "The invalidation queue"; check R6 |
| (ii) a wait that does not complete, IQE or ITE, is a failed invalidation; nothing released | "The invalidation queue"; check R7 |
| (iii) DMAR `INTR_REMAP`, `ECAP.QI`, `ECAP.IR`, `ECAP.C` checked | "What a unit must offer"; `C`=0 handled by F-58's fix |
| (iv) firmware left IR, QI or x2APIC on | "Firmware's state"; check R8 |
| (v) `CFI`=0, `CFIS`=0 read back before `IRE`; the boot line says so | "Bring-up order", with G1's correction |
| (vi) remappable I/O APIC RTEs; RTE vector = IRTE vector; SID by I/O APIC ID | "The I/O APIC"; check R5 |
| (vii) the HPET needs no IRTE | "The HPET" |
| (viii) F-57 adds NMI, SMI and INIT; a forged-NMI check | "F-57's text"; check R2 |
| (ix) IRT and queue pages, the allocator, memory and timing | "Memory and timing" |
| (x) IR faults 0x20–0x26 apart from DMA faults | "Faults" |
| (xi) 6 → 8 points | "Points" |
| C3: a GPU refused without isolated interrupts | "Unisolated interrupts" (no accepting boot option, by the customer's call so far) |
| C4: boot checks on KVM (split irqchip) and TCG, each with a control | "The boot checks", R1–R10 |
| C5: Arm code unchanged; the GICv2m residual recorded on its own | "Arm" |
| C6: order | §12 intro, with F-58 first (ruling 1) |
| EIME=0 and the xAPIC destination format | "The table and its entries" |
| IRTE written high half first, low with Present last; IEC and wait before unmask; a present IRTE never edited | "The table and its entries" |
| G1: QEMU implements no `CFI`; `CFIS`=0 proves nothing there, and the boot line must not claim "blocked" from it | "The patched QEMU"; "Bring-up order"'s line |
| G2 (ruling 2): option (a) required, in xtask's QEMU and in the `ferrix-3060` domain's `<emulator>` | "The patched QEMU" (the customer said yes) |
| G3: the console's line is live before IR bring-up: convert it at bring-up; service the port once; a defined no-scope outcome; no compatibility-format MSI minted by then; IRTE and IEC outside `CONSOLE_INPUT`'s lock | "The I/O APIC"; checks R5, R9 |
| G4: isolated needs `IRE`=1 and `CFIS`=0 on **every** DMAR unit, refused ones included | "Unisolated interrupts" |
| G5: the isolated-interrupts mark is set-once; devmgr's GPU launch fails if setting it fails | "Unisolated interrupts"; check R10 |
| G6: x2APIC checked per processor in `secondary_start` | "Firmware's state"; check R8 |
| G7: `msi_allocate`'s shared signature stays | "Messages" |
| G8: on `C`=0, fresh frames flushed whole before they are linked or published | F-58's fix; N0g's own frames through its helper ("The table and its entries", "Bring-up order") |
| G9: the `SW`=0 wait only through a check-only parameter; its kept frames counted | check R7 |
| Ruling 3: a dedicated 0xFC | check R1 |
| Ruling 4: units without QI refused | "What a unit must offer" |
| Ruling 8: the accepting boot option allowed, advised against | not added (customer still deciding) |

**What comes first: F-58.** The consultant confirmed the coherency gap as
**F-58** (Moderate, pre-existing). On a unit with `ECAP.C`=0, `vtd.rs`
writes root and context entries, and `ferrix_paging` writes second-level
ones, with no `clflush`. A cleared entry may then still be read as present
after its invalidation, and a fresh table may be read as stale present
entries. QEMU reports `C`=0 but walks coherently, so no gate shows it. The
SMMUv3 side (`COHACC`, `CR1`) is to be confirmed in the fix's review. Its
fix is not N0g's:

* it lands on its own, before N0g, by another agent, with the register
  invalidations as they are;
* it adds a flush helper: `clflush` plus `mfence` of every written entry,
  and of every whole fresh table frame, before its invalidation or link,
  on `C`=0;
* it adds a dirty-flag self-check, which runs on QEMU since `C`=0 there.

N0g builds on it:

* every frame N0g allocates for the unit to read is flushed whole through
  F-58's helper before it is linked or published (G8): the IRT, the queue,
  the wait-status frame;
* so is every IRTE and every descriptor N0g writes, before the IEC or the
  queue tail that publishes it;
* N0g moves F-58's publish point from the register invalidation to the
  queue's tail.

**What a unit must offer.** `Unit::open` gains these checks, in this
order:

* **The DMAR's `INTR_REMAP` flag** (`ferrix_acpi::dmar::FLAG_INTR_REMAP`,
  bit 0) and **`ECAP.IR`** (bit 3). Without both, the unit translates DMA
  as today but remaps no interrupts, and the machine's interrupts are not
  isolated (below).
* **`ECAP.QI`** (bit 1). This is now required for the unit to be used at
  all (ruling 4). Register-based invalidation is removed, not kept as a
  fallback, so "a register invalidation while `QIE` is set" cannot exist.
  IR needs QI anyway, since the IEC exists only as a queue descriptor.
  Every IR-capable unit has QI, and QEMU's always has it
  (`s->ecap = VTD_ECAP_QI | …`). A unit without it is refused, with "it
  has no invalidation queue" on its line. Its functions get untranslated
  domains, so `DMA_TRANSLATED` is clear for them, and the machine's
  `INTERRUPTS_ISOLATED` is clear (G4).
* **`ECAP.C`** (bit 0) is read and acted on by F-58's fix.

**Firmware's state.** `Unit::open` already refuses a unit that firmware
left translating. For the rest:

* **IR left on** (`GSTS.IRES`): `GCMD` is written without `IRE`, and
  `IRES` must read 0 within `PATIENCE`, else the unit is refused ("firmware
  left it remapping interrupts and it would not stop").
* **QI left on** (`GSTS.QIES`): `IQH` must equal `IQT` (the queue is
  idle) within `PATIENCE`, then `QIE` is cleared and `QIES` must read 0,
  else refused.
* **A table pointer left behind** (`GSTS.IRTPS`): `IRTA` is overwritten by
  the kernel's own before `SIRTP`, so nothing of firmware's table is used.
* **`CFI` left set**: removed from `STANDING`, so the first kernel `GCMD`
  write clears it, and "Bring-up order" reads `CFIS` back.
* **x2APIC left on** (`IA32_APIC_BASE.EXTD`, bit 10), **on every
  processor** (G6). Ferrix drives the local APIC through its MMIO window
  (`apic.rs`), which does not exist in x2APIC mode. INIT does not take a
  processor out of x2APIC mode, so an AP can be in it whatever the boot
  processor found.
  * `apic::init` on the boot processor, and `smp.rs`: `secondary_start`
    on every AP, read the MSR before touching the MMIO APIC.
  * If `EXTD` is set and `IA32_XAPIC_DISABLE_STATUS` (0xBD, if CPUID
    enumerates it) does not say legacy xAPIC is disabled, they take the
    SDM's path: x2APIC → disabled (`EN`=0, `EXTD`=0) → xAPIC (`EN`=1). The
    boot line says "firmware left x2APIC on, on N processors; switched to
    xAPIC".
  * If xAPIC is disabled (a locked platform), the boot stops with a named
    reason, "processor N: the platform locks the local APIC in x2APIC
    mode, which this kernel does not drive yet". That is the customer's
    decision 4: fail-safe, and better than today, where such a machine
    would drive an APIC window that does not exist.
  * The DMAR's `X2APIC_OPT_OUT` flag is read and printed. It changes
    nothing while the kernel runs xAPIC.

**Bring-up order**, per unit, replacing `Unit::enable`:

1. `RTADDR`, then `SRTP`, then wait for `RTPS` (as today).
2. **The queue**: one zeroed 4 KiB frame, flushed whole on `C`=0, of 256
   descriptors of 128 bits. `IQA` gets its address with `QS`=0 and `DW`=0
   (128-bit descriptors, legacy mode). `IQT` = 0, then `QIE`, then wait
   for `QIES`. No register invalidation has been issued yet, so none is in
   flight.
3. A queued global context-cache invalidation, a global IOTLB
   invalidation and a wait (below). This replaces today's two register
   commands.
4. **The table** (below): `IRTA` = its address, `EIME`=0, `S`=7 (256
   entries). Then `SIRTP`, wait for `IRTPS`, then a queued global IEC with
   a wait.
5. **The console's line is converted** (below, "The I/O APIC"), if one is
   routed.
6. `GCMD` with `IRE` and *without* `CFI`. Wait for `IRES`, then read
   `GSTS` back and require `CFIS`=0. If `CFIS` reads 1, IR is turned off
   again on that unit, and the machine's interrupts are not isolated, with
   "compatibility format still accepted" as the reason.
7. `TE`, as today.

The boot line per unit becomes
`iommu    vt-d unit 0xfed90000: translating, queue on, remapping on (256 entries, xAPIC format), CFIS=0 read back`.
It states the read-back, a register fact, and not "blocked" (G1). On
QEMU that read-back is meaningful only with the patched QEMU below, where
`CFIS` follows `CFI`. That compatibility format **is** blocked is check
R1's line, which test-boot requires on every x86-64 boot together with
the bring-up line.

**The invalidation queue.** `Unit::invalidate_context` and
`Unit::invalidate_iotlb` keep their signatures. Their bodies become a
submission of one descriptor plus a wait descriptor, under the unit's
existing `commands` gate. The gate is held with interrupts on (`gate.rs`),
so the wait can block as today's register wait does. Because these two
functions are the only invalidation entry points, every caller moves with
them, and no call site changes:

* `Unit::attach`, `Unit::detach` (domain teardown);
* `Unit::flush` (every `Domain::pin` after a map in caching mode, every
  `Domain::unpin`, `map_all`'s unwind);
* `object::pin::release` (the quarantine).

A third, `invalidate_interrupt_entry(index)`, adds the IEC.

The wait descriptor (type 5) has `SW`=1 and `FN`=1. It writes a status
word into a kernel frame. The word's value is a per-unit sequence number,
so a late completion of an earlier, timed-out wait is never taken for the
current one. The wait succeeds only when that word is written within
`PATIENCE`, 100 ms as today. **A failed invalidation** is any of:

* the word not written within `PATIENCE`;
* `FSTS.IQE` (bit 4), `ICE` (bit 5) or `ITE` (bit 6) set at any poll.

It is returned as `Err`, and every caller already treats an `Err` from
an invalidation as "the unit may still reach it":

* `Domain::unpin` hands the pin back, and `Pin::drop` then keeps its
  frames for good, in N0f's `kept` count;
* `object::pin::release` leaks the entry back (`back.leak()`), also into
  `kept`;
* `Unit::detach` keeps the root table.

N0g adds that **an IRTE index whose IEC failed is never reused**. Vectors
are never freed today in any case (below). After `IQE` the queue has
stopped at a bad descriptor and the unit takes no further invalidation.
The unit is then marked **failed**. Every later invalidation on it fails
at once, without touching the queue, so nothing is released from any of
its domains again until reboot. The fault is recorded as a unit fault in
the audit (`audit_faults`), and FX-1007 fails a boot on it. There is no
recovery: a wedged queue is a broken unit, and holding memory is the safe
side.

**The table and its entries.**

* **Size and index.** One table per unit, one zeroed 4 KiB frame, flushed
  whole on `C`=0 before `SIRTP` (G8): 256 entries of 128 bits. An IRTE's
  index is the vector's slot in `TAKEN` (`vector − 0x40`). Every vector
  the arch layer hands out, MSI, MSI-X or an I/O APIC input, already comes
  from those 64 slots (`msi::allocate_vector`). So no second allocator and
  no second lock is needed: the index is taken by the existing
  compare-and-swap, and two vectors never share an index. Indexes 64–255
  are never present, and a message naming one faults with 0x21 or 0x22.
* **EIME=0, xAPIC format.** Ferrix runs xAPIC and refuses APIC IDs above
  255 (`apic.rs`: `send`; `smp.rs`). The destination is built by one
  function, `irte_destination(apic_id) -> Result<u32>`, which puts the ID
  in bits 15:8 of the IRTE's 32-bit destination field and refuses an ID
  above 255. **The later x2APIC work must set `IRTA.EIME`=1 and widen this
  function to the full 32 bits.** Its doc comment says so.
* **The fields**, fixed by the IRTE builder and checked by a host test:
  * `P`=1, `FPD`=0 (faults are recorded), `DM`=0 (physical), `RH`=0;
  * `TM` edge for MSI/MSI-X, or the input's trigger for an I/O APIC
    line;
  * `DLM`=000 (fixed). Never NMI, SMI, INIT, ExtINT or lowest priority.
  * `IM`=0: remapped, never posted. The posted-interrupt capability is
    never used, and the builder's doc says so.
  * `V` is the minted vector; `DST` is from `irte_destination`;
  * high half: `SID` is the requester ID, `SQ`=00 (all 16 bits
    compared), `SVT`=01 (verify by SID).
* **Writing an entry, only while it is not present:**
  1. Read the low qword. If `P` is set, refuse ("an interrupt entry in
     use is never rewritten"). Since the index is fresh from `TAKEN`, this
     is a defence, not a path.
  2. `write_volatile` the high qword (`SID`, `SQ`, `SVT`), then a
     compiler fence.
  3. `write_volatile` the low qword, with `P` last in the same store.
     x86's write-back stores are seen in program order, so the unit never
     reads a present entry with a stale `SID`.
  4. On `C`=0, F-58's helper flushes the entry's line.
  5. IEC, index-selective (`G`=1, `IIDX`=index, `IM`=0), then a wait.
     This is required under QEMU's caching mode too, after
     not-present → present.
  6. Only now is the device's message programmed, still masked (below),
     and only after that may a holder unmask it.
* **Never edited while present, never freed.** Vectors are minted once
  and kept "for the life of the machine" (`device.rs`), and N0g does not
  change that. So the design has no retarget and no free. If either is
  added later, it must use clear-then-rewrite: mask the device; clear `P`;
  IEC and wait; rewrite as above. Or it must use a 128-bit `cmpxchg16b`.
  It must never be two plain stores to a present entry. The builder
  module's doc states the rule.

**Messages.** `arch::msi_allocate(device: u32)` keeps the signature the
three architectures share (G7). The Arm implementations
(`arch/aarch64/mod.rs`, `arch/armv7a/mod.rs`, `gic.rs`, `gicv2.rs`) are
untouched, and "no Arm file changes" holds. x86-64's implementation
resolves the unit inside itself: it asks `iommu` for the unit whose
placement holds that requester ID on segment 0, the only segment x86-64
Ferrix enumerates. Then:

* **Remapping on for that unit**: `iommu::remap(requester, slot, vector,
  apic_id)` writes the IRTE as above. The message is
  `address = 0xFEE0_0000 | (index & 0x7FFF) << 5 | 1 << 4 | (index >> 15)
  << 2` (format bit 4 set, `SHV` bit 3 clear) and `data = 0`. The 32-bit
  MSI capability holds it; it is below 4 GiB, so N0b's `Msi::reaches`
  passes.
* **A function `iommu` places as aliased** (its requester ID is not its
  own; `topology::Behind::Aliased`), or found on no unit while the machine
  remaps, gets **no vector**. `mint` returns "the function's messages
  arrive under another's source ID". Its DMA is already blocked: it has
  no context entry. Matching an aliased requester by `SVT`=10 (bus range)
  is deferred until a driver needs a device behind a PCIe-to-PCI bridge.
* **No unit remaps** (no IR anywhere): today's compatibility message,
  unchanged, and `INTERRUPTS_ISOLATED` is clear (below).
* MSI-X entries are written the same way, with each entry's address
  carrying its own handle.

**The I/O APIC (G3).** `IoApicInput` gains the I/O APIC's MADT ID
(`ferrix_acpi::IoApic::id`) and its source ID. The source ID comes from
matching that ID against the `enumeration_id` of a DMAR scope of type
`SCOPE_IOAPIC` (`ferrix_acpi::dmar::DeviceScope`), never by order. Under
QEMU this is source ID 0xFF00: the scope's start bus is
`Q35_PSEUDO_BUS_PLATFORM` (0xFF) and its one hop is devfn 0
(`Q35_PSEUDO_DEVFN_IOAPIC`), in QEMU 9.2.4 and 10.2.1 alike.

**The console's line is converted at IR bring-up, not routed afresh.** It
has been live in compatibility format since stage 3, and `console::input`
has no way back to polling once its interrupt has started. So at step 5
of the bring-up order:

1. Under `CONSOLE_INPUT`'s lock, mask the RTE (bit 16), and read the
   vector `V0` and destination it is routed to now. Drop the lock.
2. Outside that lock (the gate spins there), take a **fresh vector `V1`**
   and write `V1`'s IRTE with `V1`, that destination, `TM` from the input,
   and `SID` = the I/O APIC's. Then an IEC and a wait. `V0` stays taken
   for good, so its IRTE index is never present. Move the console's
   handler from `V0` to `V1` through the generic interrupt layer
   (`irq::move_handler`) now, while the line is masked, leaving `V0` a
   handler that counts what still arrives on it: from here, until the
   rewrite, a delivery on `V0` can only be a compatibility route that
   survived `IRE`, and it is counted.
3. Set `IRE` and read back `CFIS` (step 6).
4. Under the lock again, write the RTE in remappable format: bit 48 set,
   handle bits 14:0 in 63:49, handle bit 15 in bit 11, bits 10:8 zero. The **vector field is
   the IRTE's vector, `V1`**, because the I/O APIC matches EOIs to RTEs by
   vector for level-triggered lines (VT-d §5.1.5.1). The trigger bit
   equals the IRTE's `TM`. Then unmask, and drop the lock.
5. **Service the port once**, as its interrupt handler would. An edge
   raised while the line was masked is lost, and a byte left in the UART
   would otherwise wait for the next one.

**Why a fresh vector (amended 2026-10-03, the consultant's ledger 271).**
Under KVM's split irqchip a refused route update leaves the old route
delivering, and `IRE` recomputes no route (below, "A refused route keeps
the old one"). With the IRTE written for the line's own `V0`, a stale
compatibility route and the IRTE would deliver the same vector, and no
check could tell which did. With `V1`, a delivery on `V1` can come only
through the IRTE, and a stale or compatibility route only on `V0`: check
R5 loops a byte back through the port and requires it on `V1` and nothing
on `V0`, and any delivery on `V0` after the conversion fails test-boot.
The consultant accepted it on these terms: `V0` stays taken; a delivery on
it fails the boot; the handler is on `V1` before the unmask, moved through
the generic layer; the RTE's vector is the IRTE's `V1`; and the port is
still serviced once.

**No matching scope.** If a routed input's I/O APIC has no DMAR scope,
the conversion cannot be done, and the console cannot go back to polling.
So IR is **not enabled** on that unit. The console keeps its
compatibility RTE, the machine's interrupts are not isolated, and the
line says why: `iommu    vt-d unit …: remapping not enabled: no DMAR scope names I/O APIC 0, which carries the console`.
Only when no input is routed at all (no MADT, no I/O APIC covering IRQ 4)
is the I/O APIC unused, and IR goes on without it (the consultant's
earlier suggestion).

**No compatibility-format MSI exists when IR goes on.** The mints are at
stage 10, after `bring_up`. Bring-up asserts it: every slot taken in
`TAKEN` at step 6 must be an I/O APIC input that bring-up converted, or
the boot stops by name ("a vector was minted in compatibility format
before interrupt remapping"). That is check R9.

QEMU's I/O APIC sends without a source ID (`X86_IOMMU_SID_INVALID`,
`vtd_mem_ir_write`), so QEMU checks no SID for it. The SID match is shown
by a host test over QEMU's DMAR, and argued for hardware.

**The HPET.** Ferrix reads the HPET only as a counter
(`arch/x86_64/clock.rs`), and never programs a comparator or an FSB
message. So it needs no IRTE. An HPET FSB message, if firmware left one
armed, would be compatibility format and blocked. The clock module's doc
says so.

**Faults.** `Unit::take_fault` reads the fault-reason byte (`FR`, bits 7:0
of the dword at `+12`) once `F` is set:

* **FR 0x20–0x26** is an interrupt fault: a new
  `Cause::Interrupt { reason, index }`, with the index read from
  `FI[63:48]` of the low qword and the page not used.
* Its display is "stream 0x200, interrupt index 3, reason 0x26 (source
  ID check)". The audit record writes it as `interrupt-fault`, not
  `dma-fault`.
* `provoked` gains `record_provoked_interrupt(stream, reason)`. A check
  registers the one it provokes, and `audit_faults` counts any other as
  stray. FX-1007 then fails the boot, as for DMA.
* xtask's `dma_faults` parser splits the two kinds. The IR checks' line
  must show exactly the faults they provoked: 0x25 for R1 and R2, and
  0x26 for R3. Any other reason, 0x20–0x24, fails test-boot by name.

**Unisolated interrupts (C3, G4, G5).** Compatibility format is a
machine property: one device behind a unit that does not remap, or behind
no unit, can send a compatibility message that no unit blocks. So:

* **`device_isolation(device)`** → bits, a new call with no buffer. Any
  device handle will do.
  * Bit 0, `DMA_TRANSLATED`: the device's domain is translated.
  * Bit 1, `INTERRUPTS_ISOLATED`. It is set only when **every** unit the
    DMAR lists, refused units included (no QI, `RWBF`, left translating,
    would not stop remapping), has `IRE`=1 and `CFIS`=0, no PCI function
    is counted as bypassing every unit, and this device's vectors are
    remapped (G4). A refused unit blocks nothing, so one clears the bit
    for the whole machine.
  * x86-64 sets bit 1 only after N0g. On AArch64 it is set for a device
    behind a GICv3 ITS, and never under GICv2m (below). ARMv7-A never sets
    it.
* **`DEVICE_LIMIT_ISOLATED_INTERRUPTS`**, a second limit for N0f's
  `device_set_limit` (`SET_LIMIT`, devmgr only).
  * With it set, the kernel refuses `interrupt_create` and `vmo_pin` for
    that node with `ACCESS_DENIED` while bit 1 is clear. The refusal is
    the kernel's, not devmgr's.
  * It is **set-once** (G5): a `device_set_limit` that would clear it is
    `ACCESS_DENIED`, and nothing else clears it.
  * devmgr sets it for every kind whose device runs firmware of its own;
    the `Gpu` kind is the first. **If setting it fails, devmgr does not
    start the GPU's driver**, and prints
    `devmgr   gpu 01:00.0 not started: the kernel refused its isolated-interrupts mark`.
    A devmgr host test and its control show that (R10).
  * `nvrm` also checks bit 1 at start, as a second guard.
* **No accepting boot option.** A device marked this way on a machine
  whose interrupts are not isolated is simply refused. The customer is
  still deciding whether a logged option to accept the residual should
  exist, and the consultant advised "not yet" (ruling 8). If it is wanted,
  it can come later as its own reviewed change. ITEM.md §5 keeps one boot
  option.
* Disk, net, sound, input and display drivers are not marked, and work
  as today without IR. The consultant accepted that for the machine as a
  whole, with ARMv7-A's V-03 as the precedent.
* **F-57 stays open where interrupts are not isolated.** The Security
  Target's T.DMA claim and O.DMA's interrupt half become conditional on
  bit 1: "where interrupt remapping is active".

**The patched QEMU (G1, G2; the customer's yes to option (a)).**

* **The patch** is
  `tools/common/data/qemu/0002-intel_iommu-honour-CFI-and-block-compatibility-format.patch`.
  It sits beside the virtio-gpu-gl fix that the Windows build already
  carries from that directory (`tools/common/fetch/fetch-qemu-windows.sh`).
  Against v10.2.1 it does two things:
  * `vtd_handle_gcmd_write` honours `GCMD.CFI` and sets or clears
    `GSTS.CFIS` to match, as `VTD_GSTS_CFIS` names it;
  * `vtd_interrupt_remap_msi`, on a compatibility-format message with IR
    on and `CFIS` clear (or `IRTA.EIME` set, as VT-d 5.1.2.1 also blocks
    in x2APIC mode), reports `VTD_FR_IR_REQ_COMPAT` (0x25) against the
    source ID (when `do_fault`) and returns `-EINVAL`, so the message is
    dropped. QEMU's stderr then carries, once per run,
    `vtd_interrupt_remap_msi: compatibility format interrupt blocked (sid=…, address=…, data=…)`,
    followed by the existing `Interrupt Mask set, irq is not generated`
    when the fault event is masked. xtask passes both through today; R1's
    parser work (the `dma_faults` split above) should count them as
    provoked remarks.
  * A qtest, `/q35/intel-iommu/cfi`, which the build script runs every
    time: `CFIS` follows `CFI`, and with IR on a compatibility I/O APIC
    entry faults 0x25 while `CFIS`=0 and is delivered once `CFI` is set.
    Each half of the patch removed fails it.
  
  The same function serves `vtd_mem_ir_write` (TCG, and a device's own
  writes) and the KVM route fix-up (`int_remap`). So under the split
  irqchip a compatibility route is refused too: no new route, and no
  fault recorded at route time, since QEMU calls it with `do_fault=false`
  there. The patch is offered upstream. Once a release has it, the patch
  is dropped and the pin moves to that release.
  
  **A refused route keeps the old one (for the kernel half).** Under the
  split irqchip QEMU recomputes a KVM MSI route only when the guest
  writes the I/O APIC entry or the MSI message, or invalidates the
  interrupt entry cache (`SIRTP`, IEC). Setting `IRE` recomputes nothing.
  When the IOMMU refuses a recomputed route
  (`kvm_irqchip_update_msi_route` fails in `kvm_arch_fixup_msi_route`),
  KVM's previous routing entry stays in place and keeps delivering. So an
  I/O APIC input routed in compatibility format before `IRE` is still
  delivered after it, by KVM, with no fault, until it is rewritten into a
  route that succeeds. The console conversion's order in "Bring-up
  order" (mask; IRTE; IEC and wait; remappable RTE; unmask) is therefore
  required, not an optimisation: QEMU skips masked inputs when it
  recomputes, and a masked input is not delivered. An unmasked
  compatibility entry left across `IRE` is a leak on KVM that TCG does not
  show. R1 and R2 are unaffected: edu's MSI goes through `msi_notify` and
  `vtd_mem_ir_write`, which fault 0x25 and drop on KVM and TCG alike.
* **The build** is a new `tools/common/fetch/fetch-qemu-linux.sh`, shaped
  like the Windows script:
  * it fetches the v10.2.1 release tarball by its pinned sha256, and
    applies the patches named in `tools/common/data/qemu/series-10.2.1`;
  * it configures `--target-list=x86_64-softmmu
    --with-pkgversion=ferrix-cfi`, with the display features the gates use
    from Ubuntu's build: GTK, OpenGL, virglrenderer, VNC, and spice
    protocol for `qemu-vdagent`, plus slirp, PulseAudio, PipeWire, ALSA
    and the TCG plugins;
  * it runs the qtest, then installs into `~/.local/share/ferrix/qemu`
    (`bin/`, `share/`; QEMU finds its firmware relative to the binary).
* **How xtask selects it.** `paths.rs`: `own_qemu` looks in
  `~/.local/share/ferrix/qemu/bin`, or the directory itself for the
  Windows build's layout, before `PATH`, on any host. With only
  `qemu-system-x86_64` built there, x86-64 boots take it, and AArch64 and
  ARMv7-A keep the QEMU on `PATH`. `FERRIX_QEMU` still overrides, as
  today.
* **How xtask refuses an unpatched QEMU.** Before an x86-64 boot,
  `qemu.rs` reads the chosen binary's `--version`. If it lacks
  `(ferrix-cfi`, the boot is refused with
  `x86-64 boots need a QEMU that blocks compatibility-format interrupts (F-57); run tools/common/fetch/fetch-qemu-linux.sh`.
  On Linux hosts only for now: the Windows build (11.1.0) does not carry
  the patch yet, which is a BACKLOG row. The kernel's check R1 decides independently: on an unpatched QEMU the
  forged message arrives, and R1 fails by name.
* **CI** runs the same script in the two jobs that boot x86-64 (`boot`'s
  x86_64 entry and `rustc`). Its build is cached by the hash of the
  script and of `tools/common/data/qemu/`.
* **The `ferrix-3060` domain's `<emulator>`.** libvirt's `qemu:///system`
  runs QEMU as its own user under its AppArmor profile, which does not
  let it run a binary from a home directory. So:
  * the same build is installed once by the user, as root, to
    `/usr/local/lib/ferrix/qemu/`. The script prints the `sudo install`
    line and never runs it. It never touches `/usr/local/bin`, which
    holds the root-installed 9.2.4.
  * The domain's XML (`~/ferrix-nvidia-vm/ferrix-3060-rootport.xml`, N0e's
    template) names `/usr/local/lib/ferrix/qemu/bin/qemu-system-x86_64`.
  * `run-nvidia` refuses to start the domain unless its `<emulator>` is
    that path and that binary's `--version` says `ferrix-cfi`.
  
  This matters there as much as in the gates. `nvrm` could produce a
  compatibility message through vfio's emulated MSI capability, by way of
  the `NV_PCFG` mirror quirk, and the host QEMU's route fix-up would pass
  it. With the patched emulator, F-57's closure covers the 3060 VM.
* Customer and consultant agree that without the patch,
  `INTERRUPTS_ISOLATED` would never be set under QEMU. With it, R1 shows
  the block on every x86-64 gate boot.

**F-57's text.** FINDINGS.md is changed with N0g's landing, not here.
The new text:

* adds that a compatibility-format message chooses its **delivery mode**
  as well as its vector: NMI, SMI and INIT, not only fixed. A device could
  send an INIT and stop a processor, or an SMI into firmware. IR closes
  that, because an IRTE's `DLM` is always fixed and compatibility format
  is blocked;
* replaces "a GICv2m frame raises only the SPIs it owns" with the
  residual below, kept out of F-57's closure;
* names the QEMU patch as part of the reference configuration's evidence
  on QEMU.

F-57 closes when N0g lands with checks R1 to R10 and their controls,
where `INTERRUPTS_ISOLATED` holds on x86-64. That covers bare metal with
VT-d IR, the gates' patched QEMU, and the `ferrix-3060` domain on it.

**Arm (C5).** No Arm code changes. The documents gain T.DMA **path 6's
GICv2m residual**, recorded apart from F-57:

* Every device whose domain maps the v2m doorbell can raise **any** of
  the frame's MSI SPIs by the data it writes, its siblings' included.
* It cannot raise SGIs, PPIs or the kernel's IPIs.
* xtask's AArch64 reference machine is GICv2 with v2m (`virt`, the
  default `gic-version`), so this residual is the reference
  configuration's.
* GICv3 with an ITS (`FERRIX_ARM_MACHINE=gic-version=3`) translates by
  the DeviceID from the requester ID, and is resisted.
* `device_isolation`'s bit 1 is never set under v2m.

**Memory and timing (ix).**

* The table, the queue and the wait-status words are kernel frames that
  no domain maps: three 4 KiB frames per unit, allocated at stage 10's
  bring-up.
* The IRTE index needs no new lock (the `TAKEN` CAS).
* The queue is under the unit's `commands` gate. Its hold is one
  descriptor pair plus the wait, bounded by `PATIENCE` (100 ms) per
  invalidation, as the register wait is today.
* The console's conversion takes `CONSOLE_INPUT`'s lock twice, for one
  RTE write each, and the gate between, never both at once (G3's
  advisory).
* MEMORY-AND-TIMING gains:
  * the queue's wait;
  * the gate's order with `Domain`'s `changing` gate (`changing` then
    `commands`, as today);
  * the failed-unit state.

**xtask's machine.**

* x86-64 boots get `-device intel-iommu,intremap=on,eim=off`, on the
  patched QEMU.
  * `eim=off` keeps QEMU from offering what the kernel does not use.
    QEMU's `auto` turns EIM on under an in-kernel irqchip.
  * Under KVM they also get `-machine q35,kernel-irqchip=split`. QEMU
    refuses `intremap=on` with a whole in-kernel irqchip (`x86-iommu.c`).
    TCG needs no change.
  * `seam.rs`'s machine gets the same, so measurements stay comparable.
* The `ferrix-3060` domain (N0e) gets
  `<iommu model='intel'><driver intremap='on' caching_mode='on' eim='off'/></iommu>`,
  `<ioapic driver='qemu'/>`, and the `<emulator>` above. There, the GPU's
  own DMA writes are also checked by the host's VT-d, through VFIO. In the
  guest, Ferrix's IR isolates the vectors QEMU's vIOMMU translates.
* The split irqchip moves the I/O APIC and PIC into QEMU. Ferrix uses
  neither on the hot path (MSI-X and the local APIC timer), so the
  boot-time cost should be small. It is measured on the gate and reported
  in the landing, given the test-time priority.

**The boot checks (C4)**, x86-64 under KVM with the split irqchip and
under TCG, both in test-boot, on the patched QEMU. Each has a negative
control, and the landing message gives each control's FIRED count:

| Check | What it shows | Its control |
|---|---|---|
| R1 | edu's MSI capability programmed by the check in **compatibility format**, aimed at **0xFC**, then raised: fault 0x25 from edu's SID, and no delivery. Per ruling 3, 0xFC is outside `TAKEN` (0x40–0x7F) and the IRT-indexed range; its handler only counts and sends EOI; and a 0xFC delivered outside the check's window is counted and fails test-boot | IR left off (`IRE` not set): the message arrives. A second forged message after the window: test-boot fails on the stray count |
| R2 | the same in compatibility format with **delivery mode NMI**: fault 0x25, and `paranoid`'s `NMIS` count unchanged | IR left off: the NMI arrives and `NMIS` moves. INIT is never forged: a delivered INIT would stop a processor |
| R3 | edu's MSI programmed in remappable format with **virtio-rng's IRTE handle** (rng at 01:00.0, edu at 02:00.0), then raised: fault 0x26 from SID 0x200, and no delivery on rng's vector | `SVT`=0 in the builder: rng's vector receives edu's message |
| R4 | edu's **own** vector through its own handle (N0b's `check_msi`, now remappable): delivered unmasked, not delivered masked | `SID` written as rng's: 0x26, and `check_msi` fails "nothing arrived" |
| R5 | the console's **I/O APIC line**, converted at bring-up: after it, COM1 put in loopback for one byte gives one delivery on the console's vector through the remappable RTE; and a byte left in the UART during the masked window is read by the one service | the IRTE left not present: fault 0x22 and no delivery; and the service after unmasking dropped: the byte is not read |
| R6 | **every invalidation is queued**: the boot's invalidations counted by kind (context, IOTLB, IEC), none by register. With QI on, QEMU leaves a register invalidation pending (`vtd_handle_ccmd_write`) | one `flush` left on the old register path: QEMU never clears `IVT`, and the boot fails at the first unpin that flushes. That is the PCI check's out-of-domain probe, so the line the control expects is the PCI check's pin refusal, "virtio-rng: the check's own domain refused its pin", which fires before R6's own line is reached |
| R7 | **a failed invalidation releases nothing**. The check submits its unpin's wait descriptor with `SW`=0, through a check-only parameter of the submission (as `quiet` is for pins), never a runtime switch (G9). No status is ever written, `Domain::unpin` fails after `PATIENCE`, the pin is kept, and its frames are held and counted in `kept` | the `Err` mapped to `Ok`: the frames are released, and the check fails "frames released after a failed invalidation" |
| R8 | **x2APIC per processor** (G6): every processor reports `EXTD` clear before its MMIO APIC is used. The control path is exercised by the check build turning x2APIC on, on one AP, before its `secondary_start` check (QEMU offers x2APIC under KVM, and under TCG since 8.1). That AP must report "switched to xAPIC" and come up | the `secondary_start` check removed: the AP touches the MMIO window in x2APIC mode and never comes up, and the boot fails on the processor count |
| R9 | **no compatibility-format MSI before IR**: bring-up's assertion over `TAKEN` holds, and its line counts the converted inputs (1, the console) | edu's vector minted before `bring_up` in a check build: the boot stops by name |
| R10 | **the isolated-interrupts mark** (G5): set on edu, `interrupt_create` is allowed while isolated, and refused with `ACCESS_DENIED` when the check forces the machine's bit 1 clear through a check-only path; a `device_set_limit` that would clear the mark is `ACCESS_DENIED`. devmgr's host test: setting the mark failing stops the GPU launch with its line | the refusal skipped: `interrupt_create` succeeds; devmgr ignoring the failure: the launch goes on |

**Amended 2026-10-03 (the C6 review, ledger line 278).**

* *R1's second half and R2's control* cannot be one-file controls: with
  `CFI` kept, R1 fires before R2 runs, and the stray 0xFC needs R1's own
  check out of the way. Both are run by hand as two-file controls, with
  ruling 262's record.
* *R3* holds the other function's vector for the unit's patience, 100 ms,
  before it requires that nothing arrived, so a late delivery still fails.
* *R5 on real hardware:* a PC's 16550 loops OUT2 back internally in
  loopback mode, so its line to the I/O APIC is not driven. Only a delivery
  on `V0` fails R5; with nothing on either vector it prints "R5: the UART
  raised no interrupt in loopback; not observable here" and the boot goes
  on, and test-boot still requires the positive line on the reference
  machine. G3's service-once reads the port, so it is shown either way.
* *A function no remapping unit places* gets no message vector while any
  unit remaps (`L.device.28`): fail-closed, and an availability loss on a
  machine whose units do not cover every function. The boot names each
  such function, and SAFETY-MANUAL AoU-23 records it.
* *AArch64 behind a GICv3 ITS* gets no isolation bit yet, so a device that
  runs firmware of its own cannot be driven on AArch64 until that path sets
  it.

There is no check for **the IEC skipped**. QEMU does not cache IRTEs:
`vtd_interrupt_remap_msi` reads the entry from memory on every message,
and KVM's routes are rebuilt on the IEC notifier. So the first delivery
after unmask would succeed with or without the IEC, and a check would
prove nothing. The consultant accepted the argument from the code: one
function writes an IRTE, and its last statement before returning is the
IEC and wait. A host test over a mock unit asserts the descriptor order.

**Host tests.** These are new, in `ferrix_paging::vtd` or a new
`ferrix_pci`-style host crate for the encodings:

* the IRTE builder's every field, and `irte_destination`;
* the remappable MSI address, and the RTE encoding for handles 0, 63 and
  0x8000;
* `take_fault`'s decoding of FR and FI;
* the descriptor encodings (context, IOTLB, IEC, wait) against the
  spec's layouts;
* the DMAR IOAPIC-scope match by enumeration ID, over QEMU's DMAR
  (`ferrix_acpi`'s test table) and over one with two I/O APICs listed out
  of order;
* the `INTERRUPTS_ISOLATED` rule over a list of units, one refused (G4).

**Requirements.** See §12.4: `L.iommu.47`–`55`, `L.x86_64.129`–`132`,
`L.device.27`–`28` and `H.DMA.9`. `H.DMA.9`: "a device raises only the
interrupt vectors minted for it". On x86-64 it holds where
`INTERRUPTS_ISOLATED` holds, and on AArch64 under a GICv3 ITS. GICv2m,
ARMv7-A and machines without VT-d IR are its stated residuals.

**Documents in N0g's landing.**

* **SAFETY-MANUAL**:
  * **AoU-21**: the integrator's devmgr marks every device that runs
    firmware of its own (its match table), and accepts that on a machine
    whose interrupts are not isolated such a device is not started.
  * ASR-4 names `H.DMA.9`.
  * The reference configuration on QEMU is the patched QEMU.
* **VULNERABILITY-ANALYSIS**:
  * T.DMA path 6 is resisted on x86-64 where interrupts are isolated, is
    residual elsewhere for unmarked devices, and keeps its GICv2m
    residual;
  * a sentence that DMA and interrupt isolation both rest on the requester
    ID, and that root ports' ACS source validation is not enabled. This
    is pre-existing and shared with H.DMA.1, recorded as an advisory.
* **FINDINGS**: F-57's text as above, and its closure.
* **SECURITY-TARGET**: T.DMA's and O.DMA's claims conditional on IR.
* **SOUP.md** and ITEM.md §5: the patched QEMU as test infrastructure
  of the reference configuration.
* **MEMORY-AND-TIMING**: as above.
* **TRACEABILITY**: the new ids.

**Rings.** `iommu/vtd.rs`, `iommu.rs`, `arch/x86_64/msi.rs`, `apic.rs`
and `smp.rs` are `core`. `device_isolation` and the mark are `item`'s
native calls. No Arm file changes (G7).

**Points.** 10, recorded in §7.

* 8 for interrupt remapping, the consultant's estimate.
* 1 for the QEMU patch, its Linux build script, xtask's version gate and
  the domain's emulator.
* 1 for what this review added: the console's live conversion, the
  per-processor x2APIC check, and the set-once mark with devmgr's
  refusal.

The coherency flushes left N0g for F-58's own row.

### 12.4 The requirement ids

Reserved on this branch, in one commit to land on `main` before any of the
three is written. Each slice's landing releases its own:

| Slice | Branch | Ids | Of which new to the file |
|---|---|---|---|
| N0a–c's conditions | `nvidia-n0` | `L.user.109`–`110`, `L.x86_64.128` | — (kept from the N0 reservation) |
| N0d | `nvidia-n0d` | `L.device.24`–`26`, `L.object.117` | — (moved from the N0 reservation) |
| N0f | `nvidia-n0f` | `L.object.118`–`120` | — (moved) |
| N0g | `nvidia-n0g` | `L.iommu.47`–`55`, `L.x86_64.129`–`132`, `L.device.27`–`28`, `H.DMA.9` | `L.iommu.48`–`55`, `L.x86_64.131`–`132`, `L.device.27`–`28`, `H.DMA.9` |

What each states:

* **N0d.**
  * `L.device.24`: `device_aperture` reports each aperture's address,
    length, BAR, offset and flags as minted.
  * `L.device.25`: a configuration write touching any byte outside the
    function's writable ranges is refused whole and changes nothing; reads
    of the function's 4 KiB are answered; a node whose kernel-owned
    registers read back other than minted, at a HELLO or a driver's death,
    is refused with bus mastering off.
  * `L.device.26`: the writable ranges are the allowlist of §12.1,
    computed from both capability lists (verified by
    `ferrix_pci::window`'s host tests).
  * `L.object.117`: every configuration write to a published node's
    function, by the kernel or a driver, is made under the node's one
    lock.
* **N0f.**
  * `L.object.118`: a device's pin budget is set only through
    `SET_LIMIT`, never under live pins, and never past the ceiling.
  * `L.object.119`: a pin past the budget is refused with nothing pinned.
  * `L.object.120`: a pin is refused when quarantined, kept, live and new
    pages would pass twice the budget; `live` is reserved under the
    quarantine lock before the pin and given back on failure; the counts
    carry across a driver's death, and the release takes off only what it
    gave back.
  * `L.object.47`–`49` are reworded.
* **N0g.**
  * `L.iommu.47`: only queued invalidation, and a unit without QI is
    refused.
  * `L.iommu.48`: a failed invalidation (no status within `PATIENCE`, or
    `IQE`, `ICE` or `ITE`) releases nothing; `ICE` is cleared and counted
    as it is taken; after `IQE` or `ITE` the unit takes none.
  * `L.iommu.49`: **released** in slice 1's landing. Its meaning -- on
    `C`=0 every frame and entry N0g gives the unit flushed before it is
    published -- is F-58's `L.iommu.56` and `L.iommu.57`, whose
    `write_entry`, `table` and `publish` N0g's queue and table go through.
  * `L.iommu.50`: IR only with `INTR_REMAP` and `ECAP.IR`, `EIME`=0,
    `IRES`=1 and `CFIS`=0 read back.
  * `L.iommu.51`: firmware's IR or QI turned off and read back, or the
    unit refused.
  * `L.iommu.52`: an IRTE written only while not present, high half
    first, then IEC and wait before its message is programmed.
  * `L.iommu.53`: the IRTE's fixed fields.
  * `L.iommu.54`: IR faults decoded apart from DMA faults, and only the
    provoked ones accepted.
  * `L.iommu.55`: `INTERRUPTS_ISOLATED` set exactly when its conditions
    hold.
  * `L.x86_64.129`: remappable-format messages with remapping on.
  * `L.x86_64.130`: a routed I/O APIC line is converted to a remappable
    RTE at IR bring-up (masked, IRTE and IEC outside the console's lock,
    RTE vector = IRTE vector, unmasked, the port serviced once), its SID
    matched by MADT ID; with no matching scope IR is not enabled on that
    unit; no compatibility-format MSI exists when IR goes on.
  * `L.x86_64.131`: firmware's x2APIC switched to xAPIC on every
    processor before its MMIO APIC is used, or the boot stopped by name.
  * `L.x86_64.132`: one destination encoder, refusing IDs above 255.
  * `L.device.27`: a node marked for isolated interrupts is refused
    vectors and pins while the machine's interrupts are not isolated, and
    the mark, once set, cannot be cleared.
  * `L.device.28`: no vector for an aliased function while remapping is
    on.
  * `H.DMA.9`: as in §12.3.

AoU numbers are not in the reservation file. `AoU-20` stays with N0c's
condition B1. `AoU-21` is N0g's, and `AoU-22` is N0d's. The coherency
finding is **F-58**, recorded in FINDINGS.md's "reserved and not yet
filed" paragraph by this branch, and filed with its fix.

---

## Appendix A: the probe's domain

`~/ferrix-nvidia-vm/ferrix-3060.xml`, defined with
`virsh -c qemu:///system define`. The image is the branch's
`build/x86_64/ferrix.img`, copied next to it. Serial output is read with
`python3 capture.py serial.log` before `virsh start`.

```xml
<domain type='kvm' xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'>
  <name>ferrix-3060</name>
  <memory unit='GiB'>4</memory>
  <vcpu>2</vcpu>
  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
    <loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
    <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd' templateFormat='raw' format='raw'>/var/lib/libvirt/qemu/nvram/ferrix-3060_VARS.fd</nvram>
  </os>
  <features><acpi/><apic/></features>
  <cpu mode='host-passthrough' check='none'><maxphysaddr mode='passthrough'/></cpu>
  <on_poweroff>destroy</on_poweroff><on_reboot>destroy</on_reboot><on_crash>destroy</on_crash>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='raw'/>
      <source file='/home/sebastian/ferrix-nvidia-vm/ferrix.img'/>
      <target dev='sda' bus='sata'/><boot order='1'/>
    </disk>
    <serial type='tcp'>
      <source mode='bind' host='127.0.0.1' service='47060'/><protocol type='raw'/><target port='0'/>
    </serial>
    <rng model='virtio-non-transitional'>
      <backend model='random'>/dev/urandom</backend><driver iommu='on'/>
      <address type='pci' domain='0x0000' bus='0x00' slot='0x07' function='0x0'/>
    </rng>
    <iommu model='intel'><driver intremap='off' caching_mode='on' aw_bits='48'/></iommu>
    <video><model type='none'/></video>
    <memballoon model='none'/>
    <hostdev mode='subsystem' type='pci' managed='no'>
      <source><address domain='0x0000' bus='0x01' slot='0x00' function='0x0'/></source>
      <address type='pci' domain='0x0000' bus='0x00' slot='0x05' function='0x0'/>
    </hostdev>
  </devices>
  <qemu:commandline>
    <qemu:arg value='-device'/><qemu:arg value='isa-debug-exit,iobase=0xf4,iosize=0x04'/>
  </qemu:commandline>
</domain>
```

`ferrix-3060-rootport.xml` next to it is the same domain with the
hostdev's guest address left to libvirt, which puts it behind a root port.
That is the run that showed N0a.

## Appendix B: the ioctls `nvrm` answers

* **`/dev/nvidiactl` and `/dev/nvidia0`**:
  * `_IOWR('F', 200 + n)`: `CARD_INFO` 0, `REGISTER_FD` 1, `ALLOC_OS_EVENT` 6,
    `FREE_OS_EVENT` 7, `STATUS_CODE` 9, `CHECK_VERSION_STR` 10,
    `IOCTL_XFER_CMD` 11, `ATTACH_GPUS_TO_FD` 12, `QUERY_DEVICE_INTR` 13,
    `SYS_PARAMS` 14, `EXPORT_TO_DMABUF_FD` 17, `WAIT_OPEN_COMPLETE` 18;
  * the RM API escapes 0x27–0x5F (`src/nvidia/arch/nvalloc/unix/include/nv_escape.h`),
    `IOCTL_XFER_CMD` carrying any of them past the 14-bit size field.
* **`/dev/nvidia-modeset`**: `_IOWR('m', 0, struct NvKmsIoctlParams)`.
* **`/dev/nvidia-uvm`** (N5): raw numbers from `UVM_INITIALIZE`
  (`0x30000001`) and `UVM_IOCTL_BASE(i)`; which of them CUDA needs is in
  §11.1.
* **`renderD129`** (N3b):
  * DRM core: `GEM_CLOSE`, `PRIME_HANDLE_TO_FD` and `PRIME_FD_TO_HANDLE`,
    `SYNCOBJ_*`, `GET_CAP`, `VERSION`;
  * nvidia-drm's private ioctls 0x00–0x18
    (`kernel-open/nvidia-drm/nvidia-drm-ioctl.h`).
