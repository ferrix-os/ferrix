# GNOME on Ferrix: what a probe found

Asked by the customer on 2026-10-18 (session 025c420a): can GNOME run on
Ferrix, and what would it take? That session assessed it from the tree and
the certification consultant answered a GNOME port **not yet** (ledger line
2289, blockers B1 to B5 below). The step agreed then, and done here on
2026-10-22, was a feasibility probe: run Debian 13's own mutter and
gnome-shell on a Ferrix guest and write down exactly how far they get and
what stops them, before any design. This is an assessment, not a decision
and not a stage.

## Where it stands, 2026-10-22

* **gnome-shell 48.7 runs on Ferrix**, unmodified Debian 13 binaries on
  Ferrix's kernel, in two ways:
  * **headless** (`--headless --virtual-monitor 1280x720`), drawing with
    Mesa's virgl on `/dev/dri/renderD128`. It starts in 0.7 s, answers
    `org.gnome.Shell` on the session bus, and its own screenshot shows the
    top bar, the Debian wallpaper, a libadwaita demo window, GTK 4's widget
    factory, GTK 4 gears at 59 fps and an X11 client (xeyes) through
    gnome-shell's own Xwayland, with mutter's frames.
  * **nested in hyprix**, as an X11 client of Debian's Xwayland running
    rootless on hyprix (`gnome-shell --nested`): GNOME Shell in a window on
    the Ferrix desktop, a libadwaita app inside it. Its own first
    notification is *"Screen Lock disabled: Screen Locking requires the
    GNOME display manager"* -- blocker B1, said by GNOME itself.
* **GTK 4 and libadwaita apps run on hyprix today**, from the same Debian
  tree: the widget factory, the libadwaita demo, and GTK 4's GL gears on
  virgl at 73 fps.
* **What did not work**, in the order the probe met it (§3 has each with
  its error, its place in the source and a size):
  1. a GL client of mutter on virgl crashes in Mesa, because the render
     node refuses `DRM_CAP_PRIME` (kernel, load ring, 1 point);
  2. with that answered, the client's dmabuf frames are composited **blank
     white** by mutter (hyprix shows the same client right); not yet
     root-caused;
  3. the system bus cannot drop to its own user: `prctl(PR_SET_KEEPCAPS)`
     and the other capability options are `EINVAL` (kernel, load ring);
  4. **mutter on the real screen stops at once**: *"Failed to setup: Failed
     to find any matching session"* -- there is no
     `org.freedesktop.login1`. Behind it: no udev database and no uevents,
     legacy KMS only, and card0 without GL.
* **Nothing in the tree changed** for the probe. Every workaround was in
  the guest's Debian tree (a configuration file, two preloaded shims) and
  is written down in §2, so nothing here needed the consultant's review
  as a change; the document itself was reviewed as a design (§6).
* **Recommended**: stage G1, GTK 4 and libadwaita apps on hyprix, now
  (§5). A full GNOME session waits on the customer's choice against the
  consultant's B1 to B5, and is sized in §5 at roughly 60 to 80 points
  before GDM.

---

## 1. Why a probe and not a design

The 2026-10-18 assessment listed what GNOME needs and Ferrix lacks: D-Bus,
logind, a `systemd --user` stand-in, udev, EGL, atomic KMS, PipeWire,
polkit, accountsservice and GDM. It could not say which of those are walls
and which are warnings, because GNOME degrades gracefully around most
missing services and fails hard on a few. Only running it says which.

## 2. How the probe ran

**The tree.** A Debian 13 (trixie) root built in Docker on the host from
`debian:trixie`, with `apt-get install --no-install-recommends mutter
gnome-shell dbus-daemon dbus-bin libegl-mesa0 libgl1-mesa-dri libgles2
libglx-mesa0 xwayland gtk-4-examples libadwaita-1-examples strace
fonts-cantarell adwaita-icon-theme gsettings-desktop-schemas procps less`:
550 packages, 1.2 GB, mutter and gnome-shell 48.7, Mesa 25.0.7, GLib 2.84,
GTK 4.18. `docker export` into a tree, `mkfs.btrfs --rootdir` into a
volume. The package list is kept beside it
(`~/.local/share/ferrix/gnome-probe/pkglist.txt`). Unpinned: this is a
probe, not a fetch script.

**The boot.** `cargo xtask run-compositor --chrome --interpreter glibc
--release --ssh 2251 --vnc :51 --memory 4096 --tmpfs-root --no-dotfiles`,
x86-64 under KVM on nazuna, with `FERRIX_CHROME_VOLUME` pointing at the
probe's directory, so the volume is mounted at `/data` as Chrome's would
be (`tools/common/xtask/src/compositor/run.rs`, `data_disk`). The desktop
is hyprix as root, as on every non-session image.

**A Debian root inside Ferrix.** Over `sshdt`: `mount --bind` of `/dev`,
`/proc`, `/sys`, `/tmp`, `/run` and `/dev/shm` into `/data`, then `chroot
/data`. Both are Ferrix's own (`syscall/fsctl.rs`), and Debian's
programs then see a Debian filesystem exactly as on Linux. **`apt-get`
works inside it**, over the desktop's NAT: `gdb`, `mesa-utils`,
`gstreamer1.0-plugins-base`, `x11-apps` and `x11-utils` were installed on
Ferrix during the probe.

**The buses.** `dbus-daemon --session` started first try. The system bus
needed a configuration without `<user>messagebus</user>` (gap 3, §3).

**Seeing a crash without a debugger.** `ptrace` is `ENOSYS` (gap 10), so
neither gdb nor strace runs. A 50-line `LD_PRELOAD` library caught
`SIGSEGV`, printed the faulting address and the return addresses on the
stack with `dladdr`, and the host resolved them against Debian's debug
symbols from `debuginfod.debian.net`. **Seeing the screen**: gnome-shell's
own `org.gnome.Shell.Screenshot` (under `--unsafe-mode`), and for hyprix a
raw-encoding RFB client reading QEMU's VNC server.

Every guest script, every log and every screenshot is in
`~/.local/share/ferrix/gnome-probe/` on nazuna (`guest/`, `shots/`,
`shims/`); the screenshots are not committed, since they carry Debian's
and the user's artwork.

## 3. What failed, in the order it was met

Each gap: what was seen, why, where in Ferrix it would change, which ring
of `tools/common/data/certification-item.json` that is, and a size in
points. None was changed here: all but gaps 4, 5 and 8 are in the kernel.

### Gap 1: the render node refuses `DRM_CAP_PRIME` -- 1 point, kernel, load ring

**Seen.** Under headless mutter, every GL client crashed:
`gtk4-widget-factory` and `gtk4-demo --run=gears` with `SIGSEGV` at
address 0, the return address in `libgallium-25.0.7` at `+0x10c9c7`,
`kopper_allocate_textures` (`src/gallium/frontends/dri/kopper.c:323`),
called from `dri_st_framebuffer_validate`. Mesa printed `libEGL warning:
wayland-egl: display does not support prime` first.

**Why.** Mesa's Wayland platform asks the compositor's main device for
`DRM_CAP_PRIME` and, without import and export, takes a software path --
which with the virgl driver loaded calls a screen hook only Zink has, a
null pointer. Ferrix's render node answers `DRM_IOCTL_GET_CAP` for the
sync-object capabilities alone, `EINVAL` for everything else
(`src/kernel/src/interfaces/render/node.rs`, `get_cap`), although it
implements both PRIME calls (`export`, `import`, the same file).

**Shown.** An `LD_PRELOAD` shim that answers `DRM_CAP_PRIME` with
`DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT` (3) when the kernel says
`EINVAL`: the same clients then run on virgl, and `eglinfo -p wayland`
reports `virgl (... radeonsi ...)`. Without the shim, the clients run
under mutter with `LIBGL_ALWAYS_SOFTWARE=1` or GTK's cairo renderer.

**Proposed change.** `get_cap` answers `drm::CAP_PRIME` with 3, as Linux's
`drm_getcap` does for a driver with PRIME (since 6.6 always). A host test
and a line in `test-display`'s render checks. The Mesa null call is
upstream's, and worth a report.

### Gap 2: mutter composites a virgl client's dmabuf blank -- not yet sized

**Seen.** With gap 1 shimmed, GTK 4's gears and widget factory run on
virgl under gnome-shell, and the shell's screenshot shows their windows
**plain white** (`shots/shell-gears-virgl-blank.png`). The same program
with software GL, which sends `wl_shm` buffers, draws at 59 fps
(`shots/shell-gears-software-gl.png`). The same virgl client on hyprix is
drawn right, at 73 fps (`shots/hyprix-gears-virgl.png`). mutter itself
renders on virgl: its threads have no `llvmpipe` workers, and its
wallpaper and panel draw.

**What it is not.** The render node's import attaches the object to the
importer's context (`node.rs`, `import`, which says Linux attaches on
every handle it opens), so this is not the missing attach.

**Where to look next.** The difference between hyprix's import (its own
virgl stream, `src/user/system/linux/compositor/virgl`) and Mesa's
(`EGL_EXT_image_dma_buf_import` in mutter's process): implicit
synchronisation, which Ferrix's dmabufs do not carry (`docs/GPU.md`
§3.13), the format and modifier mutter asks for, and the resource's bind
flags. A day's investigation (3 points) before it can be sized. Until then
GNOME's own GL path is software, and its clients' GL must be too
(`LIBGL_ALWAYS_SOFTWARE=1`) -- workable, and how the probe ran.

### Gap 3: `prctl`'s capability options are `EINVAL` -- 2 to 3 points, kernel, load ring

**Seen.** `dbus-daemon --system`: *"Failed to start message bus: Failed to
drop capabilities: Invalid argument"*. A probe program on Ferrix:
`PR_SET_KEEPCAPS`, `PR_GET_KEEPCAPS`, `PR_CAP_AMBIENT` and
`PR_GET_SECUREBITS` all `EINVAL`.

**Why.** libcap-ng's `capng_change_id`, which dbus-daemon uses to become
`messagebus`, starts with `prctl(PR_SET_KEEPCAPS, 1)`. `sys_prctl`
(`src/kernel/src/syscall/attributes.rs`) knows the bounding-set options and
nothing else of capabilities. polkitd, accountsservice, NetworkManager and
every other system service that drops root the libcap-ng way will stop at
the same place.

**Proposed change.** `PR_SET_KEEPCAPS`/`PR_GET_KEEPCAPS` with Linux's
semantics (the permitted set survives a `setresuid` away from 0 only with
the flag, cleared at `execve`), `PR_CAP_AMBIENT` at least for
`CLEAR_ALL` and `IS_SET`, and `PR_GET_SECUREBITS` answering 0. The
`setresuid` half is in `syscall/credentials.rs`, also the load ring, but
it changes how privilege is dropped, so it goes to the consultant as a
change that touches the item's evidence of credentials.

**The probe's workaround.** A copy of `system.conf` without the `<user>`
line, so the system bus ran as root.

### Gap 4: the volume lost its set-uid helpers -- 2 points, ring 3 (xtask, fetch)

**Seen.** Every activated system service: *"The permission of the setuid
helper is not correct"* (`dbus-daemon-launch-helper`), so polkit,
colord and accountsservice never started.

**Why.** The probe's own doing: the tree was unpacked as a user with
`--no-same-owner --no-same-permissions`, so the helper is neither root's
nor set-uid, and `messagebus` owns nothing. A real volume needs a tree
built with owners and modes kept (`fakeroot`, or `mkfs.btrfs --rootdir`
from a user namespace), and an answer to whether a set-uid binary on a
data volume is honoured at all. That second point is a security
decision, not a packaging one: B3 and B5.

### Gap 5: session plumbing -- 2 points, ring 3

Each of these warned or stopped one piece, none was a wall, and each is
work a session start does on Linux through systemd and tmpfiles:

* `/tmp/.X11-unix` must be mode 1777, or mutter's Xwayland refuses with
  *"Directory "/tmp/.X11-unix" is not writable"* and gnome-shell, for
  which Xwayland is then mandatory, exits.
* an X client needs `XAUTHORITY` from mutter's
  `.mutter-Xwaylandauth.*`;
* the accessibility bus (`org.a11y.Bus`, at-spi2-core), fontconfig's
  `fonts.conf`, `XDG_RUNTIME_DIR` made per user, the two buses as units.

### Gap 6: no logind -- 8 to 13 points, ring 3 (sessiond)

**Seen.** `mutter --display-server --wayland --no-x11`, the real screen:

```
Failed to get the session from environment: XDG_SESSION_ID is not set
Failed to get the session from login1: ... org.freedesktop.login1 without an owner
Failed to get any session: User 0 has no sessions
Failed to setup: Failed to find any matching session
```

This is the wall for GNOME on the screen, and the first one: mutter's
native back end opens no device by itself, it asks logind.

**What it needs** of `org.freedesktop.login1` on the system bus: the
manager's `GetSession`, `GetSessionByPID`, `ListSessions`; the session's
`TakeControl`, `ReleaseControl`, `TakeDevice`, `ReleaseDevice`,
`PauseDeviceComplete`, `Activate`, `SetType`, with the `PauseDevice` and
`ResumeDevice` signals and the `Active`, `Seat`, `Type`, `Class`, `State`
properties; the seat's `ActiveSession` and `CanGraphical`. And
`sd_pid_get_session`, which libsystemd answers from `/proc/self/cgroup`
by the `session-N.scope` component -- the scope `sessiond` already makes
(`docs/AUTH.md` §6.4), so that half may already line up.

**Where.** `sessiond` (`src/user/system/linux/compositor/sessiond`) is
already logind's device half -- it opens the card, render node and `event*`
nodes and passes them over `SCM_RIGHTS` -- and since 2026-10-18 the card
has a real DRM master that can be dropped and taken back
(`docs/DISPLAY.md` §2.5) and input has `EVIOCREVOKE`. A login1 front on
top of it is ring-3 D-Bus work, not kernel work. It is the honest form of
B2.

### Gap 7: no udev database, no uevents -- 3 points ring 3, or 8 in the item

**Seen.** `/sys/class/drm` and `/sys/class/input` enumerate, so libudev's
enumeration works (headless mutter found `renderD128` through it). But
`/run/udev/data` does not exist, so no device has `ID_SEAT`, the `seat`
tag, or `ID_INPUT_KEYBOARD` and the rest that libinput needs before it
will open a node. Debian's `systemd-udevd` starts on Ferrix; `udevadm
trigger` then fails on every device with *"Failed to write 'add' to
'.../uevent': Permission denied"*: the `uevent` files are read-only, and
the kernel sends no uevents (`src/kernel/src/net/netlink/mod.rs` has none).

**Two ways.** (a) A ring-3 writer of the udev database from what `devmgr`
already knows, for the devices present at boot -- 3 points, no hotplug.
(b) Real uevents: the kernel broadcasting `NETLINK_KOBJECT_UEVENT` and
honouring writes to `uevent` -- about 8 points, and the consultant has
already said it is a core `discovery/devmgr.rs` change through a hook the
item defines (ledger 2289). (a) is enough for one seat with fixed devices.

### Gap 8: KMS for mutter -- not measured, 5 points to probe after gaps 6 and 7

Not reached: the probe stopped at gap 6. What is known from the tree:
card0 refuses `DRM_CLIENT_CAP_ATOMIC` with `EOPNOTSUPP`, which mutter
takes as the legacy path (it has one); card0 has dumb buffers but no GL,
so mutter must draw on `renderD128` and show the result on card0, its
multi-GPU copy path, through card0's `PRIME_FD_TO_HANDLE`
(`docs/GPU.md` §3.13) or a CPU copy. Whether mutter accepts a card with no
render half as its display device is the first thing to try once a logind
exists.

### Gap 9: PipeWire -- 5 to 8 points, ring 3

*"Error connecting to the screencast service"*. Screencasts, screenshots
through the portal, remote desktop and the `--devkit` viewer all need
PipeWire; sound in GNOME goes through PulseAudio's protocol, which
Ferrix's own pulse server speaks (`docs/AUDIO.md`). Not tried.

### Gap 10: no `ptrace` -- tooling only, decided

gdb says *"ptrace: Function not implemented"*. The customer decided on
2026-10-21 that `ptrace` stays out of the certified `present` profile and
comes in a debug build flag (`docs/DEBUG.md` §4, S2). It costs GNOME
nothing at run time; the probe used the preload above instead.

### What was missing and did not matter

gnome-shell started and ran without each of these, with a warning: colord,
polkit, accountsservice, geoclue, ibus, the calendar server
(evolution-data-server), GDM, NetworkManager, `org.freedesktop.locale1`.
Before the system bus existed it did stop: `endSessionDialog.js` asks
for the system bus at start-up and the exception ends the shell (and
gjs then crashes freeing its error, `g_error_free` from
`libdconfsettings`). So the system bus is a hard requirement, the services
on it are not.

## 4. The consultant's blockers, gap by gap

The consultant's NOT YET of 2026-10-17 (ledger 2289), and what the probe
adds to each:

| | Blocker | What the probe showed | Gaps |
|---|---|---|---|
| B1 | unlock through GDM and Debian's libpam, not `authd`'s grant | GNOME's own notification: *"Screen Locking requires the GNOME display manager"*. Without GDM there is no lock at all; with it, the unlock is PAM's. Either way §3.7 of `docs/AUTH.md` is not what unlocks | -- |
| B2 | devices not taken back the way logind takes them | card0's master and `EVIOCREVOKE` now exist; what is missing is the logind interface over them | 6, 7, 8 |
| B3 | polkit and accountsservice, a second place that signs people in | both are activated services and both failed here only on the probe's packaging; making them start makes them real | 3, 4 |
| B4 | gnome-session or `systemd --user` restarting a crashed shell, against "the session ends with its compositor" | gnome-session was not installed and the shell ran; a session without gnome-session is possible for the shell itself | 5 |
| B5 | the security case redone for Debian's SOUP | 550 Debian packages under the shell, against Chrome's ~40 libraries today | all |

Stage G1 below touches none of B1 to B4: its apps are clients of hyprix,
which keeps the screen, the lock and the session.

## 5. A staged plan

**G1: GTK 4 and libadwaita apps on hyprix -- about 8 points.** Works in
the probe now. What it takes: a fetch script that pins a Debian tree of
GTK 4, libadwaita and the apps wanted (GNOME Text Editor, Files, Calculator,
...) as `fetch-claude-desktop.sh` pins its packages, merged into the
`--everything` volume; their GL on virgl through gap 1 (or software until
it lands); a session bus for the user, started by `sessiond` with the
compositor; the accessibility bus or `NO_AT_BRIDGE=1`. The portal
(`xdg-desktop-portal-gtk`) for file choosers can follow. No credential, no
device, no session change, so none of B1 to B4.

**G2: GNOME Shell as a window on hyprix -- about 5 points more.** Works in
the probe now, nested on Xwayland. Useful as a demonstration and for
working on GNOME without leaving the Ferrix desktop; its lock is disabled
and must stay so, which is what B1 wants anyway.

**G3: a GNOME session on the screen, as the user, without GDM -- about 40
to 55 points.** In order: gap 3 (prctl) and gap 1, kernel, each reviewed;
gap 4, a volume with owners and modes, and the decision on set-uid
helpers on a data volume (B3, B5); gap 6, login1 on `sessiond` (B2);
gap 7 (a); gap 8; gap 2; gap 5's units. `sessiond` starts gnome-shell
directly in the session's scope, no gnome-session (B4), and the lock is
hyprlock's or nothing until B1 is answered.

**G4: the full session -- unsized.** GDM through a PAM shim onto `authd`
(`docs/AUTH.md` §4.3 already plans the shim for sudo and OpenSSH),
polkit's agent onto `authd`, gnome-session or not, gnome-settings-daemon,
PipeWire (gap 9), NetworkManager or Ferrix's own network through a
stand-in. Each of these is one of the consultant's blockers, and the
customer's choice first.

The consultant's recommendation of 2026-10-17 was option A, a floating
window manager of Ferrix's own, over GNOME. G1 is compatible with either:
it brings GNOME's apps, not GNOME's session.

## 6. The certification consultant's review

*(Recorded below once given.)*

## 7. What was checked, and what was not

* Checked: everything in §2 and §3 marked *seen* was seen on 2026-10-22 on
  `main` at 08b0e1380, x86-64 under KVM on nazuna, with the logs and
  screenshots named.
* Not checked: AArch64; the real screen (gap 8); any session as a user
  rather than root; gnome-session, gnome-settings-daemon, GDM, PipeWire
  and Nautilus; memory use; how long any of it stays up.
* The sizes are estimates against this tree's own past landings, not
  measurements.
