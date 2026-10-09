# Steam's window on Ferrix

Valve's Steam client, unchanged, draws its sign-in window on hyprix through
yserver, on Ferrix, in a guest under KVM. This is the first step of stage
22's exit ("the Steam client starts on Ferrix, logs in and shows its store,
with the browser helper drawing"): the client and its browser helper start
and draw, and nobody has signed in yet. Input has not been tried.

It runs with launch-side workarounds: stand-ins and flags, each for
something Ferrix does not do yet. The table in §3 lists every one, the real
fix that retires it, and who owns that fix. None of them is in the kernel,
and no library is preloaded into the client any more.

## 1. Running it

```
tools/common/fetch/fetch-steam-window.sh     # once: the volume, about 7 GB sparse
cargo xtask test-steam-window           # the gate: waits for the window, judges the screen
cargo xtask run-steam                   # the same boot, screens dumped until the timeout
cargo xtask test-steam-store            # the --everything desktop's Steam, on ferrousli: sign-in, then the store
cargo xtask test-steam-game             # the same, then Teeworlds installed from the library and started (§7; not passing yet)
```

Both need KVM and the internet, as `test-steamcmd` does, and neither is in
`cargo xtask check`. The volume is attached under `snapshot=on`, so every
boot starts from Valve's bootstrap: the client downloads and installs itself
(about 500 MB), restarts, and opens its window about twelve minutes after the
boot on the gate host. The guest has 16 GiB (`--memory` changes it; see §3,
`SIGBUS`).

`test-steam-window` passes when hyprix lists a window titled "Sign in to
Steam" and a screen dump twenty seconds later has the colours of a drawn one;
the dump is `build/x86_64/steam/login-window.ppm`. `run-steam` keeps every
screen that differs from the last in `build/x86_64/steam/` until the
timeout (`--timeout`, 2400 s by default). The serial transcript's
`steam-window:` lines are the guest's: the client's output, the titles of
hyprix's windows as they change, and the client's logs at the end.

What shows: the sign-in window ("SIGN IN WITH ACCOUNT NAME", the password
field, "Sign in", and the QR code for the mobile app), top left in a tile of
hyprix's that fills the screen, the rest of the tile black.

**On the desktop.** `cargo xtask run-compositor --everything` makes the
volume when it is missing (by running `fetch-steam-window.sh`, and a failed
fetch stops the run), merges its tree into the desktop's volume and
starts Steam beside Chrome and a terminal: `tools/common/steam/desktop.sh` waits
for the desktop's yserver on `:0` and runs the client's half as uid 1000,
its output in the guest's `/tmp/steam.log`. The guest has 16 GiB then,
unless `--memory` says otherwise. Steam's tree carries its own yserver, so
the desktop takes it and never merges yserver's own volume. The first start installs the client, as above, and the
desktop's volume is attached under `snapshot=on` too, so every boot does.
fuzzel lists Steam too (`steam.desktop`, which runs `desktop.sh` again), so
a client that was closed can be started again; the entry is there only when
the volume is merged.

**The store, gated.** `test-steam-store` boots that desktop's Steam as
`run-compositor --everything` does -- the merged volume, the same archive
with ferrousli's loader at `/lib64`, `desktop.sh` and `client.sh`, 16 GiB --
without the host's `hyprland.conf`, the terminal, Chrome's window, the
wallpaper, the clipboard and the 3D card (QMP cannot dump its screen), and
with `tools/common/steam/store-watch.sh` listing hyprix's windows. It passes
its first step when hyprix lists "Sign in to Steam" and the screen has the
colours of a drawn window, as `test-steam-window` judges it, and the
window its two fields (`build/x86_64/steam-store/sign-in.ppm`); an empty
frame, which has enough colours for `test-steam-window`, does not pass. The
second step needs a Steam account kept for the gate, with Steam Guard off:
a mobile authenticator needs the phone, and email Steam Guard sends a code
for every new machine, which every boot is. Put its name and its password,
one a line, in `~/.config/ferrix/steam-test-account` on the machine that
runs the gate, readable by its owner alone (`chmod 600`), or name another
file with `FERRIX_STEAM_ACCOUNT_FILE`; never in a checkout. Then the gate
clicks into the sign-in window's fields, types both through QMP on a US
layout, presses Sign in, waits for the main window titled "Steam", and
passes when that window shows the store: at least 15% of it the store's
dark blues (`#171d25` to `#1b2838`) and at least 10,000 colours, which its
art brings and an empty page does not. Without the file it says the store
step was skipped, and how to enable it, and passes on the first step. A
sign-in window that shows red while it still has its account name field
fails as a sign-in not taken, with the connection log's answers: a wrong
name or password, Steam refusing the address after several failures, or
email Steam Guard's code prompt, which has red enough to count (the test
account's first run met that one). Red without the field is not counted:
once Steam has taken a sign-in the window says "Loading user data" over
game art, whose reds counted 2232 pixels on 2026-10-01. One still up three
minutes after Sign in with its account name field gone fails as Steam
Guard. The gate's desktop has hyprix's blur off: without the 3D card it
composites in software, and the blur behind Steam's translucent windows
took 19 of every 23 ms of a frame, two of the guest's four processors. With the test account and Guard off, the gate
signed in and found the store on 2026-09-30. The account is typed into the guest and nowhere
else: every line of the transcript and the gate's error are redacted of
both, and a screen dumped after the typing is kept shrunk eight times, too
small to read (`store.ppm`, `not-signed-in.ppm`, `after-sign-in.ppm`).

**A game, gated (not passing yet).** `test-steam-game` boots the same
desktop with `tools/common/steam/game-watch.sh` beside the store's watcher,
needs the account file, and goes on from the store: the guest hands the
running client `steam://install/380840` (Teeworlds) the way a second
`steam` does (`client.sh` with an argument), the gate presses Install in the
Install dialog Steam opens inside its main window, after unticking "Create
an application shortcut", the guest follows the app's manifest to
`StateFlags` 4 and hands the client `steam://rungameid/380840`, and the gate
passes when hyprix lists a window titled "Teeworlds" that has drawn. The
account must own the game: `steamcmd +login <account> <password>
+app_license_request 380840 +quit` claimed it for the test account on
2026-10-01. The gate presses nothing on a store page. What it found, and
why it does not pass yet, is §7. Screens are in `build/x86_64/steam-game/`:
each window that is not the main one cropped to itself, the Install dialog
as the main window less its header (where the account shows) and with every
other window blacked out, and the game's window.

## 2. How the pieces fit

| Piece | Where | What |
|---|---|---|
| The volume | `tools/common/fetch/fetch-steam-window.sh` | yserver's tree (`fetch-yserver.sh`, the fork at its pinned commit), Valve's bootstrap and the Debian tools under it (`fetch-steam.sh`), i386 Mesa with llvmpipe for the 32-bit client's own GL UI, i386 libstdc++, and Debian's amd64 `lsof` |
| The boot | `tools/common/xtask/src/compositor/steam_window.rs` | hyprix, the links the volume's programs need, the scripts below, a `uname` that says `Linux` |
| The root half | `tools/common/steam/run.sh` | yserver on `:0` as a Wayland client of hyprix, a lease, then the client's half as uid 1000; a watcher for the window's title |
| The client's half | `tools/common/steam/client.sh` | `ubuntu12_32/steam` started directly with `steam.sh`'s environment, again while it exits 42 |
| Stand-ins | `tools/common/steam/_v2-entry-point`, `logger-0.bash`, `lsof` | see §3 |

The client runs as uid 1000: run as root, it moves its effective uid to the
home's owner partway through, and GTK2's setuid check then exits
(`docs/I386.md`, I5b). That is how Linux behaves too, and is not a
workaround.

The flags `-no-cef-sandbox -cef-disable-gpu -cef-disable-gpu-compositing`
are the host spike's: Chromium's sandbox needs user namespaces, and its GPU
process would look for DRI3, which yserver on a guest without a render node
does not offer; it renders in software instead.

It presents those software frames to yserver with MIT-SHM: Chromium's
`XShmImagePool` makes a System V segment (`shmget(IPC_PRIVATE, size,
IPC_CREAT | 0600)`), attaches it, hands yserver its id, and removes it
while both still have it attached. The kernel answers those calls since
2026-10-02 (the customer's decision; `src/kernel/src/syscall/shm.rs`,
`cargo xtask test-shm`, branch `steam-sysv-shm`); before, they were
`ENOSYS`, and every frame went through the socket as a core `PutImage`.

## 3. The workarounds

| Workaround | Why | Real fix | Owner |
|---|---|---|---|
| `_v2-entry-point` stand-in | Valve's steamrt64 entry point starts pressure-vessel, which needs user and mount namespaces for bubblewrap | namespaces | N2–N6 (`docs/NAMESPACES.md`) |
| `-no-cef-sandbox` | Chromium's sandbox is two layers: user and pid namespaces (network ones optional), and a seccomp-bpf filter in every child. Measured on the host (`docs/SECCOMP.md` §1.2): with `CLONE_NEWPID` refused, Chrome with its sandbox on does not start at all | user namespaces, pid namespaces and seccomp together | N4 (`docs/NAMESPACES.md`); pid namespaces and a loopback-only network namespace, os-98 after N4 (the customer's decision of 2026-09-30); seccomp S1–S6, os-7c, and S7–S8, Steam's own filters (`docs/SECCOMP.md`) |
| `-cef-disable-gpu -cef-disable-gpu-compositing` | yserver on the Wayland backend offers no DRI3, so the web helper's GL is llvmpipe, which CEF 126 rejects: its GPU process falls back to SwiftShader after three or four restarts | ANGLE on Vulkan through Venus, presenting with `MESA_VK_WSI_DEBUG=sw`: user copies through a device window's own mapping (F-55's second landing), the render node opened to a `render` group, and the helper's flags (§6) | not started; os-9f's conditions are in §6 |
| `logger-0.bash` stand-in | The Steam Runtime's logger failed on Ferrix under `steamwebhelper.sh`; this one logs nothing | whatever the logger meets: `/dev/fd` through process substitution, and the `/proc` gaps below | steam-proc-gaps |
| (not worked around) `lsof` warns "unsupported format" for `/proc/net/tcp6` and `udp6`, and cannot identify Unix sockets | the IPv6 tables' columns differ from Linux's, and `/proc/net/unix` names no inodes | Linux's formats | steam-proc-gaps |
| `test-steam-game` unticks the Install dialog's "Create an application shortcut" | With it ticked, Steam ran `xdg-icon-resource`, which the volume does not have, and the client then aborted, "pure virtual method called" (exit 134, 2026-10-01) | `xdg-utils` on the volume, and the client's abort without it understood | open |
| 16 GiB guest | At 8 GiB several processes died of `SIGBUS` on execute faults of mapped library pages while Chromium started | find and fix the refault | steam-sigbus |
| the window fills its tile, black around the login | hyprix tiled a window of a fixed size, and yserver did not pass on its size hints (`WM_NORMAL_HINTS`) | a floating window of the size Steam asks for: hyprix ec4ce6b2 and the yserver pin c5b5935; and a floating window that follows the size its program gives it later, without which Steam's dialogs float as 130x70 miniatures of themselves ("Steamwebhelper is not responding" did) | done: the sign-in window floats at its own size on the `--everything` desktop (2026-09-30), and a dialog that resizes itself afterwards takes its new size (`follow_own_size`, with `a_floating_dialog_takes_a_size_its_program_gives_it_later`) |

Five things the sprint needed are no longer workarounds. A shim that
retried `pipe2` without `O_DIRECT`: the kernel makes packet pipes now, each
write a packet and each read one packet at most, which the client's
`controllerxinput_linux.cpp` asserts it gets. And yserver's own
patch making a client's socket blocking before its setup (the pinned fork
has 14197fb, and the kernel no longer passes a listener's `O_NONBLOCK` to
`accept`, 18388c70); a `getresuid` shim, not needed once the client runs as
uid 1000; and two shims for `/proc`, retired when `test-procfs` landed.
One was preloaded into `lsof`, because `stat` through `/proc/<pid>/fd/<n>`
of a socket was `ENOENT`, so `lsof` could not tie the client's websocket to
the web helper and the client rejected it ("Unexpected Transport Error
0x3008"). The other was preloaded into the client, because `/proc`'s inode
numbers and its entries' offsets did not fit 32 bits, so the client's
`readdir` there was `EOVERFLOW` and it found no web helper process at all.
The inode numbers were fixed first (f577b9b1); the offsets only after the
client, without the shim, still logged `Checked: <pid>/<pid>` and rejected
the connection, since the shim had truncated `d_off` as well (a46797f7).
cgroupfs and sysfs have offsets past 2³¹ too; nothing 32-bit lists them yet.

## 4. What the sprint found, in order (2026-09-28 and 29)

1. The updater chooses its UI by `dlopen`: `libX11.so.6`, then `libGLX.so`
   by its development name, then `libXrandr.so.2`; without `libGLX.so` it
   uses its console UI.
2. GTK2's setuid check exits under root (above).
3. `pipe2(O_DIRECT)`.
4. yserver's RandR output lost its CRTC, and Chromium found no display
   (fixed in the fork).
5. The web helper's websocket was rejected: `accept` gave the new socket the
   listener's `O_NONBLOCK` (fixed, 18388c70); `/proc/net/tcp` named the
   stack's socket id as the inode (fixed, 5b14b91b); `stat` through a socket's
   descriptor link, and 32-bit `readdir` on `/proc`, both its inode numbers
   and its offsets (worked around, then fixed; `test-procfs` checks both).
6. The client looks for `lsof` only at `/sbin`, `/bin`, `/usr/sbin` and
   `/usr/bin`, and says so nowhere visible when it finds none.

## 5. On the `--everything` desktop, under ferrousli (2026-09-30)

`cargo xtask run-compositor --everything` starts Steam beside Chrome and a
terminal (`tools/common/steam/desktop.sh`). There, `/lib64`'s loader is
ferrousli's, so the client's 64-bit side runs on ferrousli's C library, not
glibc's; the 32-bit client itself still runs on the volume's i386 glibc. On
2026-09-30 the sign-in window came up there, the customer signed in with the
Steam app's QR code, and the client showed its store. What the 64-bit side
needed of ferrousli, in the order it was met:

1. `libGL.so.1` reads its TLS by initial-exec: a `dlopen`ed library's TLS
   image is now copied into every running thread, as glibc does.
2. No GLX visual: Mesa's software renderer needs LLVM, which needs
   `libstdc++`, whose `STB_GNU_UNIQUE` symbols the loader did not take for
   definitions; and `logf128`, `pthread_mutex_clocklock`, `iopl` and a few
   `_chk` names.
3. "steamwebhelper is not responding": `libc.so.6` was loaded where
   `libdl.so.2` is named, ahead of `libcef.so`, so Chromium's
   `dlsym(RTLD_NEXT, "localtime")` found nothing, and logging that
   deadlocked in its own `localtime_r` wrapper. `libc.so.6` now loads where
   its own name puts it.
4. "futex robust_list not initialized by pthreads", then "…is corrupt":
   the web helper links its robust mutexes into the thread's list the way
   glibc does. Every thread now registers a list, laid out as glibc's (a
   `robust_prev` word before the head), and the kernel keeps it per thread.
5. Scout's `setup.sh` exited 127: `client.sh` ran it with the runtime's own
   `zenity` on `PATH`, which does not load (`_IO_getc`).

Each was found by running the program under ferrousli on nazuna first
(`patchelf --set-interpreter` to ferrousli's loader, Xvfb), which is
minutes where a desktop boot is ten. The workarounds of §3 are unchanged:
the helper still runs without its sandbox and outside pressure-vessel, and
its GPU process is disabled (`-cef-disable-gpu`), so the store draws in
software.

Most of what the client printed was its libudev failing, two lines at a
time and over and over: `udev_monitor_new_from_netlink_fd: error getting
socket: Protocol not supported` and `udev_has_devtmpfs: name_to_handle_at
on /dev: Function not implemented`. Since 2026-09-30 a
`NETLINK_KOBJECT_UEVENT` socket opens (and hears no event, since none is
sent yet), and `name_to_handle_at` answers `EOPNOTSUPP`, the one failure
that libudev takes quietly. A `test-steam-window` run went from 291 such
lines to none.

## 6. The GPU process: what is left (2026-10-01)

The store draws in software. To draw it on the GPU, the one route that
needs no DRI3 from yserver is ANGLE on Vulkan through Venus, with Mesa
presenting to X by copying each frame (`MESA_VK_WSI_DEBUG=sw`); plain Chrome
got that far on 2026-09-30, and the copy was F-55's kernel page fault.
F-55's first landing (c5c92781) turned that fault into `EFAULT`, and F-55
is closed (ef206bb2). Three pieces are left, 14 to 23 points in all; none
has code yet. The certification consultant (os-9f) set the conditions for
the first two on 2026-10-01, and each diff goes to os-9f before landing.

**User copies through a device window, 5 to 8 points.** Mesa's copy
`writev`s the frame straight from a Venus window to the X socket. The
conditions: a per-CPU mapping slot per copy (Linux's `kmap_local`), not a
`vmap` and unmap (a machine-wide shootdown per page) and no standing
mapping per region; preemption off for a copy of at most one page, a local
invalidation only, and no interrupt handler in the slot; the slot's
attributes exactly the user mapping's, and only normal memory copied, since
a device-type window keeps `EFAULT`; the page checked to lie in the
region's own recorded device range, with the region held for the copy; no
direct-map address and no sleeping lock; a requirement beside L.user.107, a
stage 9 check that bytes written through the copy read back through a
second mapping, both ways, across a page boundary and within a page, and
negative controls with the attribute check and the range check dropped.
What the tree has for it: `Backing::Device` records only `cached`
(`src/lib/kernel/vma`); `map_device` always stores `false` and `map_window`
what the device says, so a `cached` region is normal write-back and the
rest device-type, and nothing maps write-combining yet. The space's
`inner` lock is a spinlock and holds the region across the copy. There is
no per-CPU temporary mapping area: slots can go after `DEMAND_WINDOW` below
the vmap arena (mind armv7a's 64 MiB reserve and stage 3's probes there),
their tables built once at bring-up, with a new helper that writes one leaf
without allocating and invalidates one address on this CPU (`invlpg`,
`tlbi vale1`, `TLBIMVA`); `mm::unmap_kernel` shoots down every CPU and
cannot be used. Landing 1's stage 9 check then changes: cached windows
copy, device-type ones stay `EFAULT`.

**The render node for a `render` group, 5 to 8 points.** Mode 0660, group
`render`, the desktop's user in it, not 0666. Before the mode changes: what
a user can allocate through the node bounded, and an audit of its calls.
What the reading found: the node's metadata is `Renderer::metadata`
(`src/kernel/src/interfaces/render/mod.rs`), gid 0 like every device node so
far; `/etc/group` is written in four places in xtask (`initramfs.rs`,
`init.rs`, `auth.rs`, and `compositor/apps.rs` to check), gid 90 is taken.
The renderer's session limits, 16 contexts and 256 objects
(`src/lib/proto/renderctl/src/session.rs`), are shared by every open, so a
user could take the compositor's GPU away: the bound has to be per open
(one context, an object limit) with a per-job limit on opens, and each
object's heap charged with a kmem `Charge` as F-37 does elsewhere. For the
audit, in `interfaces/render/node.rs`: `mapping_at`'s offset against the
object's size, `transfer`'s box, level and offset, `resource_create`'s
32-bit size with no page bound of its own, and handles that stop advancing
at `u32::MAX`. A fuzz target for the request parser, and `docs/GPU.md` to
say that the host's virglrenderer is a guest-to-host surface for the host
to secure. The boot check needs a renderer, which test-boot's machine does
not have: a stand-in driver, or a gate under `--venus`.

**The helper, 4 to 7 points.** `client.sh` without `-cef-disable-gpu`,
with `--use-angle=vulkan` and Vulkan's features, `MESA_VK_WSI_DEBUG=sw`,
and without the llvmpipe variables it sets for the 32-bit client. Untested:
whether CEF 126 accepts Venus and presents this way. The host's
`__GLX_VENDOR_LIBRARY_NAME=nvidia`, from the carried `hyprland.conf`,
reaches every guest program too.

## 7. A game from the library (2026-10-01)

Stage 22's second exit step is a native Linux game from the library that
installs to btrfs, launches and draws through the GPU path with sound. On
2026-10-01 the gate for its first part, `test-steam-game` (§1), got as far
as the install's download; it does not pass yet. In order:

1. **The game.** Teeworlds (app 380840): free, `isfreeapp`, so steamcmd's
   `app_license_request` adds it to an account; a Linux build whose
   recommended runtime is `native` with no compatibility tool mapped to it;
   about 10 MB. OpenTTD (1536610) was the first choice and cannot be
   claimed on this storefront: steamcmd's request fails, and
   ArchiSteamFarm's `addlicense s/542537` (the store's web route) answers
   `AccessDenied/InvalidPackage`; the store lists it `is_free: false` with
   only a paid bundle to buy. Battle for Wesnoth, Endless Sky and DDNet are
   mapped to the Steam Linux Runtime's sniper container (pressure-vessel).
   Windows-only titles (Umamusume, for one) are Proton, stage 22's third
   step.
2. **Nothing is pressed on a store page.** A gate that looked for the store
   page's green Play Game button by its colour pressed a paid bundle's "Add
   to Cart" instead (only the cart; nothing was bought). The gate now hands
   the client `steam://install/<app>` for a game the account already owns.
3. **The Install dialog is inside the main window**, not a window of its
   own: the gate finds its Install button by the button's blue and shape
   with the Cancel button's grey beside it, which the dialog's drive bar,
   the same blue but wider, does not have. Steam's friends list floats over
   the main window's left half and is opened again after it is closed, so
   `game-watch.sh` closes it every few seconds until the game is installed.
4. **The shortcut.** With "Create an application shortcut" ticked, the
   client aborted after running `xdg-icon-resource` (§3).
5. **The download.** Steam downloads through a dozen connections at once.
   xtask's gateway kept eight segments in flight on each, more than the
   guest's driver had receive buffers for, so most were dropped and sent
   again on the timer: Steam's content log said 0.001 to 0.3 Mbps. Two
   landings fixed it: 3b1b1de7 (os-a0: 64 receive slots in the driver, 24
   segments in flight) and 020dc9b2 (the 24 bound the whole gateway, shared
   a segment at a time). The same download then took 86 s. One connection
   makes 10 to 15 MB/s since; 300 Mbps needs window scaling in the
   gateway's TCP (BACKLOG).
6. **What Steam installs with it.** Steam adds app 1070560, the Steam Linux
   Runtime 1.0 (scout), 75 MB to download and 223 MB to stage, to the
   install of a `native` game. Whether it then starts the game in that
   runtime's container, which needs pressure-vessel, is not known yet.
7. **The stall.** After the download Steam stages the files, and there it
   stood still: no progress in its logs for half an hour, the 32-bit
   client busy on about three of the guest's four processors. Its cause is
   not known. A `find`/`stat` over `steamapps/downloading` from another
   process never returned on the btrfs volume, and with the library on
   tmpfs (`game-watch.sh` links it to `/tmp`) never returned either, so a
   directory listed while Steam writes into it is a lead of its own and
   says nothing about the volume. `du` on the volume said 0 KiB while
   Steam had staged 79 MB. Whether Steam's staging stalls on tmpfs too was
   not known at the wind-down (BACKLOG).

   **The bypass, `--steam-preinstall`.** `tools/common/fetch/fetch-steam-preinstall.sh`
   installs the apps with steamcmd on the host (the gate's account, the
   host's i386 loader): Teeworlds and the Steam Linux Runtime 1.0 by
   default, each as Steam lays out a library -- `steamapps/appmanifest_<app>.acf`
   at `StateFlags` 4 and the files in `steamapps/common/<installdir>` --
   under `~/.local/share/ferrix/steam-preinstall/tree/steam`
   (`FERRIX_STEAM_PREINSTALL`). `run-compositor --everything
   --steam-preinstall` and `test-steam-game --steam-preinstall` merge that
   tree into the everything volume last, so Steam in the guest finds both
   installed at `/data/steam/steamapps` and neither downloads nor stages
   them; `game-watch.sh` then skips `steam://install` and hands the client
   `steam://rungameid/380840` at once. The volume is made again whenever the
   flag is given or left out, so a run that wants to keep the default
   volume names its own with `FERRIX_EVERYTHING_VOLUME`.

Two things on the way were not Steam's. Steam's sign-in window showed game
art in red while it loaded, which the store gate took for a refused
sign-in (§1). And on the Windows desktop `/data/steam` and `/data/home`
read `I/O error`, so yserver and Steam never started: the volume was made
in WSL, whose btrfs-progs 6.6 writes a tree of hard links into an image
wrongly (`btrfs check`: "link count wrong", "unresolved ref dir"), and C:
was full besides. a1b522ec checks every `--everything` volume with
`btrfs check` and makes it again from copies when the links come out
broken.

## 8. Smooth: the store at 60 frames a second (2026-10-02)

The customer found Steam on the `--everything` desktop all but unusable on
2026-10-01: clicks that took up to a minute, scrolling that crawled, on
Windows and on nazuna alike. Measured on nazuna under KVM with the store
maximized (1896x1016), a scroll drew 11 to 16 frames a second and a click on
a tab showed in 300 to 350 ms. On 2026-10-02 the same scroll draws 60: the
page's own `requestAnimationFrame` loop gets 59.8 frames a second over 22 s
(p99 16.8 ms), CEF's compositor draws as many, yserver hands hyprix 60 to 62
new images a second, and a click shows in 60 to 70 ms, with the guest less
than half busy. What was in the way, in the order it was found:

1. **yserver rendered on Venus.** Once the `--everything` desktop had Venus,
   Mesa's virtio ICD came before lavapipe. The Wayland backend reads every
   changed window back to the CPU, and Venus's host-visible memory is
   write-combining, which Ferrix maps uncached; on an AMD host that is
   uncached indeed. A maximized frame took 22 ms to read back, and copying
   clients' images in was half the server's time. yserver is pinned to
   lavapipe (`VK_ICD_FILENAMES`, 9363fa75d).
2. **yserver's loop woke 9,000 times a second.** It kept every client's
   socket in its poller for READABLE although a reader thread owns the
   reads, and Ferrix's `EPOLLET` counts traffic in both directions of a
   socket, so every byte either way woke it: 50,000 system calls a second,
   two thirds of the server's time in the kernel. The fork's branch
   `steam-perf` registers a client's socket only while output waits.
3. **Every frame went through four copies too many** on its way to hyprix,
   and hyprix took all of the window up again each time. yserver now reads a
   window straight into the `wl_shm` buffer (the toolkit's `draw_pixels`,
   17f8591ef) and tells hyprix only the rows that changed, found by hashing
   each row; X's own damage was tried first and missed drawing paths.
4. **`munmap` was quadratic** in a process's mappings (fc44ecda8): 2 ms at
   4,000 mappings, 0.12 ms now. CEF unmaps its frame buffers every frame.
5. **No System V shared memory**, so Chromium's MIT-SHM fell back to core
   `PutImage` and every 7.7 MB frame went through the X socket: 10.9 of the
   15.9 ms of CEF's compositor frame. Built at the customer's word
   (d2b33b961..b84bc9e06, `test-shm`); the compositor's frame is 4.8 ms.
6. **ferrousli read the clock by system call**, 36,000 a second from CEF:
   it reads the vDSO now (a618944b1), 18 ns a call instead of 600.
7. **Every virtio driver read the device status on every interrupt** — a
   register read that waits for QEMU's lock, which QEMU holds while it shows
   a frame. The display driver spent a quarter of a processor there. It
   reads it only when an interrupt may be a configuration change
   (b95437303..445b38333); 1.7% now. And only virtio-gpu had ever routed
   configuration changes to an MSI-X vector, so the other four would never
   have heard of a reset but for that read.

How it was measured, and how to measure it again: a copy of the
`--everything` volume kept between boots (`FERRIX_EVERYTHING_VOLUME`, and
the data disk attached without `snapshot=on`), so Steam installs and signs
in once; Steam started with `-cef-enable-debugging`, whose DevTools port
(8080 in the guest) gives traces and a `requestAnimationFrame` probe; and a
timer-only instruction sampler in the kernel. VNC's own refresh is 30 ms, so
frame rates read off a VNC screen stop at about 33. A benchmark that scrolls
by wheel notches every 50 ms measures the notches, not the machine: the 20 to
24 frames it showed were CEF drawing one frame a notch.

**Not yet on the desktop as the customer runs it.** The yserver changes (2
and 3 above, and a word-wide alpha store) are on the fork's local branch
`steam-perf`; `fetch-yserver.sh` pins the fork by commit, so they reach
`run-compositor --everything` only once that branch is pushed to the fork
and the pin moves, which needs the customer's word. Until then the desktop's
yserver has the 9,000 wake-ups a second. And Windows under WHPX gives the
guest one processor (QEMU 11.1's MMIO emulator, BACKLOG), on which a browser,
an X server and a compositor share one core.

**Later the same day, the desktop as a whole.** hyprix said no clock for
`wp_presentation` and stamped presentations with the wall clock, so
Chrome's BeginFrames jittered; it says `CLOCK_MONOTONIC` now and stamps a
frame with its refresh on the pace's grid (297e40931). And it flushes what
the GPU drew from a thread of its own instead of waiting 5 to 15 ms for the
host to show each frame (cd6cab079): its frames take 2.5 ms where they took
6 to 7. Steam's store still scrolls at 59–60 frames a second with the
flush off the loop, and a click shows in 0 to 80 ms.
