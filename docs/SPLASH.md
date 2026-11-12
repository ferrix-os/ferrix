# The boot splash

Version 1, 2026-11-12. The design is the customer's choice of variant A of
`docs/brand/splash/mockup.html`: **the slash is the progress.** The FX mark
stands still in grey on the brand's ink; its rust stroke starts unlit and
is lit from the foot up as the boot passes milestones. The screen says
nothing else unless something needs saying.

It is on for every desktop a person boots (the customer, 2026-11-12) and
off in the gates' images: `run-compositor`'s desktop, the release image,
the live ISO and the system it installs, the N150 box's image, and the
cards and images `flash --compositor` writes, which are the DK1's and the
Pixel 7 app's. §5 says how each is switched.

## 1. Who draws, and when

Three programs have the screen in a boot, one after the other. The kernel
is not one of them: it draws on a screen only for `ferrix.fbcon` and a
panic (`docs/ARCHITECTURE.md` §1), and the splash changes nothing in it.

| From | To | Who | What is on the screen |
|---|---|---|---|
| power on | the loader's last line | firmware | firmware's own picture and the loader's lines |
| the loader's last line | the display driver's start | the UEFI loader (`src/boot/common/uefi/src/splash.rs`) | the mark, the slash at 100 of 1000 |
| the display driver's start | `/bin/splash` finding card0 | nobody | what the driver leaves, for 0.05 s on the N150 box (§6) |
| card0 | the compositor's start | `/bin/splash` (`src/user/system/linux/compositor/splash`) | the mark, the slash rising, a line of text when one is due, the log on Esc |
| the compositor's start | | hyprix | the desktop |

Both programs draw the picture with one crate, `ferrix-splash`
(`src/lib/proto/splash`), so the second one's first frame is the first
one's last, pixel for pixel. The picture is spans of one colour on a
background of one colour; an edge pixel is the two blended by how much of
it the shape covers, which needs no read of the screen. The mark is a fifth
of the screen's shorter side tall, in the middle.

**The loader** paints once, after its last line, while boot services are
up. It paints every graphics output through firmware's `Blt`, not the
framebuffer the kernel is handed: the output kept for a panic is chosen
for where its memory is (`docs/DISPLAY.md` §2.4), which under QEMU is the
VGA while the window shows the virtio-gpu, and a virtio-gpu has no linear
framebuffer to write. With the word on the command line the hand-over's
progress bars (`stripes.rs`) stay off the screen; the red bar of a step
that failed does not.

**`/bin/splash`** is started by init beside `devmgr`, before `/` is known
(`early.target`, `docs/INIT.md` §7.3), because most of a boot's wait is the
drivers and the root volume. It waits for card0, shows the loader's
picture, and then follows the boot by reading the kernel log from its
first line. Where the kernel starts `devmgr` itself there is no
`early.target`, and `sysinit.target` wants the unit instead.

## 2. The slash's levels

| Level | Reached when | Read from |
|---|---|---|
| 100 | the kernel is loaded | the loader |
| 300 | card0 is up | `/bin/splash` has the card |
| 550 | every device has its driver | the kernel's `devmgr   started by pid 1` |
| 700 | `/` is known | init's `/ is the root volume now` or `/ stays in memory` |
| 780 | `sysinit.target` | init's `sysinit.target: active` |
| 860 | `basic.target` | init's `basic.target: active` |
| 940 | `multi-user.target` | init's `multi-user.target: active` |
| 1000 | the compositor is next | `splash --quit` |

Progress is milestones, not time. A stage that is slow shows as a slash
that waits. The level never falls.

## 3. When something needs saying

One line under the mark, and a smaller one under it for the keys.

* **A stage runs long.** Once a stage has taken twice its usual time:
  what the boot is doing and for how long, such as `starting services -
  14 s`, and `Esc shows the log`. The usual times are in `boot.rs`
  (`Stage::usual_ms`), rounded up from the slower of two boots measured on
  2026-11-12, the N150 box and QEMU under KVM: 5 s for the root volume
  (2.8 s and 4.4 s), 4 s for the drivers (0.8 s and under 0.1 s after the
  display's), the services and the desktop, 2 s for the two targets
  between, which took tens of milliseconds.
* **A driver was started again.** `intel driver restarted`, for two
  seconds, from `devmgr-events`' `restarted` line. The slash stays.
* **The boot cannot go on.** `Ferrix cannot start: <unit> failed`, in the
  text colour, naming the last service that failed or timed out, or the
  target when none did. When a unit the boot requires fails, init starts
  `rescue.target`, which stops `devmgr` and with it the card's driver and
  the keyboards'. So the note is drawn on init's `starting rescue.target`
  line, the last moment the screen answers, and stays as the screen's last
  picture, with `the rescue shell is on the console` under it: no key
  reaches the splash after that. Where init says a target failed without
  going to the rescue target, the line under it is `Esc shows the log,
  Enter restarts`, and Enter runs `svc reboot`.
* **Esc** shows the newest lines of the kernel log under a small mark, the
  last one in the text colour; Esc again returns.

The text is the boot console's font (`ferrix-fbtext`, Spleen 8x16, ASCII),
not Inter or JetBrains Mono as in the mockup.

## 4. The hand-over to the compositor

hyprix is master of the card only if nobody is when it opens it
(`docs/DISPLAY.md` §2.5), so the splash must be gone first. `hyprix.service`
gets a drop-in, `ExecStartPre=-/bin/splash --quit`. `--quit` connects to
the abstract socket `ferrix.splash` and waits for the connection to end.
The splash lights the whole slash and exits; the end of its process ends
the connection and its hold on the card. A `--quit` that finds nobody
listening leaves `/run/splash.left`, which a splash that had not bound its
socket yet reads before it opens the card.

Closing the card turns its screens off. hyprix's first buffer on each of
its first screens then holds the splash's picture with the slash whole,
not black (`hyprix/src/backend.rs`, `after_splash`), so the screen shows
the mark again from hyprix's mode set until its first frame. A monitor
plugged in later starts black as before.

## 5. What an image carries

`tools/common/xtask/src/splash.rs`: `/bin/splash`; `splash.service`
(`DefaultDependencies=no`); `early.target` and its want; `sysinit.target`'s
want; and the drop-in. The gates' images carry none of it, and their
command lines are unchanged. `ferrix.splash` is declared in
`build-flags.toml`'s registry and permitted by the `desktop` profile.

| Image | The word | The unit runs |
|---|---|---|
| `run-compositor`, `release-image` | in `DEFAULTS.TXT` | under the word |
| the live ISO, and what it installs | in the ISO's command line, which the installer keeps | under the word |
| the N150 box (`n150-install`) | in `DEFAULTS.TXT`; the box's deploy scripts write the same | under the word |
| `flash --compositor`: the DK1's card | in `DEFAULTS.TXT`, which U-Boot's loader reads | always |
| `flash --compositor --stage`: the Pixel 7 app's `desktop.Image` | none: its loader is the phone's own and its command line the app's | always |

The live ISO and the N150 box had `ferrix.fbcon` where the word is now.
The two do not go together, since the console draws over the splash; a
machine that stops before the desktop is booted with `ferrix.fbcon` in
the word's place to read why. hyprix does not look at the command line:
`splash --quit` leaves `/run/splash.shown` when a splash was there, and
that is what makes hyprix's first buffer the splash's picture (§4).

## 6. Where it stands

Seen on 2026-11-12, x86-64:

* **QEMU under KVM**, `cargo xtask run-compositor --no-gl`, the screen
  read every 30 ms over QMP. The loader's mark for 0.25 s; "Display output
  is not active" for 1.2 s; `/bin/splash` for 5.5 s with the slash rising;
  one frame of "not active"; the mark with the slash whole in hyprix's
  first buffer for 0.15 s; the desktop. The 1.2 s are QEMU's: OVMF's
  virtio-gpu driver resets the device when the loader leaves boot
  services, so the loader's picture is gone before the kernel runs.
* **The same with a unit that sleeps 30 s before hyprix**: `starting
  services - 8 s` under the mark, counting each second; Esc showed the
  log, Esc again the mark.
* **The same with a unit `multi-user.target` requires that fails**:
  `Ferrix cannot start: splash-fail.service failed` and the rescue line,
  left on the screen.
* **The N150 box** (image of a44359365, `ferrix.splash` in place of
  `ferrix.fbcon`, a DELL U2415 at 1920x1200), read from the kernel log
  only, since nothing can read the box's screen from here. The Intel
  driver took the firmware's lit plane at 8.45 s of kernel time and had
  its first frame at 8.83 s; `splash: card0 1920x1200` at 8.88 s; the
  drivers' report at 9.68 s; `/` at 12.43 s; `hyprix.service` at 12.49 s
  and the splash stopped; `hyprix: card0` at 12.63 s. The loader's
  picture is what the screen has for the kernel's first 8.4 s.
  **Nobody looked at the monitor.**

The certification consultant's review is ledger line 9005: OK with
conditions, all of them documents and gate rows. No file under
`src/kernel` changes, and no requirement, check or baseline.

Not done:

* **The slash stands at 100 while the kernel starts**, which is 8.4 s of
  the box's 12.6 s. Lighting it there means the kernel drawing, a change
  to the certified item that needs a design review first (the consultant,
  ledger 9005, C3).
* **The mark does not move to its place on the wallpaper.** The splash's
  picture is cut to the desktop's first frame.
* **Not seen on the DK1 or the Pixel 7**, and not on AArch64 or ARMv7-A
  under QEMU. The loader's part builds for both; `/bin/splash` is built
  where the compositor is.
* **A loader that hangs in the hand-over shows no bar under the word.**
  The bars of steps that went well are off the screen, so only a step
  that fails is marked. A machine that stops there without failing is
  booted without `ferrix.splash` to see where: the N150's hang in
  `ExitBootServices` was placed by those bars.
* **The Pixel 7 has no picture before `/bin/splash`**: its loader is not
  the UEFI one and paints nothing. Neither the phone nor the DK1 has been
  booted with the splash; both images build.
* **No gate.** The boots above were driven by hand. `docs/BACKLOG.md` has
  the row.
* **A log line with control characters shows them as the kernel log
  escapes them** (`\x0d`), in the log view.
