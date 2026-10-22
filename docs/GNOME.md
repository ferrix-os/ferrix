# GNOME on Ferrix: what a probe found

Asked by the customer on 2026-10-17: can GNOME run on Ferrix, and what
would it take? The certification consultant answered a GNOME port **not
yet** on 2026-10-17 (ledger line 2289, blockers B1 to B5 below) and
recommended option A; session 025c420a carried the assessment on from the
tree on 2026-10-18. The feasibility probe done here on 2026-10-22 was
chosen afterwards: run Debian 13's own mutter and
gnome-shell on a Ferrix guest and write down exactly how far they get and
what stops them, before any design. This is an assessment, not a decision
and not a stage.

## Where it stands, 2026-10-22 (evening): G1 built, gaps 1 and 2 fixed

Branch `gnome-probe`, not landed (no landing lock was taken). The customer
asked for the gaps to be fixed ("start gnome agent to fix the issues"), and
on the same day: *keep the kernel slim, we need that for certifications* --
ring-3 answers for every gap, kernel changes only where unavoidable and as
small as possible.

| Gap | State | Where | Commits | Consultant |
|---|---|---|---|---|
| 1 `DRM_CAP_PRIME` | **fixed** | kernel, load (`interfaces/render`) | dc07c52c5 | OK IF, ledger 3112 |
| 2 white virgl frames under mutter | **root-caused and fixed**: a 64 KiB command-buffer limit, not dmabufs | kernel, load (`interfaces/render`), virtio-gpu driver and lib (ring 3) | e4867eeed, a8b5c562d, 8acf91f88 | NOT YET 3121 (B1, a 512 KiB heap block), B1 closed, OK IF 3126 |
| 3 `prctl`/`capset` | design only (§8.1): ring 3 first, no kernel change for the system bus | -- | -- | design sent |
| 4 set-uid helpers | **answered for G1**: none shipped, refused by the fetch; for G3 NOT YET (G3-5) | ring 3 | 774d9f09b | 3100, 3123 |
| 5 session plumbing | **what G1 needs is built**: the session bus (sessiond), `XDG_RUNTIME_DIR`, fonts, XKB, schemas, MIME, `GTK_A11Y=none`; Xwayland's `/tmp/.X11-unix` and the a11y bus are G2/G3's | ring 3 | 4fc0770d4 | 3123 |
| 6 login1 | design only (§8.2), on `sessiond`, ring 3 | -- | -- | design sent |
| 7 udev | design only (§8.3): way (a), a ring-3 writer of the database; no kernel uevents | -- | -- | design sent |
| 8 KMS for mutter | not reached (after 6 and 7) | -- | -- | G3-8 |
| 9 PipeWire | not started | ring 3 | -- | -- |
| 10 `ptrace` | decided: debug builds only (customer, 2026-10-21) | -- | -- | -- |

* **Gap 1** (§3): the render node answers `DRM_CAP_PRIME` with import and
  export, so Mesa's Wayland platform shares GL buffers as dmabufs under
  mutter; `test-display --gl` requires `prime 3`.
* **Gap 2** (§3): mutter was never at fault. GTK 4's client sent one
  262,176-byte `EXECBUFFER`, the render core's slot was 64 KiB, the call
  was `EINVAL`, and Mesa dropped the stream ("got error from kernel -
  expect bad rendering"). The slot is now 65 pages, Mesa virgl's longest
  (`VIRGL_MAX_CMDBUF_DWORDS`), copied in a page at a time once the slot is
  taken (the consultant's B1: no 512 KiB contiguous heap block). Debian's
  gnome-shell headless now shows GTK 4's GL gears on virgl at 59 fps with
  no shim (screenshot off-tree,
  `~/.local/share/ferrix/gnome-probe/shots/shell-gears-virgl-fixed.png`);
  `test-display --gl` requires `long 266240`.
* **G1 is built** (§5): `tools/common/fetch/fetch-gnome-apps.sh` pins 171
  Debian 13 packages by SHA-256 at one snapshot; `sessiond --session-bus`
  starts the session's D-Bus bus as the user in the session's scope;
  `data-nosuid.service` remounts `/data` `nosuid,nodev` before any session;
  `cargo xtask test-gnome-apps` boots it and requires libadwaita's demo on
  hyprix as `ferrix`, on the bus, and gone with the session
  (`~/.local/share/ferrix/gnome-probe/shots/g1-test-gnome-apps.png`).
* **Kernel lines**: 95 added and 28 removed, all in the load ring's
  `interfaces/render` -- 40 added and 16 removed are code, the rest the
  comments that say why. Gap 1 is one match arm; gap 2 is the slot size,
  the work VMO's size and the page-at-a-time copy. Nothing else in G1
  touches the kernel.
* **What G2 and G3 still need**: a system bus (G2 and G3), and for G3 the
  lock on `authd`'s grant (B1), login1 on `sessiond`, the udev database,
  KMS on card0 and the session units -- §5 and §8.

## The probe, 2026-10-22

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
  (§5); the consultant's verdict is **OK IF** nine conditions (ledger 3100,
  §6). A GNOME session on the screen (G3, 40 to 55 points before GDM) is
  **NOT YET**: B1 is still open, and four of its gaps owe design reviews
  before code.

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
points. None was changed here. Gaps 1 and 3 are in the kernel's load
ring, and gap 7 is in the kernel only in its way (b); gaps 4, 5, 6, 7 (a)
and 9 are ring 3, gap 8 is unmeasured (card0, load ring, if anything),
gap 2 is not yet placed, and gap 10 is decided.

### Gap 1: the render node refuses `DRM_CAP_PRIME` -- 1 point, kernel, load ring -- FIXED (dc07c52c5, ledger 3112)

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

**Done (dc07c52c5, ledger 3112).** `get_cap` answers
`PRIME_CAP_IMPORT | PRIME_CAP_EXPORT`, the constants taken from the UAPI
probe; the card's `get_cap` is unchanged; the render probe prints
`prime <value>` and `test-display --gl` requires `prime 3`. No requirement
is owed: the render node is load-ring code, outside the claim (3112).
Without a shim, Mesa no longer says *"display does not support prime"*
under mutter.

### Gap 2: mutter composites a virgl client's dmabuf blank -- FIXED (e4867eeed, a8b5c562d, 8acf91f88; ledger 3121, 3126)

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

**Root cause, found 2026-10-22.** An `LD_PRELOAD` trace of the
render-node ioctls of mutter and of the client showed nothing wrong with
the dmabuf path -- three `PRIME_FD_TO_HANDLE`s, attached, cached -- and one
failure in the client: a `DRM_IOCTL_VIRTGPU_EXECBUFFER` of 262,176 bytes
answered `EINVAL`, after which Mesa printed *"got error from kernel -
expect bad rendering 22"* and dropped the stream. Mesa's virgl driver
fills a command buffer up to `VIRGL_MAX_CMDBUF_DWORDS`, `64 * 1024 + 1024`
words (266,240 bytes), before it submits, and a buffer that uploads pixels
inline (`RESOURCE_INLINE_WRITE`) fills to the end; GTK 4 sends one under
mutter, and none under hyprix. The render core's slot was 64 KiB; Linux's
virtio-gpu takes any length. So the white window was everything the
client drew after its first upload, never sent.

**The fix.** `COMMAND_BYTES` is 65 pages and the work VMO two megabytes
(seven slots); the virtio-gpu driver's copied stream, its command area and
its longest descriptor chain (72) grew to match (e4867eeed). The
consultant's review (ledger 3121) found the node copied the whole stream
into one kernel heap block first -- 512 KiB contiguous at the new size,
charged to nobody, held while every slot was busy -- and asked for it to
go (B1): the stream is now read from the program a page at a time into
its slot once the slot is taken (a8b5c562d). The cost, said in `submit`'s
comment and `docs/GPU.md` (A7, 8acf91f88): a slot is held while the
program's buffer faults in. `test-display --gl` submits 266,240 bytes of
virgl `NOP`s and requires `long 266240`; with the old slot it reads `long
none EINVAL`, the negative control the consultant asked for (A2).

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

And `capset` (`syscall/credentials.rs`, `sys_capset`), which refuses any
non-empty set from a process whose effective uid is not 0 in the first
namespace, so libcap-ng's `capng_apply` after the uid change fails even
with KEEPCAPS answered. In the first namespace no capability is effective
for an effective uid other than 0 (`Credentials::holds`), so "Linux's
semantics" cannot mean a non-root process holds what it kept. The 2 to 3
points are the prctl half; the capset half waits on its design review
(ledger 3100, G3-2).

**The probe's workaround.** A copy of `system.conf` without the `<user>`
line, so the system bus ran as root.

### Gap 4: the volume lost its set-uid helpers -- 2 points, ring 3 (xtask, fetch) -- for G1: none shipped

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

### Gap 5: session plumbing -- 2 points, ring 3 -- G1's part built

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

### Gap 7: no udev database, no uevents -- 3 points ring 3, or about 8 in the kernel (item and load rings)

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
honouring writes to `uevent` -- about 8 points: the producer in
`discovery/devmgr.rs`, which is in the item ring (ledger 2289 called it
core; corrected in ledger 3100), reaching the load ring's `net/netlink`
broadcast and the sysfs `uevent` files only through a hook the item
defines. The consultant recommends (a) (ledger 3100, G3-4). (a) is enough for one seat with fixed devices.

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

Stage G1 below touches none of B1 to B4 under the conditions of ledger
3100: its apps are clients of hyprix, which keeps the screen, the lock and
the session. It does bring Debian third-party software onto the volume, so
B5 partly applies.

## 5. A staged plan

**G1: GTK 4 and libadwaita apps on hyprix -- about 8 points; built
2026-10-22 (774d9f09b, 4fc0770d4), not landed.** To the consultant's
conditions of ledger lines 3100 and 3123:

* **The volume.** `tools/common/fetch/fetch-gnome-apps.sh`: 171 Debian 13
  packages -- the dependency closure of GTK 4, libadwaita, their demos,
  Mesa, `dbus-daemon` and the session bus's configuration, fonts, icons
  and schemas -- pinned by pool path and SHA-256 and fetched from
  snapshot.debian.org at one time each for trixie and trixie-security. The
  script checks that the list is the whole closure, refuses a deny-list
  (system bus, systemd, PAM, polkit, pkexec, gvfs, accountsservice, GDM,
  gnome-keyring, udisks2, portals), any set-id mode in an archive's
  listing or in the tree, any file capability, `dbus-daemon-launch-helper`
  anywhere, and any maintainer script that would set one; it writes GLib's
  schemas, gdk-pixbuf's loader list and the MIME database as the triggers
  would. Every file on the volume is root's: `mkfs.btrfs --rootdir` copies
  the owner it reads -- this machine's user, uid 1000, the guest's
  `ferrix` (Chrome's volume has the same, the consultant's L3) -- so it
  is given each file's stat as root's through a preloaded `nftw`, and the
  image is read back with `btrfs inspect-internal dump-tree` to prove
  4865 of 4865 inodes `0:0` (the consultant's A1, ledger 3137). A volume
  of its own, with its own glibc; merging it into
  `--everything` is the next step and needs no change of rule.
* **`/data` `nosuid,nodev`.** `data-nosuid.service`, a root oneshot,
  remounts it and fails unless `/proc/mounts` says both; the compositor's
  unit `Requires=` it, so a failed remount is no session. The kernel's own
  default for every data disk is backlog row p2-153.
* **The session bus.** `sessiond --session-bus DAEMON CONFIG`: the volume's
  `dbus-daemon`, as the user, forked before the compositor and put in the
  same `session-N.scope` by the same init call before either execs, its
  descriptors 0 to 2 and its address pipe, its socket
  `/run/user/<uid>/bus`. It ends with the scope; nothing restarts it. Its
  configuration is Ferrix's own (`/etc/ferrix/session-bus.conf`):
  `EXTERNAL` only, one `unix:runtime=yes` listen, the volume's service
  directory, no eavesdropping, no include. No system bus, no login1, no
  gnome-keyring, no portal.
* **Gates.** `cargo xtask test-gnome-apps` (x86_64): the session as
  `ferrix`, the volume at `/data`; required: `/data` `nosuid,nodev`, the bus
  `ferrix`'s at its socket and no system bus, `adwaita-1-demo` `ferrix`'s
  and on the bus, its window drawn, and after the compositor is killed
  neither the bus nor the app left; the volume's files root's
  (`dbus-daemon` `0 0 755`, a write by `ferrix` refused). It boots without
  the 3D card, so GTK draws with Mesa's software rasterizer: QEMU cannot
  screendump a virgl scanout. GTK on virgl with the session bus is shown
  only by the manual run of §2; `test-display --gl` carries gaps 1 and 2. `test-compositor --boot session-end` starts a
  stand-in bus on every architecture and requires it `ferrix`'s, with only
  its address pipe, beating until the session's end and not after.
* **Not yet**: the apps a person wants (GNOME Text Editor, Files,
  Calculator) beside the demos, the launcher's entries, the `--everything`
  merge, and the portal for file choosers (said as a same-uid file
  chooser, with no sandbox claim, condition 7).

**What G1 does not answer (the consultant's C1).** A program of the
user's, a service the bus started included, may move itself into a scope
of its own under `user-<uid>.slice` before the session ends and so outlive
it: the session-end boot shows exactly that (`linger-scope: 0`). It is
AUTH §6.4's E6, older than G1, kept the customer's decision
(`KillUserProcesses=`); G1 adds no way to do it that a user did not have.

**G2: GNOME Shell as a window on hyprix -- about 5 points more.** Works in
the probe now, nested on Xwayland. It needs a system bus -- §3 shows the
shell ends without one -- which puts it in gap 3 and 4's and B3's
territory, and outside the consultant's G1 verdict. Useful as a demonstration and for
working on GNOME without leaving the Ferrix desktop; its lock is disabled
and must stay so, which is what B1 wants anyway.

**G3: a GNOME session on the screen, as the user, without GDM -- about 40
to 55 points.** In order: gap 3 (prctl) and gap 1, kernel, each reviewed;
gap 4, a volume with owners and modes, and the decision on set-uid
helpers on a data volume (B3, B5); gap 6, login1 on `sessiond` (B2);
gap 7 (a); gap 8; gap 2; gap 5's units. `sessiond` starts gnome-shell
directly in the session's scope, no gnome-session (B4). G3 needs a lock
that unlocks only on `authd`'s grant (`docs/AUTH.md` §3.7) before it is
offered to a user; whether hyprlock can lock a mutter session (it needs
`ext-session-lock-v1`) is unchecked. Until then G3 is a test image only,
and B1 is open.

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

Design review on 2026-10-22 by this session's consultant subagent,
recorded in the consultant's ledger
(`~/.local/share/ferrix/cert-consultant/reviews.md`) at **line 3100**,
conditions on lines 3101 to 3104. Internal review, not independent
verification (F-27).

* **(a) The record: OK IF (a1) to (a7)** -- seven corrections, all made in
  this document: which gaps are in the kernel (§3's opening), gap 7's ring
  (`devmgr.rs` is the item ring, not core as ledger 2289 said), `capset`
  in gap 3, the dates in the opening, G3's lock, G1's "none of B1 to B4"
  qualified, and G2's system bus.
* **(b) G1: OK IF (1) to (9).** (1) A fetch script pins every `.deb` by
  snapshot.debian.org URL and SHA-256, as `fetch-claude-desktop.sh` does,
  the package list in the tree, no `apt-get` on a shipped image. (2) The
  fetch fails on any set-uid or set-gid file or file capability, and on a
  deny-list (polkit, accountsservice, gdm3, any `libpam-*`, udisks2,
  `dbus-daemon-launch-helper` set-uid); the volume is mounted
  `nosuid,nodev`. (3) No system bus and no `org.freedesktop.login1`. (4)
  The session bus is started by `sessiond` only after `authd`'s grant, as
  the user, in the session's scope, its socket in the user's 0700 runtime
  directory; it ends with the session with everything it activated, nothing
  restarts any of it, the session-end gate asserts nothing survives and the
  lock gates stay green. (5) `sessiond`'s diff comes to the consultant
  against AUTH §3.7 and §6.4 and hands out no new device. (6) No
  gnome-keyring. (7) A portal, when added, says it is a same-uid file
  chooser with no sandbox claim. (8) The vulnerability analysis and A.AUTH
  name the Debian set as user-ring third-party software. (9) Gap 1, if
  taken, gets its own load-ring review; otherwise G1 ships with software
  GL.
* **(b) G3: NOT YET.** B1 is still open (no lock on `authd`'s grant under
  mutter), and gaps 3, 6, 7 (b) and the set-uid question each owe a design
  review before code: G3-2 prctl and `capset` (fail closed: no capability
  effective for a non-root process in the first namespace; ambient RAISE
  and SET_SECUREBITS refused; reserved ids; a check with a control); G3-3
  login1 on `sessiond` (`CreateSession` refused; `TakeControl` only for the
  session leader `sessiond` started, identified by pidfd, never by cgroup
  path; `TakeDevice` only the seat's devices to the active session; pausing
  drops card0's master and revokes input, gated with a control; power
  methods root-only or refused); G3-4 udev, way (a) acceptable and
  recommended, way (b) a design review; **G3-5 set-uid helpers on a data
  volume: NOT YET** -- data volumes stay `nosuid` and system services start
  as init's declared services; G3-6 a lock on `authd`'s grant or G3 is
  never offered to a user; G3-7 no gnome-session or `systemd --user`
  restart; G3-8 each card0 change reviewed against E-02; G3-9 the B5 case
  for the session's package set.
* **G2 is not covered** by the verdict: it needs a system bus, which G1's
  condition (3) forbids.

## 7. What was checked, and what was not

* Checked: everything in §2 and §3 marked *seen* was seen on 2026-10-22 on
  `main` at 08b0e1380, x86-64 under KVM on nazuna, with the logs and
  screenshots named.
* Not checked: AArch64; the real screen (gap 8); any session as a user
  rather than root; gnome-session, gnome-settings-daemon, GDM, PipeWire
  and Nautilus; memory use; how long any of it stays up.
* The sizes are estimates against this tree's own past landings, not
  measurements.

## 8. Designs for G3's gaps, before code (2026-10-22)

The consultant's G3 verdict (ledger 3100) asks a design review before any
code for gaps 3, 6 and 7 (b). The customer said on 2026-10-22: *keep the
kernel slim, we need that for certifications* -- ring-3 answers wherever
one exists, a kernel change only where none does, and the smallest one.
So each design below starts with the ring-3 way and names the kernel lines
it would cost otherwise. None of this is built; each waits for its verdict,
and G3 as a whole stays NOT YET while B1 (a lock on `authd`'s grant under
mutter) is open.

### 8.1 Gap 3: the system bus as `messagebus`, without `prctl`

**What needs it.** Only programs that drop root themselves with
libcap-ng: `dbus-daemon --system` (its `<user>messagebus</user>`), and
later polkitd, accountsservice and NetworkManager, which G3 does not
take (B3). GNOME Shell needs a system bus to start (§3, *What was missing*),
nothing on it.

**The ring-3 way (proposed): init drops privilege, the daemon never does.**
The system bus is a unit, `dbus-system.service`, with `User=messagebus`
and `Group=messagebus` -- init's own drop, which exists (`svc`'s
`User=`) -- and a configuration of Ferrix's own with **no `<user>`
element**: `dbus-daemon` calls libcap-ng's `capng_change_id` only to obey
that element, so it never asks `PR_SET_KEEPCAPS` and the kernel needs
nothing new. `/run/dbus` is made `0755 messagebus` by a root
`ExecStartPre=`. The bus activates nothing through
`dbus-daemon-launch-helper` (absent, G3-5): every system service is one of
init's declared units with its own `User=`, started by init, and the
configuration names no `<servicedir>` for the system bus. EXTERNAL
authentication only, its socket `/run/dbus/system_bus_socket` `0666` as on
Linux, and a policy that lets nobody own a name the image does not
declare. Kernel lines: 0.

**The kernel way, only if a service turns out to need it** (G3-2's rules,
fail closed): `PR_SET_KEEPCAPS`/`PR_GET_KEEPCAPS` as a per-process flag,
kept over `fork`, cleared at `execve` and by `setresuid` per Linux's rule,
changing nothing but what `capget` reports -- in the first namespace no
capability is effective for an effective uid other than 0
(`Credentials::holds`), and that stays; `PR_GET_SECUREBITS` answering 0
and `PR_SET_SECUREBITS` refused; `PR_CAP_AMBIENT` `IS_SET` 0, `LOWER` and
`CLEAR_ALL` accepted as no-ops, `RAISE` refused `EPERM`; `capset` from a
non-root process unchanged (an empty set accepted, any other refused),
which is what libcap-ng sends after a change of id with no capability
kept. About 30 lines in `syscall/attributes.rs`, two requirement ids
reserved first, and a check that a non-root process with KEEPCAPS and a
`capset` cannot make a root-only call, with a control (`holds()`
honouring a kept capability, FIRED). Not proposed now: no program G2 or
G3 needs asks for it once the bus runs as the ring-3 way does.

### 8.2 Gap 6: `org.freedesktop.login1` on `sessiond`, ring 3

**What mutter asks** (§3, gap 6): the manager's `GetSession`,
`GetSessionByPID`, `ListSessions`; the session's `TakeControl`,
`ReleaseControl`, `TakeDevice`, `ReleaseDevice`, `PauseDeviceComplete`,
`Activate`, `SetType`, the `PauseDevice` and `ResumeDevice` signals and
the `Active`, `Seat`, `Type`, `Class`, `State` properties; the seat's
`ActiveSession` and `CanGraphical`; and `sd_pid_get_session`, which
libsystemd reads from `/proc/self/cgroup`'s `session-N.scope`, which
`sessiond` already makes.

**Where.** `sessiond` is root and already logind's device half: it owns
seat0, opens each card, render node and `event*` node `O_CLOEXEC`, hands
them over `SCM_RIGHTS`, and since 2026-10-18 card0 has a DRM master that
can be dropped and taken back and input has `EVIOCREVOKE`. The front is a
connection of `sessiond`'s to the system bus (§8.1) owning
`org.freedesktop.login1`, a small D-Bus server in Rust beside the seat
channel it has. No kernel change. Its rules, G3-3's:

* `CreateSession`, `CreateSessionWithPIDFD` and `ReleaseSession` are
  refused: sessions come only from `sessiond` itself, after `authd`'s
  grant or decision 8's automatic login, as today.
* **Who controls the session.** `TakeControl` succeeds only for the
  session leader `sessiond` started, identified by a pidfd `sessiond`
  holds from the fork, compared with the caller's pid as the bus reports
  it (`GetConnectionUnixProcessID`, which works on Ferrix: the G1 gate
  reads the app's pid through it) and then re-checked through the pidfd,
  so a reused pid is not the leader. Never by the caller's cgroup path: a
  user may make scopes under its own slice (AUTH §1).
* `TakeDevice` answers only seat0's nodes -- card0, the render node,
  `event*` -- and only to the controlling connection of the active
  session; anything else, or a second controller, is refused. A device is
  opened by `sessiond` and sent as a unix fd, as the seat channel does.
* **Pausing.** Before another session becomes active, `sessiond` drops
  card0's master on the paused session's descriptor and `EVIOCREVOKE`s its
  input descriptors, then sends `PauseDevice`; `ResumeDevice` gives fresh
  descriptors. A gate shows a paused session can neither scan out nor read
  a key, with a control that skips the revoke (FIRED).
* Power methods (`PowerOff`, `Reboot`, `Suspend`, `Inhibit`) are refused
  to everyone but root: there is no polkit.
* The session ends with its compositor (§6.4 of AUTH, unchanged): the
  login1 session object goes with the scope.

Size: 8 to 13 points. It needs the system bus of §8.1 and is G3's, so it
waits for B1 as well.

### 8.3 Gap 7: the udev database, written by a root program, no uevents

**Way (a), proposed (the consultant's recommendation, G3-4).** `udevdb`,
a root oneshot unit ordered after the device manager has published the
boot's devices, writes what libudev reads, from `/sys/class/drm`,
`/sys/class/input` and `/sys/class/sound`, which Ferrix already
enumerates:

* `/run/udev/data/c<major>:<minor>` per device: `E:ID_SEAT=seat0` (and the
  `seat` tag), `E:ID_INPUT=1` with `ID_INPUT_KEYBOARD`, `ID_INPUT_MOUSE`,
  `ID_INPUT_TABLET` or `ID_INPUT_TOUCHSCREEN` from the event node's
  capability bits in sysfs (`capabilities/key`, `rel`, `abs`), `ID_PATH`
  from the device path, and for card0 the `master-of-seat` tag;
* `/run/udev/tags/seat/c<major>:<minor>`, so seat enumeration finds them;
* `/run/udev` root's, `0755`, each file `0644` and written to a
  temporary name then renamed.

Seat0 and the devices present at boot only, said in the unit and in this
document: **no hotplug**. A device that comes later -- a driver restarted
by its supervisor republishes the same node -- keeps its record; a new
device has none until the unit runs again. libudev's monitor hears
nothing, which libinput and mutter tolerate with a fixed set. Kernel lines:
0, and `NETLINK_KOBJECT_UEVENT` keeps hearing nothing. Size: 3 points.

**Way (b), kernel uevents: not proposed** (customer, 2026-10-22). It
would put a producer in the item's `discovery/devmgr.rs`, a broadcast in
the load ring's netlink and writable `uevent` files, about 8 points, all
of it kernel.

### 8.4 What these need from each other

G2 (the shell nested on hyprix) needs §8.1 alone. G3 needs §8.1, §8.2,
§8.3, card0's KMS for mutter (gap 8, each card0 change reviewed against
E-02, G3-8), the session's own units (gap 5's rest: `/tmp/.X11-unix`,
the accessibility bus) and, before it is offered to anyone, a lock that
unlocks only on `authd`'s grant under mutter (B1, G3-6).
