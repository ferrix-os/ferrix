# Backlog detail: The desktop and the GPU, what is left

Detail kept out of [`docs/BACKLOG.md`](../BACKLOG.md), which holds a short line per item linking here. Each item's text is as it stood in the backlog when it was split out (2026-10-16); add new findings here, under the item's id, and keep the backlog line short.

### dg-001

| Item | Stage |
|---|---|
| hyprix draws the top layer (waybar) over a fullscreen window, and under `--no-gl` its blur is then the frame's largest phase: "over 35 to 72 ms" of the slowest frames with a terminal fullscreen (softgl-gap, 2026-10-10; `~/ferrix-logs/softgl-desk/base2.log`, `bord2.log`, shot `base2-fs.png`: the bar over the fullscreen window). Hyprland does not draw the top layer over a fullscreen window, only the overlay. Start at `over_windows` in `src/user/system/linux/compositor/hyprix/src/state/draw.rs` and `render_onto`'s `Phase::Over`; with it gone, measure `--no-gl` fullscreen frames a second again on a quiet host: 14.2 at a load of 31 before the border's fix (12dfc447e, which took the border's 37 to 53 ms a frame away) and 11.0 at a load of 45 after it, so the gain in frames is not shown yet. Also seen: a translucent terminal over the video wallpaper costs "blur 150 to 380 ms" a frame under `--no-gl` (`base.log`) | open |

### dg-002

| Item | Stage |
|---|---|
| The video wallpaper stops decoding under a window that covers it (0765b49a9: `pattern --video` from 0.59 of a processor to 0.02 under a fullscreen terminal, back to 0.6 windowed) only when the covering window's buffer is opaque: XRGB or an `opaque` rule, opacity 1, square corners. Not checked: which format yserver gives a game's window, so the case the softgl-gap study saw (Teeworlds through yserver, `pattern` at 0.5 to 0.64 of a processor) is unverified. Run it, and if the buffer is ARGB decide whether a fullscreen X window's alpha is honoured at all | open |

### dg-003

| Item | Stage |
|---|---|
| Settings (`docs/SETTINGS.md`, the customer's order of 2026-10-11): the app, its game-mode page, the user's `~/.config/ferrix/game-mode.conf` and the launcher's mark landed with this row, with `test-compositor --boot settings` on x86-64 and its negative control. Game mode is a mark with no effect, and the page says so; the customer accepted that as the first version and decided what comes next (*Decisions*, 2026-10-11): the speculation protections and the other latency and frame-rate trades, behind administrator privilege (per application is the aim; `docs/SETTINGS.md` §7.3 says what a grant reaches), whose design round is to be opened on branch `game-mode-design`. Left: the boot on AArch64 and ARMv7-A; the page looked at on the `--everything` desktop as the user `ferrix`; the first real effect as decided, which waits on that design round, the one-way barrier and a bare-metal figure; the list following the file and the installed applications while the window is open; a second page. The driver-domain opt-in is a proposal only (§7) and waits on the customer (§8). 2026-10-30: the design round is `docs/GAME-MODE.md` (tiers T0 to T4, measured on the box; the customer decides which to build, its §10). Owner: open | 24 |

### dg-004

| Item | Stage |
|---|---|
| `run-compositor --everything` is everything (the customer's rule, 2026-10-01): nothing on that desktop may be conditional or silently left out. Done: every volume is fetched when missing, and a failed fetch stops the run (f7777c17); Steam's launcher entry (cb21ecd8); every app installed, the opt-in ones too, and a script app with no package (btop) built, through WSL on Windows (211bc0e5, os-24); the ports built on Windows through WSL, with LLVM's clang where WSL's gcc is older than 15 (503d65aa, os-df); a missing port or busybox built and a missing Bad Apple!! fetched rather than left out (os-24, 2026-10-01); launcher entries and icons for btop and ferrofetch, and Bad Apple's icon (48f440dc, os-3c); the ported programs (curl, git, sshdt, foot, vkgears, ALSA's) are apps, so `--everything` builds a missing one as it builds btop, and `ports::everything` is gone with them (os-3c, 2026-10-01). Left: choosing Steam from fuzzel after closing the client, which no live desktop has tried yet. vkgears needs Venus, which WHPX does not give, so a Windows host's desktop cannot list it | 19 |

### dg-005

Done; removed from the backlog on 2026-10-16.

| Item | Stage |
|---|---|
| Two btop builds at once wreck each other: every checkout's btop app builds, by its `build.sh`, in the one `$FERRIX_PORTS/btop/build`, which each starts with `rm -rf`, so a second build deletes the first's `obj/` mid-compile (`opening dependency file obj/btop.d: No such file or directory`). Seen twice on nazuna on 2026-10-01, os-24's `--everything` control beside os-3c's apps-launcher `build-apps`; alone, the same script built 6 of 6. Since 211bc0e5 `--everything` builds btop when it has no package, and `test-apps` always does, so two gates at once keep meeting here. **Fixed 2026-10-01 (os-3c, with the ports' move into apps):** ferrousli's port toolkit has `lock_port`, a `flock` on `$builds/<port>.lock` held until the script exits, and every port's and script app's `build.sh` takes it before it empties its work directory, so a second build waits | done |

### dg-006

| Item | Stage |
|---|---|
| `test-audio --arch x86_64`'s mix boot failed once, on 2026-10-01 at a load of 15 to 19, gating the ports' move into apps on 1c4dbcf4: "the mix has 1000 Hz at 7537, not 8192 within 5%, and 440 Hz at 8188". That boot is pulsed mixing two tones, which the branch does not touch; the same boot on the rebased tip 4be395fa passed at a load of about 9. Kept: `~/ferrix-logs/apps-ports/b-audio.log` on nazuna. A mix level 8% low on one tone looks like a lost or late period under load, not noise | open |

### dg-007

| Item | Stage |
|---|---|
| A graceful STOP for an input driver, and a gate row for it. Nothing sends one: the kernel's input core (`src/kernel/src/interfaces/input`) never sends STOP, and devmgr answers a sysfs `unbind` by killing the driver's job, so `ferrix-driver`'s stop path -- STOP, `Driver::shutdown`, the transport's `Stopped`, `Dma::free` -- has never run since d5b03142 moved virtio-input onto it (`test-restart --boot input` kills with `kill -9`, which the kernel's quarantine covers). Needs the core to send STOP when devmgr unbinds a device gracefully, then a `test-restart` variant that unbinds instead of killing and requires STOPPED, the device's node gone and a restarted driver serving again; with a negative control that frees a `Dma` before the reset (the one-line sabotage `Stopped` exists to prevent) and shows the row noticing. The core change touches `interfaces/`, so it goes to the certification session before landing (os-9f's suggestion, 2026-09-30) | 17 |

### dg-008

| Item | Stage |
|---|---|
| Input hotplug: devices exist from boot in the input iteration. A device that arrives or leaves later, and how a compositor learns of it without udev (`inotify` on `/dev/input`, in the kernel since 2026-09-27 but not for devfs's own nodes, which it makes without a call and so without `IN_CREATE`; or a rescan) (`docs/INPUT.md` §3.4, §6, os-f6 2026-09-16) | 17 |

### dg-009

| Item | Stage |
|---|---|
| The GPU for clients: Mesa's virgl on ferrousli (`docs/GPU.md` §3a), for clients that render on the GPU themselves, 40 points or more, with `zwp_linux_dmabuf_v1` version 4's feedback, which Mesa's EGL reads; zero-copy presentation for Venus through the dmabuf import (§6.1); and the Khronos Vulkan loader, which waits on `dlopen` of a library with thread-local storage. A4, `zwp_linux_dmabuf_v1` version 3 in hyprix and a GBM-shaped allocator, is done (po10-dmabuf, 2026-10-07, §3.13, `test-compositor --gl --boot dmabuf`) | 19 |

### dg-010

| Item | Stage |
|---|---|
| NVIDIA's own driver on the RTX 3060 (`docs/NVIDIA.md` §7, decided 2026-10-02). **N0, the kernel prerequisites, is complete** (N0a–N0d, N0f, N0g, each through the consultant; F-57, F-58 and F-59 closed; 2026-10-03). **N1 is done (2026-10-03)**: N1a fetch, N1b the skeleton, N1c RM in `nvrm` with its core from the volume (af8d242ab), N0e `run-nvidia`, N1d GSP boot on the RTX 3060, N1e the chardev core and N1f `nvidia-smi` listing the card, the last three landed ahead of their review conditions (ledger 297; owed rows below). Then N2 Vulkan offscreen (14), N3 frames to hyprix, copy layer first (30), N4 Chrome and Steam on the card, with yserver's DRI3 and Present (20). CUDA (N5): C0a landed with `test-uvm`; N6 waits for a monitor on the 3060 | 21 |

### dg-011

| Item | Stage |
|---|---|
| The DK1's USB host after U-Boot's `ums`: ending mass-storage mode switches the PMIC's `vdd_usb` (STPMIC1 LDO4, on I2C4) off, and the kernel never turns it on, so the USB PHY is unpowered and nothing enumerates; U-Boot's `regulator dev vdd_usb; regulator enable` before `bootefi` is the workaround (`docs/vendor/st/stm32mp157-dk.md`, 2026-09-23). The kernel's USB preparation should turn LDO4 on itself -- a write to the PMIC every rail of the board hangs off, so with the care the RCC gets | 17, P3 |

### dg-012

| Item | Stage |
|---|---|
| fuzzel in Rust (`docs/DESKTOP-CLIENTS.md` §4): the pure core landed 2026-09-26 (`a29407d6`, `02afa6c8`); the window, hyprix giving an interactive layer surface the keyboard, the image's `.desktop` entries and icons, and `test-compositor --boot fuzzel` landed with this row. Left: the clipboard pastes, taking the keyboard back from an `on-demand` layer surface on a click elsewhere, a 30-second start with the user's 20 host fonts, an aarch64 boot. **The user's SUPER R landed 2026-09-27** (b9cb64d5): `crate::dotfiles` carries `~/.local/bin/hypr-launcher` unchanged and links the fuzzel it names to `/bin/fuzzel` (`--boot fuzzel-user`). Still left: the script's `pkill -x fuzzel` toggle (its own row, P2), and fuzzel's lock file, which it builds from the absolute `WAYLAND_DISPLAY` (`/tmp/fuzzel-/tmp/wayland-1.lock`, a warning). Handover: `~/.local/share/ferrix/ferrix-d5/HANDOVER.md`. Owner: fuzzel. Estimated 21 points | 19 |

### dg-013

| Item | Stage |
|---|---|
| waybar in Rust for the user's own `~/.config/waybar` (the waybar app in ferrix-os/apps, the desktop clients' design). **Draws the user's bar on the desktop (2026-09-27):** the config, formats, GTK3 stylesheet, layout and painter (landed 2026-09-26); the bar on the toolkit with hover, cursor, clicks, scrolls and tooltips as layer-surface popups, tested against hyprix in-process; hyprix's half (layer popups, pointer on layer surfaces, a virtual pointer's seat); a PulseAudio-protocol client; `waybar --render`; `/bin/waybar` on every desktop; and `test-compositor --boot waybar`, whose screen must show the host's drawing of the user's style pixel for pixel. Whole-pixel text as Pango places it, and the volume chip reading, setting and following `pulsed` (`--boot waybar-volume`), landed too. On Ferrix the user's three Python scripts are not there, so the desktop chips and the clock are hidden, as upstream hides them; no sound server but `pulsed` beside Chrome's sound (else `vol 0%`), no load average, no wifi, no D-Bus tray (`docs/DESKTOP-CLIENTS.md` §3). **On `run-compositor --everything` (2026-09-27):** the desktop starts this machine's own `hyprland.conf` with its dotfiles, fonts and monitor EDID, and the user's config matches its bar to that EDID's description (b9cb64d5). Chrome's `HOME=/dev/shm` now goes on Chrome's own command instead of a global `env =` line, which had hidden `~/.config/waybar` from waybar (17822571; `--boot everything-desktop`, and the customer's own command run headless). Owner: clients/waybar. Estimated 34 points, 30 spent | 19 |

### dg-014

| Item | Stage |
|---|---|
| Moving wallpapers on nazuna have never been looked at since the AV1 landing changed the format (2026-09-19 finding, restated 2026-10-03, still so on 2026-10-11). `~/.local/share/ferrix/wallpapers` holds 17 `.fxwall` and only two `.ivf`, what `wallpaper::file` looks for (`MOVIE_KIND`): `blue-archive` (925 KB) and an Amiya clip of 64 KB, whose 15 MB `.fxvid` beside it is orphaned under the name the landing replaced and read by nothing. Ten of the `.fxwall` were cut from `.mp4` or `.webm` sources before c5806e59, so they are single still frames of videos. What it wants: re-run `cargo xtask wallpapers --from <directory>` so every video is kept as `.ivf`, check the 64 KB one is a whole clip, delete the `.fxvid`, then boot one and look at it, since no gate covers the moving path; a gate would be better than a look, and the first question is whether one can afford a video in the initramfs. 1 point, wants whoever next has nazuna's display | 19 |

### dg-015

| Item | Stage |
|---|---|
| A cpuset cgroup controller (`cpuset.cpus`, `cpuset.cpus.effective`, a partition root), so that game mode's "three processors for the game, one for the rest" keeps the rest *off* the game's processors for every process, also those started during a session. `sched_setaffinity` applied (F3, p1-036) only keeps a task *on* its own; until this exists the frame mark's holder confines the system's Linux processes one by one and restores them from its record (`docs/GAME-MODE-SCHED.md` §2.1). Out of scope of p1-036 by the consultant's (D12), ledger 4446. Core ring (`sched/**`) and the cgroup core: its own design review, ids reserved first. Owner: open | 24 |

### dg-016

| Item | Stage |
|---|---|
| Arknights: Endfield dies while loading into the world when hyprix shows its buffer directly. Since card0 blends the pointer into its plane itself (`docs/DISPLAY.md` §8.2, 2026-10-31), hyprix hands a fullscreen game's buffer to card0; on the N150, 2 of 2 runs scanned out through the loading screen ended in Wine/Unity's "Fatal error in GC: SuspendThread loop failed", while 1 of 1 run with direct scanout refused (`debug:overlay`) reached the world, and switching to direct scanout once in the world held 5+ minutes (hyprix 8 % to 3 %, intel 7 % to 4 %, fps unchanged at about 27). Not root-caused; a suspect is a buffer or swapchain change during the load while card0 holds the game's buffer. Until then the box runs hyprix with `HYPRIX_DIRECT_SCANOUT=0` (set in the box's own hyprix wrapper, `n150-linux/box/tk/post-install.sh`, never in product source); QEMU and other machines keep direct scanout. Check against the proton-fix-unity-suspend work: if that fixes a Unity SuspendThread cause, repeat the box A/B with direct scanout on before lifting the box setting. Closes after 3 or more scanned-out loads into the world without the crash, or a fix of the cause. Consultant ruling OK IF, ledger 5384. Owner: card0-cursor | 21 |
