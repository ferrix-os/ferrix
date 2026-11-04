# Boot authority: who the kernel starts, and with what

`docs/MICROKERNEL.md` §5, §6.3 and §8's M5: under `personality=user` the kernel starts
three programs from the initramfs, the **partition manager**, **devmgr** and **`linuxd`**,
each with exactly the handles of §5's table, so that a safety partition's authority never
passes through init, `linuxd` or devmgr, and device authority never through init or `linuxd`.
Ids from mk-authority's block: L.object.320-328. Native calls 0x10C3-0x10C5. Panic codes
FX-0952-0954. Everything here is `#[cfg(ferrix_personality = "user")]`, not `ferrix_load_built = "none"`, which `personality=none`
also sets (BA.1): `present` and `desktop` build none of it.

**Where it stands:** ledger 5896 **OK IF BA.1-BA.13**, 6564 **OK IF Y.1-Y.8**. This commit, in mk-lift's landing on x86_64 under KVM (`personality=user` does not yet build on aarch64): the manager when a description says, devmgr on its own measured host with a measured drivers' job, then `linuxd`; each with its row (`first_rows.rs`, `Rows::judge` at each start, L.object.321); a description's filter; each one's death. **Later landings:** K14 §5's grant (so a device with `FLAG_NEEDS_CERTIFIED` stops the boot here, Y.1), `btrfsd`, `netd` and further partitions, M6's cuts, K16.

**Owed:**
* **`linuxd` is neither measured nor sealed (Y.2):** its job is plain and its image is loaded from the archive with no seal or digest, against §1 and K14 §3, since it is the QM personality and builds its programs' code at run time on purpose. Until a BACKLOG row decides, a compromised program in `linuxd` can map new code there; nothing of a safety partition's is in its reach. devmgr's job is measured and not sealed.
* **BA.3 (ledger 6343):** PCI matches location, vendor and device. A device that reaches memory needs a domain of its own, which is the proof that nothing shares its requester id, so that BA.10's listed-driver admission has no held PCI user. It must be alone on its bus and directly below a root port. Across root ports: AoU-45 and a BACKLOG row. A tree device is refused outright until D4's closure is read.
* **BA.7:** `linuxd` holds no audit-read handle here, so FAU_SAR.2 holds by nobody reading and FAU_SAR.1 is unmet in this shape: a BACKLOG row. Nothing in the initramfs VMO it holds is a secret.
* **BA.9:** `ferrix-cpio` and `ferrix-partition-desc` are in the manifest; `partmgr` and the process builder wait for M7's document set, as `btrfsd`'s do. BACKLOG row.
* A description with a files role fails closed here: `partmgr` finds its handle count short, since this commit has no file server's channels.
* The bootstrap channel is the means by which a first process is told, and not a holding: `Rows::judge` does not see it.

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
   `etc/ferrix/partitions`, `.ferrix/certified`, `.ferrix/init/{program,script,commands}`.
2. **Seal.** Each image and the description into an anonymous VMO, sealed by `Vmo::seal` (K14).
   Over `SEAL_MAX_PAGES` fails the build (xtask) long before here; here it stops the boot.
3. **Three jobs**, children of the kernel's root, each **measured** and **sealed** (`H.OBJ.22`):
   `partitions`, `drivers`, `linux`. Their reservations (K12) come from the description for the
   first, and the rest of memory is the others' limit as today.
4. **Start the partition manager first**, and wait for its one READY message (bounded, as devmgr's
   REPORT), so a safety partition's devices are claimed before devmgr hears of anything.
5. **Start devmgr**, DEVICES carrying every node **except** the ones §2 hands to the manager, plus the debug log's
   handle and the cores' channel. The kernel waits for no REPORT: its old end goes to `linuxd`, and the device cores'
   channel with it (*Retired calls*: `CORE_OPEN`, `ferrix-devmgr-proto`). The watch on it is registered before the start.
6. **Start `linuxd`** with the handles of §3. Its first program is init.
7. **The kernel's watch**: §4.

A failure at 1-3, or a first process that does not start, is a stage panic (`FX-0952`): nothing on
this machine can run. Each first process is started by the first-image loader into a fresh measured
space, recording its digest (K14 §4); its `grant_starter` is true (K14 §5).

## 2. Which device goes where

`etc/ferrix/partitions` names each safety partition's devices by location (PCI BDF, or tree path and
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
`WRITE | TRANSFER | DUPLICATE`, and **`DEBUG_WRITE` (0x10B0): `(log, *bytes, len)`**, needing
`WRITE`, bounded per call and rate-limited (a line prefix naming the process; bytes the console's
filter passes). 0x10C3 is `DEVICE_SERVED` (ledger 6542, R9); the one debug log is this call. A driver gets a duplicate from devmgr; a Linux program never does.

**The time page's VMO**: `TIME_PAGE` as a handle in the table, read-only, so `linuxd` needs no call.
M5-LIFT's 0x10B7 is withdrawn before release, agreed by reading mk-lift's branch (BA.9).

**The rows are one const table in the kernel per role, and handles are built from it, so the check reads the real source (BA.5).**
`src/kernel/src/first_rows.rs` holds them. Every right given is that table's, in a const expression, so that a kind outside the row does not
compile; `check-first-rows.py` holds the source's tables to this one and each start site to the source's. The row for a further partition
comes from M8 (N.9). What a role is given today (the prose table above says what is still owed: init's inputs, audit-read, the time page):

| Role | Kind | Rights |
|---|---|---|
| `MANAGER` | `Job` | `Rights::JOB` |
| `MANAGER` | `DebugLog` | `LOG_TO_PASS_ON` |
| `MANAGER` | `Description` | `Rights(Rights::READ.0 | Rights::MAP.0 | Rights::TRANSFER.0)` |
| `MANAGER` | `Image` | `IMAGE_RIGHTS` |
| `MANAGER` | `DeviceKept` | `KEPT_DEVICE_RIGHTS` |
| `DEVMGR` | `Job` | `Rights::JOB` |
| `DEVMGR` | `DebugLog` | `LOG_TO_PASS_ON` |
| `DEVMGR` | `Image` | `IMAGE_RIGHTS` |
| `DEVMGR` | `DeviceGiven` | `GIVEN_DEVICE_RIGHTS` |
| `DEVMGR` | `DeviceKept` | `KEPT_DEVICE_RIGHTS` |
| `DEVMGR` | `Channel` | `Rights::CHANNEL` |
| `LINUXD` | `Job` | `Rights::JOB` |
| `LINUXD` | `DebugLog` | `Rights::WRITE` |
| `LINUXD` | `Region` | `Rights::REGION` |
| `LINUXD` | `Archive` | `ARCHIVE` |
| `LINUXD` | `Console` | `Rights::READ` |
| `LINUXD` | `Channel` | `Rights::CHANNEL` |

**`linuxd`'s audit-read handle (BA.7):** it can read every audit record, safety partitions' included, on a promise outside the TSF. ST
FAU_SAR and AUDIT.md name the authorised reader under `max`, or the handle is filtered, with a V row. Nothing in the initramfs VMO is a
secret, stated.

## 4. If one of them dies

| Who | What the kernel does | Why |
|---|---|---|
| partition manager | safe state: the kernel's panic path, `FX-0954`, stated as the safe state in an AoU; a clean exit takes the same path (BA.8) | the supervisor of the safety partitions is gone, and K4's channels with it; nothing else may hold their authority |
| devmgr | its `drivers` job is killed. T0's quiesce is devmgr's own act, so the kernel does it: `disable_dma` per node, pin quarantine with no HELLO coming, bounded (BA.8). **No restart until K13**, a BACKLOG row: a line, and `linuxd` sees the channel close and its disk gone, answering `EIO` | the reference configuration "does not move to a devmgr the kernel does not start"; a kernel-driven restart is K13's row |
| `linuxd` | `linux` job killed (its programs with it, K13's row), the `onexit` policy decides (M5-LIFT §6.4) | §3's K13: a `linuxd` that dies takes its partition's programs with it |

The watch is a kernel task on each process's exit, with a stated stack, charged to nothing, and writes
one audit record each (`FIRST_PROCESS_ENDED`).

The kernel's own loader is K14 §3's one named exception (ledger 6316 P1.3, L.object.334): "executable only from sealed" holds of what a program
asks for; a first process has no creator, so the kernel makes its pages anonymously from the sealed VMO's own bytes (`boot_authority::load_sealed`),
and the digest on its record is that VMO's.

## 5. The description, and who checks it

**`etc/ferrix/partitions`**: magic `FXPD`, version, count, then per partition: name (32), image digest
(32), image path (64), reservation in pages, budget and window (K9, zero until it lands), restart rule
(never, or up to n), and up to 8 devices by `(kind, BDF or path hash and compatible)`. Fixed-width,
little-endian, no pointers, one parser in a new no-std crate, `ferrix-partition-desc`, used by the kernel
(§2's device list only, plus `max_processes`, K16.9, with a default for no description) and the manager (the rest), host-tested and
fuzzed; free of allocation. Its ring: **item**, certified with the manager; `ferrix-cpio` and `ferrix-elf` enter the manifest at the
profile's level, the cpio walk being ring-0 parsing with its own requirement, check and control (BA.9). Each device carries a
`needs_certified` flag a description can add and never remove (BA.10).

**The generator is uncertified and the checker is qualified (§5's "the plan proposes the second")**:
`tools/common/gen/gen-partitions.py` writes it from a TOML, and `tools/common/check/check-partitions.py`,
written apart from the generator, parses it with its own code and the built images and refuses: a digest
that is not the image's; a device twice; a device the TOML's board has none of; a reservation over the
board's memory. T2 / TQL-5 for the checker, T3 is not claimed for the generator. The description is
security-policy data: `A.ADMIN` in the Security Target. The checker runs on the bytes in the built image, the build fails without it,
every refusal has a FIRED control, a TOOLS.md row at T2 / TQL-5; "written apart" means separate code, not F-27 independence (BA.11).

## 6. What `present` and `desktop` see

Nothing compiled (BA.1). `DebugLog` and `ferrix-partition-desc`'s lines run on a `personality=user` kernel; until its stage checks
run: host tests, the fuzz target and an owed BACKLOG row. The first-processes path is exercised by `test-first-processes` on a `--personality user`
image, in the gate table and judged by its own lines since this kernel prints `FERRIX-BOOT-UNCHECKED` (BA.13): all three up, a line from each with its
handle table checked by the program itself (a test image of the manager reads its table and asks the kernel what each handle is; test images never
ship), devmgr's drivers bound, `linuxd`'s hello, and then each refusal below.

## 7. Requirements

**H.DEV.12**: *device authority and safety-partition authority pass from the kernel to their holders directly.* **H.DEV.13**: *each first
process holds exactly its row, and none the root job.*

| Id | Rule | Check | Control |
|---|---|---|---|
| L.object.320 | the three start in order, each measured, in a sealed job, with its image sealed and its digest on its record | `test-first-processes` reads `PROCESS_IMAGES` of its children from the manager, and asks each its table | a job left unmeasured: *a first process's job is not measured* |
| L.object.321 | each holds exactly §3's row: a test image in each role lists its handle table and each object's rights | the table compared with §3 | one more handle given: *a first process holds a handle its row does not name* |
| L.object.322 | no first process holds the root job, and devmgr no safety device | the same; DEVICES' count | the filter removed: *devmgr was handed a safety partition's device* |
| L.object.323 | a description naming no real device stops the boot | row with a made-up location: expected `FX-0953` | test removed: *a partition's device matches no node* |
| L.object.324 | `DEBUG_WRITE` (0x10B0) needs `WRITE`, is bounded, is rate-limited; a Linux program has no handle | `debug_log_check`, every boot | each removed |
| L.object.325 | the parser: well-formed accepted; bad magic, overrunning count, truncation refused; never reads past the end | host tests, fuzz target, stage 9 made-up file | a bound removed: *the description was read past its end* |
| L.object.326 | `FX-0954` on the manager's death | row kills it from a test image | the watch removed: *the manager died and the machine ran on* |
| L.object.327 | the `linux` job killed on `linuxd`'s death | the same | removed: its text |
| L.object.328 | the `drivers` job killed and every node quiesced on devmgr's death, bounded | the same | removed: its text |

## 8. The partition manager

`src/user/system/native/partmgr`: `no_std` on `ferrix-rt` with no heap (a static arena sized by
`MAX_PARTITIONS`), `ferrix-process-builder`, `ferrix-partition-desc`. **A few thousand lines with the
loader is the plan's *guess***, and it counts.

For each partition, in the description's order: re-read its digest from the description; `VMO_DIGEST` of
its sealed image and compare; make a child job (measured, K17's level when it lands); `PROCESS_MAKE`,
build, `PROCESS_RUN`; `PROCESS_IMAGES` and compare to exactly that digest; attach K4's exception channel;
hand its devices, each bound first (`device_bind_domain`; **an untranslated safety device is admitted only if its driver passes K14's grant**,
`dma_none` being D6 in disguise and removed, BA.10); and its reservation. Then READY to the kernel. Then a loop on
one port: an exception or an exit, by the partition's restart rule: restart up to n, or leave down. It
says each thing on the debug log. It holds no secret; it speaks no protocol to Linux or to devmgr at all,
which is what keeps it small (AoU: *no safety partition calls an uncertified server*).

## 9. Questions

Answered, ledger 5896. **Q1**: the wait is OK; the bound comes from the description, a miss stops the boot, and READY means started, not healthy.
**Q2**: no, not compiled into `present`. **Q3**: OK until K13, a BACKLOG row. **Q4**: agreed; the kernel reads only magic, bounds and the device list.
