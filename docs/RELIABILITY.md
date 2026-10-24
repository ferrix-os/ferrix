# Reliability

The quality gates in this tree are ported from the Starling workspace. This
document records what each one is for, and what changed in the port — because a
kernel can hold some of those gates more strictly than a userspace server, and
one of them not at all.

## The one gate that could not be inherited

Starling sets `unsafe_code = "deny"` workspace-wide and means it. A kernel
cannot: writing a page table entry, storing to an MMIO register and moving a CPU
system register *are* the program. Denying unsafe would be denying the project.

So the burden moved from the crate to the site. Unsafe is not forbidden here; it
is made expensive:

| Mechanism | What it requires |
|---|---|
| `clippy::undocumented_unsafe_blocks` (deny) | A `// SAFETY:` comment on every `unsafe` block |
| `clippy::missing_safety_doc` (deny) | A `# Safety` section on every `unsafe fn`, stating the caller's contract |
| `clippy::multiple_unsafe_ops_per_block` (deny) | One unsafe operation per block, so a `SAFETY:` comment makes one claim rather than covering a paragraph of them |
| `unsafe_op_in_unsafe_fn` (deny, and the edition-2024 default) | An `unsafe fn` body is not an unchecked region |
| `tools/common/check/check-unsafe-audit.py` | The same rules again, in CI, so a clippy release that softens a nursery lint cannot silently retire them |

The script also prints the unsafe-block count per crate on every run. That
number is not a gate — it is meant to be visible in a diff, so that unsafe
growing is something somebody noticed rather than something that happened.

## Panics

A panic in a userspace server is a 500 and a restart. In a kernel it is the
machine: there is no supervisor above us, and with `panic = "abort"` there is no
unwinding either. So the panicking constructs are denied in production code —
`panic!`, `unreachable!`, `unwrap`, `expect`, `indexing_slicing`,
`string_slice`, `panic_in_result_fn` — and the exemptions have to be argued:

```rust
#[expect(clippy::indexing_slicing, reason = "AUDIT: len checked at :212")]
```

Two rules, enforced by `tools/common/check/check-panic-audit.py`:

1. The reason begins `AUDIT:`, so it reads as an argument in the diff that adds
   it rather than as a way past the lint.
2. Exemptions use `#[expect]`, never `#[allow]`. `expect` fails the build once
   the lint stops firing, so a site refactored into safety loses its exemption
   instead of accumulating a stale one.

`.clippy.toml` exempts test bodies, where a failed assertion *should* panic with
a clear message.

### Failing in the kernel: `fatal!`

A self-check that fails in `kmain`, or a processor that never answers an
interrupt, has nowhere to return an error to. The only thing left to do is say
what happened and stop. So a fatal site in the kernel is one line that names
the catalog entry explaining the failure and says what went wrong there:

```rust
fatal!(catalog::STAGE3_TIMER, "stage 3 self-check failed: {problem}")
```

`fatal!` records the entry and panics, and `src/kernel/src/panic.rs` writes the
report:

```text
FERRIX-PANIC stage 2 self-check failed: the heap lost an allocation
  at        src/kernel/src/main.rs:742:5
  on        the boot processor, before any other was started
  stopped   this processor halts here; nothing will recover it
  trace     #0  0xffffffff80002428  __rustc::rust_begin_unwind+0x1b4
  trace     #1  0xffffffff800074fc  core::panicking::panic_fmt+0x28
  trace     #2  0xffffffff80002e54  ferrix_kernel_core::mm::check::memory_check+0x20
  trace     #3  0xffffffff80003268  ferrix_kernel::kmain+0x414
  trace     #4  0xffffffff800000fc  _start+0xfc
```

A frame's name starts with the crate that holds the function: since the
kernel split (`docs/KERNEL-CRATES.md`) `ferrix_kernel_core::`,
`ferrix_kernel_item::` or `ferrix_kernel_load::` by the function's ring, and
`ferrix_kernel::` for the binary's `main.rs`.

The marker and the message share the first line, because that is the line the
boot test judges on. What follows is context, which `xtask` keeps reading for a
moment so that the log holds the whole report.

The report masks interrupts on its own processor and then asks every other one
to stop, through the inter-processor interrupt they already answer. Otherwise
the machine keeps running around a failure it cannot survive, and the other
processors' output lands in the middle of the report.

The backtrace follows the frame-pointer chain, which is why every kernel target
is built with `force-frame-pointers`. Every step of the walk is checked before
it is trusted. A frame has to be aligned and above the last one. It has to be
within one stack's worth of where the walk began, and on a mapped page, read
from the page tables without taking their lock. A return address has to fall
inside the kernel image. Anything else ends the walk, because a fault inside
the panic handler is a report nobody sees. The kernel prints only addresses. It
carries no symbol table, so `xtask` names each frame from the ELF it booted, in
the terminal and in the log.

`panic!` itself stays denied in the kernel, as everywhere. The one inside
`fatal!` is out of clippy's sight, and that is the design: a fatal site cannot be
written without choosing the entry that explains it. `unwrap`, `expect`,
`unreachable!` and indexing, which panic without saying why, stay denied too.
Nothing in `src/lib/` gets `fatal!`. Code that can be a pure function of its input
returns its errors, and the kernel decides which of them are fatal.

The catalog is `src/kernel/src/panic/catalog.rs`. Each entry has a stable code, a
title, what the failed check establishes, the likely causes, and where to read
more, and the report prints it below the trace. `tools/common/gen/gen-panic-catalog.py`
checks the catalog and renders it into `docs/generated/PANICS.md`, so an
explanation can be read without the machine that stopped. `cargo xtask check`
fails when that document is stale.

The trap path's fatal reports open with their own lines, the saved registers
after the marker. Then they end the way a panic does: the processor, the others
stopped, the trace, the catalog entry, and the screen.

When firmware left a framebuffer, the report is drawn on it last, from the
console's own recent output, so the screen and the serial log cannot disagree.
It is the kernel's second output device, and `docs/ARCHITECTURE.md` names it as
an exception beside the serial port.

Beside the text the screen draws a QR code of the report as plain text,
because a photo of a screen is how a report leaves a machine with no cable, and
a photo of a code keeps every character where a photo of text loses some. The
text keeps 80 columns, as wide as the report's lines run, and the code gets
the width left beside it. The code holds as much of the report as fits at two
pixels or more a module, cut at the end of a line, from the marker line down.
When the report is taller than the screen, the text starts at the marker line
and is cut at the bottom. On a small screen both lose the end of the
explanation first, never the headline. The encoder is `src/lib/kernel/qr`, a port of Linux's `drm_panic_qr.rs` under
its MIT licence. It allocates nothing, and its tests read every symbol back
through an independent decoder.

All of it can be reached on purpose, as on Linux: `echo c > /proc/sysrq-trigger`
from the shell panics the kernel through `fatal!` with FX-0850, whose
explanation says nothing is wrong. Linux looks only at the first byte written
there, and so does Ferrix; any other byte is accepted and ignored.

## Overflow checks are on in release

This inverts Starling's choice, deliberately and for the opposite reason.
Starling turns them off in release because with `panic = "abort"` an overflow
inside a dependency becomes a hard production abort, and for a server aiming at
years of uptime a wrong number beats an outage.

Here the calculus reverses. A wrapped frame number, page count or physical
address does not produce a wrong answer — it produces a write to the wrong
physical page, and the symptom appears in an unrelated subsystem minutes later.
A panic that names the line is strictly better than a machine that limps.

## What the tests can actually reach

Nothing in `src/kernel/` can be run by `cargo test`. Miri cannot interpret a
privileged instruction, and a fuzzer cannot drive a page-fault handler. That is
the argument for `src/lib/`:

> Anything expressible as a pure function of bytes is written as one, in
> `src/lib/`, where `cargo test`, Miri and the fuzzers all reach it.

ELF parsing, cpio unpacking, btrfs item decoding, seccomp BPF evaluation,
page-table index arithmetic, buddy-order maths, the VMA interval tree — all of
it is host-testable by construction, and `tools/common/check/check-crate-layering.sh` keeps
the layering that makes it so.

What is left in `src/kernel/` is the part that genuinely needs a CPU, and it is
covered by the QEMU boot test instead: every roadmap stage's exit criterion
becomes a boot test and stays in CI forever after.

## Miri

Miri interprets the host tests of the `src/lib/` crates the kernel leans on
hardest, checking every borrow, index and pointer against the allocation it
came from. CI's Miri job names each crate in its own step, with the reason it
is there, split over three runners; `cargo xtask miri` runs the same list
locally, the crates side by side and nothing else built (`cargo xtask check
--miri` runs it after the whole gate), and an xtask test fails when the list
and the workflow disagree. On 2026-09-28 the local run took four minutes,
each interpreter under half a gigabyte.

A test that is slow under interpretation is first made cheap: Miri charges
for every step the test itself takes, so a loop that builds or compares its
data a byte at a time can cost more than the code under test, and a slice
copy or comparison costs one step. The pipe's model test went from hours to
seconds that way and now runs its full length under Miri. What is still slow
runs shorter under `cfg(miri)` rather than being skipped: the frame
allocator's random workload runs 2,000 of its 20,000 steps there. A test that
cannot run under Miri at all gets `#[cfg_attr(miri, ignore)]` with the reason
beside it, never a blanket skip of the crate.

## Fuzzing

The surfaces that parse bytes somebody else chose are small in a kernel, but
they are the sharp end. An ELF header comes from whatever the user just ran; a
cpio archive from an initramfs assembled elsewhere; a btrfs extent item from a
disk that may be corrupt or hostile. All three are parsed with the MMU on and
nothing above us.

CI replays the committed corpus first and searches second. The replay is what
makes it a regression test: an input that crashed once fails again in seconds,
rather than waiting for the fuzzer to rediscover it inside a timebox.

Not panicking is the floor a target starts from, never the property it
checks. A parser's target asserts that every slice it hands back lies inside
the input, that every walk ends within a bound the input's length sets, and
that the parser agrees with something independent of it: a second walk
written from the format's specification, or a round trip through a writer.
The seeds are small and real — the archive the boot image carries, what a
foreign tool wrote — and each is committed with the script that made it, under
`scripts/seed-*-fuzz-corpus.py`.

## The boot test

The gate that answers the question the others cannot. Everything else checks the
source; this one boots it — firmware to loader to kernel — on every
architecture, and fails the build if the machine does not come up. An OS that
compiles and does not boot is not a passing build.

### What booting under an emulator cannot tell you

It runs under `tcg`, QEMU's interpreter, because that is what `CI` has and
because an interpreter is reproducible. The cost is a blind spot with a sharp
edge: `tcg` resolves every access through the page tables as it finds them, so
it has no `TLB` worth the name, and a stale translation cannot be stale in a
cache that does not exist. No amount of booting under it will find a missing
invalidation.

That is not a hypothetical. `arch::flush_tlb` on x86-64 spared global entries —
which is nearly every mapping the kernel makes — for four stages, through a
boot test that passed every time. The first boot under a hardware accelerator
failed three self-checks.

So `--accel auto` exists: it boots on the real processor, with the real `MMU`
and the real `TLB`, using whatever the host offers (`whpx` on Windows, `kvm` on
Linux, `hvf` on macOS) and falling back to `tcg` when there is nothing. It is
not the default, because reproducibility is what a gate is for and because the
guest must match the host's architecture to be accelerated at all. It is what
to run before believing a change to page tables, invalidation or `SMP`.
