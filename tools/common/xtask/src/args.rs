//! A very small argument parser.
//!
//! Hand-rolled rather than `clap`, for the reason given in `Cargo.toml`: this
//! is the program that builds the operating system, and its dependency list
//! should be short enough to read. The grammar is `command --flag --key value`,
//! which is all `cargo xtask` ever needs.

use crate::paths::Arch;
use crate::{Error, Result};

/// `--mitigations`: the kernel's one build setting.
///
/// `On` is the certified reference configuration and the default; `Off` is the
/// explicit opt-out, for measuring what the defences cost and for machines
/// whose owner has decided they do not need them. See
/// `docs/certification/SPECULATION.md`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mitigations {
    /// Every side-channel defence the processor needs and offers.
    #[default]
    On,
    /// None: the kernel is built with `--cfg ferrix_mitigations_off`.
    Off,
}

impl Mitigations {
    /// `on` or `off`, and nothing else.
    fn parse(value: &str) -> Result<Mitigations> {
        match value {
            "on" => Ok(Mitigations::On),
            "off" => Ok(Mitigations::Off),
            other => Err(Error::new(format!(
                "--mitigations takes `on` or `off`, not `{other}`"
            ))),
        }
    }
}

/// Parsed command line.
#[derive(Debug, Default, Clone)]
pub(crate) struct Args {
    /// The subcommand, if one was given.
    pub(crate) command: Option<String>,
    /// `--arch`, unparsed. `None` means "whatever this host is".
    pub(crate) arch: Option<String>,
    /// `--release`.
    pub(crate) release: bool,
    /// `--iterate`, with `--release`: the kernel in cargo's `iterate`
    /// profile, release with thin LTO, for working on it rather than gating
    /// it (`docs/TEST-TIME.md`, C4).
    pub(crate) iterate: bool,
    /// `--mitigations`: whether the kernel is built with its side-channel
    /// defences, which it is unless told `off`.
    pub(crate) mitigations: Mitigations,
    /// `--gdb`: stop and wait for a debugger before the first instruction.
    pub(crate) gdb: bool,
    /// `--fast`: skip the slow half of `check`.
    pub(crate) fast: bool,
    /// `--strip-kernel`: images carry the kernel without its debug
    /// information, as `flash` writes it to a card, so that the loader's copy
    /// of the file fits a small machine's memory. Off unless asked for.
    pub(crate) strip_kernel: bool,
    /// `--ferrousli`: `check` also runs ferrousli's gates, its own workspace.
    pub(crate) ferrousli: bool,
    /// `--zinc`: `check` also runs zinc's gates, its own workspace.
    pub(crate) zinc: bool,
    /// `--installer`: the image carries `/sbin/ferrix-install` and the root
    /// volume it writes: the live image (`docs/INSTALLER.md` §11).
    pub(crate) installer: bool,
    /// `test-install`'s live disk and target, attached as `vdd` and `vde`.
    pub(crate) install_disks: Option<(std::path::PathBuf, std::path::PathBuf)>,
    /// Boot the image as a virtio disk after the others, as an installed
    /// disk is, rather than on x86-64's AHCI port.
    pub(crate) boot_virtio: bool,
    /// `--i686`: `test-threads` on x86-64 builds its program for 32-bit x86,
    /// which runs in compatibility mode (`docs/I386.md`, I4).
    pub(crate) i686: bool,
    /// `--adbd`: the image carries adbd at `/bin/adbd`, which nothing
    /// starts (`docs/ADB.md`).
    pub(crate) adbd: bool,
    /// `--app`: apps an image a person runs carries beside the default ones
    /// (`docs/APPS.md` §5).
    pub(crate) apps: Vec<String>,
    /// `--no-apps`: no apps at all.
    pub(crate) no_apps: bool,
    /// `--abi`: the ABI `new-app` writes an app for.
    pub(crate) abi: Option<String>,
    /// `--miri`: add CI's Miri steps to `check`.
    pub(crate) miri: bool,
    /// `--jobs`, how many crates `miri` interprets at once; `None` for as
    /// many as it picks itself.
    pub(crate) jobs: Option<u32>,
    /// `--reset`: the image carries `ferrix.onexit=reset` in `CMDLINE.TXT`, and
    /// `test-boot` requires QEMU to see the machine reset rather than power off.
    pub(crate) reset: bool,
    /// `--compositor`: `flash` and `deploy` put the desktop on the card --
    /// the compositor as init with its clients and a shell, the image
    /// `run-compositor` boots -- rather than the self-checks or a busybox.
    pub(crate) compositor: bool,
    /// `--reset-root`: start the btrfs root `run` and `run-compositor` boot
    /// with over from the fixture, throwing away the system's files on it.
    /// The home disk and `--persistent`'s `/data` are kept.
    pub(crate) reset_root: bool,
    /// `--reset-flash`: start everything the machine keeps over -- the root,
    /// the home disk and `--persistent`'s `/data` -- as a machine whose
    /// flash was wiped would.
    pub(crate) reset_flash: bool,
    /// `--tmpfs-root`: boot `run` and `run-compositor` with `/` in memory,
    /// as the test boots are, and leave the btrfs root off the bus.
    pub(crate) tmpfs_root: bool,
    /// `--persistent`: boot `run` and `run-compositor` with `/data` on a
    /// copy of the volume that keeps what the guest writes, and Chrome's
    /// profile on it (`crate::persistent`).
    pub(crate) persistent: bool,
    /// `--btrfs-root`: boot `test-chrome-window` with `/` on a btrfs root
    /// disk, made fresh from the fixture for the run, as the desktop's is.
    pub(crate) btrfs_root: bool,
    /// `--net`: give the guest a virtio-net device, with `xtask`'s own gateway
    /// behind it. Off by default for a boot that is judged, because every boot
    /// that does not need a network is a boot with one fewer device on the bus
    /// and one fewer thread in this process -- and because the bus a check
    /// enumerates should be the bus it has always enumerated. A watched boot
    /// has one anyway; see `no_net`.
    pub(crate) net: bool,
    /// `--no-net`: take the network away from a boot that would have had one.
    ///
    /// Only `run-compositor` has one to take: somebody watching a screen
    /// expects a machine that can reach the network, so that boot asks for
    /// it unasked, and this is how they say they would rather it did not.
    pub(crate) no_net: bool,
    /// `--no-dotfiles`: `run-compositor --config` without the user's
    /// configuration directories and fonts beside the file
    /// (`crate::dotfiles`).
    pub(crate) no_dotfiles: bool,
    /// `--chrome`: `run-compositor` with Google's Chrome on the desktop,
    /// from the volume `tools/common/fetch/fetch-chrome.sh` makes, in place of the
    /// rustc volume, and a keybind for another window.
    pub(crate) chrome: bool,
    /// `--everything`: `run-compositor` with every feature a watched desktop
    /// has -- `--gl`, `--release`, `--clipboard` and `--chrome`, which it
    /// sets, and `rustc` and `cargo` beside Chrome on one data volume made
    /// of the two (`crate::everything`). The network and the hypervisor a
    /// watched boot has already.
    pub(crate) everything: bool,
    /// `--login`: `run`'s getty asks who is there with `/bin/login`
    /// instead of starting root's shell (`docs/AUTH.md` §6.2), and the image
    /// carries `authd` for it. A person's account with no password chooses
    /// one at the console the first time (§5.4).
    pub(crate) login: bool,
    /// `--nvidia`: `run-compositor --chrome`'s desktop on the RTX 3060's
    /// own monitor, in the libvirt domain `run-nvidia` boots (`crate::nvidia`):
    /// nvrm drives the card through NVKMS, and Chrome renders on NVIDIA's
    /// Vulkan, on the volume's glibc. Sets `--chrome`.
    pub(crate) nvidia: bool,
    /// `--session`: `flash --compositor`'s desktop runs as the user
    /// `ferrix` through `sessiond`, as `run-compositor --everything`'s does
    /// (`crate::session`): `--config`'s dotfiles seed the home disk once,
    /// and what is edited there is kept. The Pixel 7's VM desktop with the
    /// user's own `hyprland.conf` (`tools/vendor/google/pixel7`).
    pub(crate) session: bool,
    /// `-h`/`--help`.
    pub(crate) help: bool,
    /// `--smp`, virtual CPUs.
    pub(crate) smp: u32,
    /// Whether `--smp` was given, rather than left at its default: under WHPX
    /// the default is one processor, and a count asked for is kept.
    pub(crate) smp_given: bool,
    /// `test-nvrm`'s unisolated boot: the VT-d unit with interrupt
    /// remapping off, so no device's interrupts are isolated
    /// (`docs/NVIDIA.md` §12.3). Set by that gate, never by a flag.
    pub(crate) unisolated_interrupts: bool,
    /// `--memory`, guest RAM in MiB.
    pub(crate) memory: u32,
    /// Whether `--memory` was given: `test-rustc` needs more than the
    /// default, and a size asked for is kept.
    pub(crate) memory_given: bool,
    /// `--timeout`, seconds `test-boot` waits for the kernel to report.
    pub(crate) timeout: u64,
    /// Whether `--timeout` was given, for the same reason as `memory_given`.
    pub(crate) timeout_given: bool,
    /// A btrfs image mounted at `/data`, under QEMU's `snapshot=on` so a
    /// boot never changes it. `test-rustc` requires it; x86-64 `run` and
    /// `run-compositor` attach it when the toolchain has been fetched.
    pub(crate) data_image: Option<std::path::PathBuf>,
    /// Whether what the guest writes to `data_image` reaches the file:
    /// `test-selfhost`'s volume, made afresh for each run, which the host
    /// reads the built image back out of.
    pub(crate) data_image_kept: bool,
    /// Whether `data_image` is attached read-only (QEMU's `readonly=on`,
    /// which the guest's virtio-blk sees as `VIRTIO_BLK_F_RO`): a program
    /// volume whose clean pages the page cache may give back under pressure,
    /// as it may for no writable btrfs yet (`docs/CHROME.md` §10).
    pub(crate) data_image_read_only: bool,
    /// A home volume a test boot attaches, kept: `test-shell`'s K7 boots
    /// (`crate::init_file`). `run` and `run-compositor` attach
    /// `build/home.img` beside their root instead.
    pub(crate) home_image: Option<std::path::PathBuf>,
    /// The account a desktop's session runs as, through `sessiond`: set by
    /// `run-compositor --everything` (`crate::session`), and `None` for a
    /// compositor that runs as root, as every judged desktop does.
    pub(crate) session_user: Option<String>,
    /// `--seeds`, how many power failures `test-powerfail` makes.
    pub(crate) seeds: u64,
    /// `--accel`, which QEMU accelerator to boot under. `None` means `tcg`,
    /// except for an x86-64 guest on an x86-64 Linux host outside CI, where it
    /// means `kvm` (`qemu::accelerator`), and to `run`, which asks for `auto`
    /// unless it is given `--gdb`.
    pub(crate) accel: Option<String>,
    /// `--since`, the base `gate-rows` reads a branch's changes from.
    pub(crate) since: Option<String>,
    /// `--moved`, the old base `gate-rows` compares a moved `main` with.
    pub(crate) moved: Option<String>,
    /// `--to`, the mounted boot partition `flash` writes to. `None` means
    /// "find the only one".
    pub(crate) to: Option<String>,
    /// `--alternate`, a git ref `bench-ipc` builds in a tree of its own and
    /// times turn about with this one.
    pub(crate) alternate: Option<String>,
    /// `--rounds`, how many turns `bench-ipc --alternate` and its
    /// `--against-*` runs take. `None` is three.
    pub(crate) rounds: Option<u32>,
    /// `--against-sel4`: `bench-ipc` alternates with the seL4 image in
    /// `~/.local/share/ferrix/sel4`.
    pub(crate) against_sel4: bool,
    /// `--against-redox`: `bench-ipc` alternates with the Redox image in
    /// `~/.local/share/ferrix/redox-bench`.
    pub(crate) against_redox: bool,
    /// `--record`: `bench-ipc` files its result under
    /// `docs/hotpaths/results/` (`docs/HOTPATHS.md` §6).
    pub(crate) record: bool,
    /// `--pin`, the host processors (`taskset -c`'s list) QEMU runs on.
    /// `bench-ipc` pins to 11 unless given another, or `none`.
    pub(crate) pin: Option<String>,
    /// `--stage`, a directory `flash` writes the card's files into instead of
    /// a card: for a board on another machine than the build, whose card is
    /// then given the directory's contents by hand.
    pub(crate) stage: Option<String>,
    /// `--port`, the serial device `watch-serial` reads. `None` means "find
    /// the only one".
    pub(crate) port: Option<String>,
    /// `--init`, the program `test-shell` and `test-vfs` build in, and that
    /// `build` and `run` put in the kernel and at `/bin/busybox`. `{arch}` in it
    /// is replaced by each architecture's name, so one path serves `--arch all`.
    /// `ferrousli` names the busybox built against ferrousli, in `busybox.rs`.
    pub(crate) init: Option<String>,
    /// `--init-path`, a file in the image the kernel starts as pid 1:
    /// `ferrix.init=<PATH>` in `CMDLINE.TXT`. Absolute, and one word, since
    /// the command line is split at white space.
    pub(crate) init_path: Option<String>,
    /// `--kernel-option`, each a word added to `CMDLINE.TXT` as it is, for a
    /// kernel option no flag here names: `ferrix.fbcon`, which the Pixel 7's
    /// loader passes, is one a QEMU boot has to be able to ask for too.
    pub(crate) kernel_options: Vec<String>,
    /// Not a flag: set by `test-compositor` for every boot of an architecture
    /// after its first, whose images then carry `ferrix.checks=skip`. The
    /// first boot runs the self-checks on the compositor's machine, and
    /// `test-boot` runs them on every row (`docs/TEST-TIME.md`, C2).
    pub(crate) checks_skipped: bool,
    /// `--interpreter`, a dynamic linker `test-shell` carries in the initramfs
    /// at the path `--init`'s `PT_INTERP` names. `{arch}` is replaced as it is
    /// in `--init`.
    pub(crate) interpreter: Option<String>,
    /// `--library`, as many as given: shared libraries `test-shell` carries in
    /// the initramfs's `/lib`, which glibc's linker and ferrousli's both search
    /// when nothing else says where. `{arch}` is replaced as in `--init`.
    pub(crate) libraries: Vec<String>,
    /// Where the gateway's `10.0.2.3:53` forwards to. No flag sets it: it is
    /// how `test-net` points the guest's DNS at the answers it serves itself,
    /// so that the test says the same thing on a machine with no network.
    /// `None` is the host's own resolver.
    pub(crate) resolver: Option<std::net::SocketAddrV4>,
    /// `--forward <HOST>:<GUEST>`, as many as given: the gateway listens on
    /// the host's `127.0.0.1:<HOST>` and opens each connection it accepts to
    /// the guest's `<GUEST>`, which is how `ssh -p 2222 root@127.0.0.1`
    /// reaches a server in the guest. Turns `--net` on, since a forward with
    /// no network behind it forwards nothing.
    pub(crate) forwards: Vec<crate::gateway::Forward>,
    /// `--ssh <PORT>`: `run-compositor` starts `sshdt` in the guest and
    /// forwards the host's `127.0.0.1:<PORT>` to it; `crate::ssh` says who
    /// may log in. The forward is one of `forwards`, added with it.
    pub(crate) ssh: Option<u16>,
    /// `--ssh-key <FILE|KEY>`, as many as given: another public key that may
    /// log in, beside this machine's guest key and its user's own. A file is
    /// read whole; a key written out on the command line is taken as it
    /// stands. For a client whose key is in neither place -- another user on
    /// this machine, a sandbox, a CI step.
    pub(crate) ssh_keys: Vec<String>,
    /// `--auth-seed ACCOUNT`, as many as given: ask on this terminal for a
    /// password, hash it here, and give the image it as the account's first
    /// password (`docs/AUTH.md` §5.3).
    pub(crate) auth_seeds: Vec<String>,
    /// `--auth-seed-file ACCOUNT=FILE`: the same, with the password the
    /// file's first line, for a script.
    pub(crate) auth_seed_files: Vec<(String, std::path::PathBuf)>,
    /// `--sabotage NAME`: `test-auth` against an `authd` built with one of
    /// its refusals turned off, which must fail (`authd`'s `sabotage.rs`).
    pub(crate) sabotage: Option<String>,
    /// `--display`: a virtio-gpu device on the bus, and for `run` a window
    /// that shows it. `test-display` turns it on.
    pub(crate) display: bool,
    /// `--gl`: the virtio-gpu is the *3D* device, `virtio-gpu-gl-pci`, so
    /// the guest can negotiate `VIRTIO_GPU_F_VIRGL` and the host's own GPU
    /// driver is behind it through virglrenderer (`docs/GPU.md` Path A).
    ///
    /// Asked for rather than assumed: a QEMU built without OpenGL and
    /// virglrenderer has no such device, and a GPU boot is not what most
    /// boots want. A QEMU that has not got it says so and the 2D device is
    /// used instead.
    pub(crate) gl: bool,
    /// `--venus`: the 3D card offers Venus, Vulkan on the host's GPU, with
    /// the blob resources and the host-visible window it needs
    /// (`docs/GPU.md` §6.1). Turns `--gl` on. The host must be Linux with a
    /// virglrenderer built with Venus; QEMU refuses the card otherwise.
    pub(crate) venus: bool,
    /// `--no-gl`: a watched desktop draws in software even where the host
    /// could have given it the 3D card (`window::watched_gl`).
    pub(crate) no_gl: bool,
    /// `--clipboard`: a `virtio-serial` device carrying the port named
    /// `com.redhat.spice.0`, with QEMU's own half of the SPICE agent
    /// protocol behind it, for a clipboard shared with whoever is watching
    /// the screen (`docs/CLIPBOARD.md`).
    ///
    /// The device and the host's half of the protocol only: the guest has no
    /// driver for it, no `/dev/vport0p1` and no agent, so today this puts a
    /// device on the bus that nothing claims and shares nothing. §8 of that
    /// document says what is still to build.
    ///
    /// Off by default, and asked for rather than brought by `--display`, for
    /// the reason `net` gives: it puts another device on the bus, and the bus
    /// a check enumerates should be the bus it has always enumerated.
    pub(crate) clipboard: bool,
    /// For `test-clipboard`: the clipboard port's far end is a Unix socket
    /// at this path that xtask listens on and speaks vdagent over, instead of
    /// QEMU's own `qemu-vdagent`.
    pub(crate) clipboard_socket: Option<std::path::PathBuf>,
    /// `--audio BACKEND`: a virtio-snd card on the bus, its far end QEMU's
    /// audio backend `BACKEND` -- `pipewire` or `pa` to hear it on a Linux
    /// host, `wav:PATH` to write what the guest plays to a file
    /// (`docs/AUDIO.md` §4). `test-audio` sets a `wav` one of its own.
    pub(crate) audio: Option<String>,
    /// `--input`: a virtio keyboard and a virtio tablet on the bus, which
    /// QMP's `input-send-event` drives. `test-input` turns it on, and
    /// `--display` brings them as well.
    pub(crate) input: bool,
    /// `--screens N`: how many virtio-gpu devices are on the bus, one
    /// screen each. Zero and one both mean one. A second device rather than
    /// a second output of the first, because QEMU enables a second output
    /// only when a window manager of the host's resizes it, and a test with
    /// no window has none: two devices are two consoles, and a screendump
    /// names each by its device id.
    pub(crate) screens: u32,
    /// Where QEMU serves QMP. No flag sets it: `test-display` picks a port to
    /// ask QEMU for its screendump over.
    pub(crate) qmp_port: Option<u16>,
    /// Where QEMU serves the card over VNC for a judged boot, on the
    /// loopback. No flag sets it: the cursor boot asks QEMU for the
    /// pointer's shape there, which is the one place a cursor plane shows.
    pub(crate) judge_vnc: Option<u16>,
    /// `--vnc <display>`: serve the screen over VNC at this address rather
    /// than in a window of this host's, which is what a machine reached over
    /// `ssh` has. `window` says why the default is the loopback.
    pub(crate) vnc: Option<String>,
    /// `--rendernode <PATH>`: which GPU `egl-headless` draws `--gl`'s frames
    /// on, as `/dev/dri/renderD128`.
    ///
    /// Only that backend takes one, and it is the backend a served screen
    /// uses, so this is the `--gl --vnc` case and no other: a window's GL
    /// goes to whichever GPU the host's own display is on, which is not a
    /// thing QEMU lets anybody choose. Left out, QEMU opens the first render
    /// node it can, which on a machine with one GPU is that GPU and on a
    /// machine with several is a guess -- and the wrong guess is a
    /// proprietary driver that does not do what virglrenderer asks, or an
    /// idle card while the fast one watches.
    pub(crate) rendernode: Option<String>,
    /// `--layout <LIST>`: the keyboard layouts a watched boot is configured
    /// with, as `input:kb_layout` takes them -- one name or a comma-separated
    /// list, `de` or `de,us`.
    pub(crate) layout: Option<String>,
    /// `--variant <LIST>`: their variants, as `input:kb_variant` takes them,
    /// read alongside the names: `nodeadkeys,` is a variant for the first
    /// layout and none for the second.
    pub(crate) variant: Option<String>,
    /// `--config <PATH>`: the `hyprland.conf` `run-compositor` carries into
    /// the guest, in place of the small one it writes itself.
    pub(crate) config: Option<String>,
    /// `--wallpaper <NAME>`: which of the kept wallpapers `run-compositor`
    /// shows, by any part of its name, or `none` for a plain background.
    /// Given nothing it shows one of them, another each run.
    pub(crate) wallpaper: Option<String>,
    /// `--edid <DESCRIPTION>`: the monitor of this machine whose EDID
    /// `run-compositor` gives the guest's screen, by the start of its
    /// description, or `none` for no EDID. Given nothing it looks for
    /// `crate::edid::DEFAULT_MONITOR`.
    pub(crate) edid: Option<String>,
    /// `--from <WHERE>`: where `wallpapers` finds pictures to convert, a
    /// directory of this machine's or `host:directory`.
    pub(crate) from: Option<String>,
    /// `--plan <DIR>`: a plan of builds `FERRIX_BUILDS=record:` wrote, which
    /// `test-selfhost` has Ferrix carry out and `builds-execute` carries out
    /// (see `crate::builds`).
    pub(crate) plan: Option<String>,
    /// `--volume <IMG>`: a volume Ferrix built on elsewhere, which
    /// `test-selfhost` judges instead of building one: `btrfs check`, the
    /// image and kernel taken out, and the image booted (the Pixel 7's
    /// `tools/vendor/google/pixel7/selfhost.sh` makes one on the phone).
    pub(crate) volume: Option<String>,
    /// `--size <W>x<H>`: the screen `run-compositor` gives the guest and
    /// `wallpapers` cuts pictures for, 1920x1080 for both when not given.
    pub(crate) size: Option<(u32, u32)>,
    /// `--scale <N>`: the compositor's scale for the screen, 1 when not
    /// given: a phone's 1080x2400 at 1 is text a few millimetres high.
    pub(crate) scale: Option<String>,
    /// `--bar-zoom <N>`: `flash --session`'s waybar this many times its
    /// size: the lengths in the carried waybar style and configuration
    /// multiplied, so that a desktop's bar reads on a phone
    /// ([`crate::session::zoom_bar`]).
    pub(crate) bar_zoom: Option<f64>,
    /// `--bar-drop <a,b,...>`: `flash --session`'s waybar without these
    /// modules, for a screen too narrow for a desktop's bar
    /// ([`crate::session::trim_bar`]).
    pub(crate) bar_drop: Vec<String>,
    /// `--bar-margin-right <N>`: that bar this many pixels short of the
    /// screen's right edge, where the Pixel 7 app's buttons float.
    pub(crate) bar_margin_right: Option<u32>,
    /// `--fps <N>`: how many frames a second of a video `wallpapers` keeps.
    ///
    /// A wallpaper that moves is its frames, so this is a size as much as a
    /// rate: twice the rate is twice the initramfs.
    pub(crate) fps: Option<u32>,
    /// `--seconds <N>`: how many seconds of a video `wallpapers` keeps,
    /// after which the wallpaper begins again.
    pub(crate) seconds: Option<u32>,
    /// `--video-size <W>x<H>`: how large a video's frames are kept, where
    /// they are not to be a quarter of `--size` each way. The client scales
    /// whatever it is given to cover the screen.
    pub(crate) video_size: Option<(u32, u32)>,
    /// `--host <DESTINATION>`: the machine `remote-desktop` boots on, as
    /// `ssh` names one, over whatever the config file said. No host is ever a
    /// default here: this and the config file are the only two ways one is
    /// named, which is the rule the rest of `xtask` follows.
    pub(crate) host: Option<String>,
    /// `--local-port <N>`: the port `remote-desktop`'s tunnel listens on
    /// here. Left out, it takes 5900 + the display when that is free and the
    /// next one up when it is not, which matters on a machine that runs a VNC
    /// server of its own.
    pub(crate) local_port: Option<u16>,
    /// `--send <working-tree|head>`: whether `remote-desktop` boots what you
    /// are looking at or your last commit, over what the config file said.
    pub(crate) send: Option<String>,
    /// `--no-viewer`: `remote-desktop` opens the tunnel and nothing else, for
    /// somebody who would rather point their own viewer at it.
    pub(crate) no_viewer: bool,
    /// `--viewer <tigervnc|realvnc|auto|none>`: which viewer `remote-desktop`
    /// opens, over what the config file said. The two named ones are told
    /// apart because they send the keyboard differently: `TigerVNC` sends keys,
    /// `RealVNC` characters, which QEMU needs `--keymap` to turn back into keys.
    pub(crate) viewer: Option<String>,
    /// `--keymap <NAME>`: the keymap QEMU turns a VNC viewer's characters
    /// back into keys through (`-k`), for a viewer that sends characters
    /// rather than keys. An XKB layout (`de`, `us`) or QEMU's own name for one
    /// (`en-us`). A viewer that sends keys must not be given one.
    pub(crate) keymap: Option<String>,
    /// `--print-command`: say what `remote-desktop` would send, run and open,
    /// and do none of it. The first thing to run when something is not where
    /// it was expected.
    pub(crate) print_command: bool,
    /// `--stop`: end a boot left running over there, and do nothing else.
    pub(crate) stop: bool,
    /// Everything after `--`, passed to the remote `cargo xtask` as it
    /// stands. Only `remote-desktop` reads it: it is the one command whose
    /// arguments are partly another command's.
    pub(crate) passthrough: Vec<String>,
    /// `--boot <name>`: run only the boots of `test-compositor` whose name
    /// holds this, rather than all of them.
    ///
    /// Each boot takes minutes under emulation and there are fourteen, so a
    /// change to one of them is otherwise an hour a try. The gate runs them
    /// all; this is for the person writing one.
    pub(crate) boot: Option<String>,
}

/// The flags [`Args::scales`] takes, each with a value.
const SCALE_FLAGS: [&str; 4] = ["--scale", "--bar-zoom", "--bar-drop", "--bar-margin-right"];

/// The flags [`Args::root`] takes.
const ROOT_FLAGS: [&str; 5] = [
    "--reset-root",
    "--reset-flash",
    "--tmpfs-root",
    "--btrfs-root",
    "--persistent",
];

impl Args {
    /// `--everything`: the flags it stands for, set as if each were given,
    /// but for `--gl` beside a `--no-gl`, before or after it.
    fn everything(&mut self) {
        self.everything = true;
        self.chrome = true;
        self.gl = !self.no_gl;
        self.display = true;
        self.release = true;
        self.clipboard = true;
    }

    /// `--no-gl`, which also takes away the 3D card an earlier
    /// `--everything` asked for.
    fn no_gl(&mut self) {
        self.no_gl = true;
        if self.everything {
            self.gl = false;
        }
    }

    /// `--mitigations on|off`.
    /// `--ssh PORT` forwards that host port to the guest's sshd, which
    /// needs the network, so it turns the network on as well.
    fn ssh(&mut self, items: &mut impl Iterator<Item = String>) -> Result<()> {
        let port: u16 = number(items, "--ssh")?;
        if port == 0 {
            return Err(Error::new("--ssh wants a port other than 0"));
        }
        self.ssh = Some(port);
        self.forwards.push(crate::gateway::Forward {
            host: port,
            guest: crate::ssh::GUEST_PORT,
        });
        self.net = true;
        Ok(())
    }

    fn mitigations(&mut self, items: &mut impl Iterator<Item = String>) -> Result<()> {
        self.mitigations = Mitigations::parse(&value(items, "--mitigations")?)?;
        Ok(())
    }

    /// `--release` and `--strip-kernel`: how the kernel is built, and how an
    /// image carries it.
    fn build(&mut self, flag: &str) {
        match flag {
            "--release" => self.release = true,
            "--iterate" => self.iterate = true,
            _ => self.strip_kernel = true,
        }
    }

    /// `--adbd`, `--app` and `--no-apps`: more programs the image carries,
    /// or fewer; and `--abi`, the kind of app `new-app` writes.
    fn carry_more(&mut self, flag: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        match flag {
            "--adbd" => self.adbd = true,
            "--app" => self.apps.push(value(items, flag)?),
            "--abi" => self.abi = Some(value(items, flag)?),
            _ => self.no_apps = true,
        }
        Ok(())
    }

    /// `--accel`, `--since` and `--moved`: each one word, kept as given.
    fn named(&mut self, flag: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        if matches!(
            flag,
            "--alternate"
                | "--rounds"
                | "--pin"
                | "--against-sel4"
                | "--against-redox"
                | "--record"
        ) {
            return self.bench(flag, items);
        }
        let given = Some(value(items, flag)?);
        match flag {
            "--to" => self.to = given,
            "--stage" => self.stage = given,
            "--accel" => self.accel = given,
            "--since" => self.since = given,
            _ => self.moved = given,
        }
        Ok(())
    }

    /// `--statd` and `--installer`: programs the image carries. `--statd`
    /// is `--app statd`, kept for the phone's scripts, which say it.
    fn carry(&mut self, flag: &str) {
        match flag {
            "--statd" => self.apps.push("statd".to_owned()),
            _ => self.installer = true,
        }
    }

    /// `--no-dotfiles` and `--session`: what becomes of `--config`'s
    /// dotfiles -- left behind, or seeding the session user's home.
    fn dotfiles(&mut self, flag: &str) {
        match flag {
            "--session" => self.session = true,
            _ => self.no_dotfiles = true,
        }
    }

    /// `--scale`, and the `--bar-` options: the screen's scale, and how
    /// the bar is made to fit it.
    fn scales(&mut self, flag: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        let raw = &value(items, flag)?;
        match flag {
            "--bar-drop" => {
                self.bar_drop = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            "--bar-margin-right" => {
                self.bar_margin_right = Some(raw.parse().map_err(|_| {
                    Error::new(format!(
                        "--bar-margin-right wants a number of pixels, got `{raw}`"
                    ))
                })?);
            }
            "--scale" => {
                let _ = scale(flag, raw)?;
                self.scale = Some(raw.to_owned());
            }
            _ => self.bar_zoom = Some(scale(flag, raw)?),
        }
        Ok(())
    }

    /// `--reset-root`, `--reset-flash`, `--tmpfs-root`, `--btrfs-root` and
    /// `--persistent`: where `/` is for the boot, what of what the machine
    /// keeps starts over, and whether `/data` is kept too.
    fn root(&mut self, flag: &str) {
        match flag {
            "--reset-root" => self.reset_root = true,
            "--reset-flash" => self.reset_flash = true,
            "--tmpfs-root" => self.tmpfs_root = true,
            "--persistent" => self.persistent = true,
            _ => self.btrfs_root = true,
        }
    }

    /// These arguments, unless they ask for a root that is both kept and
    /// in memory.
    fn root_agrees(self) -> Result<Self> {
        if self.persistent && self.tmpfs_root {
            return Err(Error::new(
                "--persistent keeps / on the btrfs root disk, which --tmpfs-root leaves off",
            ));
        }
        Ok(self)
    }

    /// `--plan` and `--volume`, what `test-selfhost` is given instead of
    /// building one image on Ferrix (`crate::selfhost`).
    fn selfhost(&mut self, flag: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        let raw = value(items, flag)?;
        if flag == "--plan" {
            self.plan = Some(raw);
        } else {
            self.volume = Some(raw);
        }
        Ok(())
    }

    /// `--auth-seed`, `--auth-seed-file` and `--sabotage` (`crate::auth`).
    fn auth(&mut self, flag: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        let raw = value(items, flag)?;
        match flag {
            "--auth-seed" => self.auth_seeds.push(raw),
            "--auth-seed-file" => {
                let (account, file) = raw.split_once('=').ok_or_else(|| {
                    Error::new(format!("--auth-seed-file wants ACCOUNT=FILE, not {raw}"))
                })?;
                self.auth_seed_files
                    .push((account.to_owned(), std::path::PathBuf::from(file)));
            }
            _ => self.sabotage = Some(raw),
        }
        Ok(())
    }

    /// Parse an iterator of arguments, `cargo xtask` and the command name
    /// having already been stripped by the caller.
    /// A flag that only turns something on: `--chrome`, `--login`, and
    /// `--nvidia`, which is the `--chrome` desktop on the 3060's monitor.
    fn switch(&mut self, flag: &str) {
        match flag {
            "--chrome" => self.chrome = true,
            "--login" => self.login = true,
            "--nvidia" => {
                self.nvidia = true;
                self.chrome = true;
            }
            _ => {}
        }
    }

    pub(crate) fn parse(raw: impl Iterator<Item = String>) -> Result<Self> {
        let mut args = Args {
            smp: 4,
            memory: 512,
            timeout: 120,
            seeds: 8,
            screens: 1,
            ..Args::default()
        };

        let mut items = raw.peekable();
        while let Some(item) = items.next() {
            match item.as_str() {
                "-h" | "--help" => args.help = true,
                "--release" | "--strip-kernel" | "--iterate" => args.build(&item),
                "--mitigations" => args.mitigations(&mut items)?,
                "--gdb" => args.gdb = true,
                "--fast" => args.fast = true,
                "--ferrousli" => args.ferrousli = true,
                "--zinc" => args.zinc = true,
                "--statd" | "--installer" => args.carry(&item),
                "--i686" => args.i686 = true,
                "--adbd" | "--app" | "--no-apps" | "--abi" => args.carry_more(&item, &mut items)?,
                "--miri" => args.miri = true,
                "--reset" => args.reset = true,
                "--compositor" => args.compositor = true,
                flag if ROOT_FLAGS.contains(&flag) => args.root(flag),
                "--net" => args.net = true,
                "--no-net" => args.no_net = true,
                "--no-dotfiles" | "--session" => args.dotfiles(&item),
                "--chrome" | "--login" | "--nvidia" => args.switch(&item),
                "--everything" => args.everything(),
                "--forward" => {
                    let raw = value(&mut items, "--forward")?;
                    args.forwards.push(crate::gateway::Forward::parse(&raw)?);
                    args.net = true;
                }
                "--ssh" => args.ssh(&mut items)?,
                "--ssh-key" => args.ssh_keys.push(value(&mut items, "--ssh-key")?),
                "--auth-seed" | "--auth-seed-file" | "--sabotage" => {
                    args.auth(&item, &mut items)?;
                }
                "--display" => args.display = true,
                // A 3D card is still a card: `--gl` on its own turns the
                // display on, so nobody has to write both; and Venus is
                // the 3D card with more switched on.
                "--gl" | "--venus" => {
                    args.venus |= item == "--venus";
                    args.gl = true;
                    args.display = true;
                }
                "--no-gl" => args.no_gl(),
                "--clipboard" => args.clipboard = true,
                "--input" => args.input = true,
                "--audio" => args.audio = Some(value(&mut items, "--audio")?),
                "--screens" => args.screens = number(&mut items, "--screens")?,
                "--arch" => args.arch = Some(value(&mut items, "--arch")?),
                "--smp" | "--memory" | "--timeout" => args.machine(&item, &mut items)?,
                "--seeds" | "--jobs" => args.counts(&item, &mut items)?,
                "--accel" | "--since" | "--moved" | "--alternate" | "--rounds" | "--pin"
                | "--against-sel4" | "--against-redox" => args.named(&item, &mut items)?,
                "--to" | "--stage" | "--record" => args.named(&item, &mut items)?,
                "--port" => args.port = Some(value(&mut items, "--port")?),
                "--init" => args.init = Some(value(&mut items, "--init")?),
                "--init-path" => args.init_path = Some(init_path(&mut items)?),
                "--kernel-option" => args.kernel_options.push(kernel_option(&mut items)?),
                "--interpreter" => args.interpreter = Some(value(&mut items, "--interpreter")?),
                "--library" => args.libraries.push(value(&mut items, "--library")?),
                "--boot" => args.boot = Some(value(&mut items, "--boot")?),
                "--host" => args.host = Some(value(&mut items, "--host")?),
                "--local-port" => args.local_port = Some(number(&mut items, "--local-port")?),
                "--send" => args.send = Some(value(&mut items, "--send")?),
                "--no-viewer" => args.no_viewer = true,
                "--viewer" => args.viewer = Some(value(&mut items, "--viewer")?),
                "--keymap" => args.keymap = Some(value(&mut items, "--keymap")?),
                "--print-command" => args.print_command = true,
                "--stop" => args.stop = true,
                // Everything after `--` belongs to the `cargo xtask` at the
                // other end of an `ssh`, and this parser must not have an
                // opinion about any of it -- including whether it knows the
                // option, which is the whole point of passing it on.
                "--" => args.passthrough.extend(items.by_ref()),
                "--vnc" => args.vnc = Some(value(&mut items, "--vnc")?),
                "--rendernode" => args.rendernode = Some(value(&mut items, "--rendernode")?),
                "--config" => args.config = Some(value(&mut items, "--config")?),
                "--layout" => args.layout = Some(value(&mut items, "--layout")?),
                "--variant" => args.variant = Some(value(&mut items, "--variant")?),
                "--wallpaper" => args.wallpaper = Some(value(&mut items, "--wallpaper")?),
                "--edid" => args.edid = Some(value(&mut items, "--edid")?),
                "--from" => args.from = Some(value(&mut items, "--from")?),
                "--plan" | "--volume" => args.selfhost(&item, &mut items)?,
                "--size" => args.size = Some(dimensions(&value(&mut items, "--size")?, "--size")?),
                flag if SCALE_FLAGS.contains(&flag) => args.scales(flag, &mut items)?,
                "--video-size" => {
                    let raw = value(&mut items, "--video-size")?;
                    args.video_size = Some(dimensions(&raw, "--video-size")?);
                }
                "--fps" => args.fps = Some(count(&mut items, "--fps")?),
                "--seconds" => args.seconds = Some(count(&mut items, "--seconds")?),
                other if other.starts_with('-') => {
                    return Err(Error::new(format!("unknown option `{other}`")));
                }
                other if args.command.is_none() => args.command = Some(other.to_owned()),
                other => {
                    return Err(Error::new(format!("unexpected argument `{other}`")));
                }
            }
        }

        args.root_agrees()
    }

    /// `--seeds` and `--jobs`: how many of something a command makes or runs.
    fn counts(&mut self, key: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        match key {
            "--seeds" => self.seeds = number(items, key)?,
            _ => self.jobs = Some(count(items, key)?),
        }
        Ok(())
    }

    /// `--smp`, `--memory` and `--timeout`: the machine's size, each
    /// remembered as asked for, so a command with defaults of its own keeps a
    /// size somebody gave.
    fn machine(&mut self, key: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        match key {
            "--smp" => (self.smp, self.smp_given) = (number(items, key)?, true),
            "--memory" => (self.memory, self.memory_given) = (number(items, key)?, true),
            _ => (self.timeout, self.timeout_given) = (number(items, key)?, true),
        }
        Ok(())
    }

    /// The architectures this invocation applies to.
    ///
    /// `--arch all` is the reason this returns a list: a change that breaks
    /// one architecture and not the others is the characteristic failure of a
    /// multi-architecture kernel, and the default local workflow should catch
    /// it. `both` stays a synonym for `all`, so that a habit formed when there
    /// were two does not quietly drop the third.
    pub(crate) fn arches(&self) -> Result<Vec<Arch>> {
        match self.arch.as_deref() {
            None => Ok(vec![Arch::host()]),
            Some("both" | "all") => Ok(Arch::ALL.to_vec()),
            Some(name) => Ok(vec![Arch::parse(name)?]),
        }
    }

    /// The single architecture for a command that can only mean one.
    pub(crate) fn single_arch(&self) -> Result<Arch> {
        let arches = self.arches()?;
        match arches.as_slice() {
            [one] => Ok(*one),
            _ => Err(Error::new("this command needs exactly one --arch")),
        }
    }
}

/// Take `--init-path`'s value: absolute, since the kernel has no working
/// directory to resolve it from, and one word, since the command line is
/// split at white space.
fn init_path(items: &mut impl Iterator<Item = String>) -> Result<String> {
    let path = value(items, "--init-path")?;
    if !path.starts_with('/') || path.contains(char::is_whitespace) {
        return Err(Error::new(format!(
            "--init-path wants an absolute path with no spaces, not `{path}`"
        )));
    }
    Ok(path)
}

/// Take `--kernel-option`'s value: one word, since the command line is split
/// at white space.
fn kernel_option(items: &mut impl Iterator<Item = String>) -> Result<String> {
    let option = value(items, "--kernel-option")?;
    if option.is_empty() || option.contains(char::is_whitespace) {
        return Err(Error::new(format!(
            "--kernel-option wants one word, not `{option}`; give the flag once for each"
        )));
    }
    Ok(option)
}

/// Take the value following a `--key`.
fn value(items: &mut impl Iterator<Item = String>, key: &str) -> Result<String> {
    items
        .next()
        .ok_or_else(|| Error::new(format!("{key} needs a value")))
}

/// Take and parse a numeric value following a `--key`.
fn number<T: std::str::FromStr>(items: &mut impl Iterator<Item = String>, key: &str) -> Result<T> {
    let raw = value(items, key)?;
    raw.parse()
        .map_err(|_| Error::new(format!("{key} wants a number, got `{raw}`")))
}

impl Args {
    /// `bench-ipc`'s own flags: `--alternate`, `--rounds`, `--against-sel4`,
    /// `--against-redox`, `--record` and `--pin`.
    fn bench(&mut self, key: &str, items: &mut impl Iterator<Item = String>) -> Result<()> {
        match key {
            "--alternate" => self.alternate = Some(value(items, key)?),
            "--rounds" => self.rounds = Some(count(items, key)?),
            "--against-sel4" => self.against_sel4 = true,
            "--against-redox" => self.against_redox = true,
            "--record" => self.record = true,
            _ => self.pin = Some(value(items, key)?),
        }
        Ok(())
    }
}

/// Take a count following a `--key`, which may not be none: a video of no
/// frames a second is not a slower video, it is no video.
fn count(items: &mut impl Iterator<Item = String>, key: &str) -> Result<u32> {
    let taken: u32 = number(items, key)?;
    if taken == 0 {
        return Err(Error::new(format!("{key} wants more than none")));
    }
    Ok(taken)
}

/// `<width>x<height>`, both more than none.
fn dimensions(raw: &str, key: &str) -> Result<(u32, u32)> {
    raw.split_once('x')
        .and_then(|(wide, tall)| wide.parse().ok().zip(tall.parse().ok()))
        .filter(|&(wide, tall): &(u32, u32)| wide > 0 && tall > 0)
        .ok_or_else(|| Error::new(format!("{key} wants <width>x<height>, got `{raw}`")))
}

/// A monitor scale as `hyprland.conf` writes it: a number above 0 and at
/// most 8, kept as it was typed so `2` stays `2` in the `monitor =` line.
fn scale(flag: &str, raw: &str) -> Result<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|scale| *scale > 0.0 && *scale <= 8.0)
        .ok_or_else(|| {
            Error::new(format!(
                "{flag} wants a number above 0 and at most 8, got `{raw}`"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &[&str]) -> Result<Args> {
        Args::parse(line.iter().map(|item| (*item).to_owned()))
    }

    #[test]
    fn a_scale_is_a_positive_number_kept_as_typed() {
        let args = parse(&["run-compositor", "--scale", "2.5"]).expect("parsed");
        assert_eq!(args.scale.as_deref(), Some("2.5"));
        for bad in ["0", "-1", "nine", "9"] {
            assert!(
                parse(&["run-compositor", "--scale", bad]).is_err(),
                "{bad} refused"
            );
        }
    }

    #[test]
    fn remote_options_and_passthrough_are_kept_separate() {
        let args = parse(&[
            "remote-desktop",
            "--host",
            "builder",
            "--local-port",
            "5902",
            "--send",
            "head",
            "--no-viewer",
            "--print-command",
            "--",
            "--config",
            "guest.conf",
            "--future-option",
        ])
        .unwrap();
        assert_eq!(args.host.as_deref(), Some("builder"));
        assert_eq!(args.local_port, Some(5902));
        assert_eq!(args.send.as_deref(), Some("head"));
        assert!(args.no_viewer && args.print_command);
        assert!(args.config.is_none());
        assert_eq!(
            args.passthrough,
            ["--config", "guest.conf", "--future-option"]
        );
    }

    #[test]
    fn takes_a_command_and_its_options() {
        let args = parse(&[
            "test-boot",
            "--arch",
            "aarch64",
            "--release",
            "--timeout",
            "30",
        ])
        .unwrap();

        assert_eq!(args.command.as_deref(), Some("test-boot"));
        assert_eq!(args.single_arch().unwrap(), Arch::AArch64);
        assert!(args.release);
        assert_eq!(args.timeout, 30);
    }

    #[test]
    fn the_kernel_keeps_its_debug_information_unless_asked() {
        let args = parse(&["test-boot", "--strip-kernel", "--memory", "16"]).unwrap();
        assert!(args.strip_kernel);
        assert_eq!(args.memory, 16);
        assert!(!parse(&["test-boot"]).unwrap().strip_kernel, "opt-in");
    }

    #[test]
    fn jobs_are_a_count_of_at_least_one() {
        assert_eq!(parse(&["miri"]).unwrap().jobs, None, "unset picks its own");
        assert_eq!(parse(&["miri", "--jobs", "3"]).unwrap().jobs, Some(3));
        assert!(parse(&["miri", "--jobs", "0"]).is_err());
        assert!(parse(&["miri", "--jobs", "many"]).is_err());
    }

    #[test]
    fn miri_is_off_unless_asked_for() {
        let args = parse(&["check", "--fast", "--miri"]).unwrap();
        assert!(args.fast && args.miri);
        let plain = parse(&["check"]).unwrap();
        assert!(!plain.fast && !plain.miri, "both are opt-in");
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let args = parse(&["build"]).unwrap();
        assert_eq!(args.smp, 4);
        assert!(!args.smp_given, "the default is not a count asked for");
        assert_eq!(args.memory, 512);
        assert_eq!(args.timeout, 120);
        assert!(!args.release);
    }

    #[test]
    fn arch_all_expands_to_every_architecture() {
        for spelling in ["all", "both"] {
            let args = parse(&["build", "--arch", spelling]).unwrap();
            assert_eq!(
                args.arches().unwrap(),
                vec![Arch::X86_64, Arch::AArch64, Arch::Armv7a],
                "`{spelling}` must not leave an architecture out"
            );
            assert!(
                args.single_arch().is_err(),
                "a command needing one architecture must refuse several"
            );
        }
    }

    #[test]
    fn iterate_is_a_flag_of_its_own_and_off_by_default() {
        assert!(!parse(&["build", "--release"]).unwrap().iterate);
        let args = parse(&["build", "--release", "--iterate"]).unwrap();
        assert!(args.iterate && args.release);
    }

    #[test]
    fn mitigations_default_on_and_take_on_or_off_only() {
        use super::Mitigations;
        assert_eq!(parse(&["build"]).unwrap().mitigations, Mitigations::On);
        assert_eq!(
            parse(&["build", "--mitigations", "off"])
                .unwrap()
                .mitigations,
            Mitigations::Off
        );
        assert_eq!(
            parse(&["build", "--mitigations", "on"])
                .unwrap()
                .mitigations,
            Mitigations::On
        );
        assert!(parse(&["build", "--mitigations", "auto"]).is_err());
        assert!(parse(&["build", "--mitigations"]).is_err());
    }

    #[test]
    fn accel_defaults_to_none_and_is_taken_verbatim() {
        assert_eq!(parse(&["run"]).unwrap().accel, None);
        assert_eq!(
            parse(&["run", "--accel", "whpx"]).unwrap().accel.as_deref(),
            Some("whpx")
        );
    }

    #[test]
    fn takes_the_board_options() {
        let args = parse(&[
            "deploy",
            "--to",
            "/media/johndoe/bootfs",
            "--port",
            "/dev/ttyACM0",
        ])
        .unwrap();
        assert_eq!(args.to.as_deref(), Some("/media/johndoe/bootfs"));
        assert_eq!(args.port.as_deref(), Some("/dev/ttyACM0"));
    }

    #[test]
    fn the_board_options_default_to_discovery() {
        let args = parse(&["flash"]).unwrap();
        assert_eq!(args.to, None, "no --to means find the only card");
        assert_eq!(args.port, None, "no --port means find the only port");
    }

    #[test]
    fn rejects_an_unknown_option() {
        assert!(parse(&["build", "--wat"]).is_err());
    }

    #[test]
    fn rejects_a_missing_or_unparseable_value() {
        assert!(parse(&["build", "--arch"]).is_err());
        assert!(parse(&["build", "--smp", "lots"]).is_err());
        let given = parse(&["run", "--smp", "2"]).expect("a count parses");
        assert!(
            given.smp_given && given.smp == 2,
            "a count asked for is kept and marked"
        );
    }

    #[test]
    fn rejects_a_second_bare_argument() {
        assert!(
            parse(&["build", "run"]).is_err(),
            "two commands is a typo, not a request"
        );
    }

    #[test]
    fn ferrousli_is_off_unless_asked_for() {
        assert!(!parse(&["check"]).unwrap().ferrousli);
        let args = parse(&["check", "--fast", "--ferrousli"]).unwrap();
        assert!(args.fast && args.ferrousli);
    }

    #[test]
    fn zinc_is_off_unless_asked_for() {
        assert!(!parse(&["check"]).unwrap().zinc);
        assert!(parse(&["check", "--zinc"]).unwrap().zinc);
        assert_eq!(parse(&["build", "--statd"]).unwrap().apps, ["statd"]);
        assert!(parse(&["build"]).unwrap().apps.is_empty());
        assert!(parse(&["test-threads", "--i686"]).unwrap().i686);
        assert!(!parse(&["test-threads"]).unwrap().i686);
    }

    #[test]
    fn reset_is_off_unless_asked_for() {
        assert!(!parse(&["test-boot"]).unwrap().reset);
        assert!(parse(&["test-boot", "--reset"]).unwrap().reset);
    }

    #[test]
    fn kernel_options_are_kept_in_order_and_one_word_each() {
        assert!(parse(&["test-boot"]).unwrap().kernel_options.is_empty());
        let args = parse(&[
            "test-boot",
            "--kernel-option",
            "ferrix.fbcon",
            "--kernel-option",
            "nosmp",
        ]);
        assert_eq!(args.unwrap().kernel_options, ["ferrix.fbcon", "nosmp"]);
        assert!(parse(&["test-boot", "--kernel-option", "a b"]).is_err());
        assert!(parse(&["test-boot", "--kernel-option", ""]).is_err());
    }

    #[test]
    fn the_clipboard_is_off_unless_asked_for() {
        assert!(
            !parse(&["run"]).unwrap().clipboard,
            "a boot has no virtio-serial device unless one was asked for"
        );
        assert!(
            !parse(&["run", "--display"]).unwrap().clipboard,
            "a screen does not bring one: the bus a check enumerates should \
             be the bus it has always enumerated"
        );
        assert!(parse(&["run", "--clipboard"]).unwrap().clipboard);
    }

    #[test]
    fn everything_turns_on_every_feature_of_a_watched_desktop() {
        let args = parse(&["flash", "--compositor", "--session"]).unwrap();
        assert!(args.session && args.compositor && !args.everything);
        let args = parse(&["flash", "--session", "--bar-zoom", "3", "--scale", "2"]).unwrap();
        assert_eq!(
            (args.bar_zoom, args.scale.as_deref()),
            (Some(3.0), Some("2"))
        );
        assert!(parse(&["flash", "--bar-zoom", "0"]).is_err());
        let args = parse(&[
            "flash",
            "--bar-drop",
            "cpu, tray",
            "--bar-margin-right",
            "150",
        ])
        .unwrap();
        assert_eq!(args.bar_drop, ["cpu", "tray"]);
        assert_eq!(args.bar_margin_right, Some(150));
        let args = parse(&["run-compositor", "--everything"]).unwrap();
        assert!(args.everything && args.chrome && args.clipboard && args.release);
        assert!(args.gl && args.display);
        // A feature taken away after it is still taken away.
        let args = parse(&["run-compositor", "--everything", "--no-gl"]).unwrap();
        assert!(args.everything && args.no_gl && !args.gl);
        let args = parse(&["run-compositor", "--no-gl", "--everything"]).unwrap();
        assert!(args.everything && args.no_gl && !args.gl);
    }

    #[test]
    fn net_is_off_unless_asked_for() {
        assert!(
            !parse(&["run"]).unwrap().net,
            "a boot has no network device unless one was asked for"
        );
        assert!(
            parse(&["run", "--net"]).unwrap().net,
            "--net turns the device and the gateway on"
        );
    }

    #[test]
    fn a_forward_is_two_ports_and_brings_the_network() {
        let args = parse(&["run", "--forward", "2222:22", "--forward", "8080:80"]).unwrap();
        assert!(
            args.net,
            "a forward with no network behind it forwards nothing"
        );
        assert_eq!(
            args.forwards,
            [
                crate::gateway::Forward {
                    host: 2222,
                    guest: 22
                },
                crate::gateway::Forward {
                    host: 8080,
                    guest: 80
                },
            ],
            "every --forward is kept, in order"
        );
        let ssh = parse(&["run-compositor", "--ssh", "2222"]).unwrap();
        assert_eq!(ssh.ssh, Some(2222));
        assert_eq!(
            ssh.forwards,
            [crate::gateway::Forward {
                host: 2222,
                guest: 22
            }],
            "--ssh is a forward to the guest's port 22 as well"
        );
        assert!(parse(&["run-compositor", "--ssh", "0"]).is_err());
        for bad in ["22", "2222:", ":22", "0:22", "2222:0", "ssh:22", "70000:22"] {
            assert!(
                parse(&["run", "--forward", bad]).is_err(),
                "`{bad}` is not <host port>:<guest port>"
            );
        }
    }
}
