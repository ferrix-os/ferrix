# Suspending one application whole, and resuming it later

**Feasibility design, 2026-10-30, branch `claude/app-suspend-research-50489d`.
Nothing in it is built.** The customer's complaint: on every other desktop
OS a game cannot be stopped at some point and picked up later from exactly
that point. Every handle it holds (GPU contexts, files, sockets, windows,
audio) is dropped. The question is whether Ferrix can freeze one
application with all its state and resume it later: within one boot, from
disk, across a reboot, and on another machine.

**The short answer.** Yes, with limits that depend on the stage and on
whether the application talks to a server off the machine.

* **In RAM, within one boot (S1):** possible now, with about two weeks of
  ring-3 work and no kernel line. `cgroup.freeze` already parks a Linux
  process with every blocking call made restartable. What is missing is
  that the processes the application talks to (hyprix, yserver, pulsed,
  the GPU driver) must treat a frozen client as asleep, not broken.
* **Its memory paged to disk, same boot (S2):** possible. It is swap for
  a frozen cgroup, a core-ring change to `mm`. Nothing is serialized:
  every object stays in the kernel and only the pages leave.
* **Across a reboot (S3):** possible for a native or Wayland application
  launched to be suspendable. For Endfield through Steam and Proton it is
  possible only as far as the game survives losing its server connection,
  which only the game decides. It is a large project in the certified
  item: the kernel gains calls that dump and rebuild a process.
* **On another machine (S4):** S3 plus a common CPU baseline, the same GPU
  generation and moving the files. Worth it only after S3 is in daily use.

§1 is the prior art, §2 the inventory of one game's state, §3 the staged
design, §4 the decisions for the customer, §5 where it stands.

Points are in the backlog's units. Where a figure is a guess it says so.

---

## 1. Prior art: what each solved and what it gave up

### 1.1 CRIU (Linux, 2012 to now)

**What it does.** Checkpoint/Restore In Userspace dumps a process tree from
outside. It freezes the tree (`cgroup.freeze` or `ptrace`), injects a
*parasite* into each process to read state only the process can reach,
writes memory, registers, file descriptors, namespaces, timers and signal
state to image files, and on restore forks the tree again with the same pids
(`clone3` `set_tid`, inside a pid namespace) and rebuilds each object. It
handles files by path (checking size and mtime), unlinked files as "ghost"
copies, pipes, Unix sockets inside the tree, eventfd, signalfd, epoll and
inotify (watches resolved back to names). Established TCP connections are
saved and restored without the peer noticing, with `--tcp-established`,
through the kernel's *TCP repair* mode (Linux 3.5).
[criu.8](https://manpages.ubuntu.com/manpages/xenial/man8/criu.8.html),
[Wikipedia: CRIU](https://en.wikipedia.com/wiki/CRIU).

**Where it fails, and why.**

* **Anything whose other half is outside the tree.** A Unix socket to
  another process needs `--ext-unix-sk`, and a stream socket with only one
  end in the tree cannot be dumped at all
  ([crac-dev, 2025-09](https://mail.openjdk.org/pipermail/crac-dev/2025-September.txt)).
  A Wayland or X11 client is exactly that: its connection is to a
  compositor that CRIU does not checkpoint and that would not recognise the
  restored connection anyway. A desktop application cannot be restored
  with its window. DMTCP's answer was to run X clients under a private VNC
  server, which is checkpointed together with the application (§1.3).
* **Device files.** A DRM render node's state is in the kernel driver
  (GEM objects, VMs, contexts, fences). CRIU knows none of it except through
  a vendor plugin (§1.2), and no plugin exists for graphics.
* **Files changed between dump and restore.** CRIU refuses or restores
  stale state. It owns no filesystem, so it cannot pin a file's contents
  to the image.
* **Time.** Since Linux 5.6 CRIU restores into a time namespace whose
  offsets make `CLOCK_MONOTONIC` continue where it stopped. `RDTSC` read by
  the program itself still jumps.
* **The CPU.** A program dumped on a CPU with AVX-512 and restored on one
  without it dies on the first such instruction. CRIU checks the CPUID
  features and refuses.

**Its gift:** everything a single kernel owns can be dumped and rebuilt
from outside a process, with pids and file numbers kept. **What it gave
up:** the halves of a program's state held by other processes, and by
devices.

### 1.2 GPU checkpointing: AMD's CRIU plugin and NVIDIA's cuda-checkpoint

* **AMD (`amdgpu_plugin`, CRIU 3.17, 2022).** A CRIU plugin for ROCm
  compute processes. New KFD ioctls let it save and restore the process's
  GPU memory mappings, VRAM contents, user-mode queues and signals; the
  state goes into a `kfd.<id>.img` file. It began with single-GPU Gfx9 on
  the same machine and later added multi-GPU and restore on another machine
  with the same GPUs
  ([XDC 2021 talk](https://media.ccc.de/v/xdc2021-5-fast_checkpoint_restore_for_amd_gpus_with_criu),
  [man page](https://manpages.opensuse.org/Tumbleweed/criu-plugin-amdgpu/criu-amdgpu-plugin.1.en.html),
  [CRIU PR 1519](https://github.com/checkpoint-restore/criu/pull/1519)).
  Compute only: a graphics context on the render node is not covered.
* **NVIDIA (`cuda-checkpoint`, driver 550, 2024).** The opposite shape.
  Rather than dumping GPU state into CRIU, it *suspends CUDA inside the
  process*: in-flight work is waited for, device memory is copied into host
  memory the driver manages, and every GPU resource is released, so the
  process no longer refers to the GPU and plain CRIU can dump it. Restore
  re-acquires a GPU, copies memory back *at the original device addresses*
  and recreates contexts and streams. UVM memory and exported IPC memory
  are not supported; 570 added process trees through CRIU 4.0; 580 added
  migration to another GPU
  ([NVIDIA/cuda-checkpoint](https://github.com/NVIDIA/cuda-checkpoint),
  [NVIDIA blog](https://developer.nvidia.com/blog/checkpointing-cuda-applications-with-criu)).
  CUDA only: no OpenGL or Vulkan.

**What both teach.** A GPU process can be checkpointed only by the driver
that holds its state, and only after its in-flight work drains. Restoring
it means recreating objects at the *same GPU virtual addresses*, because the
program has written them into its own memory and command buffers. Both gave
up graphics, whose state spans the driver, the compositor and the display.

### 1.3 DMTCP (Ansel, Arya and Cooperman, 2009 onwards)

DMTCP is user-level: it is `LD_PRELOAD`ed into every process and wraps
system calls, so it needs no kernel support and checkpoints distributed jobs
through a coordinator. Its *plugins* let an application disconnect from an
external resource at checkpoint and reconnect at restart (InfiniBand, GDB
sessions, X through a private VNC server)
([plugins](https://dmtcp.sourceforge.io/plugins.html),
[dmtcp(1)](https://dmtcp.sourceforge.io/dmtcp.1.manpage.html),
[Cooperman 2014](https://mug.mvapich.cse.ohio-state.edu/static/media/mug/presentations/2014/cooperman.pdf)).

**Solved:** checkpointing without kernel help, and a general pattern for
external resources: every boundary gets a plugin that knows how to sever
and remake it. **Gave up:** statically linked programs and anything that
makes system calls DMTCP does not wrap. Wine makes system calls directly
from Windows code, and anti-cheat does it on purpose.

### 1.4 Process migration: Sprite and MOSIX

* **Sprite (Douglis and Ousterhout, 1987 to 1991)** moved a running process
  to another workstation. Every location-dependent system call was sent back
  to its *home* machine; file I/O went straight to the network file
  servers, so files needed no moving. The cost was *residual dependencies*:
  the source machine must keep serving a migrated process. Eviction was
  allowed only with none left. Paging went through the file server, so
  dirty pages were flushed there and never left on the source
  ([SPE 1991](https://web.stanford.edu/~ouster/cgi-bin/papers/migration-spe.pdf),
  [UCB tech report](https://www2.eecs.berkeley.edu/Pubs/TechRpts/1987/5363.html)).
* **MOSIX (Barak, from 1983)** split a migrated process into the user part,
  which moves, and a *deputy* kept on its home node, which runs every
  site-dependent system call. If the home node dies, so does the process,
  and I/O routed through the home node was slow
  ([Barak slides](https://cs.adelaide.edu.au/~yval/pdfs/mosix.pdf),
  [openMosix HOWTO](https://www.iitk.ac.in/LDP/HOWTO/openMosix-HOWTO/x210.html),
  [arXiv 1907.00194](https://arxiv.org/pdf/1907.00194)).

**What both teach:** moving a process is easy and moving its *boundary* is
not. Both kept the boundary where it was and forwarded to it. For a game
that is no answer: its boundaries are the screen, the GPU and the sound
card, which are local.

### 1.5 Persistent capability systems: KeyKOS, EROS, Coyotos

KeyKOS (1980s) and its successors EROS (Shapiro, SOSP 1999) and Coyotos
checkpoint *everything*: the whole system state, processes and their
capabilities included, is written to disk periodically as one consistent
snapshot, and the machine restarts from the last one after a crash or power
loss. EROS's single-level store treats disk as the backing of all memory,
so persistence is transparent to programs
([EROS, SOSP 1999](https://courses.cs.washington.edu/courses/cse551/19wi/readings/eros-sosp99.pdf);
Landau, *The checkpoint mechanism in KeyKOS*, POS-2, Paris, 1992;
[Shapiro and Adams, USENIX 2002](https://usenix.org/legacy/events/usenix02/shapiro.html)).

**Solved:** the dropped-handle problem, completely, because *a capability is
data* and the kernel owns all of it. No process ever has to be told it was
restored. **Gave up:** devices and the outside world. A device's state is
not in memory, so drivers must re-initialise hardware after a restart, and
anything off the machine (a network peer) sees the gap. Shapiro later called
the kernel persistence mechanism complicated and rethought it
([l4-hurd, 2005](https://lists.gnu.org/archive/html/l4-hurd/2005-10/msg00865.html)).
It is the closest model to Ferrix's native ABI: Ferrix's handles are
capabilities, and §2.4 makes use of that.

### 1.6 Consoles and handhelds

* **Nintendo Switch.** HOME suspends the one running title in RAM, and
  sleep keeps it there. It survives no power-off. Online play is dropped
  when the console sleeps, and a title must handle that itself
  ([Nintendo support](https://www.nintendo.com/en-gb/Support/Troubleshooting/Console-Loses-Connection-to-the-Internet-When-Put-in-Sleep-Mode-2939628.html),
  [Nintendo PH](https://www.nintendo.com/ph/support/switch/setting/play.html)).
* **PlayStation 5.** Rest mode keeps a suspended game in RAM. A restart,
  an update or a power loss ends it. Some titles refuse to suspend
  ([GamesRadar](https://www.gamesradar.com/ps5-standby-mode),
  [PSU](https://www.psu.com/news/the-ps5-appears-to-let-you-suspend-multiple-games-at-once/)).
* **Steam Deck / SteamOS.** The whole machine goes into ACPI suspend to
  RAM. Nothing is per-application: every process stops, and an online game
  loses its connection like any Linux laptop.
* **Xbox Series X|S Quick Resume (2020).** The one that does what the
  customer asks. Several titles are kept suspended at once, on the SSD, and
  they survive a power cut and a system update. It works because a title
  runs in a **virtual machine of its own**: the console runs a Hyper-V
  hypervisor, the game in the "exclusive" partition and the dashboard in
  another. Quick Resume hibernates the game's VM: its memory, up to about
  13.5 GB per title, is written to the SSD by the host
  ([Thurrott](https://www.thurrott.com/games/xbox/xbox-one/230883/xbox-series-x-will-let-gamers-resume-games-even-after-a-reboot),
  [GameSpot](https://www.gamespot.com/articles/xbox-series-x-see-quick-resume-and-faster-loading-/1100-6474794/),
  [TechRadar](https://www.techradar.com/news/xbox-series-x-games-will-pick-up-where-you-left-off-even-after-a-system-reboot),
  [Digital Trends via Wikipedia's summary](https://vectree.io/c/xbox-one-and-xbox-series-xs-system-software),
  the last a secondary source). The VM boundary is the trick: inside it, the
  game's OS, graphics runtime and connections are one thing that can be
  saved, and its boundary to the host is a small *virtual* device interface
  the host controls. Online titles are told they resumed and must
  reconnect, and many put the player back on the title screen
  ([TechRadar](https://www.techradar.com/news/why-xbox-series-x-quick-resume-doesnt-always-work)).

**What the consoles teach.** In RAM is cheap and every console does it.
Surviving a reboot was done by one vendor, by making the title's boundary
small and owned (a VM), and by making titles responsible for the network.
Nobody restores a server-side session; nobody can.

### 1.7 What Ferrix takes from each

| From | Ferrix takes |
|---|---|
| CRIU | freeze, then dump from outside; restore with the same pids (`clone3` `set_tid`, which Ferrix has, `syscall/family.rs`) in a pid namespace, and a time namespace for monotonic time |
| cuda-checkpoint | drain the GPU first; restore objects at the same GPU virtual addresses |
| DMTCP | every boundary gets a *sever and remake* handler |
| Sprite, MOSIX | keep residual dependencies out: none may outlive S3 |
| EROS | handles are data; the kernel can re-issue them at the same numbers |
| Xbox | make the suspend unit's boundary small, owned and versioned; let the title handle the network |

What Ferrix has that none of them had: it owns the kernel, the GPU driver,
the compositor, the X server, the sound server and the filesystem. Every
boundary in a Linux desktop that CRIU stops at is code in this tree.

---

## 2. One game's state, inventoried

The hard case is **Endfield through Steam and Proton** on the N150: Steam's
client and browser, pressure-vessel (absent today, `docs/STEAM.md`), the
game's `Endfield.exe` under Wine with `wineserver`, its own browser process,
DXVK over Mesa's anv on the ring-3 Intel driver, X11 through yserver, which
is a Wayland client of hyprix, sound through pulsed, input through hyprix,
an online server, and the ACE anti-cheat. The easy case is **a Wayland
application on the Linux ABI** with no GPU and no network, such as foot with
its shell.

"Freeze" is S1, "disk" is S2 and S3 (S2 keeps the kernel objects, §3.2),
"rebuild" is what must be recreated or renegotiated on resume. ★ marks what
Ferrix can do because it owns the stack and Linux with CRIU cannot.

### 2.1 Memory

| State | Where, who owns it | Freeze | To disk, across a reboot | Rebuild |
|---|---|---|---|---|
| Anonymous memory, heap, stacks | VMOs in the process's `AddressSpace` (`user/space.rs`, `user/vmo.rs`, core ring) | stays | the pages; VMAs as (address, length, protection, VMO, offset) | mapped again at the same addresses |
| File mappings (DLLs, `.so`, game data) | page-cache VMOs of btrfs files | stays | path, inode and generation; private copy-on-write pages saved | mapped from the same file, which must be unchanged (§2.3) |
| Shared memory with *other* processes | memfd or `MAP_SHARED` VMOs: wineserver's shared data, pulsed's memfd, Steam's IPC | stays | saved once, mapped into each restored sharer | a sharer outside the unit is a boundary (§2.7) |
| GPU objects (GEM) | VMOs **the Intel driver owns**, mapped into the client (`MapReply::Vmo`, INTEL-GPU §4.2), pinned in its IOMMU domain | stays, pinned | ★ the driver saves them (§2.5); the iGPU has no VRAM, so they are system RAM | recreated by the driver and mapped at the same address |
| Pins and quota | `object::pin`, the job's memory charge | stays | not saved | charged again |

Ferrix has **no swap**, so today a frozen game keeps every page resident
(`memory.reclaim` exists but takes only page cache, `user/cache.rs:449`).

### 2.2 Threads, registers, signals, futexes

| State | Where | Freeze | Disk | Rebuild |
|---|---|---|---|---|
| User registers, FPU and vector state (XSAVE) | each `Task` (`sched/task.rs`, core) | saved at the park | saved, in a fixed layout (XSAVE sizes differ across CPUs) | loaded into a new task |
| TLS base (`FS`/`TPIDR`), `set_tid_address`, robust list, sigaltstack, seccomp | `Thread`, `syscall/thread.rs`, `seccomp.rs` | stays | saved | set again |
| A thread inside a blocking call | the call's wait queue | ★ **interrupted and made to restart at thaw** (`docs/CGROUPS.md` §11) | nothing to save: the thread is *between* calls, its registers say "make the call again" | the call is made again |
| Futex waits | `syscall/futex.rs`, keyed by space and address, or by object and offset | interrupted, re-waited at thaw | **no futex table to save**: every waiter is a thread about to call `futex` again | none |
| Pending signals, masks, dispositions | `syscall/signal.rs`, per thread and process | stays | saved | set again |
| pid, tids, process group, session | `syscall/process.rs`; glibc caches the tid in its thread structure and Wine stores Unix pids and tids in wineserver | stays | saved | ★ same numbers through `clone3`'s `set_tid` in a pid namespace of the unit's own (`docs/PIDNS.md`) |
| Credentials, namespaces, rlimits, umask, cwd | `syscall/process.rs` | stays | saved | set again, never raised (§3.3) |

The freeze's restart rule is the largest single simplification here. CRIU
must cope with threads stopped inside the kernel; a frozen Ferrix process
has none, so a checkpoint is registers, memory and object tables, and no
kernel wait state at all. A thread interrupted inside the vDSO is the one
exception (its program counter is in the vDSO's page): §3.3 keeps the vDSO
where it was.

### 2.3 File descriptors

| State | Where | Freeze | Disk | Rebuild |
|---|---|---|---|---|
| btrfs files: game data, the Wine prefix, saves | `fs/btrfs.rs`, page cache | stays | path, inode, offset, flags | ★ opened again; Ferrix owns btrfs, so the checkpoint can **reflink every open file into the image** and restore the file as it was, which CRIU cannot (it checks mtime and size and refuses). Reflink is not built (no `FICLONE` in the tree); stage C's snapshots are the alternative |
| Unlinked open files, tmpfs files, memfds, `/dev/shm` | `fs/pages.rs`, tmpfs | stays | contents saved | made again with the contents |
| Pipes, FIFOs, ptys inside the unit | `fs/pipe.rs`, `fs/pty.rs` | stays, buffers kept | buffers and both ends | made again, buffers refilled |
| Unix sockets inside the unit (game to wineserver) | `fs/socket.rs`, `SCM_RIGHTS` included | stays | both ends, queued data and queued descriptors | made again as a pair |
| Unix sockets to outside (hyprix, yserver, pulsed, Steam, D-Bus) | `fs/socket.rs` | stays; the peer sees a silent client (§2.6) | **a boundary**: the peer must save its half, or the connection is severed | §2.6 and §2.7 |
| eventfd, timerfd, signalfd, epoll, inotify, pidfd | `fs/eventfd.rs`, `timerfd.rs`, `signalfd.rs`, `epoll.rs`, `inotify.rs`, `pidfd.rs` | stays; a timerfd keeps counting | counters, timers (remaining time on the monotonic clock), interest lists, watches as paths | made again; epoll's ready list rebuilt by polling once |
| TCP and UDP sockets | the kernel's net core (`net/`, logic in `src/lib/network/net`) | stays; ★ the kernel's stack keeps the connection alive, answering zero-window probes, for as long as the *peer's application* waits | ★ the stack is Ferrix's, so the connection's state (sequence numbers, windows, buffers) can be saved without a repair mode | after a short gap restored as it was; after a long one or a reboot the peer has given up, so the socket is restored **reset** and the program sees `ECONNRESET` at its next call |
| The render node and card nodes | the chardev core (`interfaces/chardev`), served by the driver | a held request is interrupted (`ERESTARTSYS`) and made again at thaw | §2.5 | §2.5 |

### 2.4 Native kernel objects and handles

A Linux game holds almost none; a native application holds them all.

| State | Where | Freeze | Disk | Rebuild |
|---|---|---|---|---|
| The handle table | `object::HandleTable` in the core process (`object/process.rs`) | stays | (number, type, rights, object id) per entry | ★ **re-issued at the same numbers**: the kernel owns the table, so it can fill a fresh one exactly. EROS's model (§1.5) |
| VMOs, ports, channels inside the unit | `object/` | stays | contents, queued messages and the handles they carry | made again, both ends |
| Channels to outside (devmgr, lode, a service) | `object/channel.rs` | stays; messages queue | a boundary | the peer reconnects, or the restored end reads `PEER_CLOSED` |
| Jobs, quotas | `object/job.rs` | stays | limits | a new job under the user's slice |
| `Interrupt`, `IoMapping`, device pins | drivers only | **not suspendable**: a driver is never inside a suspend unit | n/a | n/a |

Native processes are **not parked by `cgroup.freeze` today** (`docs/CGROUPS.md`
§11, GAME-MODE §3.2). A native application needs that first, in the core.

### 2.5 The GPU

Two drivers, two answers.

**The ring-3 Intel driver** (the N150). What it holds per client: GEM
objects (VMOs, system RAM), each client's PPGTT (one VM per client), the
bindings of objects into it, contexts with their LRC images, syncobjs and
sync files, dmabufs exported to hyprix, and requests in flight on the
engines.

* **Freeze, S1.** Nothing to do while the driver stays up: the frozen
  process makes no calls, the work it submitted finishes, its objects stay.
  ★ If the driver *restarts* while the game is frozen, the keeper of
  INTEL-GPU §12 already keeps every client's objects, VMs, contexts and
  fences across a worker's death and replays the journal. Step 1 (the
  protocol and the journal) is on `develop`; step 2 (the product worker)
  is not, so today a game frozen across a driver crash thaws to
  `VK_ERROR_DEVICE_LOST` (§11).
* **Drain.** Before S2 or S3, wait until none of the client's requests is
  in flight (what cuda-checkpoint does). With the client frozen, nothing
  new is submitted; the wait is one frame's work.
* **S2.** The objects are idle after the drain. ★ The driver can *unpin*
  them (the PPGTT entries cleared and the IOMMU pins dropped) and let the
  kernel page them out with the rest of the job, then pin and bind again
  at thaw, at the same GPU addresses but on new pages. On a GPU without
  VRAM that is the game's whole GPU memory made pageable. On Linux, i915
  would need a new eviction path and a cgroup hook; here it is a request
  between two of our programs.
* **S3.** ★ The keeper's journal is already a description of a client's
  GPU state as a list of operations (`remote::wire`: `VM_CREATE`, `BIND`,
  `CONTEXT_CREATE`, ...). Serializing it, with the objects' contents and each
  context's saved register state, is a *restore from journal*, the same
  replay as a worker restart, run on a fresh driver. **GPU virtual
  addresses can always be kept**, because each client has its own PPGTT and
  no other client's objects can be in the way: anv's soft-pinned addresses
  in its own memory and batches stay valid. GEM handle numbers and syncobj
  handles are re-issued as they were. The context image's GGTT pointers
  (ring, status page) are rewritten by the driver, as i915's
  `lrc_update_regs` does at a reset. **Restore requires the same GPU
  generation**: the keeper already refuses a worker whose `Facts` differ,
  and that check becomes the restore's.
* **What cannot be saved.** A batch interrupted mid-way: the drain avoids
  it. A context whose last batch hung: restored lost.

**NVIDIA through `nvrm`** (the RTX 3060 path). The resource manager is
NVIDIA's closed code. VRAM holds the objects, and only RM knows its
layout. S1 works as for Intel: the process is frozen and its VRAM stays
allocated. S2 and S3 need RM to save one client: cuda-checkpoint does that
for CUDA only. RM's own system suspend (preserving video memory allocations
across S3) is for the whole GPU, not one client. **Per-application save on
NVIDIA is not possible without NVIDIA**; this design does not try.

### 2.6 The compositor, the X server and the protocols

**hyprix.** Holds per client: the Wayland object map (ids to surfaces,
`xdg_toplevel`s, buffers, callbacks, seats), each surface's committed state
and buffer, configure serials, the window's place in the layout, and focus.

* **Freeze.** A frozen client reads nothing, so hyprix's queue to it grows.
  `compositor/socket` drops a client once more than `MAX_QUEUED`, 1 MiB,
  waits (`socket/src/connection.rs:33`). An unfocused frozen client gets
  almost nothing (its one pending frame callback), so it would survive
  for a long time. A focused one gets pointer motion: at about 40 bytes an
  event and a 1000 Hz mouse, about **25 s of moving the pointer over it
  drops it** (an estimate, not measured). So hyprix needs a **parked
  client**: told by whoever froze the job, it sends nothing to it (no frame
  callbacks, no input, no configure; focus moves away), keeps showing its
  last buffer, dimmed with a "suspended" mark, and resumes when told.
  Small, and ring 3.
* **S3.** ★ hyprix's half of the connection is hyprix's data. It can write
  a client's object map and surface state to the image and, on restore,
  accept the restored socket as that client with the same object ids, its
  last buffer re-imported from the restored GEM objects or shm. What it
  must renegotiate it does with events clients already handle: the outputs
  (sizes, scale, the monitor itself may differ: `wl_output` hotplug),
  a fresh `configure` for each toplevel, the keymap, and focus. Global names
  in `wl_registry` must keep their numbers, or be removed and announced
  again, which clients handle.

**yserver.** An X client's state is in the server: windows, pixmaps,
GCs, properties, selections, and resource ids from the client's own range.
**Atoms are global** to the server, interned by every client, so restoring
one client into another server's atom table needs the same atom numbers.

The answer is to **put an X server inside the suspend unit**: a suspendable
game gets a rootless yserver of its own, started with the game, and that
yserver is saved together with the game, as DMTCP saves X clients together
with a private VNC server (§1.3). Its own boundary is then a Wayland
connection to hyprix (above) and the GPU (§2.5). The cost is a second X
server's memory per suspendable game (tens of MB, a guess) and that Steam's
own windows live in the other X server, which they do already. yserver's
behaviour towards a frozen client (output buffer limits) is not checked
yet; with the server inside the unit it no longer matters.

### 2.7 Audio, input, network, Wine, Steam, anti-cheat

| State | Where | Freeze | Across a reboot |
|---|---|---|---|
| A sound stream | pulsed, ring 3 (`media/pulsed`); the game is a PulseAudio client over a Unix socket and a memfd | pulsed sees the stream underrun. ★ It is told the client is parked and corks the stream rather than playing silence | pulsed's half is a few dozen bytes (format, volume, buffer attributes): it saves it and accepts the restored socket. Otherwise the stream is severed and libpulse reports the server gone, which not every program handles |
| Input devices | evdev nodes held by hyprix; the game gets input through Wayland or X | nothing: the game never holds a device | nothing to save; a controller unplugged meanwhile arrives as an unplug |
| A raw input device the game opened itself (a controller through `hidraw`) | the driver | stays | severed: the device may be gone; the game sees a disconnect |
| Network connections to a game server | the kernel's stack (§2.3); the *server's* half is not ours | for seconds: nothing. After the server's heartbeat timeout (tens of seconds is typical, a guess): **the server drops the session** whatever Ferrix does | the session is gone; the game sees a reset and must reconnect |
| wineserver | a process of the prefix, beside the game; holds every NT object (handles, events, mutexes, windows' Wine side) | ★ inside the unit, frozen with the game | inside the unit, saved with it: wineserver is just another process |
| fsync / ntsync waits | futexes (Proton's fsync), `docs/WINE.md` | futex waits are restarted (§2.2) | nothing to save |
| The Steam client | outside the unit; the game reaches it through `steamclient.so` over shared memory and sockets | Steam waits for a silent client | **a boundary**: Steam restarted after a reboot does not know the restored connection. Either Steam and the game are suspended together (§3.3) or the Steam API calls fail until the game restarts its Steam connection, which few games do |
| ACE anti-cheat | inside the game's processes under Wine (no kernel driver on Ferrix) | a stall, which a laptop's sleep also causes on Windows | unknown: it may check the time gap, the tick count, or tokens with its server. A restore is byte-for-byte the same memory, so integrity checks pass; a server-side token has expired |

### 2.8 Time and the CPU

* **`CLOCK_REALTIME`** jumps by the suspended time on every OS, as after a
  laptop's sleep, and should: the wall clock must be true.
* **`CLOCK_MONOTONIC`** runs on through an S1 freeze; the game sees one
  long frame, as after sleep on Windows, and every engine clamps its delta.
  **Across a reboot it would go backwards** (a new boot starts near zero),
  which programs do not survive. ★ Ferrix has time namespaces with per
  namespace offsets for the monotonic and boot-time clocks
  (`syscall/timens.rs`): the restored unit gets a time namespace whose
  offset makes its monotonic clock continue from the checkpoint, as CRIU
  does.
* **`RDTSC`** read by the game, by Wine or by the anti-cheat directly is
  not covered by a namespace. Across a reboot the TSC restarts near zero.
  ★ The kernel can write `IA32_TSC_ADJUST` before the restore so the TSC
  is above the saved value: one machine-wide, forward-only jump, which the
  kernel's own clock already accounts for. On another machine the TSC's
  *frequency* may differ too, and a game that calibrated it at start then
  runs its clock at the wrong speed; only trapping `RDTSC` (`CR4.TSD`,
  microseconds a read) fixes that, so S4 asks for the same TSC frequency.
* **CPU features.** A program chose its code paths from CPUID at start.
  Restored on a CPU that lacks one, it dies on the first such instruction.
  Same machine: no issue. Another machine: ★ launch suspendable programs
  with **CPUID faulting** (Intel's per-thread CPUID trap, Linux's
  `ARCH_SET_CPUID`, not in Ferrix yet) reporting a baseline both machines
  have.

### 2.9 What follows from the inventory

Sort the state by where its other half is:

1. **Inside the process or the kernel, owned by the unit:** memory,
   threads, signals, the descriptors and handles whose both ends are in the
   unit. The kernel can freeze, page out, dump and rebuild all of it. CRIU
   shows it can be done, and Ferrix's freeze makes it simpler (no kernel
   wait state, §2.2).
2. **In a peer on this machine that Ferrix owns:** the GPU driver, hyprix,
   yserver, pulsed, and the filesystem. Each needs a *park* (S1), an
   *evict* (S2) or a *save and restore one client* (S3) operation. This is
   where Ferrix can do what Linux cannot, because each peer is code in this
   tree. A peer that cannot do it can be pulled *inside* the unit instead
   (yserver, wineserver), as the Xbox puts the game's whole OS inside its
   VM.
3. **In a peer Ferrix does not own:** the game server, NVIDIA's RM, Steam's
   protocol, the anti-cheat's server. Nobody can save these. The best any
   system does is sever them cleanly and let the program reconnect. That is
   the limit of S3 for an online game, and it is the same limit as the
   Xbox's.

---

## 3. The design, in stages

The unit of suspension is a **job** (a cgroup): the application's scope,
with every process in it. Every stage freezes it first. The user's view:
a "suspend" action on a window or a game in the launcher, a dimmed window
or an entry in a list of suspended applications, and "resume".

### 3.1 S1: freeze and thaw in RAM, within one boot

**What it is.** `cgroup.freeze` on the application's job, and each peer
told that the job's clients are parked.

**What exists.** `cgroup.freeze` for Linux processes, with blocking calls
restarted at thaw, a process forked into a frozen cgroup starting frozen,
`SIGCONT` not thawing, and `cgroup.events` saying when all have parked
(`docs/CGROUPS.md` §11, `fs/cgroupfs.rs`, `fs/cgroupfs/freeze_check.rs`).
Every program hyprix starts is already in its own `app-<name>-<pid>.scope`.

**What is missing.**

| Piece | Where | Size |
|---|---|---|
| A suspend service: freezes a scope, tells hyprix, yserver and pulsed it is parked, thaws in the reverse order; refuses to freeze anything in the keep list of GAME-MODE §3.1 (the compositor, drivers, sessiond) | ring 3, beside the game-mode service; one place that owns "which jobs are frozen" so game mode's freezes and the user's do not undo each other | 3 |
| hyprix's parked client (§2.6): no events to it, focus moved, last buffer shown dimmed, the action in the window menu | hyprix | 3 |
| pulsed: cork a parked client's streams | pulsed | 1 |
| A Steam game in a scope of its own, holding the game, wineserver and the game's other processes but not Steam. Today the game is Steam's child; check what `app-*.scope` it lands in, and move it with a launch wrapper if needed | the launcher or hyprix's scope rules | 2 |
| The intel keeper's step 2 (the product worker), so that a game frozen across a GPU driver restart thaws intact | INTEL-GPU §12.8; already planned | (37, counted there) |
| **Total, not counting the keeper** | | **9 points** (a guess) |

**Hard parts.** None in the kernel. The work is in agreeing who may freeze
what: game mode (GAME-MODE §3) freezes the *background* while the user may
suspend the *game*, and both write the same file.

**Risks.**

* **Online games.** Endfield frozen for longer than its server's heartbeat
  timeout will find its session gone at thaw. Whether it reconnects in
  place or drops to the title screen is the game's choice. Endfield also
  ships on phones, where the OS suspends apps all the time, so a reconnect
  path probably exists (assumed, not checked).
* **Memory.** With no swap, a frozen game keeps all of its RAM (several GB
  for Endfield). S2 is what gives it back.
* **Native applications** are not frozen (§2.4). S1 covers Linux-ABI
  programs only until a native freeze lands.
* **Time** is not hidden (§2.8), deliberately: the game sees one long frame.

**The certified item.** **Not affected.** Everything is ring 3 and uses
`cgroup.freeze`, a file the load ring already serves. No consultant review,
unless a native freeze is added: that is a change to `object/process.rs`
(core ring) and to the scheduler's park, and goes through the review.

**Cheap proof of concept** (QEMU, no code):

1. Boot `test-intel-uapi`'s image (Mesa on `intel-fake`). Start
   `vk-survive` (a submit and wait every 2 ms over a mapped buffer) in a
   cgroup, write `cgroup.freeze` 1 for 60 s, thaw. It must go on with its
   buffer intact.
2. Freeze it, `kill -9` the Intel driver, let devmgr restart it, thaw.
   Today: `VK_ERROR_DEVICE_LOST` at thaw (§11). With the keeper's step 2:
   `survived`. This is the one-line demonstration of what Ferrix's stack
   gives that Linux cannot: an application frozen across a crash of its
   GPU driver and thawed with its GPU state intact.
3. Under `test-compositor`, freeze a focused Wayland client and move the
   pointer over it, to measure when hyprix drops it.

On the box, **only with the customer's agreement** (memory
`n150-box-full-access`; the brief said not to use it unasked): freeze
Endfield's scope for 30 s, 2 min and 10 min and record what the game does at
each thaw. That measures the online limit directly.

### 3.1.1 S1 as built (2026-10-30, the customer's go-ahead)

Three findings changed the plan above while building it:

* **In a user's session there was no per-program cgroup to freeze.** A
  root compositor (every test boot) puts each program in
  `app.slice/app-<name>-<pid>.scope`, but a session's compositor runs as
  the user, and the init refuses a user a scope outside
  `user-<uid>.slice` (`docs/INIT.md` §10). So on the box every program the
  user starts is in `session-<n>.scope`, beside the compositor, and GAME-MODE
  §2.1's "every program hyprix starts is in `app.slice/…`" holds for root
  desktops only. A Steam game is in Steam's cgroup either way.
* **No new freeze service and no init call are needed.** The compositor
  knows the window, its client and the client's pid, so it freezes the
  program itself. It writes the `cgroup.freeze` of the program's cgroup
  when the program is alone in it. Otherwise it makes `suspend-<pid>`
  beneath that cgroup, freezes it first, and moves the program's
  processes in until none is left outside. For that a session's scope is
  delegated to its user, as systemd delegates; the kernel already
  supports delegation (`docs/CGROUPS.md`, G4).
* **pulsed needs nothing.** A stream whose client sends nothing is mixed
  as silence (`pulsed`'s "what no stream has is not made up"), and nothing
  is queued to a client that does not write.

What landed:

| Piece | Where |
|---|---|
| `suspend [window\|pid:<n>]` and `resume [window\|pid:<n>\|all]`, hyprix dispatchers (`hyprctl dispatch suspend`, or a bind). A window whose class is `steam_app_<id>` means Steam's `reaper` with `AppId=<id>` and everything beneath it, wineserver included; another X window is refused with a hint to name the pid, since yserver serves Steam too | `hyprix/src/suspend.rs`, `act.rs` |
| The frozen program's connections parked: no frame callbacks (kept for the thaw), no input, the window at half opacity | `state.rs` (`Slot::park`, `frames_done`), `deliver.rs` (`awake`) |
| The session's scope delegated to its user | `sessiond/src/scope.rs`, `docs/AUTH.md` §6.4 |
| `test-compositor --boot suspend`: as `ferrix`, a counter in the session's scope is frozen in `suspend-<pid>` for five seconds; not a tick while frozen, and the first tick after is the next number. Then the pattern's window is suspended and resumed and is still a client | `xtask/src/compositor/hyprlock.rs` |

No kernel line, no init change, no certified-item file. Left for S1: the
keeper's step 2 (INTEL-GPU §12.8), so that a game frozen across a GPU
driver restart thaws intact; a menu entry or bind in the default
configuration.

### 3.1.2 Endfield on the N150 (2026-10-30, 23:20 to 23:45)

The box ran develop 99382db29 (main-99382db29), Endfield from Steam in the
world at The Hub, 1280x720, 42 ms to its server. Steam had been started
over ssh and ran in `system.slice/sshd.service`, so before the test the
root shell moved Steam's processes into `session-1.scope`, where a
program started from the desktop is. Each round ran
`hyprctl dispatch suspend pid:5743` (Steam's `reaper` for the game) as
`ferrix`, held, then `resume pid:5743`.

| Hold | What hyprix did | The game during the hold | After the resume |
|---|---|---|---|
| 30 s | reaper and 20 more (Endfield, wineserver, the prefix's services, UnityCrashHandler) moved into `session-1.scope/suspend-5743`, `frozen 1` within a second | 0 CPU ticks (460 in 5 s before); RSS 5.2 GB unchanged; 3 of 24 TCP connections closed | in the world, animating, 42 ms, 24 connections again |
| 2 min | the same | 0 ticks | in the world, no dialog after 15 s or 60 s; the ping readout said 3 ms, then normal |
| 10 min | the same | 0 ticks; 18 connections left | the game runs and draws, but its server had dropped the session: `[Network] Connection timed out. Please login again.` (Player.log) and a dialog saying so; the game connection on port 10012 now went to another host |

What this shows:

* S1 works on a real Proton game: the whole tree freezes in a second,
  takes no CPU at all, and thaws where it stopped, GPU and swapchain
  included (one boot, no driver restart).
* Endfield's own server keeps a silent session at least 2 minutes and
  less than 10. That is the server's limit (§2.7), not Ferrix's; the
  client noticed it on its own at the thaw and said so. A shorter hold
  than 10 minutes would find the exact limit.
* The window is yserver's, so hyprix reported `(0 windows)` and did not
  dim it; yserver kept showing the last frame for ten minutes and did
  not drop the frozen client. The `steam_app_<id>` route (suspend the
  window) would pick the same reaper; this run named the pid.
* The dialog drew slowly after the 10-minute thaw: blank for two
  minutes, then complete after one key press, and a click on its Confirm
  did nothing in the minute watched. Whether that is the game or input
  through yserver after a thaw was not followed up.
* On Ferrix, a log file Unity and the SDK append to keeps its old mtime
  (`u8sdk_pc.log` said 20:32 holding lines from 22:16, box clock).

### 3.2 S2: page the frozen application out to disk, same boot

**What it is.** Not a checkpoint. The job stays frozen and every kernel
object stays where it is; only its pages leave RAM and come back on demand
or ahead of the thaw. It is **swap scoped to a frozen job**: the user's
benefit (the RAM back) with none of S3's rebuilding.

**What is missing.**

| Piece | Where | Ring | Size |
|---|---|---|---|
| Anonymous pages written to a swap area and faulted back: a swap entry in place of a page, the write, the fault-in, the area's allocator, an encryption key per boot so the pages on disk are unreadable after it | `mm`, `user/space.rs`, `user/vmo.rs`, the block ring | **core** | 20 |
| `memory.reclaim` extended to anonymous pages, allowed only on a frozen job at first (a running one keeps today's rule) | `fs/cgroupfs`, `user/cache.rs` | load | 3 |
| Prefetch at thaw: read the job's pages back in order before thawing, so the game does not fault them in one at a time | `mm` | core | 3 |
| The Intel driver's *evict client*: drain, clear the client's PPGTT entries, unpin its objects; *restore client*: pin and bind again at the same GPU addresses | the driver (ring 3) | none | 5 |
| hyprix and yserver keep their own copies of a parked client's last buffer, so it can be shown after its pages left | ring 3 | none | 2 |
| **Total** | | | **33 points** (a guess) |

**Hard parts.** Swap in a kernel that has none, in the core ring, with
fallible allocation on the write path (FX-0008), a fault path that sleeps on
disk I/O, and the interaction with pins: a pinned page is never swapped.
The disk driver is a ring-3 process (§7 of ARCHITECTURE), so a page-out
must not wait on memory the disk driver needs to allocate.

**Risks.** Disk wear and time: a 6 GB game at SATA speed (about 500 MB/s)
takes about 12 s to page out and as long to come back; NVMe about a fifth
of that (estimates). Residual information: pages of a game, its session
tokens included, on disk. A per-boot key that is never stored makes them
unreadable after a reboot. That is why S2 cannot be S3's storage: the key
is gone by design.

**The certified item.** **Affected, core ring.** Swap changes the
fault path and the VMO's page list, which the item's memory claims rest on
(`docs/certification/MEMORY-AND-TIMING.md`). The consultant reviews the
design first: the requirements (new ids reserved,
`requirement-reservations.json`), the residual-information argument for the
key, the fail-closed rules (a write error leaves the page in RAM; a read
error kills the job, never the kernel), the `unsafe` in the I/O path, and
negative controls for each.

**Cheap proof of concept.** On QEMU, freeze a small program holding 256 MB
of written anonymous memory; a throwaway kernel branch moves its pages to a
tmpfs-backed area instead of disk (no I/O path, no encryption) and back at
thaw; the program checks its memory. That shows the page-table and fault
side, which is most of the risk, without the block path.

### 3.3 S3: across a reboot

**What it is.** A real checkpoint: the frozen unit written to an image, the
machine rebooted, the unit rebuilt from the image and thawed.

**Launched to be suspendable.** Restoring needs things to be true *from the
start* of the program, so an application is launched in a **suspendable
sandbox** (the Xbox's partition, made of namespaces):

* a pid namespace of its own, so its pids and tids can be made again
  (§2.2);
* a time namespace of its own (§2.8);
* its own yserver, for an X program (§2.6), and wineserver inside it,
  which Wine's per-prefix server already is;
* later, CPUID faulting to a baseline (§2.8, needed only for S4).

**The image.** One file per unit on the user's home volume:

* the kernel's part: each process's tasks (registers, XSAVE in a fixed
  layout, TLS, signal state), VMAs and the contents of every private page,
  descriptors and handles with their numbers, the objects inside the unit
  (pipes, sockets, eventfd, timerfd, epoll, inotify, VMOs, channels), the
  monotonic and TSC values at the checkpoint;
* reflinks of every open btrfs file and the unit's tmpfs contents, so the
  files the image refers to cannot change underneath it (§2.3);
* one section per peer: the Intel driver's journal and objects (§2.5),
  hyprix's client state (§2.6), pulsed's streams (§2.7);
* a MAC over all of it, and encryption under a key bound to the user (the
  login's key, or a TPM-sealed one): the image holds every secret the game
  had.

**Who writes it.** As little as possible in the kernel (`keep-kernel-slim`):
the kernel gains two generic native calls and a ring-3 **checkpointer**
does the rest, as CRIU does on Linux.

* `job_dump`: on a frozen job, for a caller holding the job with a new
  right `CHECKPOINT`, hands out a read-only view of each process's state:
  registers, VMA list, VMO handles, descriptor and handle tables as typed
  records. No pointer into the kernel leaves it.
* `process_restore`: makes a process, frozen, in a given job, with given
  tasks, VMAs (from VMOs the caller supplies), handles **at given numbers**
  and descriptors at given numbers, and refuses anything the caller could
  not have made itself: no credentials above the caller's, no handle the
  caller does not hold, no device object.

Everything else (the format, the files, the peers, the order of the
rebuild) is the checkpointer's, in ring 3.

**Restore, in order.** Boot; the user logs in; the checkpointer checks the
image's MAC, the kernel version (the vDSO and the image format), the GPU's
`Facts` and the CPU; it makes the namespaces, writes the time offsets and
`IA32_TSC_ADJUST` if needed; asks each peer to restore its half, which
hands back the objects to map (GEM VMOs) and the server ends of the
sockets; rebuilds the processes frozen; then thaws. Any check that fails
refuses the restore and keeps the image; nothing is half-restored.

**Steam.** Two choices (§4, decision 3): the unit is the game alone and its
Steam API connection is severed at restore (the game may or may not cope),
or Steam and the game are suspended together, so Steam's IPC is inside the
unit and only Steam's network connection is severed, which the Steam client
handles every time a laptop sleeps.

**What it costs.**

| Piece | Ring | Size |
|---|---|---|
| `job_dump`, `process_restore`, the `CHECKPOINT` right, their requirements and checks | **item and core** (`syscall/native.rs`, `object/process.rs`, `user/space.rs`) | 25 |
| Serialize and rebuild the Linux personality's objects: descriptors, pipes, Unix sockets, eventfd, timerfd, signalfd, epoll, inotify, pidfd, futex robust lists, signal state | load | 30 |
| The checkpointer: image format, versioning, files and reflinks, encryption and MAC, the rebuild order | ring 3 | 20 |
| btrfs reflink (`FICLONE`), for the files | load (btrfs is in the certified item since 2026-10-02) | 8 |
| Time: offsets at restore, `IA32_TSC_ADJUST` | load and core | 4 |
| The Intel driver: save and restore one client (journal, objects, context state) | ring 3 | 12 |
| hyprix: save and restore one client | ring 3 | 10 |
| yserver inside the unit; pulsed save and restore | ring 3 | 8 |
| The suspendable sandbox at launch | ring 3 | 5 |
| Gates: a native app, a Wayland app, a Vulkan app on `intel-fake`, each checkpointed, the VM rebooted and restored | xtask | 10 |
| **Total** | | **about 130 points** (a guess; S1 and S2 not included) |

**Hard parts.** The two native calls, which by design let one process read
and write another's whole state. The number of object types: every one the
Linux personality has must be saved or refused, and refusing must be the
default for a type nobody wrote a saver for. hyprix's per-client state,
which has never been made explicit. The kernel version: an image is valid
for one kernel build until the format and the vDSO are versioned, so an
update invalidates suspended programs. The Xbox survives system updates
only because its guest OS is separate from the host's.

**Risks.**

* **Online games and anti-cheat.** The game server's session is gone after
  a reboot. Endfield resumes, finds its connection reset and reconnects or
  returns to its title screen. ACE may consider a restore suspicious; it is
  unknown and cannot be tested safely on an account the customer cares
  about. A further rule, by policy: the image of an **online** game is
  consumed when it is resumed, so a save state can never be loaded twice.
  Ferrix should not become a tool for undoing a turn in an online game.
  Offline programs may keep an image and restore it again, an emulator's
  save state.
* **Time jumps.** Hidden from the monotonic clock; the wall clock jumps.
  Certificates and tokens may have expired; that is the program's business.
* **Devices removed meanwhile.** A controller, a second monitor, a sound
  card: each arrives as an unplug, which programs handle. A different GPU:
  the restore is refused, and the user may instead start the game fresh.
* **Files changed meanwhile.** The reflinks restore the open files as they
  were; files the game had *closed* (a save it wrote and closed) are as
  they are now. If the user started the same game fresh meanwhile and
  saved, the restored instance may later overwrite that save. Rule: while
  an image of a program exists, launching it fresh asks whether to resume or
  to discard the image, as the Xbox does.
* **Security.** An image is a program's whole memory. A forged image
  restored is code injection into whatever it claims to be; a stolen one is
  every secret it held. The MAC and the encryption are not optional, and a
  restored process never runs with more authority than the user restoring
  it.

**The certified item.** **Affected, core and item rings, and the largest
review of the three.** The consultant would review:

* the two native calls: their authority (`CHECKPOINT` on the job, the
  caller's credentials as the ceiling), every refusal, and that they are
  the only way state leaves or enters a process; under Common Criteria
  they are new TSF interfaces that by design cross the process isolation
  the TOE claims, so their access-control policy needs new SFRs and the
  vulnerability analysis new rows;
* the requirements, reserved first;
* the image's integrity and confidentiality: who holds the key, what a
  wrong MAC does (refuse, fail closed);
* that no handle to a device, `Interrupt`, `IoMapping` or pin can be in an
  image;
* btrfs reflink, because btrfs is in the item;
* `IA32_TSC_ADJUST`: a write to an MSR that the kernel's own time rests
  on;
* the microkernel plan (`docs/MICROKERNEL.md`): once the Linux personality
  moves to `linuxd` in user space, the personality's half of the dump (the
  30-point row) moves with it and out of the item. Building it as a module
  that serializes the personality's own objects, behind one interface,
  keeps that move cheap.

**Cheap proof of concept.** A throwaway kernel branch with a minimal
`process_restore` (one thread, anonymous VMAs only, no descriptors) and a
host-side checkpointer: on QEMU, a static program counting in a loop is
frozen, dumped to the disk image, the VM rebooted, the program restored in
a fresh pid and time namespace, and it goes on counting from where it was,
its pid and its monotonic clock unchanged. A second PoC, native: a program
holding a VMO and a channel to a second program in the same job, both
restored, the handles at the same numbers and the channel's queued message
still there. That is EROS's idea, shown on Ferrix's handles.

### 3.4 S4: onto another machine

**What it is.** S3's image restored on a second Ferrix machine.

**What it adds.**

| Piece | Size |
|---|---|
| CPUID faulting at launch, with a baseline both machines share (an x86-64 level, say v3) | 6 (core: a per-thread CPUID trap) |
| The image and its reflinked files sent, with a MAC key both machines trust | 5 |
| Checks: same kernel build, same GPU generation, same TSC frequency (or `RDTSC` trapped) | 3 |
| Network identity: the restored sockets are reset; nothing more is promised | 0 |
| **Total** | **about 15 points on top of S3** (a guess) |

**Risks.** Every S3 risk, plus a different screen, a different sound card,
different keyboard layout, a different IP address. A different GPU
generation cannot be restored under §2.5's rule: the program would need to
recreate its device, which DXVK does for `DEVICE_REMOVED` only if the game
asks it to. A program licensed to one machine may object.

**The certified item.** As S3, plus the CPUID trap in the core ring.

**Cheap proof of concept.** S3's counting program, restored on a second
QEMU guest with a different CPU model, the baseline enforced.

### 3.5 The order, and what to do first

| Stage | What the user gets | Kernel | Size (guess) | Consultant |
|---|---|---|---|---|
| S1 | Pause any Linux app, the game included, and come back to it within the boot; survives a GPU driver restart once the keeper's step 2 lands | none | 9 + the keeper's 37 | no |
| S2 | S1, and the paused app gives its RAM back | core: swap for frozen jobs | 33 | yes, design first |
| S3 | Resume after a reboot, from exactly that point, offline programs fully, online ones up to their reconnect | item and core: dump and restore | ~130 | yes, the largest review |
| S4 | Move it to another Ferrix machine | core: CPUID trap | ~15 | yes |

**S1 first.** It is ring 3, about two weeks, needs no review, and its proof
of concept costs an afternoon on QEMU. It also forces the decision every
later stage needs: which peers park a client and how. The keeper's step 2
is planned for its own reasons and gives S1 its strongest demonstration.
S2 next, because it is what makes a suspended game cheap to keep. S3 only
after S1 and S2 have been used, and after the customer has decided how far
an online game should be promised (§4).

---

## 4. Decisions for the customer

1. **Whether to build S1 now.** About 9 points of ring-3 work, no kernel
   change, no review.
2. **Whether S2's swap is wanted**, in the core ring with a design review
   first. Ferrix has no swap today at all; this would be its first.
3. **Steam and the game for S3:** suspend the game alone, its Steam API
   connection severed at restore, or Steam and the game together.
4. **Online games:** S1 only (a pause measured in minutes), or S3 too with
   the promise limited to "the game reconnects as it does after a network
   loss"; and whether an online game's image is consumed on resume (§3.3,
   proposed).
5. **The N150 box:** whether the S1 measurement on Endfield (30 s, 2 min,
   10 min frozen) may be run on it.

---

## 5. Where it stands

2026-10-30, later: **S1 is built** (§3.1.1), on the customer's go-ahead,
and ran on Endfield on the N150 (§3.1.2).
S2 to S4 remain a design.

The research, the same day: nothing was measured on any machine; every
size is a guess, and every behaviour said to be "today" was read in the
code at develop 38b492f28:

* `cgroup.freeze` parks Linux processes and restarts their blocking calls
  (`docs/CGROUPS.md` §11, `fs/cgroupfs.rs`); native processes are not
  parked.
* hyprix drops a client with more than 1 MiB waiting
  (`compositor/socket/src/connection.rs:33`).
* The Intel driver's keeper and journal are on `develop`
  (`intel-xelp/src/remote`, a0f356fdb); the product worker is not.
* There is no swap; `memory.reclaim` takes page cache only.
* Time namespaces and `clone3` `set_tid` exist; `FICLONE`, `ptrace`
  attach, CPUID faulting and `IA32_TSC_ADJUST` do not.

The box run (§3.1.2) answered three of the open questions: yserver keeps
a frozen client and its last frame; a game lands in Steam's cgroup (on
the box, wherever Steam was started); Endfield's server drops a session
silent for 10 minutes but not one silent for 2. Not checked: ACE's
reaction to a pause or a restore, and the exact session limit.
