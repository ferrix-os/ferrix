# PCIDs on x86-64

Process-context identifiers let a `CR3` write keep the outgoing address
space's translations, tagged, instead of dropping every non-global entry.
This is `docs/OPAQUE-KERNEL.md` §9.5 step 3's first item ("PCIDs on
x86-64"), deferred in §9.6 because nazuna has none. The customer asked for
it on 2026-10-29 for the N150 (Intel Gracemont, `pcid` and `invpcid` on all
four cores): "if we know we run on an intel CPU or a CPU that has it we can
use it ... a general check for hardware". So it is switched on by `CPUID`,
never by vendor, and with no table of parts.

## 1. What changes, and what does not

- **Detection.** `CPUID.1:ECX[17]` (PCID). `CPUID.(7,0):EBX[10]` (INVPCID)
  is read and reported only: nothing below needs it, because every
  PCID-scoped flush is a `CR3` write with bit 63 clear.
- **The decision is the boot processor's**, made in
  `arch::x86_64::init_speculation` *before* `speculation::init` plans the
  defences, so the plan's ERAPS test (`eraps_empties_on_switch`, which reads
  `CR4.PCIDE`) sees the final bit. It is never changed later.
- **Only where every processor has PCIDs** (the customer's rule; the
  consultant's P1). The boot processor sets its own `CR4.PCIDE` at the
  decision, with `CR3[11:0]` zero, before `CpuStarter::new` copies its `CR4`.
  Each secondary adopts that copy whole, after loading the kernel's root;
  one whose own `CPUID` lacks PCID stops the boot first (FX-0410,
  `L.x86_64.215`). So one machine-wide flag (`pcid::on`) decides every
  install.
- **Off (no PCID, nazuna, every QEMU row): the same writes as before.**
  `install_user_root` writes the space's bare root and nothing else, which
  the switch-back check reads back on every gated row (`L.x86_64.219`);
  `uninstall_user_root` the bare kernel root; every write flushes; the
  space's set is joined and left at the same points. The off path adds one
  relaxed store per install (the value written, for that check).
- **No KPTI.** Ferrix has no user/kernel table split (`speculation.rs`:
  "KPTI is not built", AoU-11), so there is one PCID per space, not a pair.
- **INVPCID is not used.** A full flush stays `CR4.PGE` toggled
  (`flush_tlb_including_global`), which invalidates every PCID, global
  entries included (SDM 4.10.4.1).

## 2. The allocator: per processor, Linux's design

Linux's x86 allocator (`arch/x86/mm/tlb.c`: `TLB_NR_DYN_ASIDS`, `ctx_id`,
`tlb_gen`), not arm64's global one: x86 has no broadcast invalidation, so a
global PCID buys nothing and needs a rollover protocol across processors.

- **PCID 0** is the kernel's root, and only it. Uninstall writes it *with*
  the flush (bit 63 clear), exactly as today: PCID 0 holds nothing worth
  keeping.
- **PCIDs 1 to 6** (six slots, Linux's number) are user spaces. A slot holds
  the space's **id** -- a 64-bit number from a counter, given at
  `AddressSpace::assemble`, never reused, so a new space at a freed root's
  frame never matches a dead space's slot (no ABA) -- and **synced**, the
  space's shootdown count (`Flushes::begun`) as read before the last
  flushing write of that slot.
- **The table is per processor**, touched only by its own processor with
  interrupts masked (install and uninstall already require that): plain
  relaxed atomics, no new `unsafe` for it.
- **Install of space S** (id `i`, `begun` read *after* joining S's set):
  1. a slot holding `i` with `synced >= begun`: write `root | k | 1<<63`
     (no flush);
  2. a slot holding `i` with `synced < begun`: write `root | k` (flushes
     PCID k), `synced = begun`;
  3. no slot: take the next slot round robin (the "rollover": the evicted
     space's entries go with the flush), record `i`, write `root | k`,
     `synced = begun`.
  The 12-bit field holds 4,095 user PCIDs; six are used, as Linux uses
  six, because a slot costs TLB capacity and the search is linear.

## 3. Shootdowns, and leaving a space's set

Today a processor leaves a space's set after the `CR3` write that dropped
its translations. With PCID on, that write *keeps* them, tagged. The set
is still left at the same place, and that is safe for Linux's reason:

- `begin_shootdown` takes translations out, then counts the shootdown
  (`begun`, made `SeqCst`), then reads the set. Install joins the set, then
  reads `begun`. Under one total order, either the shootdown's set read sees
  the join (the processor is interrupted, and invalidates with S current or
  will flush on return, below), or install's `begun` read sees the count
  (and install flushes the slot, since `synced` was read before it).
- A processor that has left S's set may keep S's entries under PCID k. They
  are never used while k is not current: the processor uses only the
  current PCID's TLB and paging-structure-cache entries (SDM 4.10.1). Before
  k is current for S again, install compares `synced` with `begun`; any
  shootdown of S since the last flushing write makes it flush.
- An interrupted processor that switched away before it answered runs
  `invlpg` against the wrong PCID. Harmless: its slot's `synced` predates
  the count, so the return flushes.
- **F-36** (tables freed after the shootdown): a stale paging-structure
  entry under a non-current PCID is not walked, and is flushed before its
  PCID is current for that space again.
- **A dead space's** entries stay under its slot until the slot is reused,
  and reuse flushes. Its id never comes back.
- **Lazy TLB (FX-0009).** `develop` has none (`swap_address_space`
  uninstalls for a kernel thread; os-35's lazy TLB never landed). If one
  lands, a processor holding a space lazily is in its set (current
  PCID), which is the case above; the slot rule covers the rest.
- **Whole flushes** (`flush_tlb_everywhere`, a request two or more
  generations behind, a processor coming online) toggle `PGE`: every PCID.

## 4. The speculation defences

- **ERAPS** (AMD; the predictor emptied by a `CR3` write *that flushes*)
  stands in for the in-domain refill only where `CR4.PCIDE` is clear. The
  plan reads the boot processor's `CR4` after the PCID decision, so on a
  machine with PCID on, ERAPS is never claimed and the refill runs at every
  in-domain switch. That keeps the guarantee unchanged rather than arguing a
  replacement: ERAPS claimed => the boot processor's PCIDE clear => no
  processor sets PCIDE => every `CR3` write flushes.
- **`IBPB` and the switch barrier** key on the root (`entered_space`),
  unchanged.
- **No user mapping is global** (every `map_in` in `user/space.rs` passes
  `global: false`), so no user translation escapes the PCID tag.

## 5. Checks and negative controls

**No emulator runs the on path.** QEMU 10.2.1's TCG refuses `+pcid` and
`+invpcid` ("TCG doesn't support requested feature", 2026-10-29), and
nazuna's KVM has no PCID to offer. So the gated evidence for the on path
is the decision logic run on described spaces, the loom model, and the
written values; the hardware behaviour is shown on the N150 only.

- **K1a, the slot rule** (`pcid/check.rs`, every boot): the decision equals
  the boot processor's CPUID; on a table of its own, a new space flushes, a
  hit with no shootdown since keeps, a hit after one flushes, a seventh
  space evicts round robin and its owner flushes on return, and each value
  has the PCID in 1..=6 and bit 63 exactly when kept. *Controls:* the
  comparison taken out; a miss that keeps.
- **K1b, the written values** (`user/check.rs`, switch-back check, every
  boot): each install's value read back. Off: exactly the space's root
  (*control:* the off path writing PCID 1, which faults with `PCIDE`
  clear). On (N150): flush, keep, flush after the move, flush after
  eviction, as K1a; and PCID 0 after every uninstall.
- **K2, the stale read** (the same check): a page moved to a new frame
  while its space is not installed reads as the new frame after the switch
  back. Decisive on PCID hardware only; a hand run on the N150 is evidence
  beside the argument, not a control (consultant, P9).
- **K3, ERAPS and PCIDE** (`speculation/check.rs`): ERAPS claimed and PCIDs
  on never together, and the ERAPS decision asked for a processor described
  as offering ERAPS refuses it where `PCIDE` is set. Its control can fire
  only on a boot with `PCIDE` set, so only on the N150.
- **The loom model** (`src/tests/loom/tests/pcid.rs`, `L.user.169`): the two
  orders, with three controls (either order reversed, a hit always
  keeping). `loom` treats `SeqCst` accesses as acquire/release, so the
  model puts a `SeqCst` fence after each read-modify-write, standing in for
  the total order the kernel's `SeqCst` operations have (and x86's locked
  instructions give).

**Lines no gated boot reaches** (R3): `pcid::install`'s tagged branch and
`init`'s `CR4` write run only where the processor has PCIDs (the N150; R1
ran them there); `require_on_secondary`'s FX-0410 needs processors with
different features; `next_id`'s FX-0011 needs 2^64 address spaces; and
the slot table's overflow branch (a logical number past `MAX_CPUS`) is a
processor the scheduler never starts.

## 6. Measuring

`/sbin/ipc-bench`'s `domain-call` p50 (the skill's figure, fast path off):

- **N150** (Intel N150, Gracemont; bare metal, the desktop image by
  `deploy.sh`, `ferrix.checks=skip`), five runs a boot, ABAB by deploy:
  PCIDs 3,632 and 3,637 ns against develop 0736ab1d8's 4,930 and 4,947:
  **0.737 and 0.735**. A first pair, each right after its build's boot,
  read 4,778 against 7,222 (0.66). The box's clock differs boot to boot;
  the pairs are what hold.
- **nazuna** (KVM, no PCID: the off path), `bench-ipc --alternate
  0736ab1d8 --rounds 5` on a quiet host (load 3 to 5, the SMT sibling 2 to
  12% busy): **1.000, spread 0.993 to 1.007**, 1,408 to 1,418 ns against
  1,408 to 1,428: no change, by the skill's 1% rule. (A first run on a busy
  host, load up to 61, read 1.020 with spread 0.826 to 1.363 and is not
  evidence.)

Logs: `~/.local/share/ferrix/logs/pcid/` -- the N150's `box-run-1-fbac16c0b.log`
to `box-run-4-0736ab1d8.log`, `box-pcid-run1.log`, `box-base-run1.log`;
nazuna's `nazuna-bench-abab-2.log.out` (quiet) and
`nazuna-bench-abab.log.out` (busy).

## 7. Where it stands

- 2026-10-29: design reviewed, **OK IF P1-P12** (ledger line 4734). P1 (only
  where every processor has PCIDs) is built as §1 says; P3 (ids checked,
  zero never given, FX-0011) built; P7 answered by the residual-risk entry
  in `docs/certification/SPECULATION.md` §9 ("TLB state across a switch");
  P8's ids reserved (develop 0736ab1d8) and written.
- QEMU's TCG has no PCID (§5): the `+pcid` TCG rows of P9 are withdrawn
  by the consultant (ledger line 4755); K1a and K1b on every boot with
  controls C1-C3 on KVM stand as P9's gated evidence, and the hardware
  controls are owed under BACKLOG p3-019.
- 2026-10-29: code review of 1a5ef8360, **OK IF R1-R6** (ledger line 4755).
  R1, a boot of the landing on the N150 with the boot checks on (the
  customer's OK, 2026-10-29): `pcid: on`, stage 6's user checks passed
  (the switch-back check's on-path writes and its stale read among them),
  and the speculation read-back passed (K3's positive case). The boot then
  stopped at stage 10's IOMMU check (FX-1012), as develop 0736ab1d8 does
  with the same words, so not PCIDs; a first try with the deploy flow's
  `ferrix.msrlog` stopped earlier at the console log check (FX-0505).
  Summary and logs: `~/.local/share/ferrix/logs/pcid/r1-summary.txt`.
- Measured (§6): N150 0.737 and 0.735 of develop's round trip; nazuna (off
  path) 1.000.
