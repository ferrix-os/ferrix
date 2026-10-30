# The RAM scrubber: free memory tested while the machine idles

Item 7 of the eight hardware-error follow-ups the customer ordered after
`docs/MACHINE-CHECK.md` landed (2026-10-30: "go for all items on the list";
items 1 and 6 are `docs/MACHINE-CHECK-AMD.md` and `docs/CORE-QUARANTINE.md`).
Design v1, for the certification consultant's design review.

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
  `RETIRED_CAPACITY` (256) frames, never handed out again until restart; a
  free frame is claimed off the free lists, an allocated one kept when its
  last user lets go. The machine check retires frames this way.
  `/proc/ferrix-mce-retired` lists them, root alone.
* **Claiming one free frame** (`mm::claim_frame`): takes a named free frame
  off the free lists, or answers `None`.
* **Cache maintenance**: AArch64 `dc civac`, ARMv7-A `clean_to_poc` (clean
  only), x86-64 `clflush` in the framebuffer code. No facade evicts a range
  so that the next read comes from memory.
* **SMBIOS** (`ferrix_smbios`): identity fields only; type 16 (Physical
  Memory Array), whose "Memory Error Correction" field says whether the
  memory has ECC, is not decoded.
* **Policy in ring 3** (`docs/CORE-QUARANTINE.md`): a capability token the
  kernel gives pid 1, which grants a service a copy (`GRANT`), the
  service's policy in a `.conf` file.

## 3. The mechanism (kernel)

### 3.1 One call tests a batch of free frames

`mm::scrub::test_batch(budget)` walks a cursor over the allocator's frames
in physical order and, for up to `budget` frames (at most 64 a call):

1. **Claims** the frame (`claim_frame`): a frame in use, reserved or
   retired is skipped -- the scrubber only ever touches memory nobody
   holds, so no program's or device's data is read or written.
2. **Tests** it with four passes over its 4 KiB, each a write of the whole
   frame, an eviction of every line to memory, and a read back compared:
   `0x5555..`, `0xAAAA..` (every bit both ways), then each 64-bit word its
   own physical address and that inverted (an address line stuck or
   shorted puts one word's value in another's place). Writes are whole
   cache lines, so no line is read before the scrubber has written it.
3. **Confirms** a mismatch by testing the frame once more: a frame that
   fails twice is **bad**; one that fails once is counted as a transient
   and released.
4. A good frame is **zeroed and released** (`deallocate_frames`); a bad one
   is **retired** (`retire_frame`, which finds it claimed and keeps it),
   counted, and said on the console: `scrub: a frame failed its pattern
   test twice and was retired` (no address: `/proc/ferrix-mce-retired`,
   root's, lists it).

The call runs in the asking task with interrupts on and no spin lock held
across a frame's test; it holds `FRAMES` only to claim and to release, as
any allocation does. It stops early when free memory falls below twice
the watermark (the scrubber never makes an allocation fail) or when the
retired set is full (a machine with 256 bad frames has told its story).

### 3.2 Evicting to memory (arch facade)

`arch::evict_to_memory(virt, len)`: after it returns, a read of the range
comes from memory, not a cache. x86-64: `clflush` per line then `mfence`
(SDM vol. 2A, CLFLUSH: invalidates from every level of the hierarchy in the
coherence domain). AArch64: `dc civac` per line then `dsb sy`. ARMv7-A:
`DCCIMVAC` per line then `dsb` (clean *and* invalidate; today's
`clean_to_poc` only cleans). Read through the direct map, which maps RAM
write-back cacheable.

### 3.3 Where ECC is present, the scrubber stands down

A read of a line whose ECC finds an uncorrectable error raises a machine
check, and in ring 0 that stops the machine (`H.FAIL.1`). On ECC memory the
hardware patrol-scrubs already, and an uncorrectable error found by the
scrubber's own read would halt a machine that might never have touched
that page. So the kernel decodes SMBIOS type 16's Memory Error Correction
field at boot, and `test_batch` refuses (`NOT_SUPPORTED`) where any memory
array reports single-bit, multi-bit or CRC correction. Where SMBIOS is
absent or says "unknown" the scrubber runs: the writes-first patterns mean
it reads only what it has just written, and an AoU names the residual (a
machine that has ECC and does not say so).

### 3.4 Counts

`/proc/ferrix-scrub` (0444): frames tested, transients, bad, retired by the
scrubber, full sweeps completed, and whether the scrubber stands down for
ECC. No addresses.

## 4. Who may ask (one door)

The kernel makes one `Object::MemoryScrub` token and gives it to pid 1
after the processor service token (`bootstrap::MEMORY_SCRUB_MAGIC`), with
`MANAGE`, `DUPLICATE` and `TRANSFER`. init grants a copy (`GRANT`, name
`memory-scrub`) to `memscrub.service`, which drops `TRANSFER`.
`MEMORY_SCRUB(token, budget, answer)`: `INVALID_ARGS` for a budget of 0 or
over 64, `ACCESS_DENIED` without `MANAGE`, `NOT_SUPPORTED` where ECC is
reported; the answer is what the batch did (tested, transients, bad,
retired, sweeps). No root door: scrubbing is not a setting, and a root
that wants it run harder edits the service's configuration.

## 5. The policy (ring 3: `memscrub`)

A small native service, `memscrub.service` (`Type=native`, in a sealed
job, weight lowest), started by default where the token is given:

```
period = 86400      # seconds for one sweep of free memory
idle = 90           # percent of the last second the processors were idle
batch = 16          # frames a call
```

It reads `/proc/stat` once a second; when the machine was at least `idle`
percent idle it calls `MEMORY_SCRUB` with `batch`, as often as it takes
to sweep the free memory once a `period`, and never while the machine is
busy. A configuration it cannot read or with a value out of range means
the defaults, said once. It logs each sweep's end and every bad frame
through the kernel's lines, which lode keeps.

## 6. What it does not do

* It does not test memory in use: a program's pages, the kernel's heap and
  image, DMA buffers. Those are tested only once freed and swept.
* It does not run on ECC memory, where the hardware does it better.
* It does not retire a frame that failed once: a transient is counted.
* It is not a memtest replacement at boot; a boot-time sweep is a later
  option (`ferrix.scrub=boot`).
* Under QEMU it finds nothing: emulated memory does not fail. Its gate
  injects the failure.

## 7. Requirements and checks

**Hazard** H.FAIL.9 (reserved): the scrubber must never touch a frame in
use, never make an allocation fail, never halt the machine where ECC is
reported, and never retire a frame that is good. **Requirements** L.mm.101
to L.mm.108 (reserved): the claim-test-release cycle and its skip rules;
the four patterns and the eviction; the double failure before retirement;
the watermark and full-set stops; the ECC stand-down; the token and the
call's arguments; the counts; the arch eviction facade's three forms.

**Checks**:

* **Stage check** (mm): a batch over a quiet machine tests frames and
  frees them all (the free count unchanged after); a frame held by the
  check is skipped and its contents unchanged; an injected stuck bit (a
  check-only hook that flips one bit of one frame's read) is retired after
  two failures, an injected one-off is counted as a transient and
  released; the watermark stop; the eviction (a line written, evicted, its
  memory read through an uncached alias equals what was written).
* **Native check**: the token's rights pinned; `MEMORY_SCRUB`'s
  refusals; a call's answer adds up.
* **`test-scrub`** (x86-64 KVM and TCG, AArch64, ARMv7-A): init and
  `memscrub` with `period = 20`, 512 MiB of memory: a full sweep completes
  and is said, `/proc/ferrix-scrub` counts every free frame tested, no
  allocation failed, and a busy machine (four CPU-bound loops) scrubs
  nothing while busy. An SMBIOS table with ECC (QEMU's `-smbios type=16`
  where the version allows, else the stage check's decode of a recorded
  table) makes the call refuse.
* **Negative controls**: the claim skipped (the check's held frame
  changes); one pass's eviction skipped (the injected failure on that pass
  is not seen -- the read came from the cache); retirement after one
  failure (the injected transient retires a good frame); the ECC
  stand-down removed.

**Docs**: SAFETY-MANUAL AoU (a machine with ECC that does not report it;
the scrubber is an aid, not a guarantee), VULNERABILITY-ANALYSIS (a new
privileged input, bounded; no addresses leave root), MEMORY-AND-TIMING (a
batch's cost: 64 frames, each four passes of 4 KiB and 64 evictions, no
lock held across a frame).

## 8. Questions for the review

1. ECC stand-down by SMBIOS type 16, with "unknown" allowed: enough, or
   should "unknown" stand down too?
2. Policy in its own small service (`memscrub`) rather than in lode, as
   quarantine's is: agreed?
3. Two failures before retirement, one counted as a transient: the right
   threshold for a pattern test that flipped once?
4. Physical-order cursor and claim-or-skip, so in-use memory is swept only
   once freed: acceptable coverage?

## 9. Where it stands

Design v1 written 2026-10-30. Not reviewed.
