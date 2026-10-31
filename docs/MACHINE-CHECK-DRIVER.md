# Poison a ring-3 driver consumes

Design v2, 2026-10-31 (v1 627097cef; consultant NOT YET (B1, B2), ledger
5435, carried here). The customer's item 5 of the machine-check follow-ups
(2026-10-30: "poison consumed by a ring-3 driver ends only that driver;
devmgr/T0 restarts it"). It builds on `docs/MACHINE-CHECK.md` (H.FAIL.8,
§3.5 SRAR from ring 3), `docs/MACHINE-CHECK-COPY.md` (H.FAIL.10, a system
call's copy), `docs/DEVMGR.md` §4 (a driver's death, the quiesce, the
restart, the crash loop) and `object/pin.rs`'s quarantine (F-38).

## 1. What is there today

* **A driver's own load of poison** in ring 3 is a ring-3 SRAR: the frame is
  retired and the running program gets `SIGBUS` `BUS_MCEERR_AR`
  (L.x86_64.228), or is ended without a handler where `RIPV` is clear. A
  native driver is a process like any other here (the consultant read it,
  5435 question 3): `launch::load_native` makes the personality's own
  Process, the one global `ReturnPath` is the personality's,
  `signal::force` resets a blocked or ignored `SIGBUS` to its default, and
  nothing in `ferrix_rt` installs a handler; so the signal ends it with
  status 135 (128 + 7), through `fault_signal`, or `fault_end` where `RIPV`
  is clear. While lode crashd holds `/dev/ferrix-crashes` the end is held up
  to 2 s (LOGGING §21.7).
* **A system call's copy** from the driver's memory -- a `channel_write` of
  a buffer, a `vmo_write` -- recovers (H.FAIL.10): the call fails, the frame
  is retired, the driver gets `SIGBUS` and ends the same way.
* **devmgr** sees the death (`TERMINATED`), quiesces the device, writes DIED
  with the status, and starts the driver again by the kind's policy, with
  the crash loop's growing delays (a second death within 10 s is a streak:
  restarts at 1 s, then 2 s; a third reinitialises) (DEVMGR §4).
* **A frame the driver pinned for its device**: a dead driver's pins on a
  translated domain stay mapped, their frames held, until the next driver's
  HELLO releases them (F-38), freed through `mm::release_frame`; a retired
  frame in use is kept allocated when its last user lets go (L.mm.91), so it
  is never handed out again, whether the quarantine releases it or not.
* **The quarantine reads every pinned page in ring 0** (the consultant's
  B1): `object/pin.rs` `fold` sums each page through the direct map with
  `read_volatile`, after `flush_cache_lines`, once when the dead driver's
  pin closes into the quarantine and again at `quarantine_release`, to tell
  whether the device wrote it after its driver died. The victim's poisoned
  frame is one of them: on hardware that read is a ring-0 consumption
  outside a copy, FX-9005. QEMU loads no real poison, so no gate would show
  it. This design changes it (§2.1).

### 1.1 Every ring-0 touch of a dead driver's frames (B2)

From the consumption to the frames' release, each with how it is handled:

| Where | What | Handled |
|---|---|---|
| `arch/x86_64/mce.rs` `from_user`, `signal_page` | the record's address, the space's walk for the program's page | no read of the frame's data |
| `mm::retire_frame` | the retired set, the allocator's lists | no read of the frame's data |
| `syscall/deliver.rs` `force_held`, `fault_end` | the kill; no handler, so no signal frame is written | no touch of the frame |
| `syscall/process.rs` exit's `clear_child_tid` (`uaccess::copy_to_user`) and `futex::wake_address` | a 4-byte store, and the futex word's read, at the thread's `clear_child_tid` | a store is not a consumption; the futex read is `uaccess`, so `copy_mc` (L.syscall.26); a native driver sets none |
| `fs/crashes.rs`, lode crashd | the hold; crashd reads threads, status, maps and descriptors, never the program's memory (no `/proc/<pid>/mem`, no `process_vm_readv`: grep of `src/user/system/native/lode`) | no touch |
| address-space teardown (`user/space`) | unmap, page tables freed, the shootdown | page tables only, no data |
| VMO teardown, `mm::release_frame` | the reference dropped; a retired frame kept, never zeroed or handed out (`holds_retired`) | no read of the frame's data |
| `object/pin.rs` `fold`, at the pin's close (`*sum = fold(frame)`) and at `quarantine_release`, with `flush_cache_lines` | every pinned page read through the direct map | **skipped for a retired frame** (§2.1, L.object.262) |
| ring cores reading driver-shared memory in ring 0: `interfaces/block_ring/mod.rs` (the ring's pages through the direct map, `read_volatile`), `interfaces/net_ring/mod.rs` (the same), `interfaces/display/mod.rs` (a buffer's bytes through the direct map), `interfaces/audio/mod.rs` (the same) | descriptors, indices and buffers a live driver wrote | **residual** (§4): poison there, read by the kernel while the driver lives, stops the machine as FX-9005 |

The residual is the last row and nothing more.

## 2. What it does

* **The claim**: poison a ring-3 driver consumes in its own memory, or that
  a system call's copy consumes from that memory, ends that driver alone,
  with status 135; its frame is retired, never read again by the kernel's
  quarantine, and never handed out again, also after its pins are released;
  no other program ends and the kernel runs on. devmgr then quiesces the
  device and starts the driver again by the kind's policy: the evidence's
  environment (DEVMGR §4), not the item's claim.
* **The residual** (§1.1's last row, §4), with a BACKLOG row.

### 2.1 The quarantine leaves a retired frame unread (L.object.262)

`fold` is not called for a frame that is retired: at the pin's close its sum
is recorded as not taken, and at `quarantine_release` it is not compared
and is counted apart (`Released::retired`); the release line gains `; N
retired, not read` at its end (after `written after it died`, which
test-audio's judge reads). The test is `mm::is_retired`, under the frame's
reference the quarantine holds; a frame retired between the close and the
release is skipped at the release. The flush is skipped with the read.

A check-only hook, `pin::retire_for_check(frame)` (compiled into check
builds only, an empty `const fn` elsewhere), makes the quarantine treat one
frame as retired, since the retired set is already full in a check build
(stage 12's poison check and others take its slots) and a real retirement
would answer `Full`.

## 3. How it is checked

**Stage check** (`object/pin/check.rs`, beside P4): a dead owner's one-page
pin, its frame named retired through the hook; the pin closed into the
quarantine and released: the quarantine's count of pages left unread rises
by two (the close and the release), `Released::retired` is 1 and `written`
0, the frame still allocated after the release. A second pin, not named,
reads as before. Verifies L.object.262.

**A victim driver** (`src/user/system/native/mce-victim`, test-only, carried
by xtask under the name `intel` on the pci-testdev stand-in for ADL-N's GPU,
as `crash-at-start` is for `test-restart --boot crashloop`): it reads START
as an engine's, pins one page of a VMO to its device and maps that VMO,
fills the page with words `MAGIC << 32 | (pid & 0xffff) << 16 | n` -- the
pid tells the gate which start a frame is (C4: no count is kept anywhere)
-- says `mce-victim: pid <p> va <hex>` and reads the page in a loop in
ring 3. In the copy mode it instead writes the page into a channel of its
own in a loop (`channel_write`, a native call through `uaccess`, C8) and
drains the other end.

**`test-mce-driver`** (x86-64, one processor unless said):

* **Own** (KVM with AMD's records, whose model reports `SUCCOR`; TCG with
  Intel's, whose model reports `MCG_SER_P`): after the first start's line
  the gate stops the machine, finds the processor in ring 3 whose `gva2gpa`
  of the victim's address gives a frame holding that pid's words (so the
  victim, not another program mapping that address, is the consumer),
  injects an action-required record at it (`RIPV`, `EIPV`) and resumes.
  Required, within the crash hold's 2 s plus 10 s: `mce: frame <f>
  retired`; `signal pid <p> ended by signal 7`; devmgr's `died` for the
  stand-in's location with status 135; a second start (another pid) whose
  page's frame is not `<f>`; its `restarted 1`; no other `ended by signal`
  line; the shell answering; after the second start's HELLO, the release
  line with `1 retired, not read`; `/proc/ferrix-mce-retired` still holding
  `<f>`. One injection a boot (C6).
* **Kill**: Own with `RIPV` clear: ended without a handler (`fault_end`),
  status 135, the rest as Own.
* **Copy** (KVM with AMD's records only; TCG runs the copy in one step,
  `MACHINE-CHECK-COPY.md` §6): the gate injects at the copy as test-mce-copy
  does: the `channel_write` fails, `SIGBUS`, `died` 135, a second start, the
  frame retired, `1 retired, not read` at the release.
* **Bystander** (TCG with Intel's records only: on AMD's a record with
  `EIPV` clear stops the machine, L.x86_64.236, C1): Own on two processors,
  the other given the same record with `EIPV` clear: that one returns,
  nothing else ends.

"Started again" means started, pinned and running (the second start's line
and its page), not "serves": the victim serves nothing (C3).

**Negative controls**, each through `gate.sh control --expect`:

* the quarantine's skip removed at the close (the stage check fires: `a
  retired frame's page was read by the quarantine`);
* the same at the release (the stage check fires, the same text);
* the retired-frame keep on free removed: stage 2's own L.mm.91 check fires
  first (its text named in the code review, C2);
* devmgr's restart of the stand-in removed (no second start: the judge's
  `no second start` line, `--expect-line`);
* `signal_page` answered as `None`: the ring-3 path then stops the machine
  (`mce.rs` `from_user`, `stop(frame)`): `--expect FX-9005` (C7).

## 4. What it does not do

* **The ring cores' ring-0 reads of driver-shared memory** (§1.1's last row):
  poison there, read while the driver lives, stops the machine, as H.FAIL.8
  says of every ring-0 consumption outside a copy. A BACKLOG row asks those
  reads through `copy_mc`, a stopped read ending the ring as a dead
  driver's does. Accepted as this item's residual (5435 question 2).
* **Latent poison in a frame the fold reads**: a frame not yet retired when
  the quarantine reads it -- another processor's poll retiring it during
  the read, or poison nothing has reported -- is a ring-0 consumption and
  stops the machine, as H.FAIL.8 says of any ring-0 read outside a copy.
  Only a frame already retired is left unread.
* **Poison a device reads by DMA**: no processor consumes it; a device's own
  error (PCIe AER, a poisoned TLP) is not a machine check this kernel takes.
* **A driver with a `SIGBUS` handler** (none today): its handler runs; that
  driver's choice, as a program's is (L.x86_64.228).
* **devmgr, init or lode consuming poison**: programs too, ended the same
  way; what their death means is `docs/INIT.md`'s.
* **Hardware**: QEMU's monitor believes the record and loads no real
  poison; the hardware row (`docs/BACKLOG.md`, mce-copy's) gains a driver
  run, which would also show §2.1 on real poison.

## 5. Requirements

No new H id (5435 question 1): to the kernel a driver is a program.

* **L.object.262** (new, reserved first; parent H.FAIL.8): the quarantine
  shall not read a retired frame -- neither at a dead driver's pin's close
  nor at its release -- and shall count each frame left unread; every other
  frame is read and compared as before.
* Criteria amended: H.FAIL.8 and L.x86_64.228 gain test-mce-driver's Own,
  Kill and Bystander; H.FAIL.10 and L.syscall.25 gain Copy; L.mm.91 gains
  "a pinned retired frame stays kept after `quarantine_release`" (Own's
  check of `<f>`).

## 6. Docs

SAFETY-MANUAL AoU-36, extended (a driver that consumes poison ends and is
restarted by devmgr; the quarantine leaves a retired frame unread; the
residual rows of §1.1); VULNERABILITY-ANALYSIS (the residual); `MACHINE-
CHECK.md` §6; DEVMGR §4 (a driver ended by a machine check is one more
death, status 135); BACKLOG (the ring cores through `copy_mc`; the hardware
row's driver run).

## 7. Where it stands

* Design v1 (627097cef) reviewed 2026-10-31, ledger 5435: **NOT YET (B1,
  B2)**, with question 1 (no new H id), 2 (the ring cores' residual
  accepted, the fold not) and 3 (status 135, confirmed) ruled. v2 carries
  B1 (§2.1, L.object.262, the stage check and two controls), B2 (§1.1) and
  C1-C8 (§3, §1.1).
* Design v2 (80fdc02aa) reviewed 2026-10-31, ledger 5444: **OK IF (1)-(6)**:
  L.object.262 reserved first, its statement saying the count in the
  release line; `mm::is_retired` made product code (its CHECKS-ONLY marker
  off, the narrowest visibility); `retire_for_check` a check-build hook
  only, so the real test is shown by test-mce-driver's release line on KVM
  and TCG; the rows and the five controls; the clear_child_tid row's
  "a native driver sets none" cited from source or moved to the residual;
  the docs, criteria and coverage.
