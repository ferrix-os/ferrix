# The RAM scrubber: free memory tested while the machine idles

Item 7 of the eight hardware-error follow-ups the customer ordered after
`docs/MACHINE-CHECK.md` landed (2026-10-30: "go for all items on the list";
items 1 and 6 are `docs/MACHINE-CHECK-AMD.md` and `docs/CORE-QUARANTINE.md`).
Design v2.1: v2 answered the consultant's NOT YET (B1)-(B6), ledger 5045;
v2.1 carries the re-review's conditions, OK IF (1)-(9), ledger 5057.

## 1. Why

Memory without ECC -- most desktops, and every overclocked one -- reports
nothing when a cell flips or sticks. The first sign is a program's wrong
answer, a corrupted file, or a crash far from the cause. Linux offers
`memtest=` at boot (a pattern test of free memory before anything runs) and
nothing at run time; hardware ECC patrol scrubbing exists only where ECC
does. The customer wants Ferrix to hold up better than Linux and Windows on
an overclocked machine: a frame that fails a pattern test should be found
while the machine idles, before anything is stored in it, and taken out of
use for the rest of the boot.

## 2. What is there (develop, 2026-10-30)

* **Frame retirement** (`mm::retire_frame`, `L.mm.91`, `L.mm.92`): up to
  `RETIRED_CAPACITY` (256) frames, never handed out again until restart. A
  free frame is claimed off the free lists (`Claimed`); an allocated one --
  the scrubber's own claimed frame among them -- is `Marked` and kept by
  `keep_retired` when it is freed. `/proc/ferrix-mce-retired`, root's,
  lists them.
* **Claiming one free frame** (`mm::claim_frame`, `Frames::claim`): finds
  the free block of order `m` holding the frame, unlinks it and splits it
  down, leaving the buddies free: one order-`m` block fewer on the lists.
* **Cache maintenance**: x86-64 `clflush` (`cpu.rs`'s `clflush_line`, the
  line from `CPUID` leaf 1) in the framebuffer and the kernel's own
  flushes; AArch64 `dc civac` over `CTR_EL0.DminLine` (`cpu.rs`); ARMv7-A
  `clean_to_poc` over `CTR.DminLine`, clean only. No facade evicts a range
  for a read-back test.
* **SMBIOS** (`ferrix_smbios`): identity fields only; type 16 (Physical
  Memory Array) is not decoded. QEMU's own type 16 reports multi-bit ECC
  (06h).
* **Policy in ring 3**: the processor service token pattern
  (`docs/CORE-QUARANTINE.md` §4): a token the kernel gives pid 1, which
  grants a service a copy (`GRANT`); the service's pace in a `.conf` file.

## 3. The mechanism (kernel)

### 3.1 A batch tests free frames, one at a time

`mm::scrub::test_batch(budget)` (one batch at a time machine-wide, under a
sleeping lock, C1) walks a cursor over the allocator's frames in physical
order and, for up to `budget` frames (at most 64 a call), one frame at a
time:

1. **Claims** the frame, in one hold of `FRAMES`, only if all of these hold
   in that same hold (B3): the frame is free and not retired; free memory
   is at least twice the watermark; and the free lists hold **another**
   block of the claimed block's order `m` or above, so a request of any
   order up to `m` that could be met before the claim can still be met.
   Otherwise the frame is skipped. The claim moves no counter: the
   release goes back through the same uncounted path (C4).
2. **Tests** it with four passes over its 4 KiB (§3.2), each a pass of
   writes that read nothing, then an eviction of every line to the point of
   coherency, then a read back compared: `0x5555..`, `0xAAAA..` (every bit
   both ways), then each 64-bit word its own physical address and that
   inverted (an address bit stuck or shorted within the frame; address
   faults across frames are not caught, C8).
3. **Judges** (§8 answer 3): a frame that fails is tested again at once; a
   second failure makes it **bad**. One that passes the retest is a
   **suspect**: entered in a bounded list (64 entries, oldest dropped) and
   released; a suspect that fails again on any later sweep is bad.
4. **Releases** a good frame, zeroed, through the uncounted free. A **bad**
   frame is retired (`retire_frame`), which answers `Marked` for the
   scrubber's own claimed frame; the scrubber then frees it through a free
   path that makes the same `holds_retired` / `keep_retired` decision
   `deallocate_frames` makes, so the frame stays Allocated, owned by
   nobody, and in `RETIRED` -- the stage check asserts all three (B6,
   condition 4). If the retired set is full, or the scrubber's share of it
   used (192 of the 256, the rest left to the machine check), a bad frame
   is **not released**: it stays claimed, owned by nobody, counted and
   said -- and the scrubber **stops** either way, so at most one frame is
   ever held out. Each is
   said on the console: `scrub: a frame failed its pattern test twice and
   was retired` (or `... and is held: the retired set is full`); no address
   (`/proc/ferrix-mce-retired` lists retired ones, root's).

The call runs in the asking task, preemptible, with interrupts on; it holds
`FRAMES` only to claim and to release, and no spin lock across a frame's
test. It stops at the first claim refused for the watermark, and when the
retired set is full or the scrubber's share of it used.

**The residual** (B3): two requests of order `m` made at once while exactly
two such blocks are free and the scrubber holds a frame split from one:
the second fails for the length of one frame's test (§7, MEMORY-AND-TIMING),
as it would have failed had a third request arrived first. It is named in
the AoU; the allocator does not retry.

### 3.2 Writes that read nothing, and an eviction (arch facade)

An ordinary store that misses the cache reads the line from memory first (a
read for ownership on x86, a linefill on Arm), so the first pass of plain
stores would read the frame's old contents -- on ECC or parity memory an
uncorrectable error there is a machine check in ring 0, which halts (B1).
So every pass's writes are made by an operation that writes a whole line
without reading it, and the requirement is: **the scrubber reads no line
whose memory contents it has not itself written in the current pass**.
Every read of memory is of contents the pass wrote -- the read-back after
the eviction included -- and none is of the frame's old contents
(condition 1). On AArch64 a store
after `dc zva` may fill its line from memory, and what it reads there is
the zeros `dc zva` wrote in the same pass, so it meets the requirement.

* **x86-64**: `movnti` over every 8 bytes of each whole line (non-temporal,
  write-combining, no read for ownership), then `sfence`; the eviction is
  `clflush` per line (the line from `CPUID` leaf 1), then `mfence`. A
  write-combining buffer flushed part-way -- by an interrupt between the
  eight `movnti`s of a line -- writes a partial line, and an ECC memory
  controller completes a partial line by reading the old one. Ring 0 keeps
  no vector state (the kernel is built without SSE and AVX, and saves no
  vector registers on entry), so a single full-line non-temporal store
  (`movdir64b`, or an AVX-512 `vmovntdq`) is not available to it; each
  line's eight stores are instead made with interrupts masked, which
  leaves an SMI or an NMI as the only thing that can split a line. That
  residual matters only where the scrubber runs on memory that corrects
  errors without saying so (Other, Unknown, or no type 16), and the AoU
  names it (condition 5).
* **AArch64**: `dc zva` per block (the size from `DCZID_EL0.BS`; the
  scrubber stands down if `DCZID_EL0.DZP` is set), which writes zeros
  without a linefill, followed, for the non-zero patterns, by ordinary
  stores: a store's linefill, if the zeroed line is not held in a cache,
  reads only the zeros the scrubber has just written; the
  eviction is `dc civac` per line (`CTR_EL0.DminLine`), then `dsb sy`.
* **ARMv7-A** has no write-without-read: it **stands down** unless the
  table says the memory has no ECC (§3.3) and the platform has no outer
  cache -- the device tree names none: no `arm,pl310-cache` and no other
  `arm,*-cache` node (a PL310's lines are not maintained by the CP15
  operations, B5). On every ARMv7-A platform supported today it stands
  down: neither DK1 nor QEMU's `virt` gives a type 16 saying None
  (condition 6).
  Where it runs, the eviction is `DCCIMVAC` per line (`CTR.DminLine`),
  then `dsb`.

`arch::scrub_write`, `arch::evict_to_coherency`: each issues one
maintenance operation per line over the whole range, the line read from
the processor, never a constant, with its barrier after (B4).

**To the point of coherency, not the cells** (B5): these operations reach
the point of coherency. A memory controller's write queue, an Arm system
cache beyond the point of coherency, or an x86 memory-side cache (2LM,
MCDRAM in cache mode) can answer the read-back, and then a weak cell is not
seen. The review asked for a batch to write all its frames before reading
any back, or for a reason not to: one frame at a time is kept, since B3's
claim rule takes at most one frame off the lists, and the test reads a
frame back only after it has written and evicted the whole frame (4 KiB, 64
lines on a 64-byte line), more than a memory controller's write queue
holds. The requirement says "to the point of coherency", and the AoU says
detection is weaker past it.

### 3.3 Where the memory has ECC, or the machine is a guest, it stands down

The kernel decodes SMBIOS type 16 at boot (B2), counting only arrays whose
Use is System memory (03h):

* every such array says **None** (03h): it runs;
* any says **Parity** (04h), **single-bit ECC** (05h), **multi-bit ECC**
  (06h) or **CRC** (07h): it stands down -- the hardware corrects, and its
  patrol scrubbing is the right tool;
* **Other** (01h), **Unknown** (02h), or no type 16 at all: it runs only
  where §3.2's write-without-read exists (x86-64, AArch64 without DZP),
  and stands down on ARMv7-A;
* a type 16 that is truncated, or whose length or fields cannot be read,
  makes it stand down (fail closed).

It also stands down **as a guest**, where the architecture lets it tell:
touching every free guest frame makes the host back all of them (C2).
x86-64: `CPUID` leaf 1 ECX bit 31. AArch64 (condition 3): the device tree
has a `/hypervisor` node, or PSCI's conduit is not `smc` (it is `hvc`, or
the firmware names none: fail closed) -- so QEMU's `virt` and the Pixel 7's
crosvm VM stand down. (The SMCCC vendor hypervisor UID adds nothing to
these: a hypervisor that answers it is reached by `hvc`.) A guest none of these betrays is the AoU's
residual.

The gate needs the scrubber on under QEMU, whose own type 16 says
multi-bit ECC. One boot option, `ferrix.scrub=test`, overrides both
stand-downs, and only where it cannot defeat the ECC stand-down on bare
metal (condition 2, ledger 5140 (1)): it is compiled into a build with the
checks (`ferrix_checks = "on"`), which every certified profile is, so what
keeps it off bare metal is that it is honoured only where a hypervisor is
positively seen -- x86-64's hypervisor bit, AArch64's `hvc` conduit or
`/hypervisor` node, never the fail-closed answer for firmware that names no
conduit. The option is declared test-only in `build-flags.toml`, which a
certified profile's command line may not carry, and the integrator shall
not pass it (AoU-38). Taken, it says `scrub: runs: stand-down overridden
for the gate`. Only `test-scrub` passes it. Otherwise the decision and its
reason are a boot line: `scrub: runs` / `scrub: stands down: <why>`.

### 3.4 Counts

`/proc/ferrix-scrub` (0444): frames managed, frames tested this sweep and
in all, suspects, bad, retired by the scrubber, held because the set was
full, full sweeps completed, and whether and why it stands down (§8 answer
4: coverage visible as tested against managed). No addresses; the bad count
is readable by all (VULNERABILITY-ANALYSIS says why, C6).

## 4. Who may ask (one door)

The kernel makes one `Object::MemoryScrub` token and gives it to pid 1
after the processor service token (`bootstrap::MEMORY_SCRUB_MAGIC`), with
`MANAGE`, `DUPLICATE` and `TRANSFER`. init grants a copy (`GRANT`, name
`memory-scrub`) to `memscrub.service`, which drops `TRANSFER`.
`MEMORY_SCRUB(token, budget, answer)`: `INVALID_ARGS` for a budget of 0 or
over 64, `ACCESS_DENIED` without `MANAGE`, `NOT_SUPPORTED` where it stands
down, `SHOULD_WAIT` while another holder's batch runs (C1); the answer is
what the batch did. No root door: scrubbing is not a setting.

## 5. The policy (ring 3: `memscrub`)

A small native service, `memscrub.service` (`Type=native`, a sealed job,
the lowest weight), started where the token is given:

```
period = 86400      # seconds for one sweep of free memory
idle = 90           # percent of the last second the processors were idle
batch = 16          # frames a call
```

It reads `/proc/stat` once a second; when the machine was at least `idle`
percent idle it calls `MEMORY_SCRUB` with `batch`, as often as it takes to
sweep the free memory once a `period`, never while the machine is busy. A
configuration it cannot read, or with a value out of range, means the
defaults, said once. The bounds -- budget, watermark, contiguity, the
share of the retired set, one batch at a time -- are the kernel's; only the
pace is the service's (§8 answer 2).

## 6. What it does not do

* It does not test memory in use: programs' pages, the heap, page tables,
  the page cache, DMA buffers. Memory held for the whole boot is never
  tested (the AoU says so).
* It does not run on ECC or parity memory, under a hypervisor, or on
  ARMv7-A without a table saying no ECC and no outer cache.
* It does not test past the point of coherency (§3.2).
* It is not a boot-time memtest; `ferrix.scrub=boot` is a later option.
* Under QEMU it finds nothing, and QEMU models no caches: its gate injects
  the failure, and the eviction is shown by build evidence (§7).

## 7. Requirements and checks

**Hazard** H.FAIL.9: the scrubber never touches a frame in use, never makes
an allocation fail but for §3.1's residual, never halts the machine where
the firmware reports that the memory corrects errors (an ECC machine that
reports Other or Unknown is the AoU's residual), and never takes more than
its share of the retired set. (Losing a good 4 KiB to a false failure is not a hazard; the
share bounds it.)

**Requirements**, one check-provable property each (C3), reserved before
code: in L.mm -- the claim and its skip rules (free, not retired, the
watermark, another block of the order, all in one hold); the release
uncounted; the four patterns; the judgement (retest, suspects, bad); the
bad frame kept (retired and held, or held when the set is full; the share);
the stand-down decision from type 16 and the hypervisor bit, fail closed;
the counts. In the arch parts, three facade ids, one per architecture
(x86-64, AArch64, ARMv7-A): the write-without-read and the eviction per
line with its barrier. In the object and syscall parts, as quarantine's
were: the token, its rights, and `MEMORY_SCRUB`'s arguments and answers.
Ids, reserved in one docs-only landing before code (condition 7):
H.FAIL.9; L.mm.101-108; L.x86_64.243, L.aarch64.68, L.armv7a.29;
L.object.252-253; L.syscall.24.

**Checks, all of which run and fail under QEMU** (B4):

* **Stage check** (mm): a batch over a quiet machine tests frames and
  frees them all, the free count and every order's free-block count
  unchanged after; a frame the check holds is skipped, its contents
  unchanged; a frame that is the only free block of its order is skipped;
  the watermark stop; an injected stuck bit (a check-only hook that flips
  one bit of one frame's read-back) retires the frame after the retest,
  its state then held; an injected one-off makes a suspect, released, and
  the same frame failing on a later sweep is retired; the share and the
  full set hold a bad frame out; the type-16 decoder over recorded tables
  (None, Parity, ECC, Other, Unknown, absent, truncated).
* **Build evidence for the facade** (B4): a host test reads the facade's
  loop and requires one maintenance operation per line over the whole
  range with the line from the processor, and the barrier after; the
  disassembly of each architecture's `scrub_write` and
  `evict_to_coherency` pinned (no ordinary store in the x86-64 write loop;
  `dc zva` and `dc civac` on AArch64). A `docs/BACKLOG.md` row asks for
  the hardware run on the N150, an AArch64 board and DK1.
* **Native check**: the token's rights pinned; `MEMORY_SCRUB`'s refusals
  (budget 0 and 65, no `MANAGE`, another object); a call's answer adds up;
  `SHOULD_WAIT` while a batch runs.
* **`test-scrub`** (x86-64 KVM and TCG, AArch64; ARMv7-A's row requires
  the stand-down line, its reason no write of a whole line that reads
  nothing: the architecture is judged before the table): init and `memscrub` with `period = 20`, 512 MiB of memory,
  `ferrix.scrub=test`: a full sweep completes and is said,
  `/proc/ferrix-scrub` counts the frames tested against managed, no
  allocation failed, and with four CPU-bound loops it scrubs nothing while
  busy. A boot without `ferrix.scrub=test` stands down, said with the
  reason (QEMU's type 16).
* **Negative controls that fire under QEMU**: the claim's "free" test
  removed (the check's held frame changes); the "another block of the
  order" test removed (the only block of its order is taken); retirement
  after one failure (the injected one-off retires a good frame); a bad
  frame released when the set is full; the type-16 stand-down removed (the
  boot without the option runs).

**Docs**: SAFETY-MANUAL AoU (the scrubber is an aid, not a guarantee;
detection stops at the point of coherency; memory held all boot is never
tested; a machine with ECC that reports Other or Unknown; §3.1's
residual); VULNERABILITY-ANALYSIS (a new privileged input, bounded; the
bad count readable by all; no addresses leave root); MEMORY-AND-TIMING
(one frame's test: four passes of 4 KiB written and evicted, about 256
evictions and 2048 reads; `FRAMES` held only to claim and release; the
call preemptible between frames).

## 8. Answers to the review's questions (ledger 5045)

1. "Unknown" runs only with the write-without-read (x86-64, AArch64);
   otherwise it stands down.
2. A service of its own; the kernel keeps the bounds, the service the pace.
3. A failed immediate retest retires; a suspect list catches a weak cell
   that passes the retest and fails a later sweep.
4. Coverage is shown as tested against managed; the AoU names what is
   never tested.

## 9. Where it stands

* Design v1 (ec1748ed8) reviewed 2026-10-30, ledger 5045: **NOT YET
  (B1)-(B6)**: the first pass's stores read memory (B1); the type-16 rule
  incomplete (B2); a claim could make an allocation fail (B3); the
  eviction cannot be shown under QEMU (B4); the operations reach the point
  of coherency, not the cells (B5); the bad frame and the retired set
  (B6); C1-C8.
* Design v2 (a020b2dd3) answered them: §3.2 (B1, B5), §3.3 (B2, C2), §3.1
  (B3, B6, C1, C4, C8), §7 (B4, C3, C5), §3.4 (C6), §2 (C7).
* Re-reviewed 2026-10-30, ledger 5057: **OK IF (1)-(9)**. v2.1 carries
  (1) the requirement's wording, (2) the override compiled into check
  builds alone and honoured on x86-64 only under a hypervisor, (3)
  AArch64's guest stand-down, (4) the bad frame kept through
  `keep_retired`'s decision and the stop on a full set, (5) x86-64's
  partial-line residual and the masked stores, (6) ARMv7-A's outer-cache
  test and its stand-down everywhere today, (9) H.FAIL.9's wording. (7)
  is the reservation landing; (8) is the code landing's evidence: rows on
  the landing hash, the native check, §7's five controls FIRED, the
  facade's host test and pinned disassembly in check, the BACKLOG hardware
  row, unsafe tagged and registered, certification-item.json covering
  mm/scrub and the facade, coverage carried; commits citing 5045 and 5057.
* Code (feature/ram-scrubber, 2026-10-30) reviewed, ledger 5140: **OK IF
  (1)-(6)**, with the deviations accepted: the stand-down decision judges
  the architecture first, so ARMv7-A and an AArch64 with `DZP` set say
  their own reason whatever the table says; AArch64's guest test is the
  PSCI conduit and the `/hypervisor` node, the SMCCC UID dropped (§3.3);
  and, for (1)(b), the override honoured only where a hypervisor is
  positively seen (`arch::hypervisor_seen`), apart from the stand-down's
  fail-closed guest test. (2) stage 2's scrubber check runs only where the
  scrubber itself would (`mm::scrub::verdict`), and says its skip; (3)
  L.object.253 released unwritten; (4) MEMORY-AND-TIMING 2.2e3 corrected.
