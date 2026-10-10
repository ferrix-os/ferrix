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

`test-wine` attaches the volume at `/data` under `snapshot=on` and boots
twice. The first boot is a shell: it makes a prefix in `/tmp` with
`wineboot -i` and requires `cmd /c ver` to print `Microsoft Windows 10.0`.
The second is hyprix with yserver on `:0`, as `test-xwindow` boots it: the
same two steps, then `notepad`, which must become a window of hyprix's
titled "Untitled - Notepad" with its white page on the screen
(`build/x86_64/wine-notepad.ppm`). It needs no network. It attaches a
volume, so it runs on demand, as `test-yserver` does, and is not in
`cargo xtask check`.

The guest has 6 GiB: the prefix is about 800 MB of files, on tmpfs.

What shows: notepad's menu bar, its page with the caret, the scroll bar and
"Ln 1, Col 1", drawn by Wine's X driver in software, top left in a tile
that fills the screen.

## 2. The pieces

| Piece | Where | What |
|---|---|---|
| The volume | `tools/common/fetch/fetch-wine.sh` | Debian's `wine64`, `libwine` and `wine`, pinned by SHA-256, unpacked over yserver's tree, with glibc, libunwind, FreeType, fontconfig, Xlib and the X extension libraries, `libz-mingw-w64`'s zlib marked as a Wine builtin (what `libwine`'s postinst does), Wine's and Liberation's fonts, and `strace` |
| The shell boot | `tools/common/xtask/src/wine.rs` | zinc as init, busybox, the links Debian's paths need, and the script |
| The desktop boot | `tools/common/xtask/src/compositor/wine.rs` | hyprix, yserver started as the `--everything` desktop starts it, and the script |

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

**Checks**, all in the boot's self-checks:

| Line | What | Fails when |
|---|---|---|
| `prctl` | a kernel and a non-canonical `GS` base refused, an unknown request `EINVAL`, an unwritable address `EFAULT`, a base set and read back through `%gs:0`, `ARCH_GET_GS` and `ARCH_GET_FS`; `CR4.FSGSBASE` clear and a program's `WRGSBASE` ended with `SIGILL` | the refusal is dropped: exit 2, "accepted a kernel or non-canonical address as the GS base". On a processor with `FSGSBASE` (`FERRIX_X86_CPU=…,+fsgsbase`) and the bit set: "CR4.FSGSBASE is set" |
| `fsbase` | two programs with different `GS` bases trade one processor 10,000 times each, each reading its own word | the call writes the MSR and not the record: "did not each read their own GS base" |

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

`wineboot`, `cmd /c ver` and notepad's window pass (`test-wine`, KVM,
2026-10-10). What a session picking Wine up needs first:

**One workaround.**

| Workaround | Why | Real fix | Owner |
|---|---|---|---|
| The prefix is made with no `DISPLAY`, and `UseXRandR` is `N` in `HKCU\Software\Wine\X11 Driver` before a program gets one | On yserver's RandR 1.5, Wine's X driver registers its XRandR 1.4 display handler and then finds no monitor: `err:system:lock_display_devices Failed to read display config`, over and over. A window's monitor DPI is then 0, wineserver divides by it (`map_dpi_point` in `server/window.c`) and ends with `SIGFPE`, and every Wine program waits on it for ever | whatever `xrandr14_get_gpus`, `_get_adapters` or `_get_monitors` (`dlls/winex11.drv/xrandr.c`) gets from yserver on the Wayland backend that it does not get from Xorg: not found yet. yserver's nested backends report no RandR provider, which Wine has a path for, so the lead is the output and CRTC answers | open (yserver) |

**Leads, none of them in the kernel as far as is known.**

* With XRandR off, `explorer.exe` (Wine's desktop process) ends on an
  assertion in `dlls/winex11.drv/xvidmode.c:164`, `xf86vm_free_modes`:
  the XF86VidMode modes yserver lists are not what Wine's fallback
  expects. Notepad draws without it.
* Notepad draws at the size it asked for, about 720 by 510, and not at the
  982 by 726 of the tile hyprix gave its window, which X reports as the
  window's size. The rest of the tile is black.
* `wineboot` says `failed to open C:\windows\syswow64\rundll32.exe:
  c0000135`, which it does not say on Linux: a 32-bit Windows program does
  not start yet. Not looked into.
* A prefix on the btrfs volume ran out of space (`ENOSPC`) with 680 MB
  free; the gate uses tmpfs.
* `df` says tmpfs has used 0 of 3.0G with an 800 MB prefix on it.
* `wineboot` takes 27 s of guest time; on Linux, on the same host, a few.

**What the kernel gained for it.**

* `arch_prctl`'s `GS` base (§3).
* A 64-bit signal frame's `sigcontext` carries a fault's `trapno`, `err`
  and `cr2`, as a 32-bit one already did. Wine's `SIGSEGV` handler tells a
  page fault by `trapno` 14 and a write by `err`; with the three zero it
  logged `err:seh:segv_handler Got unexpected trap 0`, and `wineboot` took
  88 s of guest time.

Nothing else was needed for these three programs. In particular not
`futex_waitv`, `FUTEX_WAKE_OP`, `mlock`, `membarrier`, `ptrace`,
`process_vm_readv`, `modify_ldt` or `preadv`: none is in the trace of §4.
