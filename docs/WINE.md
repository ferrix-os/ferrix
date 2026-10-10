# Wine on Ferrix

Debian 13's 64-bit Wine (10.0), unchanged, on Ferrix under KVM. This is
stage 23's first step: a prefix made by `wineboot`, `cmd /c ver` answering,
and `notepad` drawing a window through yserver in software. No GPU is
involved.

## 1. Running it

```
tools/common/fetch/fetch-wine.sh        # once: the volume, about 2 GB
cargo xtask test-wine --accel kvm       # the gate
```

`test-wine` attaches the volume at `/data` under `snapshot=on`, makes a
prefix in `/tmp` with `wineboot -i`, and passes when `cmd /c ver` prints
`Microsoft Windows 10.0`. It needs no network. It attaches a volume, so it
runs on demand, as `test-yserver` does, and is not in `cargo xtask check`.

The guest has 6 GiB: the prefix is about 800 MB of files, on tmpfs.

## 2. The pieces

| Piece | Where | What |
|---|---|---|
| The volume | `tools/common/fetch/fetch-wine.sh` | Debian's `wine64`, `libwine` and `wine`, pinned by SHA-256, unpacked over yserver's tree, with glibc, libunwind, FreeType, fontconfig, Xlib and the X extension libraries, `libz-mingw-w64`'s zlib marked as a Wine builtin (what `libwine`'s postinst does), Wine's and Liberation's fonts, and `strace` |
| The boot | `tools/common/xtask/src/wine.rs` | zinc as init, busybox, the links Debian's paths need, and the script |

Wine is fetched, never vendored. The loader is started directly
(`/usr/lib/wine/wine64`) with `WINESERVER=/usr/lib/wine/wineserver64`:
Debian's `wine` and `wineserver` are shell scripts that choose between a
32-bit and a 64-bit build, and only the second is on the volume. 32-bit
Windows programs run in the 64-bit process (Wine's new WoW64 mode); there is
no 32-bit Wine.

## 3. The `GS` base

A 64-bit Windows program reads its thread block (the TEB) through `GS`.
Wine sets that base once for every thread it makes, with
`arch_prctl(ARCH_SET_GS)`, and reads the Unix side's thread pointer with
`ARCH_GET_FS`. On Linux, the trace of `wineboot`, `cmd` and `notepad` has
181 of each and 47 `ARCH_SET_FS`.

**What the kernel does.** `arch_prctl` answers four requests:

* `ARCH_SET_FS` and `ARCH_SET_GS` are answered at the `SYSCALL` entry,
  before interrupts open, as `ARCH_SET_FS` was. Each refuses an address
  that is not a user one with `EPERM`. `ARCH_SET_GS` loads a null `GS`
  selector if the register holds another (Linux does the same), writes the
  base to `IA32_KERNEL_GS_BASE`, where a program's base waits while the
  kernel runs, and writes it to the task's record.
* `ARCH_GET_FS` and `ARCH_GET_GS` write through a pointer of the program's.
  The entry does not answer them; the dispatcher does
  (`syscall::thread_area::sys_arch_prctl`), reading the base from the
  processor with interrupts masked (`arch::user_base`).

The context switch needed nothing new. Since `docs/OPAQUE-KERNEL.md` §9.8
(3b) the task's record holds both bases, `execve` zeroes them, a fork child
copies them, and the switch writes them at every switch to a task whose
selector is null. Until now every 64-bit program's `GS` record was zero.

**The rule this must keep.** The NMI, `#DB`, `#MC` and `#DF` entries cannot
tell from the saved `CS` whose `GS` is loaded, so they read `GS_BASE`: an
address with its sign bit set is the kernel's per-CPU record, anything else
is a program's (`arch/x86_64/paranoid.rs`). A program must therefore never
hold an upper-half `GS` base. Two things keep that:

1. `ARCH_SET_GS` refuses every address that is not a user one.
2. `CR4.FSGSBASE` is clear, so `WRGSBASE` in ring 3 is `#UD`. Until now
   nothing cleared it; the kernel took `CR4` as firmware left it.
   `cpu::forbid_user_base_writes` clears it on the boot processor, and the
   others copy that `CR4`.

**Why `FSGSBASE` stays off for programs.** With it on, a program writes any
canonical address to `GS_BASE` with one instruction, and the rule above is
gone. Linux has it on, and its paranoid entries then no longer trust `GS`
at all (`arch/x86/entry/entry_64.S` in 6.12): they read the processor's
number from a segment's limit, look the per-CPU base up, write it with
`WRGSBASE` and put the old base back on the way out. Without `FSGSBASE`
Linux keeps the rule Ferrix has, in its own words: "the kernel enforces
that negative GSBASE values indicate kernel GSBASE". Ferrix would need the same in all four entries
and in `load_user_gs`, and the switch would have to read both bases back at
every switch, which 3b removed for its cost. What it would buy Wine is
small. Wine uses the instructions only where `AT_HWCAP2` offers them
(Ferrix's is 0), and otherwise makes the calls above: one `ARCH_SET_GS`
per thread. Debian's Wine made a prefix and ran `cmd` on Ferrix this way.
A program that switches bases often (a Windows program in a tight loop of
system calls from 32-bit code) would pay a system call each time; none has
been measured yet.

**The fence that is not there (F-71).** The paranoid entries swap `GS` on
a branch, and a processor that mispredicts it runs the handler's first
loads through the wrong base for a few instructions. The ordinary
interrupt stub has an `lfence` after its conditional `swapgs`; these
entries have none, which the review of this change found
(`docs/certification/FINDINGS.md`, F-71, open). It was so before. What
this change adds is the range: the base a mispredicted entry reads through
was zero or below 4 GiB, and is now any user address the program chose,
never a kernel one. SMAP keeps ring 0 from reading a user page where the
processor has it.

**Checks**, all in the boot's self-checks, each with a negative control run
through `fleet/gate.sh control`:

| Line | Requirement | What | The control, and what it must say |
|---|---|---|---|
| `prctl`, first | L.x86_64.60 | a kernel thread pointer, and a kernel and a non-canonical `GS` base, refused with `EPERM`; an unknown request `EINVAL` | the `GS` refusal dropped: "arch_prctl accepted a kernel or non-canonical address as the GS base" |
| `prctl`, second | L.x86_64.173 | `ARCH_GET_FS` and `ARCH_GET_GS` into address 0 `EFAULT`; a `GS` base set and read back through `%gs:0` and `ARCH_GET_GS`, an `FS` base through `ARCH_GET_FS` | |
| `fsbase` | L.x86_64.9 | two programs with different `GS` bases trade one processor 10,000 times each, each reading its own word | the call writes the MSR and not the record, and, apart, the switch's write of the base removed: "did not each read their own GS base" |
| `gsbase` | L.x86_64.172 | `CR4.FSGSBASE` read clear at every processor's bring-up; where the processor has the feature, the bit set for the check is cleared again; a program's `WRGSBASE` is `SIGILL` on two processors | on `FERRIX_X86_CPU=…,+fsgsbase` with the clear removed: "CR4.FSGSBASE, set for the check, was not cleared again". The gates' `qemu64` has no FSGSBASE, and the line says the clear was not seen there |
| `debug`, second | L.x86_64.24, L.x86_64.25 | beside a program whose `GS` base is `0x7ffffffff000`: breakpoints on the `SYSCALL` stub's `swapgs` and `sysretq`, and NMIs taken in ring 3 on another processor, all find the kernel's `GS` | the paranoid entry deciding from the saved `CS` |
| `sigsegv` | L.x86_64.174 | a `SIGSEGV` handler reads trap 14, error code 6 and the address from its `sigcontext` (§5) | the three left zero: "did not say trap 14 for a page fault" |

**Reviewed** by wine-m1's certification consultant on 2026-10-10: OK IF
W1 to W10 (`~/.local/share/ferrix/cert-consultant/reviews.md`, line 668).
An internal review, not independent verification.

## 4. What Wine uses, against Ferrix

From `strace -f` of `wineboot -i`, `cmd /c ver` and `notepad` in the same
Debian tree on Linux 7.0 (2026-10-10; the traces are not kept in the
tree). 100 different system calls; two have no number in
`src/lib/proto/linux-abi/src/nr.rs`:

| Call | Count | Without it |
|---|---|---|
| `fadvise64` | 52 | glibc and Wine ignore the failure |
| `sendmmsg` | 8 | glibc's resolver falls back to `sendto` |

## 5. Where it stands

See the stage's file under `docs/roadmap/` for the list; this section is
what a session picking Wine up needs first.

* `wineboot` and `cmd /c ver` pass (`test-wine`).
* A 64-bit signal frame's `sigcontext` now carries a fault's `trapno`,
  `err` and `cr2`, as a 32-bit one already did. Wine's `SIGSEGV` handler
  tells a page fault by `trapno` 14 and a write by `err`; with the three
  zero it logged `err:seh:segv_handler Got unexpected trap 0`, and
  `wineboot` took 88 s of guest time where it now takes 27.
* `wineboot` still says `failed to open C:\windows\syswow64\rundll32.exe:
  c0000135`, which it does not say on Linux: a 32-bit Windows program does
  not start yet.
* A prefix on the btrfs volume ran out of space with 680 MB free; the gate
  uses tmpfs.
* `wineboot` takes 27 s of guest time; on Linux, on the same host, a few.
