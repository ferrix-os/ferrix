# A system call's copy that consumes poison

Design v2, 2026-10-30 (v1 8bbcb8ab4; consultant OK IF (1)-(12), ledger 5216, carried here). The customer's item 2 of the machine-check follow-ups
(2026-10-30: "copy_mc-style fixups so kernel user-copies hitting poison
return EFAULT / end the program"). It builds on `docs/MACHINE-CHECK.md`
(H.FAIL.8, ledger 4772/4795/4810) and `docs/MACHINE-CHECK-AMD.md`, and
narrows the residual `MACHINE-CHECK.md` §6 and SAFETY-MANUAL AoU-36 name: "the
kernel consuming it -- a system call's copy, ... -- is a ring-0 machine check
and stops as FX-9005".

## 1. What is there today

A program that reads a poisoned page itself takes the machine check in ring
3: the frame is retired and the program gets `SIGBUS` `BUS_MCEERR_AR`
(`MACHINE-CHECK.md` §3.5). The same page passed to `write(2)` is read by the
kernel: `syscall/uaccess.rs` copies it through the direct map with the
space's lock held, the load consumes the poison in ring 0, and
`mce::on_kernel_stack` stops the machine (`decode::action` answers `Stop` for
an action-required record outside ring 3). One bad page in one program's
buffer takes the whole machine down -- which is what an overclocked memory
fault most often looks like from a system call's side.

Every product read of a program's memory goes through two sites in
`uaccess.rs`: `copy_from_user_through` (behind `copy_from_user` and
`copy_from_user_present`, and so every `get_*` helper and the native ABI's
copies) and `copy_cstr_from_user`. Nothing else in the item reads a user page
for its contents (`syscall/memory.rs`'s `with_present_page` only maintains
caches). Two of their callers matter here (ledger 5216 (a), (b)):
`/proc/<pid>/environ` (`fs/procfs/debug.rs`) reads **another** process's
space, and futex reads its word with `copy_from_user_present` under its
bucket's spin lock (`syscall/futex.rs`).

## 2. What it does

A system call whose read of the calling program's memory consumes poison
ends as if that page were unmapped -- the call fails with `EFAULT` (or a
native call with its fault status), having copied nothing past the poison --
and the program gets `SIGBUS` `BUS_MCEERR_AR` with `si_addr` the page's user
address, as a ring-3 consumption would have given it. The frame is retired.
The kernel goes on.

x86-64 only, Intel and AMD, under the rules that already let a ring-3
consumption recover (`ferrix.mce.recover` on, `MCG_SER_P` on Intel, SUCCOR
on AMD). AArch64 and ARMv7-A have no RAS support in the item and keep their
behaviour; their copies are unchanged.

## 3. The mechanism

### 3.1 One copy instruction the machine check knows

`arch::x86_64::copy_mc(dst, src, len) -> Copied` (`global_asm!`, new
`arch/x86_64/copy_mc.rs`) copies with a single `rep movsb` at a symbol of its
own, `ferrix_copy_mc_movs`, and answers `Copied { left, page }` in `rax:rdx`
(a 16-byte return): `left = 0, page = 0` when it finished. The routine is:
set up `rdi`, `rsi`, `rcx`; `ferrix_copy_mc_movs: rep movsb`; `xor edx, edx`;
`ferrix_copy_mc_out: mov rax, rcx; ret`.

`rep movsb` is interruptible by architecture: on any event between its
iterations `rcx` holds the bytes not yet copied and `rip` the instruction
itself, so a machine check taken on one of its loads finds `rip ==
ferrix_copy_mc_movs` and `rcx` the remainder. It is the instruction Linux's
`copy_user` recovers on the same processors (`EX_TYPE_UACCESS`,
`MCE_IN_KERNEL_COPYIN`), and the one the compiler already emits for the
copies it replaces (the kernel is built without SSE), so the fast path costs
nothing.

The routine writes only `dst`, a kernel buffer, holds no lock and calls
nothing; stopping it at its `rep movsb` and returning leaves the first
`len - left` bytes written and nothing else changed.

AArch64 and ARMv7-A: `arch::copy_mc` is `copy_from_slice` and answers
`left = 0`.

### 3.2 The decision in ring 0

`mce::on_kernel_stack` gains one input: whether the frame's `rip` is
`ferrix_copy_mc_movs` exactly, the relocated symbol (a comparison, no lock).
The decision's `user: bool` becomes a context, `Ring3`, `Copy` or `Kernel`.

`Recover` needs every one of these, and anything short of it is today's ring-0
answer (`Stop`, or `Defer` / `Unaffected` / `Leave` for the classes that
already allow them) (ledger 5216 (1), (2)):

* the context is `Copy` and the frame's `rcx` is above 0 (at 0 the movs
  loaded nothing);
* the record's page equals the physical page of the frame's `rsi`, the next
  byte the movs loads, found by direct-map arithmetic (an `rsi` outside the
  direct map stops) -- so poison in the kernel's destination buffer, or in
  the copy's own code, never recovers;
* Intel (`decode::action`): class `Srar`, MCA code the data load's (`0x0134`
  under Linux's mask, as its recoverable-kernel row), `EIPV` and `RIPV`
  set, a usable address;
* AMD (`decode::amd_action`): class `AmdAr` with `EIPV` and `RIPV` set, a
  usable and cut address, `TCC` clear, under SUCCOR -- ring 3's rules;
* `fold`: `Recover` ranks with `Signal`; two consumptions in one machine
  check stop.

An Intel bystander, whose `rip` might sit at the movs, has `EIPV` clear and
stays `Unaffected`.

On `Recover` the handler writes the frame: `rip` to `ferrix_copy_mc_out`
(which returns `rcx`, the remainder, untouched by the check) and `rdx` to the
record's page with bit 0 set, so a page 0 still reads as poisoned. Then, as
from ring 3, it clears that bank, defers the action-optional ones, counts the
check recovered and at the copy, clears `MCG_STATUS` last and returns. No
lock, no allocation, no print (`MACHINE-CHECK.md` §3.3's rules for the IST
path hold). `ferrix.mce.recover=off` stops here as everywhere.

### 3.3 The copy's caller: nothing under a lock

`uaccess`'s two read closures call `arch::copy_mc`. A `Copied` with `page !=
0` makes the closure answer the page; `uaccess` checks it against the chunk's
source frame (the one `with_page` translated) and stops as FX-9005 on a
mismatch, then:

1. writes the poison into the current task's **poison record**, one atomic
   word set with no lock: the page, whether to signal (the space is the
   current task's), and the user page of `at`. A record already full (a
   second poisoned copy in one call) stops as FX-9005: fail closed;
2. answers `UserError::Poisoned`, a new variant. The copy reports no partial
   count and its destination is never used (`copy_cstr_from_user` truncates
   `out`). Its caller treats it as a fault at that page: most calls fail; a
   write that copies page by page and has already stored the pages before
   the poisoned one reports their count, as it does for any fault there and
   as Linux's does (`test-mce-copy` sees the memfd write end exactly at the
   poisoned page's offset).

Nothing is retired or signalled there: `copy_from_user_present` runs under
futex's bucket lock, and retiring takes the frame allocator's locks and
prints (ledger 5216 (3)). The record is acted on by the core's return to ring
3 (`trap.rs`, before `return_to_user`, both personalities), with no lock held:
retire the frame (`mce::retire`, `mce: frame <phys> retired`), then, if
marked, raise `SIGBUS` `BUS_MCEERR_AR` at the user page with `si_addr_lsb`
12, exactly as ring 3's `RIPV`-set path does: through the personality's
`fault_signal` (`force_held`), so a handler runs, and a blocked or ignored
`SIGBUS` is reset and ends the program; never `fault_end` (ledger 5216 (5)).
A native process, for which the personality has no signal, gets the call's
failure alone.

**Another process's space** (`/proc/<pid>/environ`, ledger 5216 (4)): the
record is not marked; the frame is retired and the read ends short at the
poisoned page; neither the reader nor the target is signalled (L.syscall.26).

**Every match on `Poisoned`** answers as `Fault` does, and the code review
lists them (ledger 5216 (6)): `fd.rs` and `memfd.rs` give `EFAULT` (never
`ENAMETOOLONG` or `EINVAL`), `native.rs` the fault status, `load.rs`'s
`LoadError::Copy` carries it to `launch.rs`, `signal_frame.rs` makes it a bad
frame, procfs's environ ends the read.

**`copy_cstr_from_user`** (ledger 5216 (7)) copies its span in 64-byte pieces
into the reserved room of `out` and scans each piece as it lands, stopping at
the piece that holds the terminator, so it reads no further than the cache
line holding it; any `set_len` is a traced unsafe.

### 3.4 Counts

`/proc/ferrix-mce` gains `copies` per processor: machine checks recovered at
the copy (also counted in `recovered`), traced to L.x86_64.246.

## 4. What it does not do

* The copy-on-write copy (a write fault, or a system call's write through
  `copy_to_user`, breaking a shared page) reads the old frame with an
  ordinary copy, and still stops on poison: a follow-up, through the same
  routine, with `SIGBUS` to the writer (Linux's `copy_mc_user_highpage`).
* Page-cache reads (`read(2)` of a file whose cached page is poisoned) are
  item 3 (drop and re-read a clean page).
* `copy_to_user`'s source is kernel memory: poison there is the kernel's own
  data, and stops.
* A device's DMA, Arm RAS, CMCI: as `MACHINE-CHECK.md` §6.
* Under QEMU the poison is not in memory: the injected record is believed
  and the copy restarts nowhere, so what the gate shows is the decision,
  the fixup and the caller's path, not a real load's behaviour. A
  `docs/BACKLOG.md` row asks for a hardware run (EINJ or equivalent).

## 5. Requirements

To reserve before code, in one docs-only landing (all free on origin/develop):

* **H.FAIL.10**: a system call's read of a program's memory that consumes
  poison shall end with the call failing, the frame retired and, where the
  memory is the caller's own and it has signals, `SIGBUS` `BUS_MCEERR_AR` at
  the page; the kernel shall resume only at the copy's own exit, never inside
  the copy past the poison, and stop in every other ring-0 case it stops in
  today.
* **L.x86_64.244**: `copy_mc` copies `len` bytes with one `rep movsb` at
  `ferrix_copy_mc_movs` and answers `left` 0 and `page` 0; from
  `ferrix_copy_mc_out` it answers `rcx` and `rdx`.
* **L.x86_64.245**: the decision's `Copy` context, `Recover` exactly under
  §3.2's conditions (the relocated symbol, `rcx > 0`, the page of `rsi`, the
  vendor's rules); everything else as `Kernel`.
* **L.x86_64.246**: on `Recover` the ring-0 handler moves `rip` to
  `ferrix_copy_mc_out`, writes the page with bit 0 into `rdx`, clears that
  bank, defers the rest, counts `copies` and clears `MCG_STATUS` last, with no
  lock, allocation or print.
* **L.syscall.25**: `uaccess`'s reads go through `arch::copy_mc`; a poisoned
  copy of the current task's own memory is checked against its source frame,
  recorded without a lock, fails as `Poisoned`, and at the return to ring 3
  retires the frame and raises ring 3's `RIPV`-set `SIGBUS`; a second in one
  call stops; `copy_cstr_from_user` reads no further than the terminator's
  64-byte piece.
* **L.syscall.26**: a poisoned copy of another process's space retires the
  frame and fails, signalling neither process.
* L.x86_64.230 (the counts) is updated for `copies`.

## 6. How it is checked

* **Host tests** (`decode.rs`'s, pure): the `Copy` rows of both vendors --
  each of `EIPV`, `RIPV`, the usable address, the MCA code, `TCC`, SUCCOR,
  `rcx > 0` and the page match removed in turn stops; `Kernel` and `Ring3`
  unchanged; `fold` with `Recover`.
* **Stage check** (x86-64, `mce/check.rs`), on a KASLR boot: `copy_mc` over
  lengths 0 to 9000 at every source and destination alignment 0-7 copies
  exactly and answers `(0, 0)`; the recovery applied to a `TrapFrame` the
  check builds (`rip` the relocated `ferrix_copy_mc_movs`, records it
  supplies; the decision is pure) comes back with `rip ==
  ferrix_copy_mc_out` and `rdx == page | 1`, and with another page, `rcx` 0
  or `rip` one byte off, stops; the poison record's set, full and take.
* **Build evidence**: xtask's disassembly pinned from `ferrix_copy_mc_movs`
  to `ferrix_copy_mc_out`: the one `rep movsb`, `rcx` untouched between, no
  other memory access, no call. That the paranoid exit really restores `rip`
  and `rdx` from the frame is shown by `test-mce-copy`.
* **`test-mce-copy`** (x86-64, KVM and TCG, and `--smp 2`; the gate alone
  boots `nokaslr`, so the symbol's address is the ELF's): a victim writes a
  1 MiB buffer to a memfd in a loop (a file on the root spent its time in the
  disk's device work) and prints the buffer's range. The modes use AMD's
  records: under KVM on the AMD host the guest has `SUCCOR`, so the recovery
  itself is shown, where Intel's model has no `MCG_SER_P`;
  `test-mce-copy-intel` gives Intel's records (recovered where `MCG_SER_P` is
  reported, the fail-closed stop where not, and the bystander). TCG runs a
  `rep movsb` in one step -- no stop ever finds it partway -- and reports no
  recovery, so under TCG the gate injects at the first stop in ring 0 and
  requires the stop. xtask
  stops the machine until a processor's `rip` is `ferrix_copy_mc_movs` and its
  `rsi`, translated on that processor (`gva2gpa`), is a page of the victim's
  buffer; otherwise it resumes and retries, the tries bounded and reported.
  It injects an action-required data-load record with `EIPV|RIPV` at that
  page into that processor and resumes. The victim must see `write` fail
  `EFAULT` and its handler get `SIGBUS` code 4 at the user page matching the
  injected frame (or end short exactly at that page, as above); the kernel
  must say `mce: frame <that page> retired` and go on (the victim exits 0
  after writing a fresh buffer). AMD under KVM as
  `test-mce-amd-srar` does (TCG drops SUCCOR: the fail-closed stop).
* **Modes** (each must stop as FX-9005 unless said): `RIPV` clear at the
  copy; `EIPV` clear at the copy with `RIPV` set -- on Intel the signature of
  another processor's consumption, which H.FAIL.8 leaves alone
  (`Unaffected`): nothing is recovered or retired, the copy goes on and the
  kernel with it (`test-mce-copy-bystander`; on AMD the same flags stop, as
  the host rows say); the record at another page than `rsi`'s;
  `ferrix.mce.recover=off`; AMD with `TCC` set (KVM); `rip` in the kernel
  outside the copy; and `/proc/<pid>/environ` read while the target's page is
  injected -- the read ends short, the frame is retired, no process
  signalled, the kernel goes on.
* **Negative controls** (`gate.sh control`, each FIRED): the `rip` test
  removed; the page test removed (the other-page mode recovers); the fixup's
  `rip` write removed (the copy resumes, `write` succeeds); the signal not
  raised; the frame not retired; the record's "another space" test removed
  (environ's mode signals).

## 7. Docs

SAFETY-MANUAL: around "every other machine check halts" the copy exception;
AoU-36: the system call's copy leaves the residual list, recovered only under
`ferrix.mce.recover`, x86-64 only; VULNERABILITY-ANALYSIS T.EXHAUST path 8:
the same, and that a program can now survive feeding the kernel a poisoned
buffer; MEMORY-AND-TIMING: the machine-check path adds one comparison, the
page test and two frame writes, and the record is acted on at the return to
ring 3 with no lock; `MACHINE-CHECK.md` §6 updated; a BACKLOG row for the
hardware run.

## 8. Where it stands

* Design v1 (8bbcb8ab4) reviewed 2026-10-30, ledger 5216: **OK IF (1)-(12)**.
  Five statements of v1 were wrong (environ reads another space; futex holds
  a spin lock around a copy; `fault_signal` is forced; `Poisoned` did not
  exist; AMD needs `EIPV`). v2 carries (1) the page and `rcx` tests and the
  data-load code (§3.2), (2) AMD's rules (§3.2), (3) the lock-free record
  acted on at the return to ring 3 (§3.3), (4) environ (§3.3, L.syscall.26),
  (5) ring 3's `RIPV`-set signal (§3.3), (6) every match on `Poisoned`
  (§3.3), (7) `copy_cstr` in 64-byte pieces (§3.3), (8) the targeted
  injection and `--smp 2` (§6), (9) the modes and controls (§6), (10) the ids
  (§5), (11) the docs (§7), (12) the pinned range (§6). One reading of (9):
  Intel's `EIPV` clear with `RIPV` set at the copy is the bystander's
  signature and stays `Unaffected` (the review's own answer on bystanders),
  so its mode requires nothing done rather than a stop; with both clear it
  stops (the `RIPV`-clear mode).
* Code (feature/mce-copy, 2026-10-30): `Poisoned` needed no new arm --
  `fd.rs` and `memfd.rs` match `Fault` alone and send every other error to
  `EFAULT`, and `native.rs`, `load.rs`, `signal_frame.rs` and environ map
  any error as a fault or a short read; the gate's modes live in
  `tools/common/xtask/src/mce_copy.rs`, the guest in `src/tests/mce-copy`.
* Code reviewed 2026-10-30 at 260d35b97, ledger 5283: **OK IF (1)-(5)**. (1)
  FOUND g -- `rt_sigreturn`'s frame read and a seccomp filter's read copy
  outside `trap::system_call`'s way out -- closed by (b): a poisoned copy is
  recorded only inside a program's system call (`uaccess::poison_action`,
  the processor's `IN_CALL`), and stops as FX-9005 anywhere else, as every
  ring-0 consumption did before (`sched/check.rs`'s `poison_record`, with its
  control). (3) The TCG rows are fail-closed rows only, and on this AMD
  host Intel's recovery is shown by stage 3's tables and frame; the
  end-to-end evidence is the KVM rows with AMD's records. The controls and
  the guard each checks: C1 the rip test (inverted, the copy mode stops), C2
  the page test (removed, the other-page mode recovers; and the outside mode
  still stops on that tree, the rip test holding alone), C3 the fixup's rip
  write, C4 the signal, C5 the retirement, C6 the another-space test, C7
  the in-call test, C8 the caller's re-check of the page (removed on C2b's
  tree, where the handler's page test is gone too: the other-page mode then
  goes on). C1 and C3 fire at stage 3's copy tables and fixup frame, before
  any injection; the stop modes require the machine check's own stop, so
  that the caller's re-check cannot hide a handler that recovered where it
  must not (which is what C2 shows).
