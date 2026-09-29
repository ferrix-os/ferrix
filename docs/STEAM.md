# Steam's window on Ferrix

Valve's Steam client, unchanged, draws its sign-in window on hyprix through
yserver, on Ferrix, in a guest under KVM. This is the first step of stage
22's exit ("the Steam client starts on Ferrix, logs in and shows its store,
with the browser helper drawing"): the client and its browser helper start
and draw, and nobody has signed in yet. Input has not been tried.

It runs with launch-side workarounds: stand-ins, preloaded shims and flags,
each for something Ferrix does not do yet. The table in §3 lists every one,
the real fix that retires it, and who owns that fix. None of them is in the
kernel.

## 1. Running it

```
scripts/fetch/fetch-steam-window.sh     # once: the volume, about 7 GB sparse
cargo xtask test-steam-window           # the gate: waits for the window, judges the screen
cargo xtask run-steam                   # the same boot, screens dumped until the timeout
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

**On the desktop.** Once the volume has been made, `cargo xtask
run-compositor --everything` merges its tree into the desktop's volume and
starts Steam beside Chrome and a terminal: `scripts/steam/desktop.sh` waits
for the desktop's yserver on `:0` and runs the client's half as uid 1000,
its output in the guest's `/tmp/steam.log`. The guest has 16 GiB then,
unless `--memory` says otherwise. Steam's tree carries its own yserver, so
the desktop takes it in yserver's own volume's place; make both volumes at
the same pin. The first start installs the client, as above, and the
desktop's volume is attached under `snapshot=on` too, so every boot does.

## 2. How the pieces fit

| Piece | Where | What |
|---|---|---|
| The volume | `scripts/fetch/fetch-steam-window.sh` | yserver's tree (`fetch-yserver.sh`, the fork at its pinned commit), Valve's bootstrap and the Debian tools under it (`fetch-steam.sh`), i386 Mesa with llvmpipe for the 32-bit client's own GL UI, i386 libstdc++, Debian's amd64 `lsof`, and the workarounds compiled |
| The boot | `xtask/src/compositor/steam_window.rs` | hyprix, the links the volume's programs need, the scripts below, a `uname` that says `Linux` |
| The root half | `scripts/steam/run.sh` | yserver on `:0` as a Wayland client of hyprix, a lease, then the client's half as uid 1000; a watcher for the window's title |
| The client's half | `scripts/steam/client.sh` | `ubuntu12_32/steam` started directly with `steam.sh`'s environment, again while it exits 42 |
| Stand-ins | `scripts/steam/_v2-entry-point`, `logger-0.bash`, `lsof` | see §3 |
| Shims | `scripts/steam/workarounds/*.c` | see §3; each file's header names its gap and owner |

The client runs as uid 1000: run as root, it moves its effective uid to the
home's owner partway through, and GTK2's setuid check then exits
(`docs/I386.md`, I5b). That is how Linux behaves too, and is not a
workaround.

The flags `-no-cef-sandbox -cef-disable-gpu -cef-disable-gpu-compositing`
are the host spike's: Chromium's sandbox needs user namespaces, and its GPU
process would look for DRI3, which yserver on a guest without a render node
does not offer; it renders in software instead.

## 3. The workarounds

| Workaround | Why | Real fix | Owner |
|---|---|---|---|
| `pipe2-direct.c`, preloaded into the client | `pipe2(O_DIRECT)` (packet mode) is `EINVAL`; `controllerxinput_linux.cpp` asserts without it | packet-mode pipes | steam-pipe-direct |
| `_v2-entry-point` stand-in, and `-no-cef-sandbox` | Valve's steamrt64 entry point starts pressure-vessel, which needs user and mount namespaces for bubblewrap; Chromium's sandbox needs them too | namespaces | N2–N6 (`docs/NAMESPACES.md`) |
| `logger-0.bash` stand-in | The Steam Runtime's logger failed on Ferrix under `steamwebhelper.sh`; this one logs nothing | whatever the logger meets: `/dev/fd` through process substitution, and the `/proc` gaps below | steam-proc-gaps |
| (not worked around) `lsof` warns "unsupported format" for `/proc/net/tcp6` and `udp6`, and cannot identify Unix sockets | the IPv6 tables' columns differ from Linux's, and `/proc/net/unix` names no inodes | Linux's formats | steam-proc-gaps |
| 16 GiB guest | At 8 GiB several processes died of `SIGBUS` on execute faults of mapped library pages while Chromium started | find and fix the refault | steam-sigbus |
| the window fills its tile, black around the login | hyprix tiled a window of a fixed size, and yserver did not pass on its size hints (`WM_NORMAL_HINTS`) | a floating window of the size Steam asks for: hyprix ec4ce6b2 and the yserver pin c5b5935, which a volume made after them carries; not yet seen on Steam's window | done, to be confirmed |

Four things the sprint needed are no longer workarounds: yserver's own
patch making a client's socket blocking before its setup (the pinned fork
has 14197fb, and the kernel no longer passes a listener's `O_NONBLOCK` to
`accept`, 18388c70); a `getresuid` shim, not needed once the client runs as
uid 1000; and two shims for `/proc`, retired when `test-procfs` landed.
One was preloaded into `lsof`, because `stat` through `/proc/<pid>/fd/<n>`
of a socket was `ENOENT`, so `lsof` could not tie the client's websocket to
the web helper and the client rejected it ("Unexpected Transport Error
0x3008"). The other was preloaded into the client, because `/proc`'s inode
numbers did not fit 32 bits, so the client's `readdir` there was
`EOVERFLOW` and it found no web helper process at all.

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
   descriptor link, and 32-bit `readdir` on `/proc` (worked around, then
   fixed by `test-procfs`'s landing).
6. The client looks for `lsof` only at `/sbin`, `/bin`, `/usr/sbin` and
   `/usr/bin`, and says so nowhere visible when it finds none.
