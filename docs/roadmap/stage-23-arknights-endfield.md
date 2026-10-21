# Stage 23 — Arknights: Endfield  ·  *≈ 205 points as a first guess, step 5 unknown*

The Windows game *Arknights: Endfield* runs on Ferrix: through Wine, drawing
with DXVK on NVIDIA's Vulkan, on the RTX 3060. Put on the roadmap by the
customer on 2026-10-10 as the road after Steam ("update the roadmap to
Endfield"). On that day nothing of it was built; steps 1 to 4 have begun since,
as each step below says. A feasibility study was written the same
day (`~/.local/share/ferrix/endfield/STUDY.md` on nazuna, research only); this
page is that study's route as a stage, with each claim about the tree checked
against `main` 557b939ed and the four places it was wrong corrected below.

**Exit.** The game reaches its world past the login on Ferrix and is playable
at 1920x1080 and 60 frames a second on the RTX 3060: step 6 below. It is a
recorded run on the card with the customer's account, not a boot that stays
in CI, because the game is closed, needs an account and needs the card; the
gates that stay are those of steps 1 to 3, which need neither the game nor an
account.

## How it relates to stage 22

[Stage 22](stage-22-steam.md)'s third exit step is "a Windows game runs
through Proton", and its last bullet says of Wine and Proton that what they
find missing "is found by running it". This stage is where it is run, and the
two are not counted twice:

* **Steps 1 to 3 here are stage 22's Wine bullet made concrete.** What Wine
  needs of the kernel, DXVK on the card and a first real game are sized here
  and nowhere in stage 22 (its guess of 23 points for "what the Steam
  client's launch and games still find missing" stays what it was, for the
  client and native games).
* **Stage 22's third step stays stage 22's.** It is a Windows game started
  by Steam, from its library, through the Proton that Steam installs, inside
  the Steam runtime's container. This stage does not go that way first: it
  runs Wine as a plain program, outside Steam and outside the container,
  because the container needs bubblewrap as uid 1000 (N5 to N7,
  `docs/NAMESPACES.md` §9, not built) and because Valve's Proton has been
  the build without the anti-cheat's patches (below).
* **Step 7 here, the optional one, is where the two meet.** When Steam
  installs and starts this game by itself, stage 22's third step is met by
  it. Stage 22 may meet that step sooner with any Windows game from Steam's
  library, once step 3 here and N5 to N7 are done.

## The game, as the study found it

Read by the study on 2026-10-10 in the sources it names; not read again for
this page.

| What | Finding | Source |
|---|---|---|
| Release | 2026-01-22 on Windows, PS5, Android and iOS; Unity, modified | [Wikipedia](https://en.wikipedia.org/wiki/Arknights:_Endfield) |
| Distribution on a PC | Its own launcher (`Program Files/GRYPHLINK/Launcher.exe`) and the Epic Games Store | [RPG Site](https://www.rpgsite.net/news/21525-arknights-endfield-steam-release-date-new-update-operators), [Lutris](https://lutris.net/games/install/39765/view) |
| Steam | Announced for 2026-10-15 (UTC+8) with version 1.6 and cross-progression; the store page said "Oct 14" and "not yet available" | [Automaton](https://automaton-media.com/en/news/arknights-endfield-comes-to-steam-on-october-15-cross-platform-saves-supported/), [Steam app 4732690](https://store.steampowered.com/app/4732690/Arknights_Endfield/) |
| Requirements | Windows 10 64-bit, DirectX 11, 16 GB of memory, 60 GB of disk; recommended an i7-10700K and an RTX 2060. About 45 GB more for unpacking was reported at launch | Steam page |
| Graphics API | Only DirectX 11 is listed; no DirectX 12 or Vulkan option was found | Steam page |
| Anti-cheat | Anti-Cheat Expert (ACE, Tencent), "kernel-level" on Windows | Steam page, [Wikipedia](https://en.wikipedia.org/wiki/Anti-Cheat_Expert) |
| Account | A Gryphline account, linkable to Steam; a third-party EULA | Steam page |
| On Linux | Played since launch with DW-Proton, a patched Proton, as a non-Steam game or through Lutris; ACE's driver runs inside Wine, in user space. One report of about 4K60 on an RTX 4060 Ti | [AreWeAntiCheatYet #1905](https://github.com/AreWeAntiCheatYet/AreWeAntiCheatYet/issues/1905), [Dawn PR 516](https://codeberg.org/Krock/dawn/pulls/516), [hyperlogos](https://hyperlogos.org/blog/drink/Arknights-Endfield-Linux) |

Inferred by the study, not confirmed: DirectX 11 means the translation layer
is DXVK and not VKD3D-Proton, and the RTX 3060 with 12 GB is above the
recommended card. Not known when the study was written: the launcher's UI
technology, whether the installer or the launcher is a 32-bit program, how
the login is shown, and patch sizes. The first two are answered under step
4 (2026-10-13): a 32-bit NSIS installer, and a 64-bit launcher built on Qt
5.15 with QtWebEngine.

## The route

A patched Proton build's Wine (DW-Proton first, then GE-Proton), run directly
as a glibc program with a `WINEPREFIX`, the official installer, DXVK on
NVIDIA's Vulkan, and Wine's X11 driver against yserver.

* **Outside Steam and its container first**, for the reason above.
* **X11 before Wayland.** yserver has the extensions Wine's X11 driver asks
  for (the study's reading of yserver's sources, which are not in this
  tree). Wine's Wayland driver would need NVIDIA's Wayland presentation to
  hand hyprix its buffers; hyprix has no explicit-sync protocol
  (`wp_linux_drm_syncobj_manager_v1` is not among its globals).
* **On glibc, not ferrousli**, to keep the C library out of the first passes.
* **Not the Android version.** It would need binder and an Android
  userspace, a translator for Arm code, and a Mesa-shaped graphics stack;
  none exists on Ferrix (the study's inference; no report of it was found).

## What it depends on

* **[Stage 21](stage-21-bare-metal-gpu-ferrix.md), and what of it is not
  landed.** `main` has NVIDIA's Vulkan for Chrome's WebGL (N2 to N4). A
  window of an X program drawn by the card is on branches only: `nv-next`
  (DRI3 and Present in yserver over the card, VRAM dmabufs, the zero-copy
  path, more mapping slots, modifiers) and `nv-next-stall`, both pushed and
  not landed, with `cargo xtask check` red on `nv-next`
  (`git show nv-next:docs/handover/2026-10-10-nvidia.md`). Steps 2, 3 and 6
  stand on those branches landing, which is stage 21's work and its review,
  not counted here.
* **Where that stood on 2026-10-10.** An X11 OpenGL game (Teeworlds) drew
  through NVIDIA's GLX over yserver's DRI3 on the card for the first time:
  85 to 94 swaps a second between stalls of about 5 s. The stall's cause is
  found: `nvrm` never delivers the semaphore-surface callback, so a
  presentation's sync fence is never signalled and ends at a 5 s deadline
  (`~/.local/share/ferrix/steam-race/present.md`). A fix is being written
  and is not landed. As a desktop `nv-next` stalled when its GPU clients
  started together; `nv-next-stall` came up once without that, which does
  not yet establish a fix.
* **A Linux baseline on the same card** exists since 2026-10-10
  (`~/.local/share/ferrix/linux-baseline/README.md`): Debian 13 with the
  same NVIDIA 580.173.02 in a libvirt guest of the same shape (8
  processors, 16 GiB, the 3060 passed through). The same Teeworlds reads 220
  frames a second there on software GL where Ferrix reads 83, and 2,400 on
  NVIDIA's GL. Step 0 uses that guest, and step 6 is judged against it.
* **Stage 22**, for what already runs: 32-bit x86 programs (I1 to I4),
  yserver, sound through `pulsed`, the network and TLS that Steam signs in
  and downloads over, and a root on btrfs. Stage 22's container (N5 to N7)
  is needed only by step 7.
* **Stage 13**, if step 5 takes seccomp's way: `SECCOMP_RET_TRAP` needs
  `SIGSYS` delivered, which is S4, on `main` since 2026-10-18.
* **The certified item.** Steps 1, 3 and 5 change what the kernel answers to
  programs. Part of that is in the certified item's core ring
  (`src/kernel/src/arch/x86_64/syscall.rs` and whatever else reaches
  `arch/**`, `sched/**`, `user/**`, `mm/**` or `signal_frame.rs`): E1.1, E1.6,
  and of E1.3, E3.6, E5.2 and E5.3 whatever reaches those. Part is in the
  load ring, which carries no assurance claim (the futex, memory and process
  calls under `src/kernel/src/syscall/`). Every change to a core or item
  file, and every load-ring change that adds `unsafe`, a countermeasure or a
  check the evidence relies on, goes to the certification consultant before
  it lands (`docs/CONVENTIONS.md`), with its requirement ids reserved first;
  E1.1, E1.6, E5.2 and E5.3 are structural and owe a design review before
  code. Nothing on this page changes the item's boundary or claims evidence
  for it.
* **The customer**, for three things no session can supply: an account the
  customer can afford to lose (below), the disk (the game wants about 60 GB
  and about 45 GB more to unpack; nazuna's root volume had 88 GB free on
  the evening of 2026-10-10), and the card, whose use is the product
  owner's word each time.

## The steps

Eight steps, the study's M0 to M7, in the order they can be proved. Points
are first guesses made before any of it ran, in the roadmap's sizes; a step
full of unknowns may take twice its guess ([Status](status.md), the
qualifications). Steps 1 and 2 share nothing and can run side by side. Step 3
comes before step 4 so that Wine and the GPU path mature with no anti-cheat
in the picture. Step 0 is first because it costs no engineering and its
answer can end the stage.

### Step 0: the game on plain Linux, in the same VM shape  ·  8 points

No Ferrix work. The Debian guest of the baseline above, the 3060 passed
through, DW-Proton, the official installer.

* **E0.1** Room for it: about 110 GB for the guest's disk, on nazuna or on a
  disk added for it. 1 point, and the customer's decision.
* **E0.2** DW-Proton in the guest, the installer, the download, the
  unpacking; the recipe written down as it was done. 2.
* **E0.3** The run: login, the anti-cheat, the world. What ACE says about a
  KVM guest with a passed-through card is the answer this step exists for. 1.
* **E0.4** The same with GE-Proton and with an unpatched Wine, to know
  which of DW-Proton's patches the game cannot do without. 2.
* **E0.5** What Ferrix will be asked: a system-call count of Wine and the
  game over a session (`strace -f -c`), every call sorted into answered,
  refused and missing against `src/lib/proto/linux-abi/src/nr.rs`; the
  Vulkan extensions DXVK enables; video memory in use; and the frame rate at
  1920x1080, which is the figure step 6 is judged against. 2.

**Proved by** a recorded run in which the game reaches the world past the
login and the anti-cheat, twice. **If ACE refuses the guest, the stage stops
after step 3**: steps 1 to 3 still serve stage 22, and steps 4 to 6 wait
until Ferrix runs on the machine rather than in it, which is stage 21's own
exit (an inference: nobody has reported this game under Wine in a VM either
way).

**Where step 0 stands (2026-10-13): ACE refuses the virtual machine.** On
the Linux guest `linux-3060` (Debian 13, DW-Proton `dwproton-11.0-14`, the
RTX 3060 passed through), GRYPHLINK's downloader 1.6.0.1607 ran and the game
downloaded. At the game's start, ACE Center said: "The application can not
run under a virtual machine. Please do not run game in virtual environment
or system made from virtual environment image. (13-65544-2)". By this
step's own rule, steps 4 to 6 wait for Ferrix on the machine rather than in
it, and the customer chose that way the same day. The game's data has
since been deleted from the guest on the customer's word.

* *Done:* E0.1, E0.2, and of E0.3 the answer the step exists for.
* *The machine (2026-10-15):* the customer's N150 box, bare metal, with
  software rendering; the plan, the disk image and its QEMU gate are
  [`docs/N150.md`](../N150.md) (`n150-install`, `test-n150`).
* *On the machine (2026-10-15 and 16, from the customer's Windows PC):*
  Ferrix, built from branch `ahci` (4a0db8a97), booted interactively on
  the N150 box: hyprix's desktop on the
  firmware framebuffer, the terminal on the USB keyboard, `/` on btrfs on
  the box's SATA drive (a new AHCI driver, [`docs/AHCI.md`](../AHCI.md)),
  the YT6801's link up at 1 Gbit, and `gryphlink` under Wine showing the
  launcher with its online notices. Not logged in, nothing downloaded.
  Performance is really bad: the framebuffer was uncached (write-combining
  is on `main` since, untried on the box) and, with no MSI arriving, USB
  and the NIC poll ([`docs/N150.md`](../N150.md) §13). The kernel
  bring-up is on `main` since the n150k landing (ledger 1884), evidenced on QEMU only and not yet run on the box with checks on (BACKLOG).
  The box has no NVIDIA card: its GPU route is the Intel GPU driver
  ([stage 21's page](stage-21-bare-metal-gpu-ferrix.md), `docs/INTEL-GPU.md`),
  whose M1 and M2 ran on the box on 2026-10-20; the first Endfield run on it
  (the launcher on lavapipe, only the game on anv) is prepared, not made.
* *Not known, and worth stating:* whether ACE accepts Wine or Proton on
  Linux on bare metal. The reports above are of that, not of a VM, and
  none was checked here; ACE has broken under Wine more than once in 2026.
* *Not done:* E0.4 and E0.5, which need the game past ACE.

### Step 1: plain Wine starts on Ferrix  ·  42 points

When this page was written 64-bit Wine could not start: it keeps Windows's
thread block at `%gs`, and `arch_prctl` answered `ARCH_SET_FS` alone, with
`FSGSBASE` left off for programs. It answers `ARCH_SET_GS` since
2026-10-10, and Wine starts; where the step stands is below the list.

* **E1.1** `arch_prctl`'s `ARCH_SET_GS`, `ARCH_GET_GS` and `ARCH_GET_FS`: a
  thread's `GS` base kept across a switch, a `fork` and a signal, and a
  kernel address refused, which the paranoid entry's rule depends on
  (`src/kernel/src/arch/x86_64/paranoid.rs` says so in advance). The channel
  round trip's cut that leaves `FS` and `GS` unloaded when both are zero has
  to hold with a base that is not. `FSGSBASE` stays off unless Wine is shown
  to need it. With its requirements, its check and the check's negative
  control. 8.
* **E1.2** A Wine volume and a gate: a fetch script for a pinned Wine on
  glibc, and `cargo xtask test-wine`, which boots it under QEMU with no card
  and requires `wine64 cmd /c ver` to print. 5.
* **E1.3** What `wineboot` trips over. Known to be missing or untried:
  Wine's address-space reservations (large `PROT_NONE` ranges, the low
  4 GiB, the preloader) and how they are charged; `mlock` and `munlock`;
  `membarrier`. The rest is found by running it. 13, a guess.
* **E1.4** Synchronisation through wineserver alone first, then esync,
  whose `eventfd`, `epoll` and descriptor limit are there. 3.
* **E1.5** `notepad` draws a window through Wine's X11 driver on yserver, in
  software, and takes keys and the pointer. 5.
* **E1.6** 32-bit Windows programs in the 64-bit Wine (its new WoW64):
  32-bit code inside a 64-bit process, far transfers to the 32-bit code
  segment that I1 put in the GDT, and a signal taken while in it. Needed if
  the installer or the launcher is a 32-bit program, which is not known.
  `modify_ldt` is needed only by old-style 32-bit Wine and stays out. 8.

**Where step 1 stands (2026-10-12; `docs/WINE.md`).**
Debian 13's 64-bit Wine 10.0 makes a prefix, its `cmd /c ver` answers
`Microsoft Windows 10.0.19043`, from the 64-bit `cmd` and from the
32-bit one in `syswow64`, and its notepad draws a window on hyprix
through yserver at the size of its tile and takes a typed line, under KVM:
`cargo xtask test-wine`. `cargo xtask run-compositor --wine` shows it on a
desktop to type into.

* *Done:* E1.1, with `CR4.FSGSBASE` now cleared at bring-up, which nothing
  did before. E1.2. Of E1.3, the one thing `wineboot` tripped over in the
  kernel: a 64-bit signal frame's `sigcontext` left `trapno`, `err` and
  `cr2` zero, which Wine's `SIGSEGV` handler reads. None of the calls E1.3
  names (`mlock`, `membarrier`, the reservations' charging) was met by
  `wineboot`, `cmd` or notepad. E1.4's first half: wineserver alone.
  E1.5 (2026-10-12), with no workaround left: yserver lists no RandR
  provider, as Xwayland does, so Wine finds `WAYLAND-1` at 96 DPI and drops
  `UseXRandR=N`; yserver sets `WM_STATE` on a mapped top-level, as a window
  manager does, so notepad takes its tile's size; and the gate types
  `hello ferrix` into it, finds the line on the screen and reads it and a
  double-clicked word back through X's clipboard. Nothing in the kernel was
  needed for it. A prefix on the btrfs volume ran out of space because the
  volume was too small (683 MB free for a prefix that takes 1.1 GB there),
  not because of btrfs's allocation; `fetch-wine.sh` gives it 2 GiB of room.
  E1.6 (2026-10-12, branch `endfield-wow64`): the 32-bit Windows
  DLLs from Debian's i386 `libwine` on the volume, and four things the
  kernel answered otherwise than Linux once 32-bit code ran inside the
  64-bit process -- the frame a handler is entered on, the code selector a
  64-bit frame records and gives back, `ARCH_SET_FS`'s null selector, and
  `SS` after `SYSRET` on AMD (`docs/WINE.md` §7). `modify_ldt` was not
  needed; `wineboot` starts the 32-bit `rundll32.exe`. Reviewed by the
  consultant (ledger line 762, OK IF W1 to W7, met in the landing).
  A 32-bit program draws a window (2026-10-15): `window32.c`, built with
  mingw-w64's i686 compiler, says it runs as an i386 process under WoW64
  and paints its window, which hyprix lists, in `test-wine`'s desktop boot.
  E1.4 (2026-10-15): Debian's Wine 10.0 has no esync, fsync or ntsync, and
  GE-Proton11-7 no esync either -- Proton dropped it -- so the fast path is
  Proton's fsync, for which the kernel gained `futex_waitv` (449; consultant
  OK IF (1) to (10), ledger line 1172). Proton's Wine takes fsync on Ferrix
  from its 64-bit and 32-bit sides, and a turn handed between two threads
  takes 4.9 µs instead of 90.6 (`test-wine-sync`, `docs/WINE.md` §11).
* *Still to do:* whether btrfs's write path leaves more of a
  full volume unallocated than Linux would (135 MB of 2 GB on the full Wine
  volume), filed in `docs/BACKLOG.md`.

**Proved by** `cargo xtask test-wine`: a prefix is made, `cmd /c ver`
prints, and `notepad`'s window is found on the screen with a typed line in
it, the way `test-xwindow` finds xev's.

### Step 2: a Direct3D 11 program draws through DXVK on the card  ·  18 points, on stage 21's branches

* **E2.1** `vkcube` on yserver through NVIDIA's X11 presentation at the
  screen's rate. It waits on the fence fix and on `nv-next` landing (above);
  what it finds beyond them is this point. 5.
* **E2.2** DXVK in the prefix, and a small Direct3D 11 program drawing on
  the card. A full `vulkaninfo` from Ferrix on the 3060, which does not
  exist yet, is compared with the extensions and features DXVK requires
  (the study checked them only against the same userspace on the host's
  other card). Two known weak places: `VK_NV_optical_flow` fails device
  creation, and `VK_EXT_external_memory_host` rests on a stub (the NVIDIA
  handover and the study). 5.
* **E2.3** Ten minutes at a steady rate: a frame-time record, no fence
  ended by its deadline, no request left unanswered in `nvrm`. 3.
* **E2.4** A gate without the card: the same Direct3D 11 program through
  DXVK on Mesa's lavapipe under QEMU (`cargo xtask test-dxvk`), so that
  Wine with DXVK is held by a boot that every session can run. That lavapipe
  satisfies DXVK is assumed, not tried. 5.

**Where step 2 stands (2026-10-12, branch `endfield-dxvk`; `docs/WINE.md`
§6).** Lavapipe satisfies DXVK: DXVK 3.1.1, the newest release, makes a
feature level 11.1 device on Mesa 25.0.7's lavapipe, in Wine 10.0, on Linux
and on Ferrix alike. `cargo xtask test-dxvk` runs a small Direct3D 11
program through it under KVM with no card: offscreen, comparing the frame
it reads back with one xtask draws, then presenting through a swap chain
in a window of hyprix's, by way of Wine's Vulkan surface on yserver's X11,
and finding the frame on the screen.

* *Done:* E2.4, and the card-free half of E2.2: DXVK in the prefix (the
  release's DLLs beside the program, pinned by hash, from
  `fetch-dxvk.sh`), a Direct3D 11 program offscreen and presenting, the
  gate and its negative controls. Ferrix answered nothing wrongly on the
  way.
* *Still to do:* the presented window is drawn at the size the program
  asked for and not at its tile's, as notepad's is (step 1's lead, in
  Wine's X driver on yserver; fixed with E1.5, not here). E2.1 to E2.3,
  on the card.

**Proved by** `vkcube` at the screen's rate on the card, then the Direct3D
11 program for ten minutes, recorded; and `test-dxvk` in the gate set.

### Step 3: a real Unity game on DirectX 11, without anti-cheat  ·  54 points

A 64-bit Unity game that draws with DirectX 11, has no anti-cheat and runs
under the same Wine with no Steam beside it. It is chosen and first run in
step 0's guest.

* **E3.1** The game chosen, with the customer, and run on the Linux guest
  for its figures. 2.
* **E3.2** Sound: Wine's PulseAudio driver against `pulsed`. 3.
* **E3.3** The mouse. XInput2's raw motion, grabbing and confining the
  pointer. On 2026-10-10 the mouse in Teeworlds lagged and after a while
  stopped answering, unexplained (the NVIDIA handover). 8.
* **E3.4** Full screen and a change of mode: yserver offers one synthetic
  RandR output, and a game's switch of resolution through Wine is untried. 5.
* **E3.5** Faster synchronisation, if wineserver's and esync's are too slow
  by measurement: `futex_waitv` (Wine's fsync; done 2026-10-15 with E1.4,
  `docs/WINE.md` §11), or ntsync, which is the
  larger. 8.
* **E3.6** What the game finds missing in the kernel. Candidates the tree
  shows: `FUTEX_WAKE_OP` and the priority-inheritance futexes are `ENOSYS`,
  `sched_setaffinity` is accepted and not applied, `rseq` is `ENOSYS`, and
  `preadv`, `sendmmsg`, `close_range` and `timer_create` are not in the
  table. 13, a guess.
* **E3.7** The card under a game's load: gigabytes of textures, more
  mappings than anything has made so far, shader compilation on many
  threads. Nothing larger than Chrome's WebGL, an AV1 decode and Teeworlds
  has run. 13, a guess.
* **E3.8** The record. 2.

**Where step 3 stands (2026-10-13, branch `endfield-s3`; `docs/WINE.md`
§9).** A real Unity game runs on Ferrix with no card: Unity's *Spaceship
Demo* (Unity 2021.2.3f1, the High Definition Render Pipeline, Mono,
64-bit, Direct3D 11, no anti-cheat, downloaded with no account and pinned
by hash) starts in Wine 10.0, makes a feature level 11.1 device through
DXVK 3.1.1 on lavapipe, loads its benchmark's scene and draws its
walkthrough of the ship in a window of hyprix's through yserver, under
KVM: `cargo xtask test-unity`, about 1.3 frames a second at 1024 by 768 on
8 processors.

* *Done:* E3.1's first half, the game picked for a gate (the page asked for
  it to be chosen with the customer and run on step 0's guest first; it was
  run on the Linux host instead, the guest holding the card): it stands in
  for Endfield's engine, API and renderer weight, not for IL2CPP, which no
  freely fetchable Direct3D 11 title was found with. A fetch script, the
  gate, two negative controls. Of E3.6, nothing so far: Ferrix answered
  nothing wrongly between `wineboot` and the walkthrough.
* *Still to do:* the customer's word on the game (E3.1), and its figures
  on step 0's guest. An IL2CPP title. Input into the game, which the
  benchmark needs none of (E3.3), full screen (E3.4), sound (E3.2: Wine
  finds no audio driver and Unity runs on FMOD's silent output). The rest
  of E3.6 is found by playing it, which the walkthrough is not. `wineboot`
  waits 300 s on a Wine tree with a 32-bit side, where a 32-bit
  `rundll32` hangs (E1.6's).

**Proved by** an hour of play, recorded: sound, the mouse without lag, full
screen, and no failed allocation or mapping in `nvrm`'s log.

### Step 4: Endfield's installer, launcher and login  ·  25 points

* **E4.1** Somewhere to put it: a data volume above 110 GB for the guest, on
  btrfs (the Steam volume is about 7 GB today). 3.
* **E4.2** The installer runs, with the core fonts in the prefix; E1.6 if it
  is a 32-bit program. 3.
* **E4.3** The launcher's window. Its technology is not known; if it is a
  browser engine built for Windows, that Chromium and Steam's helper run on
  Ferrix as Linux programs says little about it. 8, a guess.
* **E4.4** 60 GB down: one connection makes 10 to 15 MB/s through xtask's
  gateway, and more needs TCP window scaling there (`docs/STEAM.md` §7,
  BACKLOG). 5.
* **E4.5** Unpacking about 45 GB on btrfs, and what a full volume does to
  it. 3.
* **E4.6** The login: the account, the page it is typed into, TLS. 3.

**Proved by** the launcher showing its start button with the files verified.

**Where step 4 stands (2026-10-14; `docs/WINE.md` §8 and §10).** It stopped when step 0 found that ACE refuses a VM. Before
that, GRYPHLINK's launcher drew its first screen on Ferrix under KVM, in
Debian's Wine 10.0, in software, on hyprix through yserver, with its news
and pictures fetched from the network through xtask's gateway. Nothing was
logged into and nothing of the game was downloaded.

* *Found:* the installer, from the download endfield.gryphline.com links,
  is a 32-bit NSIS program; what it installs is 64-bit: `Launcher.exe`,
  which starts `Games.exe`, a Qt 5.15.8 program whose interface is a local
  web page in QtWebEngine (Chromium 87), with the VC++ runtime, OpenSSL
  1.1, libcurl and ANGLE beside it. No WebView2, no .NET, no system VC++
  runtime. The installer needs E1.6 (landed since); the launcher does not,
  and the branch lays the payload out as the installer does, unpacked on
  the host with 7-Zip.
* *Found and fixed (yserver 1a8752e8, 2026-10-13):* yserver refused
  `XF86VidModeSetGamma` with `ClientNotLocal` while it gave local clients
  the write permission, and Wine's X driver makes that call with no error
  handler, so the X error ended `Games.exe`. It now answers as Xwayland
  does (success, nothing applied), and the launcher starts with no Wine
  setting added (`docs/WINE.md` §8).
* *Found and fixed (branch `endfield-mmsg`):* the launcher's own queries
  to Gryphline (`launcher.gryphline.com`, `game-config.gryphline.com`)
  never connected, because their name lookups failed on Ferrix. Wine's
  `ws2_32` calls glibc's `getaddrinfo` with `AF_UNSPEC` and no
  `AI_ADDRCONFIG`, glibc then sends the A and AAAA queries together with
  `sendmmsg`, and Ferrix had neither `sendmmsg` nor `recvmmsg`, so glibc
  took `ENOSYS` as a failed query and answered `EAI_AGAIN`. Both calls are
  in the kernel now, and a static glibc probe that failed both names with
  flags 0 resolves them (`docs/WINE.md` §8). With the fix the launcher
  reached its server on Ferrix: Gryphline answered its version query with
  200 (`test-endfield-launcher`).
* *Done (2026-10-14):* the launcher's gate, `test-endfield-launcher`, and
  the same in GE-Proton11-7's Wine, `test-endfield-proton`, which the
  customer asked for because Debian's Wine 10.0 draws the launcher's
  translucent dialogs blank. In Proton the install dialog draws whole on
  Ferrix, in Open Sans for the Segoe UI the launcher names (`docs/WINE.md`
  §10). Proton needed two kernel fixes (f0c812f01): `mprotect` kept no
  `MAP_SHARED`, and a write-only region refused a read.
* *Done (2026-10-14):* E4.1 for a comparison run in the VM: a 160 GB
  btrfs image on the SN720's `ferrix-root` partition, attached writable.
  The download first failed at once: Wine's Schannel lacked GnuTLS, and
  the launcher's download SDK (libcurl over OpenSSL, roots from the
  Windows store) found no roots, because Wine's `crypt32` reads
  `/etc/ssl/certs/ca-certificates.crt` and the boot had the bundle only on
  `/data`. With GnuTLS on the volume and the bundle linked, the download
  ran at about 3.4 MB/s through xtask's gateway, against 31 MB/s from the
  host (E4.4; `docs/WINE.md` §10), until, after about 5.8 GB, every write
  was answered `ENOSPC` with some 150 GB unallocated: the 8 GiB guest ran
  out of frames, because a writable btrfs mount's page cache is never
  reclaimed (open; reclaim for its clean pages is a certified-core change).
  Wine, Proton and the launcher are on the `--everything` desktop as
  `gryphlink` (`test-endfield-proton --everything`).
* *Found, 2026-10-15 (a scratch run on the volume, not on `main`):* the
  reclaim is the branch `mm-pcache`; L.object.188-190 (reclaim of a
  writable mount's clean pages, the write that reclaims before it answers
  `ENOSPC`, the cache's totals in `/proc/meminfo`) are reserved on `main`
  (77e87403a) and the code **landed on 2026-10-15** as f0bc9ab2d. A second download (install5, 4
  vCPUs, 56.9 GB in 54 parts) ran at 2.0 to 3.3 MB/s and stopped at 00:22
  after 9.5 GB, with the button reading "Download Error": the guest's
  virtio-net driver had been restarted twice by `devmgr` (status 7, the
  serve loop stopped on a fault, about 23:29 and 00:06) and after the
  second restart sent nothing (the process at 13 to 25% CPU, xtask's
  gateway idle). A bug in the driver restart, not the disk: the net ring's
  shared indices were composed from bytes and tore. Every one is a single
  aligned access, and a ring the kernel ended makes the driver leave, since
  2026-10-15 on `main` (36a1bea84, 11d006073, bdccab15d; the branch
  `net-restart-stall`). The launcher's looping background
  video, not the download, was taking about 2.6 of the 4 vCPUs; its
  STATIC switch (a pill at the top of the window, in memory only, so
  clicked after every launcher start) pauses it. install6 (8 vCPUs, a
  fresh image, from the scratch branch `ef-night`, which never lands)
  downloads at about 3.7 MB/s. A restart starts the download over: the
  Wine prefix is on tmpfs, so a new launcher has no download path and
  clears its tasks; `ef-night` keeps the prefix on the volume
  (`FERRIX_ENDFIELD_KEEP`), and that the launcher then resumes is
  unproven.
* *Still to do:* the image path of another process (`ptrace` and
  `/proc/<pid>/mem`, E5.2, which the launcher asks for every five seconds
  and Wine cannot give it on Ferrix); the current launcher version; a download run again on
  `main`, now that `mm-pcache` and the net ring's fix are on it; a resumed
  download shown after a restart; and, when Ferrix runs on the machine, E4.1, the
  installer itself (E4.2), then E4.4 to E4.6.

### Step 5: past the anti-cheat  ·  29 points as a guess; unknown

What ACE's driver, running inside DW-Proton's Wine, asks of the kernel under
it. Nobody knows before step 0's trace (E0.5) and a first run. **Nothing in
this step is built to hide anything from the anti-cheat.** Ferrix answers
what Linux answers, truthfully about itself; if ACE refuses that, the step
ends and the customer decides.

* **E5.1** Step 0's trace read against what steps 1 to 4 built: the list of
  what is still missing. 3.
* **E5.2** Reading and writing another process's memory: `ptrace`,
  `process_vm_readv` and `process_vm_writev` are not in the table, and
  whether `/proc/<pid>/mem` serves was not checked. `ptrace` is the large
  one (the customer decided on 2026-10-21 that `ptrace` is compiled out of the
  certified present profile and exists in debug builds, 62d2b09da): it reaches the certified item's core (another process's memory and
  stopped state) and owes a design review before code. 13, if it is needed.
* **E5.3** Windows system calls made directly by the game's or the
  anti-cheat's code, which GE-Proton catches with Syscall User Dispatch
  and DW-Proton with a seccomp filter that returns `SECCOMP_RET_TRAP`.
  Both are built: S4's `SIGSYS` for `TRAP` (on `main` since 6c025174e, ledger line
  2331) and Syscall User Dispatch (on `main` since 2026-10-18,
  `docs/SECCOMP.md` §13, ledger line 2323; `test-sud`, and `test-wine-dispatch`
  runs a PE probe's direct call in GE-Proton), each landed on its own review. DW's filter also needs
  Ferrix's mmap layout to match Linux's (`ENDFIELD-GAME.md` (b)), which is
  not part of either. 8.
* **E5.4** What Ferrix says of itself. `uname -s` says `Ferrix` (the
  customer's choice, 2026-09-13), Wine can show the host's `uname` to
  Windows code, and `/proc` has known gaps in format (`docs/STEAM.md` §3).
  The gaps are closed as defects. Whether Ferrix says `Linux` to this one
  program is the customer's decision, not a session's. 3.
* **E5.5** The record. 2.

**Proved by** the login completing and the world loading on three separate
days and across one update of the game.

### Step 6: playable  ·  29 points

* **E6.1** Presentation without a copy: the game's frames from the card's
  memory to the screen through yserver and hyprix. Presentation is by copy
  today, and `nvrm` gives dmabufs only for linear system memory (the
  study, from the GPU session's notes); `nv-vram-dmabuf` and `nv-zerocopy`
  are the unlanded start. 13.
* **E6.2** Frame pacing: presentation in step with the screen, and what a
  full-screen game costs in hyprix. 8.
* **E6.3** Video memory under pressure, and DXVK's shader cache kept on the
  volume between runs. 5.
* **E6.4** The measurement: frame times over an evening, beside step 0's
  figure for Linux on the same card. 3.

**Proved by** 60 frames a second at 1920x1080 for an evening of play on the
RTX 3060, recorded with its frame times. This is the stage's exit.

### Step 7, optional: by way of Steam  ·  13 points, and N5 to N7

The game as Steam's release of it, if the customer wants that route once it
exists.

* bubblewrap as uid 1000 and pressure-vessel: N5 to N7, 16 points, counted
  in stage 22's row and not here.
* **E7.1** Proton from Steam's library in the runtime's container, with the
  card's libraries inside it. 8.
* **E7.2** Steam installs and starts the game, and the account is linked. 3.
* **E7.3** The 32-bit NVIDIA libraries on the volume, which
  `docs/NVIDIA.md` leaves out, if a 32-bit part wants Vulkan or GL. 2.

**Proved by** Steam installing and starting the game by itself, which also
meets stage 22's third exit step. Valve's own Proton has lacked the patches
ACE needs under Wine (the study's timeline), so this step may wait on Valve
or on the publisher whatever Ferrix does.

### In sum

| Step | What | Points |
|---|---|---|
| 0 | The game on Linux in the same VM shape | 8 |
| 1 | Wine starts on Ferrix | 42 |
| 2 | Direct3D 11 through DXVK on the card | 18 |
| 3 | A Unity game on DirectX 11, no anti-cheat | 54 |
| 4 | Installer, launcher, login | 25 |
| 5 | Past the anti-cheat | 29, unknown |
| 6 | Playable | 29 |
| | **To the exit** | **≈ 205** |
| 7 | By way of Steam, optional | 13, and stage 22's 16 |

74 of the 205 are guesses at what running it will find (E1.3, E3.6, E3.7,
E4.3 and all of step 5 but its record), and stage 21's unlanded GPU work is
under steps 2, 3 and 6 and counted in none of them. The points are not in
the sized scope the [burndown](status.md) is drawn on.

## What can stop this regardless of engineering

From the study, with its sources. None of these is Ferrix's to fix.

1. **The anti-cheat's tolerance of Wine is not a promise.** The game runs
   on Linux because community patches let ACE's driver run inside Wine. It
   broke in late January 2026 on GE-Proton and Proton Experimental, was
   fixed in February, and broke again for at least one player in September
   ([proton-ge-custom #433](https://github.com/GloriousEggroll/proton-ge-custom/issues/433),
   [GamingOnLinux](https://www.gamingonlinux.com/2026/02/ge-proton-10-31-brings-fixes-for-arknights-endfield-duet-night-abyss-and-more/)).
   The study found no statement from Gryphline or Hypergryph that Linux is
   supported; the Steam page lists Windows only. Tencent or the publisher
   can end it with one update.
2. **A virtual machine may be refused.** Ferrix has the card inside a KVM
   guest. Kernel-level anti-cheats commonly refuse or flag virtual
   machines, and the study found no report either way for this game under
   Wine in one. Step 0 answers it before any Ferrix work is spent on steps
   4 to 6.
3. **The account may be banned.** The study found no report of a Linux
   player banned and no statement that it cannot happen. A kernel nobody
   else runs is a sample of one: any heuristic that flags it flags this
   account only. The account used is one the customer can afford to lose.
4. **The terms of service.** The game requires a third-party EULA, which
   the study did not read. Whether running it under Wine, or with a
   third-party Proton build, is permitted is not confirmed. Reading it is
   owed before step 4, and the decision is the customer's.
5. **Ferrix is not Linux to a fingerprint.** ACE under Wine sees what Wine
   shows it, and that can include the host's `uname`, timing and `/proc`.
   Whether ACE looks is not known. Step 5 does not disguise Ferrix.
6. **The Steam release on 2026-10-15 may change the picture either way.**
   An allowance for the Steam Deck would help; a stricter configuration of
   ACE would hurt. The state is read again after that date and before
   anything is spent on step 4.

## Where the study was wrong against the tree

Checked on `main` 557b939ed; what this page says follows the tree.

* It lists seccomp filters with `SECCOMP_RET_TRAP` as present. They are not:
  the filters landed (S3), and `TRAP` is "not built until `SIGSYS` is
  delivered" (`src/kernel/src/syscall/seccomp_filters_check.rs`; S4 in
  `docs/roadmap/status.md`'s stage 13 row).
* It calls `SCM_CREDENTIALS` unclear. The file's header says it is refused,
  and the code takes it on an `AF_UNIX` socket and refuses it elsewhere
  (`src/kernel/src/syscall/sockets.rs`); the header is stale.
* It says the word `fsgsbase` appears nowhere under `arch/x86_64`. It
  appears in three comments there, each saying that `FSGSBASE` stays off for
  programs; the finding stands and the search did not.
* It gives nazuna 48 GB free; `df` showed 88 GB on the evening of
  2026-10-10. Still short of the 110 GB step 0 wants beside everything
  else.
