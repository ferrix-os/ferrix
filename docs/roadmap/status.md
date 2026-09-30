# Status, estimates and forecast

*Reviewed 2026-09-26.* Over the four days of the
points era that the fleet ran, 2026-09-14 to -17, about 445 points landed:
131, 34, 66 and 214, which is ≈ 111 a calendar day and ≈ 150 a day the fleet
was running, with 8–10 sessions, 15–20 points a session-day, and 21 points a
queue-hour on both days that were measured finely. Every estimate under 8
held, and stages 17 and 18 came in at the sizes they were given
(`docs/BACKLOG.md`, *Velocity*). The count since then, to 2026-09-24, is
≈ 870 points in 11 calendar days (≈ 79 a day), and 2026-09-24 alone landed
≈ 99, about 22 an hour over the landing window, on a code base of 703 k
lines of Rust. Counted again on 2026-09-26, the total is ≈ 1,200 points in
13 calendar days (≈ 92 a day): 2026-09-25, a day of one or two sessions on
the certification audit, landed ≈ 26, and 2026-09-26, with about twelve
sessions, ≈ 278, of which 103 had been estimated before the work started
and the rest were sized afterwards from `git log`. That is historical
velocity, not a current schedule. The table records the state now.
Stages 12 to 16 were sized in words before points existed; their points are
first guesses rather than an owner's estimate and are replaced when a session
sizes them.

| what | points | current state |
|---|---|---|
| ~~Stage 19: the GPU path, Path A (`docs/GPU.md` §3)~~ *done 2026-09-19* | ~~52~~ | done |
| ~~Stage 19: the cursor plane (`docs/GPU.md` §3.10)~~ *done 2026-09-23* | ~~13~~ | done |
| ~~Stage 19: the device queue, commands in flight and a frame that waits once (`docs/GPU.md` §3.11)~~ *done 2026-09-24* | ~~13~~ | done |
| Stage 19: the desktop's speed as it is watched (`docs/GPU.md` §3.9): client pages as texture backing | 8 | in progress |
| Stage 19: XWayland 40, and `dwindle:precise_mouse_move`, the second-pass effects and the window rule `xray` about 8 | 48 | in progress |
| The desktop's own clients: waybar, fuzzel, hyprlock and hypridle in Rust, reading the customer's own files (`docs/DESKTOP-CLIENTS.md`, on the clients' branches) | the foundation they share 21 (`docs/BACKLOG.md`); the four programs unsized | under way: fuzzel's pure core, its icons, cache, dmenu input and layout on `main` (2026-09-26); waybar on `main`, drawing the customer's bar on the desktop and gated by a boot (2026-09-27); the foundation's crates, hyprlock, hypridle and fuzzel's window on branches |
| After stage 19's 178: `zwp_linux_dmabuf` with a GBM-shaped allocator, and Mesa's virgl on ferrousli, for clients that draw on the GPU themselves (`docs/BACKLOG.md`) | 8, and 40 or more | not started |
| Gears (`docs/GPU.md` §6, the customer's order of 2026-09-24): vkgears through Venus on the Linux host 39, which is also stage 22's "Venus 8" and more; GLES2 gears on the DK1's GC400 32 | 71 | under way: vkgears draws through Venus (39 done, 2026-09-24); the GC400 runs a command buffer on the board, its events by interrupt (G1 and G2, 11 of its 32, 2026-09-24) |
| ~~Dynamic linking: the kernel half, ferrousli's loader, glibc's names~~ *done 2026-09-23* | ~~39~~ | done |
| ~~Dynamic linking: ferrousli's AArch64 and ARMv7-A port, which the customer put inside the stage on 2026-09-21~~ *done 2026-09-23* | ~~≈ 34~~ | done |
| ~~Stage 12, btrfs write~~ *done 2026-09-21* | ~~≈ 60~~ | done |
| ~~sysfs, fed by the services that own each fact (`docs/SYSFS.md`)~~ *done 2026-09-24* | ~~26~~ | done |
| ~~Chrome on Ferrix, headless and in a window, x86-64 (`docs/CHROME.md`)~~ *done 2026-09-24* | foot and its ports 13, the kernel's rows ≈ 30, spent | done |
| ~~Chrome: the zygote, its speed, ferrousli in glibc's place headless and in a window, the persistent btrfs root~~ *done 2026-09-26* | unsized, spent | done |
| ~~Chrome: `inotify`~~ *done 2026-09-27 (ferrix-e4)* | unsized, spent | done |
| Chrome: the GPU | unsized | not started |
| Chrome on the STM32MP157D-DK1 (`docs/CHROME.md` §10) | ≈ 45–55 | not started |
| The Pixel 7, the customer's phone (`src/boot/vendor/google/pixel7/HANDOVER.md`) | the USB device driver unsized; the desktop in the launcher app's VM done | under way: `main` boots it natively on all eight cores to `FERRIX-BOOT-OK stages 1-12`, and as a guest of the phone's own crosvm from a launcher app, with a monitor graphing the boot and `ferrix-statd`'s samples (2026-09-26); the customer chose the VM for a desktop the same day (option A), and it runs there with Chromium on it (2026-09-27); the USB device driver has its brief (`docs/vendor/google/pixel7/USB-HANDOVER.md`) and a read-only survey on a branch |
| Stage 13, namespaces, cgroups, seccomp | cgroups 85 (`docs/CGROUPS.md` §7: 27 for what init needs, 58 for the controllers); namespaces and seccomp unsized, the old guess for the whole stage was *month* ≈ 60 | under way: G1 to G5 done (27), which is all init needs from it, C8 for native services included; `pids`, `memory`'s charging and `cpu.weight` done (2026-09-26, as the certification's job quotas), and `memory`'s scoped OOM kill the same day (P1, M1, S1: 28), so 55 of 85; the rest of `memory.stat`, `memory`'s reclaim, freezing, `cpu.max` and `io` left, about 30; Steam's user and mount namespaces sized at 39 (`docs/NAMESPACES.md` §9), N1 and N2 done (11) |
| Stage 22, Steam: the parts with a first guess (bubblewrap's rest 13, sound 30, Venus 8; glibc's names are dynamic linking's 13 and XWayland stage 19's, both counted above) | 51; sound re-sized by `docs/AUDIO.md` as 24 for the driver, the core and a gate, spent, then alsa-lib 3 (U1) and a server unsized (U2), so 24 of the 51 left sized | sound under way: playback done 2026-09-26 -- a ring-3 virtio-snd driver, the audio core, `/dev/snd`, `test-audio`, Chrome playing through it, and `run-compositor --everything` bringing the card; U1 and U2 left; bubblewrap and Venus not started |
| Stage 22, Steam: the 32-bit x86 ABI and what the runtime and Proton find missing | unsized, ≈ 100 as a guess; `docs/I386.md` sizes I1 to I4 at 42, I5 unsized | under way: I1, a 32-bit program through `int $0x80`, done and on `main` 2026-09-26 (8 of 42); I2, threads and signals, next; Steam's sign-in window on hyprix through yserver 2026-09-29, with launch-side workarounds (`docs/STEAM.md`) |
| Stage 14, real-time domains | *month* ≈ 40 | not started |
| Stage 15, a real userland | *week* ≈ 20, of which job control is spent; most of the rest landed as zinc and uutils, and what is left is an init, sized at 67 points in `docs/INIT.md` §13, of which all 67 of L1 to L10 are spent, L11's 2 too, and 16 later; authentication (`docs/AUTH.md`, approved 2026-09-26): a kernel fix first (P0) 2, spent, phase 1 27, phase 2 31 and phase 3 about 32 later | init done (L1 to L10, 2026-09-26): `run` and every desktop image boot `/sbin/init`, with `getty`, `svc`, readiness, socket activation, resource limits and the directory over `src/lib/init/svc`; L11 (`devmgr` on its restart policy) done too; L12 and L13 are later by design; authentication's P0 -- `process_create` gave a child root's credentials -- is fixed (2026-09-26), and phase 1 is not started |
| ~~Stage 16, `rustc`~~ *exit met 2026-09-22* | ~~*the goal* ≈ 40~~ 8 spent | done |
| Stage 20, self-hosting | *longer*, unsized | in progress: the x86-64 image builds on Ferrix and boots (2026-09-23); every build of the matrix recorded, Ferrix making them stops on FX-0001 (2026-09-24) |
| Stage 21, bare metal and a GPU of Ferrix's own | over 100, unsized | planned when bare-metal work is requested |

## Burndown

Scope is the table's sized, unfinished rows on 2026-09-26, after the day's
landings: client pages 8, XWayland and the second pass 48, dmabuf and virgl
48, the GC400's remaining 21 of 32, Chrome on the DK1 50, stage 13's rest 30,
stage 15's 35 (init's L10 6, and authentication's P0 2 and phase 1 27),
the desktop clients' foundation 21, a desktop in the Pixel 7's VM 17,
stage 14 40 and stage 22's 124 (bubblewrap 13, alsa-lib 3, Venus 8, and the
guess of 100 for the 32-bit ABI and Proton) -- **≈ 442 points**. The
unsized rows (stage 20, stage 21, the audio server, Chrome's window on
ferrousli, the desktop clients' own programs, the Pixel's USB driver) are
outside it, so the chart shows when the *sized* work ends, not when the
roadmap does. Scope has grown as often as it has shrunk: since 2026-09-24,
97 points of it landed (init 45, cgroups 28, sound 24), sound's server and
its 3 points went out of it to be sized, and 67 were added
(authentication, the clients' foundation, the Pixel's desktop), so the
chart is a forecast from today, not a history.

![Burndown: 442 sized points remaining from 2026-09-26, done 10-01 at 92 a day or 10-03 at 67 a day; below it, points landed per day from 09-14 to 09-26, about 1,200 in total](../img/burndown.svg)

In the upper chart the steeper line is the running average, 92 a day,
which counts everything that landed. The shallower is 67 a day, only the
work that was estimated before it started, 2026-09-24 to -26: that is the
rate the sized scope burns at, since the unsized work that lands beside it
-- the certification, the Pixel, Chrome's polish -- takes nothing off it.
Neither allows for the qualification below: a step full of unknowns may
take twice its estimate. The lower chart is what has landed, by day, and
the running total; hatched is what was sized afterwards from `git log`, all
of 09-18 to 09-23 and most of 09-25 and 09-26 (`docs/BACKLOG.md`,
*Velocity*).

## Gantt

Done bars are dated from the rows above and the ledger, and start where the
first landing was; they overlap because the work ran in parallel. The
forecast is one queue at 67 points a day, in the order of the table, with
the sized rows only. It is a sequence for reading the size of the work, not
a plan: several of these would run side by side, and the order is the
customer's to change.

![Gantt: done work from 2026-09-13 to 09-26, eleven streams in progress, and the sized remainder as one queue at 67 points a day ending 10-03](../img/gantt.svg)

Both charts are drawn by `tools/common/gen/gen-roadmap-charts.py`, which holds their
numbers; change them there when the table or the velocity count changes, and
rerun it.

---

Three things qualify these estimates:

* **The kind of work.** The velocity was measured on tables, system calls
  and a renderer, where every estimate held. The GPU path is unknowns in
  every step and XWayland is a server; either may take the time the table
  gives it twice over. A new forecast needs a new count once that work has
  enough history.
* **The fleet.** The number is a fleet's. One session alone does 15–20 a
  day, and the same estimate then reads in weeks rather than days.
* **Unsized stages.** A word like *month* was written for one person before
  points existed; the guess beside it is only so that a date can be put
  down at all. Stage 21 and the rest of 22 have no date because they have
  no size.

---
