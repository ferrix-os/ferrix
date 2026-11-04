# Boot authority: who the kernel starts, and with what

`docs/MICROKERNEL.md` §5, §6.3 and §8's M5: under `personality=user` the kernel starts
three programs from the initramfs, the **partition manager**, **devmgr** and **`linuxd`**,
each with exactly the handles of §5's table, so that a safety partition's authority never
passes through init, `linuxd` or devmgr, and device authority never through init or `linuxd`.
Ids from mk-authority's block: L.object.320-333. Native calls 0x10C3-0x10C5. Panic codes
FX-0952-0954. Everything here is `#[cfg(ferrix_load_built = "none")]`, except what §6 says:
`present` and `desktop` do not build it.

**Where it stands:** design for review, 2026-11-04. No code.

Built on: K2 (spaces), K8 (BIND_DOMAIN, explicit nodes), K14 §2-§4 (sealing, measured spaces),
K15 (`PROCESS_MAKE`/`PROCESS_RUN`, `ferrix-process-builder`, mk-m2's), and mk-lift's first-image
loader (`first_image.rs`, `native_host.rs`, M5-LIFT.md §2, §6), whose "starts `linuxd` alone"
this replaces. The kernel keeps **one** loader: M5-LIFT's and K15's "for the first images".

## 1. The start, in order

After stage checks, memory, processors and the scheduler; in `main.rs`, replacing `devmgr::start`
and `init::run` under this cfg (`boot_authority::start`, item ring):

1. **Read the archive.** The initramfs as the loader delivered it (`A.FIRMWARE`, V-12), through
   `ferrix-cpio`'s read-only walk (already fuzzed; no VFS), stated bounds. `/sbin/partmgr`,
   `/sbin/devmgr`, `/sbin/linuxd`, `/lib/drivers/MANIFEST` and each driver it names,
   `.ferrix/partitions`, `.ferrix/certified`, `.ferrix/init/{program,script,commands}`.
2. **Seal.** Each image and the description into an anonymous VMO, sealed by `Vmo::seal` (K14).
   Over `SEAL_MAX_PAGES` fails the build (xtask) long before here; here it stops the boot.
3. **Three jobs**, children of the kernel's root, each **measured** and **sealed** (`H.OBJ.22`):
   `partitions`, `drivers`, `linux`. Their reservations (K12) come from the description for the
   first, and the rest of memory is the others' limit as today.
4. **Start the partition manager first**, and wait for its one READY message (bounded, as devmgr's
   REPORT), so a safety partition's devices are claimed before devmgr hears of anything.
5. **Start devmgr**, as `discovery/devmgr.rs` does, DEVICES carrying every node **except** the ones
   §2 hands to the manager, and report as today.
6. **Start `linuxd`** with the handles of §3. Its first program is init.
7. **The kernel's watch**: §4.

A failure at 1-3, or a first process that does not start, is a stage panic (`FX-0952`): nothing on
this machine can run. Each first process is started by the first-image loader into a fresh measured
space, recording its digest (K14 §4); its `grant_starter` is true (K14 §5).

## 2. Which device goes where

`.ferrix/partitions` names each safety partition's devices by location (PCI BDF, or tree path and
`compatible`). The kernel reads **only** that list from the description (a small bounded parser, §5),
and at step 5 leaves those nodes out of DEVICES: the filter is the kernel's, so devmgr never holds
them, whatever devmgr does. A name matching no node stops the boot (`FX-0953`): a safety partition
is not started short of its device. Handles to them go to the manager as K8's kept handles
(`KEPT_DEVICE_RIGHTS`: `BIND_DOMAIN`, `DUPLICATE`, `TRANSFER`). On a machine with no description the
list is empty and devmgr has everything, which is `fast`'s and `max`'s ordinary desktop-less boot.

The rest of what the description says (budgets, images, restart rule) is the manager's to read: the
kernel parses as little as it can of it.

## 3. What each is handed

Each first process gets a bootstrap channel whose first message is **BOOT-AUTHORITY**: magic `FXBA`,
version, a role, then a fixed table saying which handle slot is what. `ferrix-native-abi::first`.
This is §5's table, row by row:

| To | Handles |
|---|---|
| partition manager | the description's sealed VMO; each safety partition's image as a sealed VMO; each of its devices (K8's kept handle); its job (`JOB` + `RESERVE`, so K12's reservations are handed out from it); the debug log's write handle. K9's windows when K9 lands |
| devmgr | DEVICES, unchanged but for §2's filter and K8's flag; driver images sealed (K14.12); the `drivers` job; a channel to `linuxd` (§5 of `MICROKERNEL.md`, the end `linuxd`'s `/dev` service and the block driver's protocol use); the debug log's write handle |
| `linuxd` | init's three inputs as read-only VMOs, **read by the kernel** from `.ferrix/init/*` (`init.rs`, so L.init.2 stays the kernel's); the initramfs as a read-only VMO; the `linux` job; the audit-read handle pid 1 holds today, which `linuxd` moves into init's private table (K11) and holds no use of itself; the channel's other end to devmgr; the time page's VMO; the debug log's write handle; the command line |

**No process gets the root job.** No handle goes to more than its row. A first process that needs to
start a child starts it in its own job, by K15.

**The debug log's write handle** is a new small object: `Object::DebugLog`, one per kernel, rights
`WRITE | TRANSFER | DUPLICATE`, and **`DEBUG_LOG_WRITE` (0x10C3): `(log, *bytes, len)`**, needing
`WRITE`, bounded per call and rate-limited (a line prefix naming the process; bytes the console's
filter passes). It replaces M5-LIFT's open `DEBUG_WRITE`, which "until a handle guards it" was open
to the first image only. A driver gets a duplicate from devmgr; a Linux program never does.

**The time page's VMO**: `TIME_PAGE` as a handle in the table, read-only, so `linuxd` needs no call.
M5-LIFT's 0x10B7 is withdrawn before release.

## 4. If one of them dies

| Who | What the kernel does | Why |
|---|---|---|
| partition manager | safe state: the kernel's panic path, `FX-0954` | the supervisor of the safety partitions is gone, and K4's channels with it; nothing else may hold their authority |
| devmgr | its `drivers` job is killed, quiesce as T0 does; **no restart in the first landing**: a line, and `linuxd` sees the channel close | the reference configuration "does not move to a devmgr the kernel does not start"; a kernel-driven restart is K13's row |
| `linuxd` | `linux` job killed (its programs with it, K13's row), the `onexit` policy decides (M5-LIFT §6.4) | §3's K13: a `linuxd` that dies takes its partition's programs with it |

The watch is a kernel task on each process's exit, with a stated stack, charged to nothing, and writes
one audit record each (`FIRST_PROCESS_ENDED`).

## 5. The description, and who checks it

**`.ferrix/partitions`**: magic `FXPD`, version, count, then per partition: name (32), image digest
(32), image path (64), reservation in pages, budget and window (K9, zero until it lands), restart rule
(never, or up to n), and up to 8 devices by `(kind, BDF or path hash and compatible)`. Fixed-width,
little-endian, no pointers, one parser in a new no-std crate, `ferrix-partition-desc`, used by the kernel
(§2's device list only) and the manager (the rest), host-tested and fuzzed; `core`-free of allocation.
Its ring: **item**, certified with the manager.

**The generator is uncertified and the checker is qualified (§5's "the plan proposes the second")**:
`tools/common/gen/gen-partitions.py` writes it from a TOML, and `tools/common/check/check-partitions.py`,
written apart from the generator, parses it with its own code and the built images and refuses: a digest
that is not the image's; a device twice; a device the TOML's board has none of; a reservation over the
board's memory. T2 / TQL-5 for the checker, T3 is not claimed for the generator. The description is
security-policy data: `A.ADMIN` in the Security Target.

## 6. What `present` and `desktop` see

Nothing compiled. `Object::DebugLog` and `ferrix-partition-desc`'s parser are the two pieces with a
boot check in the reference configuration: `object::debug_log_check` and a made-up description through
the parser, each a stage 9 line of its own, so neither is untested code in `max` that no boot ran
(§3's "each lands with a boot check"), and each with a control. The first-processes path itself is
exercised by the new row, `test-first-processes`, on a `--personality user` image: all three up, a line
from each with its handle table checked by the program itself (a `first-check` test image of the manager
reads its table and asks the kernel what each handle is), devmgr's drivers bound, `linuxd`'s hello, and
then each refusal below.

## 7. Requirements

**H.OBJ.34**-ish ids come from the block's L.object range under a new H.DEV.12 (*device authority and
safety-partition authority pass from the kernel to their holders directly*).

| Id | Rule | Check | Control |
|---|---|---|---|
| L.object.320 | the three start in order, each measured, in a sealed job, with its image sealed and its digest on its record | `test-first-processes` reads `PROCESS_IMAGES` of its children from the manager, and asks each its table | a job left unmeasured: *a first process's job is not measured* |
| L.object.321 | each holds exactly §3's row: a test image in each role lists its handle table and each object's rights | the table compared with §3 | one more handle given: *a first process holds a handle its row does not name* |
| L.object.322 | no first process holds the root job, and devmgr no safety device | the same; DEVICES' count | the filter removed: *devmgr was handed a safety partition's device* |
| L.object.323 | a description naming no real device stops the boot | row with a made-up location: expected `FX-0953` | test removed: *a partition's device matches no node* |
| L.object.324 | `DEBUG_LOG_WRITE` needs `WRITE`, is bounded, is rate-limited; a Linux program has no handle | `debug_log_check`, every boot | each removed |
| L.object.325 | the parser: well-formed accepted; bad magic, overrunning count, truncation refused; never reads past the end | host tests, fuzz target, stage 9 made-up file | a bound removed: *the description was read past its end* |
| L.object.326 | `FX-0954` on the manager's death; `linux` job killed on `linuxd`'s; `drivers` on devmgr's | row kills each from a test image | the watch removed: *the manager died and the machine ran on* |

## 8. The partition manager

`src/user/system/native/partmgr`: `no_std` on `ferrix-rt` with no heap (a static arena sized by
`MAX_PARTITIONS`), `ferrix-process-builder`, `ferrix-partition-desc`. **A few thousand lines with the
loader is the plan's *guess***, and it counts.

For each partition, in the description's order: re-read its digest from the description; `VMO_DIGEST` of
its sealed image and compare; make a child job (measured, K17's level when it lands); `PROCESS_MAKE`,
build, `PROCESS_RUN`; `PROCESS_IMAGES` and compare to exactly that digest; attach K4's exception channel;
hand its devices, each bound first (`device_bind_domain`, refusing an untranslated one unless its
partition's `dma_none` says it has no DMA); and its reservation. Then READY to the kernel. Then a loop on
one port: an exception or an exit, by the partition's restart rule: restart up to n, or leave down. It
says each thing on the debug log. It holds no secret; it speaks no protocol to Linux or to devmgr at all,
which is what keeps it small (AoU: *no safety partition calls an uncertified server*).

## 9. Questions

* **Q1.** Boot order: manager before devmgr, with a wait for READY. A slow partition delays Linux. My view:
  the description's partitions start before Linux by design; the wait is bounded and a miss stops the boot.
* **Q2.** `Object::DebugLog` compiled into `present` for its check, as K8's call is: acceptable? Today's
  debug path has no handle at all.
* **Q3.** devmgr's death: no restart in the first landing. The reference today has none on the kernel path
  either (`KEPT` is `ferrix.devmgr=init`'s). Acceptable until K13?
* **Q4.** `.ferrix/partitions` read by the kernel at all (§2): the alternative is that devmgr gets everything
  and the manager asks: rejected, against §5.
