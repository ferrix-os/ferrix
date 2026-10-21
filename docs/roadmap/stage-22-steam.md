# Stage 22 — Steam  ·  *unsized, over 300 points*

Steam runs on Ferrix, logs in, and a game bought there plays. Put on the
roadmap by the customer on 2026-09-18 as the step after the GPU decision. It
does not wait for stage 21: it is a guest's stage first, on the GPU of Path
A, and moves to bare metal when stage 21 does.

Steam is the hardest Linux program to be somebody else's binary for, which
is why it is the right exit for `docs/ARCHITECTURE.md` §2's promise: a
closed client that is a 32-bit program starting 64-bit helpers, one of them
a whole browser; a runtime that containerises every game with bubblewrap;
and games that draw with Vulkan through Wine. Everything below is something
Steam needs that Ferrix does not have, in the order it can be tested. The
dynamic linking section above already says the first part; the rest was not
staged until now.

* **The 32-bit x86 ABI.** The Steam client itself is an `i386` program
  today, and every 32-bit Windows game Proton runs is 32-bit Wine underneath,
  so this is needed whether or not Valve's 64-bit client arrives first. The
  `i386` system call table and its `compat` layouts -- `struct stat64`,
  `off_t` pairs, `epoll_event` packed, `iovec` and `msghdr` with 32-bit
  pointers -- entered through `int $0x80` and the vDSO-less path glibc falls
  back to, 32-bit address spaces below 4 GiB on the 64-bit kernel, 32-bit
  `mmap` and `brk` limits, `TLS` through `set_thread_area` and the `GDT`
  entries it needs, 32-bit signal frames, and `AT_SYSINFO` absent as
  `AT_SYSINFO_EHDR` is. x86-64 only; the Arm architectures have nothing
  to run. Unsized: the table is as long as stage 7's was. Under way
  since 2026-09-26, at the customer's request: `docs/I386.md` sizes I1 to
  I4 at 42 points, taking it from the way in to Alpine's and Debian's i386
  busyboxes and an i686 thread program, and I5, unsized, is what the
  Steam runtime finds missing. **I1 is on `main`** (2026-09-26): the GDT
  in Linux's order and numbers with real 32-bit segments, `int $0x80` as a
  DPL-3 gate decoding through an i386 table in `src/lib/proto/linux-abi`, an
  `EM_386` image `execve`d into a 4 GiB space and entered in compatibility
  mode, and compatibility mode's `SYSCALL` and `SYSENTER` made harmless;
  the boot runs a hand-assembled i386 program. **I2a is on `main`** the
  same day (5 points): the thread pointer, through `set_thread_area`,
  `get_thread_area` and `%gs`, with each thread's GDT descriptors and
  segment selectors kept across a switch, and every saved selector checked
  against the descriptor it names before it is loaded; reviewed by the
  certification consultant. **I2b** follows (about 13 points): `fork`,
  `clone` and both i386 signal frames with their returns, every returned
  frame checked before any of it is loaded. **I3** follows (about 10 of its
  13 points): Alpine's i386 busybox, unchanged, passes `test-shell` and
  `test-vfs` under TCG and KVM. 229 i386 calls are mapped, their argument
  registers and structures read at the program's width -- the split 64-bit
  offsets, `iovec`, `flock64`, `statfs64`, the time32 `timespec`s musl
  prefers, the sockets' `msghdr` -- and an `ioctl` outside the terminal's
  is `ENOTTY`. **I4** follows (about 5 of its 8 points): Debian's dynamic
  i386 busybox runs on its own glibc and `ld-linux.so.2`, and a 32-bit Rust
  `std::thread` program passes `test-threads`. glibc wanted `socketcall`,
  the 32-bit `ugetrlimit` and the 12-byte robust list, found by tracing it
  under `qemu-i386` before booting it. Still to do: what Steam finds
  missing, which is I5. **I5a** follows (2026-09-27): Valve's steamcmd,
  the client's core without its display, updates itself and logs in to
  Steam anonymously on Ferrix (`cargo xtask test-steamcmd`, under KVM, with
  the network): the i386 `stat64` family, `waitid`, `cpu MHz` in
  `/proc/cpuinfo`, and a certificate bundle on its volume. Still to do: the
  client's bootstrapper (I5b), which needed an X server for its window and
  drew it through yserver on 2026-09-29 (below);
  System V semaphores, which steamcmd asks for through `ipc` and carries on
  without and the client does not -- built 2026-09-28 (`test-sem`,
  `docs/I386.md`); `modify_ldt` for 32-bit Wine.
* **glibc's place, taken.** The dynamic linking stage's third part, glibc's
  names, with Steam as its stress test: `ld-linux` and `libc.so.6` requested
  by name, `dlopen` from the client and from every Steam runtime library,
  `GLIBC_2.x` versions back to the ones a 2012 runtime binary asks for, and
  the `/etc/ld.so.cache`, `LD_PRELOAD` and `LD_LIBRARY_PATH` behaviour the
  runtime's launcher scripts lean on. 13 points are priced there, and 10 of
  them are done on x86-64 (2026-09-21 and 22): `ld-linux` and `libc.so.6` by
  name with glibc's versions, `dlopen`, and `LD_LIBRARY_PATH`. `LD_PRELOAD`
  is not there, and `/etc/ld.so.cache` is deliberately not read. Steam adds
  whatever those scripts find missing, unsized.
* **The container the games run in.** Steam's `pressure-vessel` runs each
  game inside bubblewrap: user, mount and pid namespaces, `pivot_root`,
  `seccomp` filters, and the Chromium helper's own sandbox on top. That is
  stage 13 entire, plus what stage 13 does not name and Steam will:
  `/proc/<pid>/` fields the runtime reads (`inotify`, `pidfd_open` and
  `pidfd_send_signal` have been answered since 2026-09-27, `SO_PEERCRED`
  since 2026-09-13), and
  `prctl` beyond what stage 7
  answers. Stage 13's month, plus 13 points of the rest.
* **Somewhere to put it.** A Steam library is tens of gigabytes on a
  filesystem that survives a reboot: stage 12, btrfs write, and a root on
  it rather than an initramfs. Both are done (2026-09-21 and 22); what is
  left is a root volume larger than the 1 GiB `build/root.img` that `cargo
  xtask run` makes.
* **XWayland.** The client is an X11 program; stage 19 lists XWayland as
  its largest single gap and this is what finally needs it. An X server on
  the compositor's protocol: Xwayland built on ferrousli with its dynamic
  loading (the C path), or a Rust X server that answers the requests Steam
  and Wine make, which is the smaller subset than it sounds and the larger
  program than it looks. `xwayland_shell_v1` on the compositor's side is a
  table. 40 points as a first guess, most of it the server, counted in
  stage 19's remainder and not again here. **Decided 2026-09-27 (customer):
  the X server is [yserver](https://github.com/joske/yserver)** (Rust,
  MIT: GLX, DRI3, Present, RENDER, XI2, RandR, SHM, Composite; it runs
  Plasma, Chromium and Electron), not C Xwayland with xwayland-satellite.
  yserver has DRM/KMS, nested-on-X11 and headless backends and no Wayland
  one, so Ferrix adds a rootless Wayland backend behind its backend trait:
  each top-level X window an `xdg_toplevel` on hyprix, with input and the
  clipboard bridged. xwayland-satellite (Rust, MPL-2.0) is the reference
  for how X windows map to xdg surfaces, read and not copied. In order:
  a feasibility pass (a pinned yserver release, its headless backend built
  for x86-64 on Ferrix, an X client run against it), a written design for
  the backend, then the backend in small landings. **The feasibility pass was met
  on 2026-09-28**: `cargo xtask test-yserver` boots yserver headless on
  Mesa's lavapipe from the volume `tools/common/fetch/fetch-yserver.sh` builds,
  and `xdpyinfo` reaches it and lists 21 extensions. Ferrix did not change
  for it. **The design is `docs/YSERVER.md`**, approved the same day: the
  backend wraps yserver's Vulkan renderer, hands frames to hyprix as
  `wl_shm` copies until hyprix has dmabuf, and is built in seven slices, 36
  points, on the customer's fork of yserver. Y1 to Y5 are done the
  same day, 29 of the 36 points: `fetch-yserver.sh` builds the fork, yserver
  connects to hyprix as a Wayland client with its root window the size of
  hyprix's screen, each top-level X window is a window of hyprix's, with
  its title and class, hyprix's keys, pointer, wheel, focus and cursor
  reach X, hyprix sizes and closes X windows and floats X's transients
  as dialogs, and X's menus are its popups (`cargo xtask test-xwindow`
  finds xev's window on the screen, requires xev to report the input put
  in, to take its tile's size, and to end when hyprix closes it, a
  transient xev to float, and xfontsel's menu to show where X put it).
  Y7 put yserver on the `--everything` desktop the same day, and Y6, the
  clipboard, was done on 2026-09-29: X's `CLIPBOARD` and `PRIMARY` and
  hyprix's clipboard and primary selection follow each other both ways,
  for text (`test-xwindow` copies with `xclip` and pastes with `clip`, then
  the reverse). All 36 points are done.
  **Steam's sign-in window is on hyprix (2026-09-29):** Valve's client,
  unchanged, downloads and installs itself from the bootstrap, starts its
  browser helper, and the helper draws "Sign in to Steam" through yserver
  on hyprix (`cargo xtask test-steam-window`, from the volume
  `tools/common/fetch/fetch-steam-window.sh` makes; `docs/STEAM.md`). It runs
  with launch-side workarounds and no kernel change of its own: a stand-in
  for the steamrt64 entry point instead of pressure-vessel, a logger that
  logs nothing, and a 16 GiB guest. `docs/STEAM.md` §3 lists each with the
  fix that retires it and its owner. The three shims preloaded at first
  went the same day, each for a kernel fix: `stat` through
  `/proc/<pid>/fd/<n>` reaches a socket, `/proc`'s inode numbers and
  directory offsets fit a 32-bit `readdir` (`cargo xtask test-procfs`), and
  `pipe2(O_DIRECT)` makes a packet pipe.
* **Sound.** Playback is done, 2026-09-26 (`docs/AUDIO.md` §8): a
  `virtio-snd` driver in ring 3, the audio core and `/dev/snd`, and Chrome
  playing through them, which `test-audio` and `test-chrome-audio` gate.
  A driver that dies is started again by devmgr and the card comes back as
  `C0`, a dead driver's DMA pins kept by the kernel until the next driver has
  reset the device (F-38, 2026-09-26). alsa-lib and `aplay` are built on
  ferrousli and play through it (U1, 2026-09-27). What stage 22 still needs
  of it is the server. As it was written: Ferrix had no audio at all: a `virtio-snd` driver in ring 3,
  an audio core with a `/dev/snd` shaped enough for a client library, and a
  server speaking the PulseAudio or PipeWire protocol over a Unix socket,
  which is what Steam and every game link against. 30 points as a first
  guess. `docs/AUDIO.md` is the design of the first two parts (drafted
  2026-09-26; the customer moved it forward to current work the same
  day): 24 points for the driver, the core and a gate on QEMU's `wav`
  backend, with the server unsized.
* **The GPU, with Vulkan.** Path A gives OpenGL, which the client and a
  native game can use. Proton draws with Vulkan through DXVK, and Vulkan
  under virtio-gpu is Venus, which needs a Linux host with KVM: on the
  Windows machine, Proton games wait for stage 21, and native GL games do
  not. Venus is Path A's steps 1 and 2 again for a second capability set,
  8 points on top of Path A, and Mesa's Venus driver on ferrousli or a Rust
  Vulkan loader over it, which is Path A's 3a question asked a second
  time.
* **Wine and Proton.** Not Ferrix's to write and everything Ferrix's to
  run: Proton is Wine plus DXVK plus the runtime, and every one of its
  kernel needs is one of the bullets above. What it will find missing is
  found by running it.

**Exit,** in three steps, each a boot of its own the way stage 18's and
19's are: the Steam client starts on Ferrix, logs in and shows its store,
with the browser helper drawing; a native Linux game from the library
installs to btrfs, launches and draws through the GPU path with sound; and
a Windows game runs through Proton. The first two are a guest's exit on
the Linux host; the third is the GPU's Vulkan, wherever that comes first.

What Wine needs of the kernel, DXVK on NVIDIA's Vulkan and a first Windows
game run as a plain program are [stage 23](stage-23-arknights-endfield.md)'s
steps 1 to 3 since 2026-10-10, sized there and not here. The third step
stays this stage's: a Windows game that Steam starts through its own Proton
in the runtime's container, which is stage 23's step 3 with N5 to N7 under
it.

The first step is met, and gated (2026-09-30): on the `--everything`
desktop, with its 64-bit side on ferrousli rather than glibc, the client
starts, its browser helper draws the sign-in window, and the client shows
its store. The customer signed in first with the Steam app's QR code
(`docs/STEAM.md` §5 lists what ferrousli needed for it); then `cargo xtask
test-steam-store` did it with no one there (`docs/STEAM.md` §1): it boots
that desktop's Steam on ferrousli, requires the sign-in window drawn, types
in a test account's name and password from
`~/.config/ferrix/steam-test-account` on the gate host (Steam Guard off,
never in a checkout), and requires the store on the screen. It passes on
nazuna with the account `ferrixsteamtest` since 2026-09-30. fuzzel lists
Steam on that desktop, so a closed client can be started again (not yet
tried on a live desktop).

What is still a workaround is `docs/STEAM.md` §3's list. The helper's
sandbox and pressure-vessel wait on stage 13: user namespaces (N4) and
then pid namespaces and a loopback-only network namespace, os-98's; seccomp
S1 to S6, os-7c's; and Steam's own filters, S7 and S8 (`docs/SECCOMP.md`).
The helper's GPU process is off (`-cef-disable-gpu`), so the store draws in
software. Its route is ANGLE on Vulkan through Venus: F-55, a kernel page
fault on a copy from a Venus window, is closed (c5c92781), and what is left
is user copies through a device window's own mapping, the render node for
a `render` group, and the helper's flags, 14 to 23 points, designed with
the certification consultant's conditions and not started
(`docs/STEAM.md` §6).

The second step has its gate and did not pass on 2026-10-01:
`cargo xtask test-steam-game` signs in as `test-steam-store` does, asks
the client to install Teeworlds, a free native game the test account owns,
presses Install, follows the download and is to start the game and require
its window drawn (`docs/STEAM.md` §1 and §7). The download works since two
gateway landings let the guest take Steam's dozen connections at once
(3b1b1de7, 020dc9b2: about 90 s for 84 MB where it had crawled at under
0.3 Mbps). Then Steam stands still staging the files, for a reason not
yet known (BACKLOG; a file walk over them from another process hangs too,
on the btrfs volume and on tmpfs alike). After that come
the runtime Steam installs beside a native game, the Steam Linux Runtime
1.0 (scout), whose container needs pressure-vessel; the GPU path; and
sound.

Since 2026-10-10 there is a way around the stall for a native game:
`--steam-preinstall` installs Teeworlds and the scout runtime with `steamcmd`
on the host and merges them into the `--everything` volume, where Steam finds
them installed (ee9eade0b, 62d6369b5), and `ferrix_direct`, a compatibility
tool the client script maps a game to, starts the game outside the runtime's
container (3a2740642), so pressure-vessel and N5 to N7 are not needed for it.
The in-guest install's staging stall is still unexplained, and
`docs/STEAM.md` does not record the gate passing, but the customer reports (2026-10-21) that Steam runs on Ferrix and a game has also run.

None of it is sized past a first guess, and the sum of the first guesses is
already over 300 points, so the stage is written as a list of what has to be
true rather than a plan.

---

