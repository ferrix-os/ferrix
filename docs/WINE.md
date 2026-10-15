# Wine on Ferrix

Debian 13's 64-bit Wine (10.0), unchanged, on Ferrix under KVM. This is
stage 23's first step: a prefix made by `wineboot`, `cmd /c ver` answering
from the 64-bit `cmd` and from the 32-bit one, and `notepad` drawing a
window through yserver in software. No GPU is involved.

## 1. Running it

```
tools/common/fetch/fetch-wine.sh        # once: the volume, about 2.6 GB
cargo xtask test-wine --accel kvm       # the gate
```

`test-wine` attaches the volume at `/data` under `snapshot=on` and boots
twice. The first boot is a shell: it makes a prefix in `/tmp` with
`wineboot -i` and requires `cmd /c ver` to print `Microsoft Windows 10.0`,
from `C:\windows\system32\cmd.exe` and again from the 32-bit
`C:\windows\syswow64\cmd.exe` (§7).
The second is hyprix with yserver on `:0`, as `test-xwindow` boots it: the
same two steps, then `notepad`, which must become a window of hyprix's
titled "Untitled - Notepad" whose white page fills the tile hyprix gave it
(70% of the screen white; `build/x86_64/wine-notepad.ppm`). Then the gate
types into it through QEMU's tablet and keyboard, as `test-xwindow` does
into xev: a click into the page, the keys of `hello ferrix`, `Ctrl+A`,
`Ctrl+C`, a double click on `hello` and `Ctrl+C`. The line must be on the
screen (`build/x86_64/wine-notepad-typed.ppm`), and X's clipboard must hold
`hello ferrix` and then `hello`: the keys arrived as the characters they
are, and the double click reached the word where it is drawn. It needs no
network.

**To watch it.** On x86-64, with the volume made:

```
cargo xtask run-compositor --wine
```

The desktop starts with a terminal, and after about a minute (yserver's
start, `wineboot` making a prefix in `/tmp`) notepad tiles beside it.
Click into its page and type; `Ctrl+A` and `Ctrl+C` copy through X's
clipboard; the File, Edit and Help menus open. `SUPER+N` starts another
notepad, `SUPER+Q` closes the focused window. What Wine says is in
`/tmp/wine/*.log`, readable from the terminal. `--wine` takes the place of
`--chrome` and `--everything`, whose volume is the one data disk otherwise.

**Where it runs.** On demand, as `test-yserver` and `test-xwindow` do: it
is not in `cargo xtask check`, not in CI, and not in the batch's `full`
profile (`batch.sh profile`), so no landing runs it unasked. A landing that
changes what Wine uses of the kernel, yserver or hyprix adds it with
`batch.sh join … --gate FILE`, the file holding `test-wine --arch x86_64
--accel kvm`. The gate never downloads: with no `wine.img` under
`~/.local/share/ferrix/wine` (or `FERRIX_WINE_VOLUME`) it stops at once and
names the fetch script. On nazuna the volume is there since 2026-10-10,
with the packages it was made from in `pool/` beside it, so making it
again downloads nothing. It needs yserver's tree
(`~/.local/share/ferrix/yserver/tree`), which `fetch-yserver.sh` makes;
`fetch-wine.sh` makes it again when it holds another commit than the one
`fetch-yserver.sh` pins. The volume records the yserver it carries
(`yserver.commit` beside `wine.img`), and `test-wine`, `test-dxvk` and
`run-compositor --wine` refuse one that is not the pin's, so **a pin move
means running `fetch-wine.sh` again** after `fetch-yserver.sh`.

The guest has 6 GiB: the prefix is 768 MB of files, on tmpfs. On the
volume it takes about 1.1 GB, which is why the volume has 2 GiB more than
its tree (§5).

What shows: notepad's menu bar, its page with the caret, the scroll bar and
"Ln 1, Col 1", drawn by Wine's X driver in software, in the tile hyprix
gives it.

## 2. The pieces

| Piece | Where | What |
|---|---|---|
| The volume | `tools/common/fetch/fetch-wine.sh` | Debian's `wine64`, `libwine` and `wine`, pinned by SHA-256, unpacked over yserver's tree, the 32-bit Windows DLLs from the i386 `libwine` (§7), with glibc, libunwind, FreeType, fontconfig, Xlib and the X extension libraries, `libz-mingw-w64`'s zlib marked as a Wine builtin (what `libwine`'s postinst does), Wine's and Liberation's fonts, and `strace` |
| The shell boot | `tools/common/xtask/src/wine.rs` | zinc as init, busybox, the links Debian's paths need, and the script |
| The desktop boot | `tools/common/xtask/src/compositor/wine.rs` | hyprix, yserver started as the `--everything` desktop starts it, the script, and the input the gate types; also `run-compositor --wine`'s scripts |
| yserver | the fork `ferrix-os/yserver`, pinned in `tools/common/fetch/fetch-yserver.sh` | the X server; §5 says what Wine needed of it |
| DXVK | `tools/common/fetch/fetch-dxvk.sh`, `tools/common/xtask/src/dxvk.rs` | DXVK's release and `test-dxvk` (§6) |
| The Unity game | `tools/common/fetch/fetch-unity.sh`, `tools/common/xtask/src/compositor/unity.rs` | Spaceship Demo on its own volume and `test-unity` (§9) |

Wine is fetched, never vendored. The loader is started directly
(`/usr/lib/wine/wine64`) with `WINESERVER=/usr/lib/wine/wineserver64`:
Debian's `wine` and `wineserver` are shell scripts that choose between a
32-bit and a 64-bit build, and only the second is on the volume. 32-bit
Windows programs run in the 64-bit process (Wine's new WoW64 mode, §7);
there is no 32-bit Wine.

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

`wineboot`, `cmd /c ver`, notepad's window at its tile's size, and a line
typed into it and copied out of it pass (`test-wine`, KVM, 2026-10-12).
There is no workaround left. What a session picking Wine up needs first:

**What yserver had to answer as Xorg and Xwayland do (fixed 2026-10-12).**
Both are in the fork at 662c61f3, which `fetch-yserver.sh` pins:

| What | Why it mattered | The fix |
|---|---|---|
| RandR's providers | yserver's Wayland backend listed its Vulkan renderer as the one RandR provider (`vulkan-unverified`, Source Output) with no CRTC and no output. Wine's X driver takes a provider's own outputs when there is a provider (`xrandr14_get_adapters`, `dlls/winex11.drv/xrandr.c`), found none and registered no monitor (`Failed to read display config`, over and over). A window's monitor DPI was then 0, wineserver divided by it (`map_dpi_point`, `server/window.c`) and ended with `SIGFPE`. Until then the prefix was made with no `DISPLAY` and `UseXRandR=N`, and with XRandR off `explorer.exe` asserted in `xvidmode.c:164` | The backend lists no provider, as Xwayland does (`Providers: number : 0` on Xwayland 24.1). Wine then reads the screen's resources: `WAYLAND-1`, its CRTC and its mode, at 96 DPI (`xdpyinfo`: 1024x768, 271x203 mm). Fork commit 6d66bff9 |
| `WM_STATE` | yserver announces itself as the window manager (`_NET_SUPPORTING_WM_CHECK`, for Steam), but set no `WM_STATE` when a client mapped a top-level. Wine's X driver waits for that `PropertyNotify`, with a serial no older than its `MapWindow`'s, before it takes any size its window is given (`window_update_client_config`, `dlls/winex11.drv/window.c`): notepad drew at the 721 by 512 it asked for in a 982 by 726 tile, the rest black | The server sets `WM_STATE` `NormalState` within a client's `MapWindow` of a root child that is not override-redirect, and `WithdrawnState` within its `UnmapWindow`, while the window manager it names is its own. Fork commit 662c61f3, with a unit test |

Controls, each with volumes made from the older commit: at 6d66bff9 (no
`WM_STATE`) notepad drew at its own size and the gate said "43% of the
screen is white, less than the 70%"; at 54aa541b with the workaround gone,
wineserver ended on a divide error and the script never ended.

Keys and the pointer needed nothing: yserver's input from hyprix's seat
(docs/YSERVER.md, Y4) reached Wine as it reaches xev, and Wine's clipboard
reached X's `CLIPBOARD`.

**The prefix on the btrfs volume (2026-10-12).** A prefix made on `/data`
ran out of space "with 680 MB free". It was the volume's size, not btrfs's
allocation: made with 1024 MiB more than its tree, the volume had 683 MB
available before `wineboot` started, and the prefix, 768 MB of files, takes
about 1.1 GB there (small files; metadata kept twice). `wineboot` ran out
at 684 MB of prefix with `df` at 100%. `fetch-wine.sh` now gives 2048 MiB
more, and on such a volume `wineboot` made the prefix on `/data`, `cmd`
answered and 627 MB were left. Read with `btrfs inspect-internal` on the
host, the full volume had every data block group full, its metadata chunk
9 MB used of 107 MB (two copies), and 135 MB of the device never allocated
to a chunk: whether Ferrix's write path holds back more than Linux's would
is a separate question, in `docs/BACKLOG.md`.

**esync is not in this Wine.** Debian's Wine 10.0 is upstream Wine, and
upstream Wine 10.0 has neither esync nor fsync -- both are wine-staging's
and Proton's patches -- nor ntsync, which upstream took later: its source
tarball holds no file and `server/thread.c` and `dlls/ntdll/unix/sync.c`
no line naming any of the three, and neither `wineserver64` nor
`ntdll.so` on the volume carries the string `WINEESYNC` (2026-10-12).
`WINEESYNC=1` is therefore ignored, and every wait goes through wineserver,
which is what `test-wine` already runs. What esync asks of the kernel
(`eventfd` with `EFD_SEMAPHORE`, non-blocking reads, `poll` and `ppoll`
over many descriptors, a high `RLIMIT_NOFILE`, shared memory for its
objects) stays untried by Wine until a Wine that has it is on a volume.
GE-Proton11-7, the Proton of §10, has none either (2026-10-15): its
`wineserver` and `ntdll.so` carry fsync and ntsync and no line of esync,
which Proton has dropped. Its fast path is fsync, on Ferrix since
2026-10-15 (§11).

**Leads, none of them in the kernel as far as is known.**

* `df` says tmpfs has used 0 of 3.0G with an 800 MB prefix on it.
* `wineboot` takes 27 s of guest time; on Linux, on the same host, a few.

**What the kernel gained for it.**

* `arch_prctl`'s `GS` base (§3).
* A 64-bit signal frame's `sigcontext` carries a fault's `trapno`, `err`
  and `cr2`, as a 32-bit one already did. Wine's `SIGSEGV` handler tells a
  page fault by `trapno` 14 and a write by `err`; with the three zero it
  logged `err:seh:segv_handler Got unexpected trap 0`, and `wineboot` took
  88 s of guest time.

* 32-bit Windows programs in the 64-bit process (§7): the frame a
  handler is entered on, the code selector a frame gives back,
  `ARCH_SET_FS`'s null selector, and `SS` after `SYSRET` on AMD.

Nothing else was needed for these programs. In particular not
`futex_waitv`, `FUTEX_WAKE_OP`, `mlock`, `membarrier`, `ptrace`,
`process_vm_readv`, `modify_ldt` or `preadv`: none is in the trace of §4.

## 6. Direct3D 11 through DXVK, with no card (`test-dxvk`)

Stage 23's E2.4: the gate that holds Wine with DXVK on a boot every session
can run. DXVK 3.1.1, the release's own 64-bit `d3d11.dll` and `dxgi.dll`,
on Mesa 25.0.7's lavapipe, in the same Wine, under QEMU with no GPU.

```
tools/common/fetch/fetch-dxvk.sh        # once, after fetch-wine.sh
cargo xtask test-dxvk --accel kvm       # the gate
```

**The pieces.** `fetch-dxvk.sh` downloads DXVK's release tarball, pinned
by its SHA-256 (GitHub's digest of the asset), keeps the two DLLs, and
builds `tools/common/test/d3d11-triangle.c` with mingw-w64's gcc, which
the host needs. Lavapipe and the Vulkan loader are not fetched again: they
are on the Wine volume already, from yserver's tree (Debian's
`mesa-vulkan-drivers` and `libvulkan1`, pinned in `fetch-yserver.sh`), and
the script stops if they are not. The three files go into the image under
`/dxvk`, and the Wine volume is attached as `test-wine` attaches it. The
script keeps a copy of the source it built from, and xtask refuses a
program built from another, so a change to the program asks for the
script to be run again rather than testing an old build.

It boots twice, as `test-wine` does.

**The first boot** (`tools/common/xtask/src/dxvk.rs`) is a shell, as
`test-wine`'s first. It starts yserver headless on `:1`, makes a prefix, and runs the
program beside DXVK's DLLs with `WINEDLLOVERRIDES=d3d11,dxgi=n` and
`DISPLAY=:1`. An X server is needed although nothing is shown: DXVK's
Vulkan instance asks for `VK_KHR_win32_surface`, which Wine offers only
with a display driver loaded, and with none instance creation fails with
`VK_ERROR_EXTENSION_NOT_PRESENT`.

**The program**, offscreen, makes a device on the first adapter, renders 30 frames into
a 64 by 64 RGBA texture, each a clear to blue and a triangle in the colour
the gate names on its command line, with shaders compiled at run time by
Wine's `d3dcompiler_47`, reads the last frame back through a staging
texture and prints one line: the counts of clear, triangle and other
pixels, two pixels, and an FNV-1a hash of the whole frame.

**The judge** requires DXVK's own `DXVK: v3.1.1` (so not Wine's `wined3d`),
a device on llvmpipe at feature level 11.1, and the frame line equal to the
one xtask computes from a frame it draws itself: 512 pixels of the
triangle, whose corners fall on pixel corners so that no pixel centre lies
on an edge, and 3,584 of blue. The same Wine, DXVK and lavapipe on Linux
(under bubblewrap on nazuna, with Xvfb) printed the same line, hash
`06a7a745`. Its negative control asks the program for another colour than
the judge expects, and the gate fails with "the frame is not the one drawn
on the host".

**The second boot** (`tools/common/xtask/src/compositor/dxvk.rs`) is
`test-wine`'s desktop: hyprix, with yserver on `:0` as its client. The
program runs presenting: a window of 512 by 512, a flip-model swap chain of
two 64 by 64 buffers on it, and the same frame drawn into the back buffer
and presented for 30 s, which DXVK scales to the window. That is the way a
game's frames go: DXVK's presentation through Wine's Vulkan surface on X11
(Wine's X driver turns `VK_KHR_win32_surface` into an X surface of
lavapipe's), yserver, and hyprix. Five seconds after the first present
xtask takes the screen. The gate requires the first frame, read back
before it is presented, to be the frame above (`frames=1`), 30 presents or
more and the program's own word that it ended, and the screen to be at
least 15% its blue and 2% its triangle's colour, which come there only
through the swap chain. On 2026-10-12 the screen was 26.7% blue and 3.7%
the triangle's (`build/x86_64/dxvk-window.ppm`), and the program presented
about 3,200 frames in 30 s (`Present` waits for no refresh on yserver).
Its negative control takes the screen before the program has a window, and
the gate fails on the screen.

**What Ferrix answered wrongly on the way:** nothing. Both boots passed on
their first run under KVM (2026-10-12): `wineboot` about 38 s of guest time,
the offscreen program 1.3 s, DXVK with 4 compiler threads.

**What is not right yet, and is Wine's on yserver, not DXVK's.** The window
is drawn at the size the program asked for, about 500 by 480, in the top
left of the tile hyprix gave it, and the rest of the tile is black: the
lead in §5 that notepad shows, now also for a Vulkan surface. The judge's
shares are set for that size and hold if the window comes to fill its tile.
The fix is E1.5's (stage 23), with notepad's.

**Where it runs.** On demand, as `test-wine` does, and for the same
reasons: it attaches a volume, and it never downloads.

## 7. 32-bit Windows programs (WoW64)

Wine 10.0 runs a 32-bit Windows program inside its 64-bit process: its
`ntdll.so` starts it "in experimental wow64 mode" when no 32-bit Wine is
installed, and Wine's `wow64cpu.dll` switches between the 64-bit code and
the program's 32-bit code with far transfers to the 32-bit code selector,
`0x23` on Linux and on Ferrix (`docs/I386.md` §3.1). No Linux system call is
made from 32-bit code: a Windows system call goes back to 64-bit code first.

**The DLLs.** Debian builds the 32-bit Windows DLLs only into its i386
`libwine`. Without them `wineboot` says `failed to open
C:\windows\syswow64\rundll32.exe: c0000135` (a DLL not found), on Linux
as on Ferrix: this page said before that Linux did not, which the trace in
`~/.local/share/ferrix/wine/trace` on nazuna does not bear out.
`fetch-wine.sh` copies that package's `i386-windows` beside the 64-bit
`x86_64-windows`, about 600 MB, and nothing else of it. With them, on Linux
7.0 in the same tree (2026-10-12), `wineboot` fills `syswow64` and the
32-bit `cmd /c ver` answers `Microsoft Windows 10.0.19043`.

**What Wine does, and what the kernel had to answer** (Wine 10.0's
`dlls/ntdll/unix/signal_x86_64.c` and `dlls/wow64cpu/cpu.c`, Linux 6.12's
`arch/x86/kernel/signal.c`, `signal_64.c` and `process_64.c`):

1. *A thread-local selector for the 32-bit thread block.* Wine's
   `alloc_fs_sel` calls i386 `set_thread_area` through `int $0x80` from
   64-bit code, base the 32-bit TEB, limit `~0`, flags `0x41`, and the
   32-bit code addresses its TEB through `FS` with that selector. Both
   already worked (I1, I2a).
2. *`ARCH_SET_FS` loads a null `FS` selector first.* Each time Wine comes
   back to its Unix side it sets its own thread pointer with `ARCH_SET_FS`
   while `FS` still holds the 32-bit selector. Linux loads a null selector
   before it writes the base; Ferrix did not, so the next switch reloaded the
   selector, whose base is the 32-bit TEB, and glibc's thread pointer was
   gone. It now does as Linux does, and as its `ARCH_SET_GS` already did.
3. *A handler is entered on the frame its installer's mode chose.* A fault
   in the 32-bit code -- a guard page, a breakpoint, a stack probe -- is
   handled by Wine's 64-bit `SIGSEGV` handler. Linux enters it on a 64-bit
   frame in 64-bit mode whatever mode the interrupted code was in, because it
   decides by the handler (`SA_IA32_ABI`, set when an i386 call installed
   it), and records the interrupted `cs`, `0x23`, in the `sigcontext`. Ferrix
   decided by the interrupted code's selector, and would have entered Wine's
   64-bit handler on an i386 frame in 32-bit mode. A disposition now records
   the ABI of the `rt_sigaction` that installed it, and the 64-bit frame
   records the interrupted code selector, which Wine reads to tell 32-bit
   code from 64-bit.
4. *A 64-bit `rt_sigreturn` gives back `0x23`.* Wine resumes the 32-bit
   code from its handler through the frame, which says `0x23`. Ferrix
   resumed every 64-bit frame in 64-bit code. It now gives back 32-bit user
   code where the frame holds it, and 64-bit user code for anything else, as
   before; Linux loads whatever the frame holds and lets `IRET` refuse a bad
   one. A frame that gives back `0x23` with a `RIP` at or above 4 GiB is
   refused with `SIGSEGV`: `IRETQ` checks the 32-bit segment's limit and
   would fault in ring 0, where Linux has a fixup and Ferrix has none.
5. *`SS` after `SYSRET`, on AMD.* Wine leaves its 64-bit side for the 32-bit
   code with a far jump, which loads `CS` and nothing else, so the 32-bit
   code runs on whatever `SS` the last return from the kernel left. AMD's
   `SYSRET` writes `SS`'s selector and not its hidden part
   (`X86_BUG_SYSRET_SS_ATTRS`). The first 32-bit stack access after a
   system call then took a stack fault (vector 12), which Wine reports as
   `Unhandled stack overflow`, in `rundll32` during `wineboot`. Two things
   were wrong. An interrupt from ring 3 leaves `SS` null, and a switch from
   there to a task that leaves a system call by `SYSRET` handed that null
   `SS` on; Linux reloads `SS` at every switch for that reason, and
   `restore_user_state` now does too (`cpu::settle_kernel_ss`). And the
   descriptor it loads, kernel data, was present-and-writable with limit 0,
   which 64-bit mode never looked at; it is now Linux's `__KERNEL_DS`, flat 4
   GiB with a 32-bit stack pointer. Intel's `SYSRET` loads all of `SS`, so
   neither was seen there.

`modify_ldt` was not needed: Wine uses the LDT on Linux only for 32-bit
Wine, and in WoW64 mode only `set_thread_area`'s GDT entries.

**Checks** (boot self-checks; the negative controls are in the landing's
record):

| Line | Requirement | What |
|---|---|---|
| `wow64` | L.x86_64.42, L.x86_64.43, L.x86_64.61 | a 64-bit program takes an i386 `set_thread_area` selector through `int $0x80`, loads it into `FS`, calls `ARCH_SET_FS` and reads `FS` 0 and its new base; in 32-bit code on a stack below 4 GiB it reads its block through the selector, and its `ud2` reaches its 64-bit `SIGILL` handler in 64-bit mode on a frame saying `cs` `0x23` and trap 6; `rt_sigreturn` resumes the 32-bit code in compatibility mode |
| `signal`, 64-bit forgeries | L.x86_64.43 | `rt_sigreturn` ends a program whose 64-bit frame holds a kernel-half `RIP` or `RSP`, or `CS` `0x23` with a `RIP` of 4 GiB, with `SIGSEGV`, resumes `IOPL` 3, `TF` and `IF` clear as `IOPL` 0 with `IF` set and `CS` and `SS` `0x10` and `0x18` as `0x33` and `0x2b`, and takes an honest change: the rest of the requirement the `0x23` case changed, unchecked until now |
| `sysret` | L.x86_64.58 | a 64-bit program pinned beside a spinning one sleeps 50 times, each time pushing a word in 32-bit code after `nanosleep` returns through `SYSRET`, and reads each back; some switch found `SS` null and loaded it |
| `inherit` | L.x86_64.11, L.x86_64.12 | a new program reads a `GS` base of 0; a `fork` child reads its parent's `GS` and `FS` bases, thread-local slot, `DS`, `MXCSR`, x87 control word and `XMM0`, a `clone` thread both bases; after `execve` all of them are back at a new program's |

**Where it stands (2026-10-12).** `test-wine` runs the 32-bit `cmd` to
completion: `Microsoft Windows 10.0.19043`. `wineboot` runs the 32-bit
`rundll32` and `iexplore` without an exception. Since 2026-10-15 a 32-bit
program draws a window: `tools/common/test/window32.c`, built by xtask with
mingw-w64's i686 compiler and run in `test-wine`'s desktop boot, prints
`window32: pointer=4 wow64=1 process=014c native=8664` and paints its
window, titled `Ferrix WoW64`, in the colour the script names; the gate
requires the line, the window in hyprix's list and the colour over half
the screen (90% on the 1024 by 768 screen under KVM, 2026-10-15). Not tried yet: a 32-bit installer. An i386 handler in a
64-bit program, delivered on a `SYSCALL`'s way back, runs in 64-bit mode,
unlike Linux; Wine installs none, and the row is in `docs/BACKLOG.md`.

**Review.** The landing's certification review is in the consultant's
ledger on nazuna (`~/.local/share/ferrix/cert-consultant/reviews.md`):
the first verdict, OK IF W1 to W7, at line 762; W1 is the 4 GiB refusal
above, W2 the `SYSCALL` row, W3 the `inherit` check. The verdict on the
landed commit fb3f9f093, W1 to W7 met, is at line 768.

The negative controls behind it, by gate.sh tag: on fb3f9f093 itself,
`endfield-wow64-i-ctl-w1` (the refusal of a 32-bit return above 4 GiB
taken out: the kernel's `iretq` takes a general protection fault in ring 0
on nazuna's AMD processor and the boot stops, FX-9004), `-ctl-inh11` and
`-ctl-inh12` (the `inherit` check's two requirements), `-ctl-cs`,
`-ctl-restore`, `-ctl-frame` and `-ctl-setfs`, each FIRED by its own line;
on 886e15119, whose SS code is unchanged since, `endfield-wow64-ctl-ssload`,
`-ctl-ssdesc` and `-ctl-flags`. (`endfield-wow64-f-ctl-w1`, which the
commit message names, ran on an earlier hash and is not the evidence.)

Only the instruction pointer is refused above 4 GiB on a return to 32-bit
code. The stack pointer needs no such refusal: it is held to the user half
by the same test as every return, `iretq` into compatibility mode does not
check it against the stack segment's limit, and 32-bit code uses only ESP,
its low half.

## 8. Endfield's launcher (stage 23, step 4; stopped 2026-10-13)

Step 4 of stage 23 started on 2026-10-12 and stopped the next day, when
step 0 found that the game's anti-cheat refuses to run in a virtual
machine (the stage's page). What was learnt is kept here; the fetch
script and the gate are on the branch `endfield-s4` and are not landed.
Nothing was logged into, no terms were accepted, and nothing of the game
was downloaded.

**The download.** endfield.gryphline.com's PC download is
`launcher.gryphline.com/launcher/get_latest_launcher?appcode=TiaytKBUIEdoEwRT&ta=endfield&channel=6&sub_channel=6`,
which redirects to Hypergryph's CDN: `GRYPHLINK_v1.6.0.1607_6_6_endfield.exe`,
174 MB, SHA-256 `b4b06cdb45c88c43fc3952f9810332f2709c3d7250a93016e9a4dc2c5ebaca73`
on 2026-10-12. Read on nazuna's host:

| What | Finding |
|---|---|
| Installer | 32-bit (`PE32`, i386) Nullsoft (NSIS 3, Unicode); 7-Zip unpacks its payload, `$0/1.6.0/`, 284 MB. It installs `Launcher.exe` beside the versioned directory, which `Launcher.exe` looks for (`C:/GRYPHLINK/1.6.0/Games.exe`); `Uninstall.exe` is 32-bit NSIS too |
| Launcher | Every program in the payload is 64-bit (`PE32+`): `Launcher.exe` (4 MB, a starter), `Games.exe` (18 MB, the window), `QtWebEngineProcess.exe`, `Patch.exe`, `Updater.exe`, `7zg.exe` |
| Technology | Qt 5.15.8 with QtWebEngine (Chromium 87.0.4280.144); the interface is a local web page (`res/web/`, version 0.2.15) shown in it. ANGLE (`libEGL`, `libGLESv2`, `d3dcompiler_47`) for Qt's GL |
| Dependencies | All in the payload: the VC++ 2015-2022 runtime and the UCRT's `api-ms-win-*` forwarders, OpenSSL 1.1 (`libssl-1_1-x64`, `libcrypto-1_1-x64`), libcurl with nghttp2, sqlite, zlib, rlottie, Tencent's CrashSight, Hypergryph's `hgdownloadsdk` and `hgeventlogsdk`. No WebView2, no .NET |
| Network | Its own queries go to `launcher.gryphline.com` and `game-config.gryphline.com`; the web view's to `gl-utils-public.hg-cdn.com` and Hypergryph's CDN; telemetry to `event-log-api-*.gryphline.com` and `pc.crashsight.wetest.net` |
| Logs | `C:\users\<user>\AppData\LocalLow\Gryphline\<id>\logs\games.log`, which says how each query was answered |

**On nazuna's Linux (linux-host), the reference.** The payload laid out as
the installer lays it out, in a fresh 64-bit prefix of the same Debian
Wine 10.0 under bubblewrap and Xvfb: the launcher shows its first screen
(the game's picture, "Log In", the news, a yellow "Download" button) and
`games.log` says Gryphline's server answered its version query with 200.
No Wine setting was needed.

**On Ferrix, under KVM.** hyprix with yserver, the network through xtask's
gateway (a lease from `udhcpc` first), the payload on a btrfs volume, the
prefix on tmpfs, 8 GiB. The launcher draws its first screen there too,
with the news and pictures fetched over TLS by its web view; its window is
titled `GRYPHLINK` and floats at 1312 by 754, wider than the 1024 by 768
screen. Two things were wrong on the way, one yserver's and one the
kernel's; both are fixed:

* **`XF86VidModeSetGamma` ended the launcher (fixed 2026-10-13).** yserver
  served XVidMode's reads and refused every write with `ClientNotLocal`,
  while it gave a local client (all of its clients are on a Unix socket)
  the write permission in `GetPermissions`. Wine's X driver sets the gamma
  through `XF86VidModeSetGamma` with no error handler when the gamma ramp
  is not 256 entries (`X11DRV_XF86VM_SetGammaRamp`,
  `dlls/winex11.drv/xvidmode.c`), so the X error went to Xlib's default
  handler and `Games.exe` ended; `UseXVidMode=N` in the prefix was the
  workaround. The fork now answers a local client as Xwayland's vidmode
  hooks do (`hw/xwayland/xwayland-vidmode.c`): `SetGamma` and
  `LockModeSwitch` succeed and apply nothing, so `GetGamma` still reads
  1.0, and `SetGammaRamp` is `BadValue`; mode setting stays RandR's
  (fork commit 1a8752e8; `fetch-yserver.sh` pins a later one). The launcher
  starts with no Wine setting added.
* **The launcher's own queries never connected.** Its Qt side resolved
  `launcher.gryphline.com` and `game-config.gryphline.com` again and again
  in one thread and never opened a socket; `games.log` said `qehr OnError
  error:3` (Qt's `HostNotFoundError`) and `nCode=-2`, so it never learnt
  the game's version. The web view's lookups (Chromium's own resolver) and
  libcurl's succeeded in the same boot. The cause was two missing system
  calls. Qt on Windows resolves through Wine's `ws2_32`, which calls glibc's
  `getaddrinfo` with `AF_UNSPEC` and no `AI_ADDRCONFIG`; glibc's resolver
  then sends the A and AAAA queries together with `sendmmsg` (`send_dg` in
  `resolv/res_send.c`), which x86-64 glibc assumes the kernel has, and
  takes `ENOSYS` from it as a failed query. Ferrix had neither `sendmmsg`
  nor `recvmmsg`, so every such lookup ended in `EAI_AGAIN`, while one with
  `AI_ADDRCONFIG` (only IPv4 configured, so one A query, sent by `sendto`)
  succeeded. A static glibc program run as init with the network showed it
  on 2026-10-13 (`~/.local/share/ferrix/endfield/probe/addrconfig.c`): both
  names failed with flags 0 and resolved with `AI_ADDRCONFIG`. Both calls
  are now in the kernel (`src/kernel/src/syscall/sockets.rs`, on
  `sendmsg` and `recvmsg` as Linux's `net/socket.c` builds them, with a
  boot check beside the other socket checks), and the same probe resolves
  both names with flags 0 (`~/.local/share/ferrix/logs/endfield-mmsg/probe-addrconfig.log`).
  i386's `socketcall` still answers its sub-calls 19 and 20 with `ENOSYS`:
  the boot check that requires it is in the core ring. With the fix, the
  launcher reached its server on Ferrix on 2026-10-13: `games.log` says
  `OnQuery_ProtocolVer_LauncherVer_GameVer_Result ... bRet=true,
  nCode=200`, and Gryphline's GPU configuration query answered 200 too
  (`test-endfield-launcher` on the branch `endfield-s4` over this kernel,
  `~/.local/share/ferrix/logs/queue/endfield-mmsg-launcher-try.log`).

**The gate.** `tools/common/fetch/fetch-endfield-launcher.sh`
pins the installer and 7-Zip 25.01's Linux build by SHA-256, checks the
three programs are 64-bit, and makes a volume of its own Wine tree (made by
`fetch-wine.sh` under its own directory; the shared tree is a root other
sessions bind for bubblewrap, and held an older yserver than its stamp on
2026-10-12) with `/GRYPHLINK` added. `cargo xtask test-endfield-launcher`
requires hyprix to list the window by its title, the yellow button on the
screen, and `games.log` saying the server answered with 200. The first two
held on Ferrix on 2026-10-13, and the third once the second defect above
was fixed the same day. It has no negative control yet.
`cargo xtask test-endfield-proton` is the same boot in GE-Proton's Wine
(§10).

## 9. A real Unity game, with no card (`test-unity`)

Stage 23's step 3 begun: a 64-bit Unity game on Direct3D 11, with no
anti-cheat, in the same Wine through the same DXVK on lavapipe, presented
in a window of hyprix's through yserver, under KVM with no card.

```
tools/common/fetch/fetch-unity.sh       # once, after fetch-wine.sh and fetch-dxvk.sh
cargo xtask test-unity --accel kvm      # the gate, about 13 minutes of guest time
```

**The game.** Unity Technologies' *Spaceship Demo*, release 2021.2.0
(`github.com/Unity-Technologies/SpaceshipDemo`, the release's
`SpaceshipDemo-Release-2021.2.0-Win64.zip`, 602 MB, SHA-256
`13111680…c9fd4`, pinned in `fetch-unity.sh`). It is a first-person walk
through a spaceship that Unity publishes, with its project, as an open
repository; the build is downloaded with no account. Why it stands in for
Endfield, and where it does not:

| | Spaceship Demo | Endfield |
|---|---|---|
| Engine | Unity 2021.2.3f1 (its `UnityPlayer.dll`) | Unity, "heavily modified" (the study); version not known |
| Graphics | Direct3D 11 at feature level 11.1 by default, the High Definition Render Pipeline and the VFX Graph: compute shaders, volumetric fog, many render targets | Direct3D 11 only, a modified render pipeline |
| Code | Mono (`MonoBleedingEdge`, `mono-2.0-bdwgc.dll`, `Assembly-CSharp.dll`) | not known; Unity games of its kind are usually IL2CPP (`GameAssembly.dll`) |
| Bits, anti-cheat | 64-bit, none | 64-bit, ACE |

So it holds what any Unity game asks of Wine and of Ferrix beneath it --
`UnityPlayer.dll`'s threads (its job workers and Enlighten's, a render
thread), its memory and file mapping, its window, Direct3D 11 through
DXVK -- and a renderer far heavier than a small game's. It does not hold
IL2CPP: Mono's runtime and its garbage collector stand where IL2CPP's
native code would. A free IL2CPP title on Direct3D 11 that can be fetched
with no account was not found; the customer's choice is owed (E3.1).

**The volume.** `fetch-unity.sh` copies the Wine volume's tree (§1), unpacks
the game at `/unity`, puts DXVK's `d3d11.dll` and `dxgi.dll` from
`fetch-dxvk.sh` beside `Spaceship.exe`, and makes `unity.img` (about
3.3 GB) with 512 MiB of room. It records the yserver, DXVK and game it
carries, and `test-unity` refuses a volume that holds others than the pins.

**The boot** (`tools/common/xtask/src/compositor/unity.rs`) is
`test-wine`'s desktop: hyprix with yserver on `:0`, 8 processors and
8 GiB. Its script makes a prefix in `/tmp` and starts the game with
`-screen-fullscreen 0 -force-d3d11 --benchmark`, DXVK chosen by
`WINEDLLOVERRIDES=d3d11,dxgi=n`, lavapipe named by `VK_ICD_FILENAMES` for
the game alone. `--benchmark` is the game's own option: it skips the menus
and plays the walkthrough on a timeline, recording each frame's time. A
float rule leaves the window at the size the game takes, which is the
screen's, 1024 by 768: the game sets its resolution from its own options
and does not keep Unity's `-screen-width`.

**The judge.** Unity's log must name its engine version, Direct3D 11 at
feature level 11.1 on llvmpipe, and `Started Recording benchmark`; Wine's
output must hold `DXVK: v3.1.1`. Then two screendumps 30 s apart: inside
the game's window, as hyprix places it, at least 30% of the pixels must
not be near black and at least 20% must have changed. On 2026-10-12 the
window was 96% lit and 98% changed, and the game's own counter said 1.3
frames a second (778 ms a frame). Negative controls, both FIRED on
f61dea0a7 under KVM (`gate.sh control`, 2026-10-13): the game stopped with
`SIGSTOP` once the scene is there (`endfield-s3-ctl-sigstop-2`: its window
goes black, 0% lit and 0% changed, so the gate fails on "so no scene is
drawn in it" before it reaches "not drawing frames"), and Wine's own
`d3d11` and `dxgi` chosen instead of DXVK's (`endfield-s3-ctl-wined3d-16g`,
"Wine's own d3d11 ran"). The second needs `--memory 16384`: at the gate's
8 GiB, wined3d on llvmpipe ran the guest out of memory and the kernel
stopped on FX-0008 (`docs/BACKLOG.md`).

**On Linux, for comparison** (linux-host: the same tree under bubblewrap on
nazuna with Xvfb at 640 by 360, 24 processors, no Ferrix): the game's
intro, its menu with the animated fog behind it, and the benchmark's
walkthrough all drew; the walkthrough had not ended after 18 minutes.
Screenshots, each with its banner: `~/.local/share/ferrix/unity/shots/` on
nazuna (`linux-host-*` and `ferrix-*`).

**What Ferrix answered wrongly on the way:** nothing yet. The game started,
loaded its scene and drew it through DXVK on its first boot that got that
far. Two things on the way were not the kernel's:

* `wineboot` took 300 s on the volume made on 2026-10-12 at 23:20: it
  ended with `run_wineboot boot event wait timed out`, because a 32-bit
  `rundll32.exe` (`Wow64Install`, started "in experimental wow64 mode")
  never ended. The shared Wine tree on nazuna had gained Debian's
  `libwine:i386` from the WoW64 work (E1.6) that evening; on `main`'s
  `fetch-wine.sh` there is no 32-bit side and `rundll32` is not found
  (§5). That 32-bit programs hang is E1.6's; the gate bounds its wait for
  the prefix's programs at 120 s and goes on.
* A volume copied while that tree was being remade carried another
  yserver than the pin's (its own hash differed), and wineserver ended on
  the divide error of §5. `fetch-unity.sh` then made the volume again from
  the finished tree. Both say the same: the gate's volume is built from a
  directory other sessions write.

**Where it runs.** On demand, as `test-wine` and `test-dxvk` do: it
attaches a volume and never downloads.

## 10. The launcher in Proton's Wine (`test-endfield-proton`)

Debian's Wine 10.0 draws the launcher's Qt dialogs that are translucent
windows -- its install dialog, its update prompt -- blank, on Linux as
on Ferrix (§8): only a line of their text shows. The customer asked for
the latest Proton's Wine instead (2026-10-13). Valve's Proton 11 ships as
source only; GE-Proton is built from it and released as a tarball, and
its 11-7 draws those dialogs whole, on Linux and on Ferrix.

```
tools/common/fetch/fetch-proton.sh                                  # once
FERRIX_ENDFIELD_WINE=proton tools/common/fetch/fetch-endfield-launcher.sh
cargo xtask test-endfield-proton --accel kvm
```

**The volume** (`fetch-proton.sh`): the Wine volume's tree with
GE-Proton11-7 at `/proton`, pinned by its SHA-512, and the Debian i386
libraries its 32-bit half needs. This build is Wine's old 32/64 layout,
not its new WoW64 mode (§7): a 32-bit Unix side
(`files/lib/wine/i386-unix`) runs 32-bit Windows programs, and without it
the 64-bit side does not make a prefix either. That half is a 32-bit
glibc program, which Ferrix runs (stage 22's I1 to I4), so glibc, Xlib and
the X extensions, FreeType and fontconfig are there in i386 too, in
Debian's multiarch places. The list is what `wineboot`, the 32-bit `cmd`
and a 32-bit window asked for on Linux. Proton's own `proton` script,
which Steam runs inside its runtime, is not used; the boot sets what it
sets for Wine (`WINEDLLPATH`, `LD_LIBRARY_PATH` into `files/lib`) and
links Proton's fonts into the prefix before `wineboot`, as it does.

**What Ferrix answered wrongly (fixed 2026-10-13, f0c812f01).** Proton's
`wineboot` faulted writing its user shared data, a memfd wineserver maps
`PROT_WRITE` alone and the client maps `MAP_SHARED|MAP_FIXED` over a
placeholder and `mprotect`s read-write. `mprotect` replaced a region's
every flag, so the shared mapping became private and the write after it
faulted; and a write-only region refused a read, where Linux's
`protection_map` makes it readable. A static probe of each step
(`~/.local/share/ferrix/endfield/probe/usd2.c`) failed three of twelve
variants on Ferrix and none on Linux before the fix, and none after
(L.user.128, L.user.129). Proton's Wine also warns that it could not
enable Syscall User Dispatch, which Ferrix does not have (stage 23's
E5.3); nothing the launcher does needed it.

**The font.** The launcher names Segoe UI (and Microsoft YaHei) for its
interface, and no prefix has Segoe UI, Microsoft's own: Qt's web view
fell back to Times New Roman and the dialogs came out serif, where Windows
draws them in Segoe UI. The volume carries Debian's Open Sans, close to it
in shape, and the boot points Wine's replacements for the Segoe UI family
at it (`HKCU\Software\Wine\Fonts\Replacements`). Microsoft's open
stand-in, Selawik, was tried first: it carries no TrueType instructions,
so Wine's FreeType auto-hints it, rounds each glyph's advance to a pixel,
and the spacing came out uneven ("N OTIFICATION"); ClearType settings and
FreeType's own properties did not change that, and Open Sans, which is
hinted, spaces as Windows does. Open Sans is a little wider than Segoe UI,
so a line may wrap a word earlier.

**The gate** requires, besides §8's window, button and server answer, the
32-bit `cmd` to answer with a Windows version. On Ferrix under KVM on
2026-10-13: `wineboot` exits 0, the 32-bit `cmd` answers `Microsoft Windows
10.0.19045`, the launcher's window and button are drawn and Gryphline's
server answers; a click on Download draws the install dialog whole, in Open
Sans (the screens are in `~/.local/share/ferrix/endfield-proton/` on
nazuna).

**Still not as on Windows:** the launcher asks every five seconds for the
image path of its `QtWebEngineProcess.exe` and gets none, because Wine
reads another process's memory through `ptrace` and `/proc/<pid>/mem`,
which Ferrix does not have (stage 23's E5.2; the certification
consultant's answer, ledger line 877, is OK IF P1 to P8, with a design and
the customer's word on the Security Target first). The version 1.6.0.1607
pinned here asks for no update; the current launcher on Windows does.

**The game's download (2026-10-14).** For the comparison with the
customer's Windows install, the game goes to a 160 GB sparse btrfs image
on the SN720's `ferrix-root` partition (mounted on nazuna at
`/mnt/ferrix-root`, the filesystem grown to the partition's 190 GB),
attached to Ferrix writable; the launcher sees 154.52 GB free for its
111.18 GB. The download failed twice within seconds, with "Download
Error" on the button and `OnGameDownloadSdkError nCode=206` in the
launcher's log:

* Wine said it could not load `libgnutls`, so its Schannel had no TLS.
  `fetch-proton.sh` now carries Debian's GnuTLS and what it loads, and the
  CA certificates as `/etc/ssl/certs/ca-certificates.crt` on the volume.
* The download SDK's own log (`HGDownloadLog.log`, beside the launcher's)
  then gave curl's error 60 for every file on `beyond.hg-cdn.com`: the
  server's chain was not verified. The SDK is `hgdownloadsdk.dll` over the
  launcher's own `libcurl.dll` 7.88.1 and OpenSSL 1.1; it names no CA file,
  and libcurl imports `CertOpenSystemStoreA`, so its roots are the Windows
  root store -- which Wine's `crypt32` fills from
  `/etc/ssl/certs/ca-certificates.crt` and a few other absolute paths.
  The boot linked none of them: the bundle was on the volume at `/data`,
  not at `/etc`, so the store held only Wine's built-in roots, and the CDN's
  chain (TrustAsia DV TLS RSA CA 2025 under DigiCert Global Root G2, which
  the bundle has) did not verify. `test-endfield-proton` now links
  `/etc/ssl/certs/ca-certificates.crt` to the volume's, as the Steam boots
  do, and the download runs. Verification stays on.

With the link, the launcher downloads its 54 files (the game 1.5.3 in
`.zip.NNN` parts). Through xtask's gateway Ferrix took about 3.4 MB/s,
steadily; nazuna itself fetches a part from the same CDN at 31 MB/s, so the
gateway, not the guest, is the limit (`docs/STEAM.md` §7, stage 23's E4.4).

**Where it stopped (open).** After 36 minutes and about 5.8 GB, every
write of the downloader failed with Windows' error 112, the disk is full
(`FlushBufferToFile ... Can not write file with 112`, then curl's 23), and
the launcher showed "Download Error". Wine maps `ENOSPC` to 112, but the
volume was not full: about 150 GB of it was still unallocated (read from
the host, its chunk tree has the 612 8 MiB data chunks `mkfs.btrfs
--rootdir` made, four 1 GiB data chunks Ferrix made, and a 1 GiB metadata
chunk 3 % used). Neither btrfs measure refused: the same volume, written
through the same writable mount on the host (`btrfs-vfs`, four files
round-robin, 10 GiB), takes every byte, and `statfs` falls by what was
written. What ran out is the guest's memory. A writable btrfs mount's page
cache is never reclaimed (`user::cache`: "a writable one never yet"), so
every page a file writes stays in memory while the file is open, written
back or not; the downloader keeps its parts open, and when the 8 GiB guest
had no frame left, the write's page commit failed and `fs/pages.rs`
answers that as tmpfs does, `ENOSPC`. A boot with more memory goes further
before it stops (the download was started again with 24 GiB); the fix is
reclaim for a writable mount's clean pages, in the certified core.

**The extraction stopped on a read** (install9, 2026-10-15). With the
download whole, the launcher runs its own 7-Zip,
`7zg.exe x -y -o<decompress>\0.0.0-1.6.3 <...>.zip.001`, over the 59
volumes of one split zip (62 GB packed, 68 GB in 1782 files). After 1 h 47
min, at 1505 of 1782 entries, it gave up on `GameAssembly.dll` with
`0x8007001F`, "General failure", and exited 2; the launcher logged
`qep run process exit bNormal:false, dwExitCode:2` and the button stayed
at "Extracting". `ERROR_GEN_FAILURE` is `STATUS_UNSUCCESSFUL`, which Wine
gives for an errno it has no mapping for -- here `ENOMEM` from `read`. The
guest's free memory had sat at the floor (about 35 MB free, 15 GB cached)
for the whole extraction, and a read that missed the cache filled its page
through `fs/pages.rs`'s `allocate_reclaiming`, which reclaimed once and
tried once more: the frames that reclaim gave back went to another
processor's allocation first, and the read answered `ENOMEM`. The archive
and the 183 MB written of `GameAssembly.dll` were whole (read from a copy
of the volume). A local boot with 1 GiB and 8 processors -- three `dd`
writers, a holder at 70 % and readers of the rustc volume's libraries --
showed 27 `cat: read error: Out of memory` in 240 s. A fill now reclaims
and retries as a write does, at most 8 times while a reclaim gives pages
back; the same boot then read 60,000 times without an error, and stage
12's `reclaim` line requires a read put two fills' runs past its job's
`memory.max` to succeed.


**On the `--everything` desktop.** `run-compositor --everything` merges the
Proton launcher volume last (`crate::everything`); where it carries
another build of a file an earlier volume has -- yserver, and libpng and
liblzma in another Debian security revision -- the earlier one is kept,
and any other difference still stops the merge. The volume's
`/usr/bin/gryphlink` makes a prefix under the user's home the first time,
as the gate does (Proton's fonts before `wineboot`, Open Sans for Segoe
UI), and starts the launcher on the desktop's X server; the desktop links
it as `/bin/gryphlink`, with Wine's and Proton's links and the
certificate bundle, and fuzzel lists it as GRYPHLINK.
`cargo xtask test-endfield-proton --everything --accel kvm` boots on the
merged volume, starts the launcher with `gryphlink`, and requires the
same window, button and server answer.

## 11. fsync: Proton's Wine without wineserver in every wait (`test-wine-sync`)

Stage 23's E1.4 and E3.5. With wineserver's synchronisation alone, every
wait and every `SetEvent` of a Windows program is a request to the server
over its socket. esync and fsync are Proton's ways around that; GE-Proton11-7
has fsync (and ntsync) and no esync (§5). Read in its binaries and in
Valve's `proton_11.0` source (`server/fsync.c`,
`dlls/ntdll/unix/fsync.c`):

* wineserver turns fsync on when `WINEFSYNC=1` and `futex_waitv` (449),
  called with no arguments, fails with something other than `ENOSYS` or
  `EPERM`; it then says `fsync: up and running.` on its standard error;
* every event, mutex and semaphore is then a word in a POSIX
  shared-memory file, `/dev/shm/wine-<prefix inode>-fsync`, which the server
  grows with `ftruncate` and each Wine process maps `MAP_SHARED`;
* a wait is `futex_waitv` over those words -- up to 64 objects and the
  thread's APC word, each `FUTEX_32` without `FUTEX2_PRIVATE`, so keyed
  shared -- with an absolute timeout on `CLOCK_MONOTONIC` or
  `CLOCK_REALTIME`; a signal is the word changed and a plain `FUTEX_WAKE`;
* a client whose own probe gets `ENOSYS` while the server runs fsync exits
  ("can't use __NR_futex_waitv"). Both Unix sides call it: Proton's 64-bit
  `ntdll.so` through `syscall`, its 32-bit one through `int $0x80`.

**What Ferrix lacked.** `futex_waitv` itself; shared futex keys on a
`MAP_SHARED` file mapping, `/dev/shm` and `FUTEX_WAKE` were there. It is
in the kernel since 2026-10-15 (`syscall/futex.rs`, 449 on x86-64, i386
and AArch64; ARMv7-A's local header ends before it), with Linux's answers
and one deliberate difference: a wake does not count a stale entry of a
vector waiter already woken through another of its words. The stage 7 self-check's
`futex` line holds it. Reviewed by the certification consultant before it
was built: OK IF (1) to (10), ledger line 1172 (all of it in the load; no
requirement id).

**The gate.** `cargo xtask test-wine-sync --accel kvm` attaches Proton's
volume (`fetch-proton.sh`) with no display and carries
`tools/common/test/syncbench.c`, built by xtask with mingw-w64 for x86-64
and i386: two threads hand a turn back and forth 20,000 times (the waiting
side in `WaitForMultipleObjects` on three handles, the one it waits for
the second), then two processes through named events. It runs each width
under a wineserver of its own without `WINEFSYNC`, then with
`WINEFSYNC=1`, and passes when with fsync the server said it took it and a
run traced with `WINEDEBUG=+fsync` shows Proton's own `Woken up by handle`
from each width, without it neither, and every run handed every turn over.
Its negative control runs it on a kernel that answers 449 with `ENOSYS`:
the server never takes fsync and the gate says so.

On nazuna under KVM, 2026-10-15, two runs (a first by hand, then the
gate pool's `es-sync`; wineserver's time is its user and system time from
`/proc` over the run, process starts included):

| | 64-bit threads | 64-bit processes | 32-bit threads | 32-bit processes | wineserver, 64 / 32-bit runs |
|---|---|---|---|---|---|
| wineserver alone | 90.6, 86.2 µs a turn | 83.8, 91.8 µs | 127.6, 86.3 µs | 196.3, 105.2 µs | 4170 / 5440, 4140 / 3810 ms |
| fsync | 4.9, 3.3 µs | 8.0, 5.5 µs | 5.4, 3.8 µs | 8.7, 6.0 µs | 880 / 320, 1240 / 200 ms |

**The launcher.** `test-endfield-proton` sets `WINEFSYNC=1`, as Proton's
own script does (`FERRIX_WINEFSYNC=0` leaves it to wineserver, to compare),
and prints the processor time wineserver and the Windows programs took
over the last two minutes of its wait, the launcher idle on its first
screen.

On nazuna under KVM, 2026-10-15, one run each with the host loaded by other
sessions (load about 30): wineserver took 30.6 s of processor time in those
120 s without fsync and 19.8 s with it, a third less; the Windows programs
(the launcher, its web engine's processes) took 395 s and 408 s, about
three and a half processors either way -- the launcher's own drawing and
its web view, not its waits, are what it spends. Both runs drew the window
and its button and reached Gryphline's server; with fsync the server said
`fsync: up and running.`

**Not done.** ntsync (`/dev/ntsync`, Linux 6.14's driver), which Proton
prefers when the device is there and which is E3.5's larger half;
`gryphlink` on the volume (`fetch-endfield-launcher.sh`) does not set
`WINEFSYNC` yet, so the `--everything` desktop's launcher waits through
wineserver unless the gate's script, which does, starts it.
