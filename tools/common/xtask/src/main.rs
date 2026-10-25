//! Host-side driver for building, imaging and booting Ferrix.
//!
//! ```text
//! cargo xtask build     --arch x86_64 [--release] [--init PATH/{arch}/busybox] [--init-path /sbin/init]
//! cargo xtask run       --arch x86_64 [--release] [--gdb] [--smp N] [--memory M]
//!                       [--accel auto|tcg|whpx|kvm|hvf] [--init PATH/{arch}/busybox] [--net]
//! cargo xtask test-boot --arch x86_64 [--release] [--timeout SECONDS] [--reset] [--net]
//! cargo xtask test-kaslr --arch x86_64 [--release] [--timeout SECONDS]
//! cargo xtask test-shell --arch all --init PATH/{arch}/busybox [--timeout SECONDS]
//! cargo xtask test-vfs  --arch all --init PATH/{arch}/busybox [--timeout SECONDS]
//! cargo xtask test-threads --arch all [--i686] [--timeout SECONDS]
//! cargo xtask test-sem --arch all [--i686] [--timeout SECONDS]
//! cargo xtask test-shm --arch all [--i686] [--timeout SECONDS]
//! cargo xtask test-container --arch all [--timeout SECONDS]
//! cargo xtask test-procfs --arch all [--i686] [--timeout SECONDS]
//! cargo xtask test-uvm  [--arch x86_64] [--timeout SECONDS]
//! cargo xtask test-nvrm [--arch x86_64] [--accel kvm|tcg] [--timeout SECONDS]
//! cargo xtask test-nvrm-link [--arch x86_64] [--accel kvm|tcg] [--timeout SECONDS]
//! cargo xtask run-nvidia [--timeout SECONDS]
//! cargo xtask test-rustc [--accel kvm] [--memory M] [--timeout SECONDS]
//! cargo xtask test-game-rig --init PATH/{arch}/busybox [--accel kvm] [--smp N] [--memory M] [--timeout SECONDS]
//! cargo xtask test-selfhost [--accel kvm] [--release] [--smp N] [--memory M] [--timeout SECONDS] [--plan DIR]
//! cargo xtask test-selfhost --arch x86_64|aarch64 --volume IMG [--release]
//! cargo xtask test-devloop [--accel kvm] [--release] [--interpreter glibc] [--memory M] [--timeout SECONDS]
//! cargo xtask builds-execute --plan DIR
//! cargo xtask check     [--fast] [--ferrousli] [--zinc] [--miri]
//! cargo xtask miri      [--jobs N]
//! cargo xtask remote-desktop [--host DEST] [--config PATH] [--vnc :N] [--send head]
//!                       [--viewer tigervnc|realvnc] [--layout de] [--no-viewer]
//!                       [--print-command] [--stop] [-- ARGS...]
//! cargo xtask busybox   [--arch x86_64]
//! cargo xtask ports     [--arch x86_64|aarch64|armv7a]
//! cargo xtask omz       --from DIRECTORY-OR-URL
//! cargo xtask zsh-functions --from DIRECTORY
//! cargo xtask flash     [--arch armv7a] [--to MOUNT | --stage DIR] [--compositor [--config PATH]]
//! cargo xtask watch-serial            [--port DEVICE] [--timeout SECONDS]
//! cargo xtask deploy    [--arch armv7a] [--to MOUNT] [--port DEVICE]
//! ```
//!
//! `build` compiles the loader and the kernel for their two different targets
//! and writes a bootable FAT32 image. `test-boot` boots that image under QEMU,
//! watches the serial port, and fails if the kernel does not report success —
//! which is the only test in this repository that can tell us the thing runs.
//!
//! `build` and `run` given a static busybox — `--init`, or the `FERRIX_INIT`
//! variable — build it into the kernel, which starts `sh -i` on the console,
//! and put it in the initramfs at `/bin/busybox` with every applet linked
//! beside it, so that the shell finds `ls` on its `PATH=/bin`.
//!
//! `omz` installs the oh-my-zsh checkout every image carries, from a directory
//! on this machine or a git repository to clone. Without it an image boots to a
//! shell with no configuration; with it, to the one oh-my-zsh gives.
//! `zsh-functions` installs zsh's own function tree -- `compinit`,
//! `is-at-least`, `add-zsh-hook` -- which oh-my-zsh calls and the image
//! carries beside it.
//!
//! `busybox` builds busybox against ferrousli, on Linux or natively on Windows,
//! and installs it for `--init ferrousli`, which names that binary instead of a
//! path and rebuilds it when it is older than ferrousli.

// AUDIT: this is a command-line build tool. Its output *is* stdout and stderr,
// and routing it through a logging facade would make `cargo xtask build` read
// like a service rather than like a build.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "AUDIT: xtask is a CLI build tool; the terminal is its interface"
)]

mod ab_boot;
mod adbd;
mod apps;
mod args;
mod audio;
mod auth;
mod badapple;
mod box_log;
mod btrfs_check;
mod btrfs_disk;
mod btrfs_pressure;
mod builds;
mod busybox;
mod bwrap;
mod cargo;
mod check;
mod chrome;
mod claude_code;
mod components;
// A Unix socket is the clipboard port's far end.
#[cfg(unix)]
mod clipboard;
// test-clipboard's codec, which nothing else here speaks.
#[cfg(not(unix))]
use ferrix_vdagent as _;
/// Elsewhere, test-clipboard only says where it runs.
#[cfg(not(unix))]
mod clipboard {
    use crate::task::Task;
    use crate::{Error, Result};

    /// The command `crate::clipboard` answers on Unix, refused.
    pub(crate) const TASKS: &[Task] = &[Task {
        name: "test-clipboard",
        help_at: 270,
        help: &[
            "test-clipboard  Boot the desktop with its clipboard port on a socket xtask speaks vdagent over, and carry text both ways",
        ],
        run: |_, _| -> Result<()> {
            Err(Error::new(
                "test-clipboard serves the clipboard port on a Unix socket: run it on the Linux host",
            ))
        },
    }];
}
mod compositor;
mod console;
mod container;
mod coverage;
mod devloop;
mod dhcp;
mod disk_offline;
mod display;
mod dma_faults;
mod dmesg;
mod dotfiles;
mod dxvk;
mod edid;
mod everything;
mod fat;
mod ferrousli;
mod flags;
mod flash;
mod frames;
mod fuzzel;
mod game_rig;
mod gate_rows;
mod gateway;
mod hotpath;
mod image_file;
mod init;
mod init_file;
mod initramfs;
mod input;
mod installer;
mod intel_uapi;
mod ipc;
mod jobs;
mod kaslr;
mod keyboard;
mod lode;
mod lode_budget;
mod lode_net;
mod lode_pstore;
mod lode_remote;
mod machine_check;
mod n150;
mod native;
mod net;
mod netboot;
mod noise;
mod nvidia;
mod nvme;
mod nvrm;
mod nvrm_link;
mod omz;
mod orphans;
mod parallel;
mod paths;
mod pe;
mod persistent;
mod phone;
mod pipe_bench;
mod pkg;
mod ports;
mod postmortem;
mod powerctl;
mod powerfail;
mod procfs;
mod pty;
mod qemu;
mod remote;
mod rename_crash;
mod restart;
mod rows_bench;
mod rustc;
mod seam;
mod seat;
mod seccomp;
mod selfhost;
mod sem;
mod serial;
mod session;
mod settingsd;
mod sha256;
mod shader_cache;
mod shell;
mod shm;
mod signing;
mod slot_image;
mod smbios;
mod ssh;
mod stall;
mod start_page;
mod steam;
mod steamcmd;
mod step_cache;
mod sud;
mod symbolize;
mod sysfs;
mod task;
mod test_cache;
mod test_disk;
mod test_runs;
mod threads;
mod uboot_env;
mod usb_serial;
mod uefi_vars;
mod uutils;
mod uvm;
mod vfs;
mod vnc;
mod wallpaper;
mod waybar;
mod window;
mod wine;
mod wine_dispatch;
mod wine_socket;
mod wine_sync;
mod workspace;
mod wsl;
mod yserver;
mod zinc;

use std::path::PathBuf;
use std::process::ExitCode;

use args::Args;
use paths::Arch;
use task::Task;

/// What went wrong, in a form that can be printed and turned into an exit code.
#[derive(Debug)]
pub(crate) struct Error {
    /// Human-readable description, already contextualised.
    message: String,
}

impl Error {
    /// Build an error from anything printable.
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::new(error.to_string())
    }
}

/// The result type every xtask step returns.
pub(crate) type Result<T> = std::result::Result<T, Error>;

/// `--help` after its commands: the options, which every command shares
/// (`crate::task::usage` puts the commands before them).
const OPTIONS: &str = "
OPTIONS:
    --arch <x86_64|aarch64|armv7a|all>   Target architecture   [default: host]
    --release                            Build with optimisations
    --iterate                            With --release: the kernel with thin LTO, relinked in
                                         seconds; for working on it, never for a gate
                                         (docs/TEST-TIME.md, C4)
    --strip-kernel                       Images carry the kernel and the initramfs's programs
                                         without their debug information, as flash writes them;
                                         the loader reads both whole into memory, so a small
                                         --memory needs it [default: off]
    --kernel-profile <NAME>              The kernel's profile in tools/common/data/profiles/
                                         [default: present, today's reference configuration]:
                                         every build flag's value (docs/BUILD-FLAGS.md)
    --set <FLAG=VALUE>                   Change one build flag, as often as needed: an ad-hoc
                                         build, profile=none level=none, which carries no claim
                                         and which the gate refuses as its profile's evidence
    --no-claim                           test-boot of a --set build: the gate compares its line
                                         with the profile and the --sets, and passes it as an
                                         ad-hoc run that carries no claim
    --mitigations <on|off>              --set mitigations=on|off by its older name. off builds
                                         with ferrix_mitigations=\"off\" and the static relocation
                                         model into target/mitigations-off: no index clamps in
                                         the kernel, no speculation controls, no barriers
                                         (docs/certification/SPECULATION.md)
    --smp <N>                            Virtual CPUs          [default: 4; 1 under whpx with a
                                         QEMU not built by fetch-qemu-windows.sh]
    --memory <MiB>                       Guest memory          [default: 512]
    --timeout <SECONDS>                  test-boot patience    [default: 120]
    --seeds <N>                          test-powerfail cuts   [default: 8]
    --accel <auto|tcg|whpx|kvm|hvf>      QEMU accelerator      [default: auto for run
                                         without --gdb, tcg otherwise]
    --vtd-aw48                           x86-64: the VT-d unit with aw-bits=48, so the kernel walks
                                         four-level tables (the N150's only walk)
    --amd-vi                             x86-64: an AMD-Vi unit (amd-iommu, intremap, dma-remap)
                                         in place of the VT-d unit (docs/AMD-VI.md)
    --gdb                                Wait for a debugger on :1234, with nokaslr in CMDLINE.TXT so the
                                         kernel runs where its ELF's symbols say, and a debug kernel
                                         built with the full DWARF rather than line tables
    --net                                run, test-boot, test-shell, test-vfs, run-compositor:
                                         a virtio-net device,
                                         behind xtask's own NAT gateway (10.0.2.2, guest 10.0.2.15);
                                         test-net turns it on whether or not it is given;
                                         run-compositor has one unless --no-net
    --no-net                             run-compositor: no network device and no gateway
    --no-dotfiles                        run-compositor: carry --config's file alone, without the
                                         directories and fonts beside it
    --chrome                             run-compositor: Chrome on the desktop, from the volume
                                         tools/common/fetch/fetch-chrome.sh makes; SUPER+B opens another
    --nvidia                             run-compositor: the --chrome desktop on the RTX 3060's own
                                         monitor (libvirt, as run-nvidia), Chrome on NVIDIA's Vulkan
    --everything                         run-compositor: all of it at once -- --gl, --release,
                                         --clipboard, --chrome, and rustc and cargo in the shell,
                                         from one volume made of the rustc and Chrome ones, with
                                         claude too once fetch-claude-code.sh has run; and
                                         this machine's ~/.config/hypr/hyprland.conf with its
                                         dotfiles unless --config or --no-dotfiles says otherwise
    --steam-preinstall                   run-compositor --everything, test-steam-game: merge the Steam
                                         library fetch-steam-preinstall.sh installed on the host
                                         (Teeworlds, the Steam Linux Runtime) into /data/steam, so
                                         Steam finds them installed and does not download them
    --session                            flash --compositor: the desktop runs as the user ferrix,
                                         started by sessiond, as --everything's does; --config's
                                         dotfiles seed the home disk once and edits there are kept,
                                         and the board's screen beats the config's monitor lines
    --bar-zoom <N>                       flash --session: the carried waybar N times its size, its
                                         style's px lengths and its config's height, width, spacing
                                         and icon-size multiplied; the host's files are not changed
    --bar-drop <a,b,...>                 flash --session: the carried waybar without these modules
    --bar-margin-right <N>               flash --session: the carried waybar N px short of the
                                         right edge
    --forward <HOST>:<GUEST>             the host's 127.0.0.1:HOST leads to the guest's port GUEST,
                                         e.g. 2222:22 for sshdt; repeatable; turns --net on
    --ssh <PORT>                         run-compositor: start sshdt in the guest, reached at
                                         127.0.0.1:PORT; the keys in ~/.ssh may log in, and so may
                                         ~/.local/share/ferrix/ssh/id_ed25519, which the boot prints
    --ssh-key <FILE|KEY>                 another public key that may log in, as a file or written
                                         out; repeatable; for a client whose key is in neither place
    --display                            run, test-boot: a virtio-gpu device; run: and a window showing it
    --gl                                 that virtio-gpu is the 3D card, `virtio-gpu-gl-pci`, with the
                                         host's GPU behind it through virglrenderer; turns --display on.
                                         A QEMU built without it says so and the 2D card is used
                                         [FERRIX_QEMU names a QEMU that is not the one on PATH: a
                                         directory of its binaries, or one binary; without it, the
                                         first QEMU on PATH that has the 3D card]
    --venus                              the 3D card offers Venus -- Vulkan on the host's GPU -- with
                                         blob resources and a 1 GiB host-visible window; turns --gl
                                         on. Linux hosts with a Venus-built virglrenderer only
    --no-gl                              run-compositor: the 2D card and the software renderer.
                                         Without either flag a VNC screen gets the 3D card when a
                                         QEMU here has it and the host has a render node
    --clipboard                          run, run-compositor: a virtio-serial port carrying SPICE's
                                         agent protocol, with QEMU's own host half behind it, for a
                                         clipboard shared with whoever is watching. The device only:
                                         no guest driver or agent exists yet, so nothing is shared
                                         today (docs/CLIPBOARD.md §8)
    --audio <BACKEND>                    run, run-compositor: a virtio-snd card whose far end is
                                         QEMU's audio backend BACKEND: `pipewire` or `pa` to hear
                                         it, `wav:PATH` to write what plays to a file (docs/AUDIO.md §4).
                                         --everything brings one on the host's sound server
                                         (pipewire, pa, coreaudio or dsound, whichever QEMU has)
    --vnc <DISPLAY>                      run --display, run-compositor: serve the screen over VNC
                                         at e.g. `:0` (127.0.0.1) rather than in a window of this host's
    --rendernode <PATH>                  --gl on a served or headless screen: which GPU egl-headless
                                         draws on, e.g. /dev/dri/renderD128. run-compositor takes the
                                         first whose driver is not NVIDIA's proprietary one otherwise;
                                         other boots leave it to QEMU, which on a machine with two GPUs
                                         is a guess. A window's GL goes to the host display's GPU regardless
    --config <PATH>                      run-compositor: the hyprland.conf the guest is given,
                                         and the user's dotfiles beside it: the directories
                                         hypr, waybar and fuzzel next to the one PATH is in go
                                         to the guest's $HOME/.config (HOME is /), and the font
                                         families they name, resolved here with fc-match, to
                                         /usr/share/fonts/host (tools/common/xtask/src/dotfiles.rs);
                                         remote-desktop: the file of answers to read instead of
                                         the ones searched [or $FERRIX_REMOTE]
    --host <DESTINATION>                 remote-desktop: the machine to boot on, as `ssh` names
                                         one, over what the config file said. No host is a
                                         default anywhere in xtask; this and that file are the
                                         only two ways one is named
    --local-port <N>                     remote-desktop: the port the tunnel listens on here
                                         [default: 5900 + the display, or the next one free]
    --send <working-tree|head>           remote-desktop: boot what you are looking at,
                                         uncommitted changes and all, or your last commit
                                         [default: working-tree]
    --no-viewer                          remote-desktop: open the tunnel and nothing else
    --viewer <tigervnc|realvnc|auto|none>
                                         remote-desktop: the viewer to open, over the config
                                         file's. TigerVNC sends keys and the guest's layout reads
                                         them; RealVNC sends characters, so the boot is given
                                         --keymap <the first --layout> as well
    --keymap <NAME>                      run --display, run-compositor over VNC: QEMU's keymap for
                                         a viewer that sends characters rather than keys (`de`,
                                         `us`, `en-gb`). Not for one that sends keys: QEMU would
                                         translate those through it too
    --print-command                      remote-desktop: say what would be sent, run and opened,
                                         and do none of it
    --stop                               remote-desktop: end a boot left running over there, and
                                         do nothing else
    --                                   remote-desktop: everything after this goes to the
                                         remote `cargo xtask` as it stands
    --layout <LIST>                      run-compositor, flash --compositor: the keyboard layout, as
                                         input:kb_layout takes it: `de`, or `de,us` for two a switch
                                         moves between [flash --compositor default: de;
                                         run-compositor default: this machine's, from
                                         /etc/default/keyboard or /etc/vconsole.conf]
    --variant <LIST>                     run-compositor: their variants, as input:kb_variant
                                         takes them: `nodeadkeys,` is one for the first layout only
    --wallpaper <NAME>                   run-compositor: which kept wallpaper to show, by part of its
                                         name, or `none`; one of them, another each run, otherwise
    --theme <NAME>                       run-compositor, flash --compositor, release-image: the
                                         desktop's look, `yoru`, `ferrix-light` or `none` [default:
                                         yoru]; under a configuration of its own, its look alone
    --edid <DESCRIPTION>                 run-compositor: give the screen the EDID of this machine's
                                         monitor whose description starts so, as Linux's
                                         drm.edid_firmware= does, so `monitor = desc:` and waybar
                                         `output` find it; `none` for no EDID [default: Lenovo Group
                                         Limited R27qe Gen2]. Found by reading every
                                         /sys/class/drm/card*-*/edid; carries /usr/share/hwdata/pnp.ids
                                         too, without which the make is the three-letter code (LEN).
                                         A machine without the monitor says so and the screen has
                                         none, its description then only its name, Virtual-1
    --from <WHERE>                       wallpapers: where the pictures are, a directory here or
                                         <host>:<directory> for one `ssh` reaches
    --listen <ADDRESS:PORT|PORT>         netboot: where to serve [default: this host's address on its
                                         default route, port 8086]
    --server <HOST[:PORT]>               netboot-stick: where the machine finds `netboot` [port 8086]
    --image <FAT>                        n150-install: the image volume for the ESP [default: build's]
    --tree <DIR>                         n150-install: the tree at /data [default: the Proton
                                         launcher volume's, ~/.local/share/ferrix/endfield-launcher-proton/tree]
    --disk <IMG>                         n150-install: where to write [default: build/x86_64/n150-disk.img]
    --disk-bytes <N>                     n150-install: the box's disk size [default: 512110190592]
    --scale <N>                         run-compositor, flash --compositor: the monitor's scale, 1 by default
    --size <W>x<H>                       run-compositor: the guest's screen; wallpapers: the screen
                                         to cut pictures for (1920x1080 for both)
    --fps <N>                            wallpapers: frames a second kept of a video
    --seconds <N>                        wallpapers: seconds of it kept, after which it begins again
    --video-size <W>x<H>                 wallpapers: how large its frames are kept
                                         These three, and which wallpaper to show, are also
                                         ~/.config/ferrix/wallpaper.toml, which a flag here beats
    --fast                               check: skip the cross-target clippy passes
    --ferrousli                          check: also ferrousli's fmt, clippy and tests, debug and release
    --zinc                               check: also zinc's fmt, clippy, tests and pty completion test
    --statd                              build, run, test-boot: --app statd, the stat service at /sbin/ferrix-statd
    --installer                          build, run: carry /sbin/ferrix-install and its root volume (the live image)
    --adbd                               build, run: carry adbd at /bin/adbd, started by nobody (docs/ADB.md)
    --app <NAME>                         run, run-compositor: also carry an app that is not in images by default;
                                         give it once for each app
    --no-apps                            run, run-compositor: carry no apps, not even the default ones
    --abi <native|linux>                 new-app: the ABI the new app's program speaks [default: native]
    --miri                               check: add CI's Miri steps (needs nightly and miri)
    --jobs <N>                           miri: crates interpreted at once, by default one per core up to 8
    --reset-root                         run, run-compositor: start the btrfs root over from a fresh install,
                                         keeping /home and --persistent's /data
    --reset-flash                        run, run-compositor: start everything kept over: the root, /home and
                                         --persistent's /data
    --tmpfs-root                         run, run-compositor: / in memory instead of on the btrfs root disk
    --persistent                         run, run-compositor: keep /data too, and Chrome's profile on it, so a
                                         Steam sign-in or a Chrome extension is there at the next boot;
                                         with --reset-flash, start /data over as well
    --btrfs-root                         test-chrome-window: / on a btrfs root disk made fresh for the run
    --reset                              test-boot: ferrix.onexit=reset in CMDLINE.TXT, and require a reset;
                                         build, run: put that CMDLINE.TXT in the image
    --to <MOUNT>                         flash: the card's mounted boot partition
    --alternate <REF>                    bench-ipc: build REF in a tree of its own and time the two
                                         turn about, --rounds times (3)
    --against-sel4, --against-redox      bench-ipc: alternate with the seL4 or Redox image
    --record                             bench-ipc: file the result under docs/hotpaths/results/
                                         (docs/HOTPATHS.md; the log's path from FERRIX_HOTPATH_LOG)
    --board-log <FILE>                   bench-ipc: read a board's serial log of one boot that ran
                                         /sbin/ipc-bench instead of booting QEMU; once per boot
    --board <NAME>                       bench-ipc --board-log: the board, which --record files under
    --fail-above <ROW>=<N>               bench-rows: fail when ROW's median p50 is above N ns; test-rustc,
                                         test-selfhost: when ROW is above N ms (once a row)
    --pin <CPUS>                         QEMU on these host processors (taskset -c); bench-ipc
                                         pins to 11 by default, `none` for no pin
    --stage <DIR>                        flash: write the card's files into DIR instead, to copy by hand
    --compositor                         flash, deploy: the desktop run-compositor boots, not the self-checks
    --port <DEVICE>                      watch-serial: e.g. /dev/ttyACM0
    --init <PATH|ferrousli>              The busybox; {arch} is replaced. build, run, flash, deploy: [or FERRIX_INIT]
                                         start `sh -i`, with the applets linked in /bin.
                                         `ferrousli`: the x86_64 busybox built against ferrousli, rebuilt when stale
    --init-path <PATH>                   build, run, test-boot: ferrix.init=PATH in CMDLINE.TXT, so the
                                         kernel starts pid 1 from that file in the image, and the
                                         program built in only if it will not start
    --kernel-option <WORD>               build, run, test-boot: add WORD to CMDLINE.TXT, e.g. ferrix.fbcon;
                                         give it once for each word
    --interpreter <PATH|ferrousli>       test-shell: a dynamic linker, carried at --init's PT_INTERP path; {arch} is replaced;
                                         test-chrome, test-chrome-window, test-rustc, run-compositor --chrome: at the program's;
                                         run-compositor --chrome and --everything, and test-chrome-audio, are on ferrousli by
                                         default, `glibc` for the volume's
                                         `ferrousli`: ferrousli's ld-ferrousli, built from this tree
    --library <PATH|ferrousli>           test-shell, test-chrome(-window|-audio), test-rustc, run-compositor --chrome: a shared library, carried in /lib; as many as needed; {arch} is replaced
                                         `ferrousli`: ferrousli linked as libc.so.6, built from this tree
    --boot <NAME>                        test-compositor: only the boots whose name holds this
    -h, --help                           This message
";

fn main() -> ExitCode {
    orphans::install();
    match test_runs::command().unwrap_or_else(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nxtask: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `build`: an image for each architecture asked for.
fn build(args: &Args) -> Result<()> {
    let arches = args.arches()?;
    cargo::build_ahead(&arches, args.release)?;
    for arch in arches {
        let (image, kernel) = build_image(arch, args)?;
        println!("built {}", image.display());
        let elf = std::fs::read(&kernel)
            .map_err(|error| Error::new(format!("reading {}: {error}", kernel.display())))?;
        // The kernel's own hash, so that two builds can be shown to make the
        // same kernel byte for byte (docs/BUILD-FLAGS.md §6, item 3).
        println!(
            "  {arch} kernel sha256 {} ({})",
            sha256::hex(&elf),
            kernel.display()
        );
    }
    Ok(())
}

/// Every module's table of the commands it answers (`crate::task`): a new
/// module with commands adds its line here, beside its `mod` line above.
const TASKS: &[&[Task]] = &[
    MAIN_TASKS,
    adbd::TASKS,
    apps::TASKS,
    audio::TASKS,
    auth::TASKS,
    badapple::TASKS,
    box_log::TASKS,
    btrfs_check::TASKS,
    btrfs_pressure::TASKS,
    builds::TASKS,
    busybox::TASKS,
    bwrap::TASKS,
    check::TASKS,
    chrome::TASKS,
    claude_code::TASKS,
    clipboard::TASKS,
    components::TASKS,
    compositor::TASKS,
    coverage::TASKS,
    devloop::TASKS,
    display::TASKS,
    dxvk::TASKS,
    everything::TASKS,
    flags::TASKS,
    flash::TASKS,
    frames::TASKS,
    gate_rows::TASKS,
    hotpath::TASKS,
    init::TASKS,
    input::TASKS,
    intel_uapi::TASKS,
    installer::TASKS,
    ipc::TASKS,
    jobs::TASKS,
    lode::TASKS,
    lode_budget::TASKS,
    lode_pstore::TASKS,
    lode_remote::TASKS,
    machine_check::TASKS,
    powerctl::TASKS,
    n150::TASKS,
    netboot::TASKS,
    nvidia::TASKS,
    nvme::TASKS,
    omz::TASKS,
    phone::TASKS,
    pipe_bench::TASKS,
    pkg::TASKS,
    ports::TASKS,
    postmortem::TASKS,
    powerfail::TASKS,
    pty::TASKS,
    remote::TASKS,
    disk_offline::TASKS,
    rename_crash::TASKS,
    restart::TASKS,
    rows_bench::TASKS,
    rustc::TASKS,
    seam::TASKS,
    seat::TASKS,
    sem::TASKS,
    serial::TASKS,
    settingsd::TASKS,
    shader_cache::TASKS,
    slot_image::TASKS,
    stall::TASKS,
    steamcmd::TASKS,
    symbolize::TASKS,
    sysfs::TASKS,
    usb_serial::TASKS,
    uutils::TASKS,
    wallpaper::TASKS,
    wine_dispatch::TASKS,
    wine_socket::TASKS,
    wine_sync::TASKS,
    yserver::TASKS,
    signing::TASKS,
];

/// The commands this file carries out itself.
const MAIN_TASKS: &[Task] = &[
    Task {
        name: "run",
        help_at: 110,
        help: &["run           Boot the image under QEMU, attached to the terminal"],
        run: |_, args| run_machine(args.clone()),
    },
    Task {
        name: "test-boot",
        help_at: 190,
        help: &[
            "test-boot     Boot the image under QEMU and assert the kernel came up, moved (KASLR) unless built",
            "              --mitigations off or told nokaslr, and that its build line is its profile's",
        ],
        run: |_, args| test_boot(args),
    },
    Task {
        name: "test-kaslr",
        help_at: 200,
        help: &[
            "test-kaslr    Boot the image twice and require the loader to have put the kernel somewhere new",
        ],
        run: |_, args| test_kaslr(args),
    },
    Task {
        name: "test-shell",
        help_at: 210,
        help: &[
            "test-shell    Boot with a static busybox built in and require its script's output; again with it",
            "              started from a file by ferrix.init=; under busybox, require reboot(2) to commit /data",
        ],
        run: |_, args| test_shell(args),
    },
    Task {
        name: "test-vfs",
        help_at: 250,
        help: &[
            "test-vfs      Boot with busybox in the initramfs and require stage 8's exit programs and applets",
        ],
        run: |_, args| test_vfs(args),
    },
    Task {
        name: "test-net",
        help_at: 260,
        help: &[
            "test-net      Boot with a network device and require busybox to configure it and fetch a file",
        ],
        run: |_, args| test_net(args),
    },
];

/// `--help`: every command's lines, then the options.
fn usage() -> String {
    task::usage(TASKS, OPTIONS)
}

/// The command `args` name, with the parts of the tree that live in
/// repositories of their own at their pins first: before anything reaches
/// for them, and before `--arch all` starts children that would each clone
/// them (`crate::components`).
fn command_of(args: &Args) -> Result<&str> {
    let Some(command) = args.command.as_deref() else {
        println!("{}", usage());
        return Err(Error::new("no command given"));
    };
    // A unit `check` runs under the step cache is a child of a run that
    // has just done this, and its trace must hold the unit's own reads and
    // writes, not git refreshing the components' indexes.
    if command != "check-unit" && command != "check-audit" {
        components::ensure()?;
    }
    Ok(command)
}

/// Every kernel and image this run builds, whichever command builds it:
/// set once, here, rather than threaded through each of them.
fn set_for_the_run(args: &Args) -> Result<()> {
    cargo::set_kernel_build(
        args.kernel_profile.clone(),
        args.sets.clone(),
        args.iterate,
        args.release,
    )?;
    cargo::set_full_debug(args.gdb);
    fat::set_strip_kernel(args.strip_kernel);
    everything::set_steam_preinstall(args.steam_preinstall);
    Ok(())
}

fn run() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    set_for_the_run(&args)?;

    if args.help {
        println!("{}", usage());
        return Ok(());
    }

    let command = command_of(&args)?;

    // `--arch all` of a command whose architectures are independent: one
    // child an architecture, all at once (`crate::parallel`).
    if parallel::wanted(command, &args) {
        return parallel::run(command, &args);
    }

    match task::find(TASKS, command) {
        Some(task) => (task.run)(command, &args),
        None => Err(Error::new(format!(
            "unknown command `{command}`\n\n{}",
            usage()
        ))),
    }
}

/// `test-vfs`: stage 8's exit programs, then its applets, on each
/// architecture asked for.
///
/// The program goes into the initramfs, at `/bin/busybox`, rather than into
/// the kernel: loading it from a file is part of what stage 8 is for. Every
/// architecture is run even after one fails, because which of the three a
/// missing call breaks is the report.
/// `test-boot`: boot each architecture asked for and require the marker.
/// `run`: the image, with the rustc volume where it has been fetched, on the
/// terminal.
fn run_machine(mut args: Args) -> Result<()> {
    let arch = args.single_arch()?;
    rustc::prepare_default(arch, &mut args)?;
    // Without a program named, the machine a person runs is the system:
    // `/sbin/init` as pid 1, a getty on the console with zinc behind it, and
    // `svc` to drive it (`docs/INIT.md`, L10). A program named with `--init`
    // is still pid 1 itself, as it was asked to be.
    let (image, _) = if optional_program(arch, &args)?.is_none() && args.init_path.is_none() {
        build_init_image(arch, &args)?
    } else {
        build_image(arch, &args)?
    };
    // Someone at the console wants the machine in front of them, so `run`
    // takes whatever hypervisor it has: WHPX on Windows, KVM on Linux, HVF on
    // macOS, emulation where there is none. The tests keep `tcg`, for the
    // reason `qemu::accelerator` gives, and so does a debugging session, whose
    // breakpoints and single steps `tcg` honours on every host and the
    // hypervisors do not.
    let args = Args {
        accel: args
            .accel
            .clone()
            .or_else(|| (!args.gdb).then(|| "auto".to_owned())),
        ..args
    };
    qemu::run(arch, &image, &args)
}

pub(crate) fn test_boot(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let (image, kernel) = build_image(arch, args)?;
        qemu::test_boot(arch, &image, &kernel, args)?;
    }
    Ok(())
}

/// Boot each architecture's image twice and require two layouts
/// (`kaslr::test_kaslr`), each boot a whole `test-boot`.
fn test_kaslr(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let (image, kernel) = build_image(arch, args)?;
        kaslr::test_kaslr(arch, || qemu::test_boot_lines(arch, &image, &kernel, args))?;
    }
    Ok(())
}

/// `cargo xtask test-shell`.
///
/// Verifies: L.init.1
fn test_shell(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        // The shell the kernel starts. zinc, the shell this tree has,
        // unless `--init` names another: the same script under a
        // static busybox is what measures the ABI against somebody
        // else's binary, and both are worth running.
        let program = match args.init.as_deref() {
            Some(init) => program_for(init, arch)?,
            None => zinc::built(arch)?.ok_or_else(|| {
                Error::new(format!(
                    "zinc is not built for {arch}; give --init PATH, a static \
                     shell for each architecture, instead"
                ))
            })?,
        };
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &program, shell::SCRIPT)?;
        let natives = native::build(arch, args.release)?;
        // A dynamically linked shell's linker and libraries, in the
        // initramfs where the kernel and the linker will look.
        let carried = shell::carried_for(arch, &program, args)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_shell(arch, &image, &kernel, args)?;
        // The same shell and script again, started from files by
        // `ferrix.init=`; and under busybox, `reboot(2)`'s commit.
        let parts = init_file::Parts {
            arch,
            loader: &loader,
            kernel: &kernel,
            natives: &natives,
            program: &program,
            carried: &carried,
        };
        init_file::test(&parts, args, args.init.is_some())?;
    }
    Ok(())
}

/// `cargo xtask test-vfs`.
///
/// Verifies: L.init.1
fn test_vfs(args: &Args) -> Result<()> {
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new(
            "test-vfs needs --init PATH, a static busybox for each architecture; \
             `{arch}` in the path is replaced by the architecture's name",
        )
    })?;
    let mut failed = Vec::new();
    for arch in args.arches()? {
        // Per image, because the commands are what its `/bin` runs: uutils'
        // `cat` and the rest, and the uutils rows, where it carries uutils,
        // and busybox's where it does not.
        let utilities = uutils::carried(arch)?;
        let carried = vfs::Utilities::carried(&utilities);
        let commands = vfs::encode(&vfs::commands(carried))?;
        let program = program_for(init, arch)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let list = paths::build_dir(arch).join("init-commands");
        vfs::write_if_changed(&list, &commands)?;
        let kernel = cargo::build_kernel_with_commands(arch, args.release, &list)?;
        let natives = native::build(arch, args.release)?;
        // zinc too: the permissions commands run a set-user-id copy of it,
        // which is the one program in the image that shows an effective id.
        let shell = zinc::build(arch)?;
        // `dmesg` too, which its rows run.
        let mut ports = apps::ported(arch, args)?;
        ports.extend(dmesg::carried(arch)?);
        let initramfs = initramfs::build_with_utilities(
            Some(&program),
            &natives,
            shell.as_deref(),
            &utilities,
            &ports,
        )?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        if let Err(error) = qemu::test_vfs(arch, carried, &image, &kernel, args) {
            eprintln!("\n  {error}");
            failed.push(arch.name());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "stage 8's exit programs or applets failed on {}",
            failed.join(", ")
        )))
    }
}

/// `test-net`: the networking exit criterion, on each architecture asked for.
///
/// The servers the guest fetches from are threads of this process, bound to
/// the host's loopback on ports its kernel chose, so they are started before
/// the guest's programs are written down: the ports are in the arguments. The
/// gateway's DNS forwarder is pointed at the stub for the run, and `--net` is
/// on whether or not it was asked for, since a network test without a network
/// device is a test of nothing.
fn test_net(args: &Args) -> Result<()> {
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new(
            "test-net needs --init PATH, a static busybox for each architecture; \
             `{arch}` in the path is replaced by the architecture's name",
        )
    })?;
    let servers = net::Servers::start()?;
    println!("  host: {}", servers.describe());
    let args = Args {
        net: true,
        resolver: Some(servers.dns()),
        ..args.clone()
    };
    let mut failed = Vec::new();
    for arch in args.arches()? {
        let program = program_for(init, arch)?;
        // The ported programs ride along where they are built, and the
        // programs that exercise them are added when they do.
        let mut ports = apps::ported(arch, &args)?;
        // The DHCP client the first programs take the lease with.
        ports.extend(dhcp::carried(arch)?);
        let curl = ports.iter().any(|file| file.path == "bin/curl");
        let git = ports.iter().any(|file| file.path == "usr/bin/git");
        let programs = net::commands(&servers, curl, git);
        let commands = vfs::encode(&programs)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let list = paths::build_dir(arch).join("net-commands");
        vfs::write_if_changed(&list, &commands)?;
        let kernel = cargo::build_kernel_with_commands(arch, args.release, &list)?;
        let mut natives = native::build(arch, args.release)?;
        // The last program makes the driver lose its ring mid-transfer, which
        // only a fault-armed build of it does (`net::restart_command`).
        native::arm_net_fault(&mut natives, arch, args.release)?;
        let initramfs = initramfs::build(Some(&program), &natives, None, &ports)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        if let Err(error) = qemu::test_net(arch, &image, &kernel, &programs, &args, net::restarted)
        {
            eprintln!("\n  {error}");
            failed.push(arch.name());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "the networking programs failed on {}",
            failed.join(", ")
        )))
    }
}

/// The command line an image carries: `ferrix.onexit=reset` under `--reset`, so
/// the machine resets when boot ends and `test-boot` can require that it did,
/// `ferrix.init=<PATH>` under `--init-path`, so pid 1 is started from
/// that file in the image, `nokaslr` under `--gdb`, so that the kernel runs
/// where the ELF a debugger reads its symbols from says it does, and each
/// `--kernel-option` after those.
pub(crate) fn image_cmdline(args: &Args) -> Option<String> {
    let mut options = Vec::new();
    if args.gdb {
        options.push("nokaslr".to_owned());
    }
    if args.reset {
        options.push(qemu::RESET_OPTION.to_owned());
    }
    if let Some(path) = &args.init_path {
        options.push(qemu::init_option(path));
    }
    options.extend(args.kernel_options.iter().cloned());
    (!options.is_empty()).then(|| format!("{}\n", options.join(" ")))
}

/// The word every ARMv7-A image carries: `ferrix.unisolated`. ARMv7-A
/// drives no IOMMU, so devmgr would start none of its drivers without it
/// (`docs/DMA-GATE.md`); the customer took ARMv7-A out of the claim
/// (2026-10-24, G4 "(a), take it off"), and its rows are development rows,
/// whose boots carry the word and no claim.
pub(crate) const ARMV7A_DEVELOPMENT_WORD: &str = "ferrix.unisolated";

/// [`image_cmdline`] for an image of `arch`: on ARMv7-A with
/// [`ARMV7A_DEVELOPMENT_WORD`] after it, and on AArch64 too when
/// `FERRIX_ARM_MACHINE` asks for `acpi=off`: the device-tree path brings no
/// SMMU up, so no device's DMA is translated there (`qemu.rs`'s
/// `fault_problem`), and it is the Pixel 7's path, a development one.
pub(crate) fn arch_cmdline(arch: Arch, args: &Args) -> Option<String> {
    let line = image_cmdline(args);
    let without_acpi = std::env::var("FERRIX_ARM_MACHINE").is_ok_and(|m| m.contains("acpi=off"));
    if arch != Arch::Armv7a && !(arch == Arch::AArch64 && without_acpi) {
        return line;
    }
    Some(match line {
        Some(line) => format!("{} {ARMV7A_DEVELOPMENT_WORD}\n", line.trim_end()),
        None => format!("{ARMV7A_DEVELOPMENT_WORD}\n"),
    })
}

/// Compile both halves for `arch` and assemble the bootable image.
///
/// The kernel ELF comes back beside the image, because it is what a panic
/// report's backtrace is resolved against: the image holds the same kernel
/// with nothing to look a symbol up in.
///
/// Given a program — `--init`, or `FERRIX_INIT` when that is absent — the
/// kernel is built with it to start `sh -i`, and the initramfs carries it at
/// `/bin/busybox` with a link beside it for every applet, so that the shell's
/// `PATH=/bin` finds `ls` where a person types it; uutils/coreutils rides
/// along at `/bin/coreutils`, its own names linked in `/usr/bin`, on the one
/// architecture it is built for. Without a program the image is the one it
/// always was. Either way the initramfs carries the tree's native
/// programs in `/sbin`, built and checked for `arch` first.
pub(crate) fn build_image(arch: Arch, args: &Args) -> Result<(PathBuf, PathBuf)> {
    let natives = native::build(arch, args.release)?;
    // Only the apps `--app` names: this is test-boot's image too, which
    // carries no app it was not asked for (`docs/APPS.md` §5).
    let mut service: Vec<ports::File> = apps::named(arch, args)?;
    if args.installer {
        service.extend(installer::files(arch)?.into_iter().flatten());
    }
    if args.adbd {
        service.extend(adbd::file(arch)?);
    }
    let Some(program) = optional_program(arch, args)? else {
        let (loader, kernel) = build_halves(arch, args)?;
        let mut links = rustc::default_links(args);
        links.extend(service);
        let cmdline = arch_cmdline(arch, args);
        let image = if links.is_empty() {
            fat::write_image(arch, &loader, &kernel, &natives, cmdline.as_deref())?
        } else {
            let archive = initramfs::build(None, &natives, None, &links)?;
            fat::write_image_with(arch, &loader, &kernel, &archive, cmdline.as_deref())?
        };
        return Ok((image, kernel.elf));
    };
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &program, "")?;
    let shell = zinc::build(arch)?;
    let utilities = uutils::carried(arch)?;
    let mut ports = apps::ported(arch, args)?;
    ports.extend(rustc::default_links(args));
    ports.extend(service);
    // The DHCP client `/etc/profile` takes the shell's lease with.
    ports.extend(dhcp::carried(arch)?);
    // `dmesg`, in busybox's place.
    ports.extend(dmesg::carried(arch)?);
    let initramfs = initramfs::build_with_utilities(
        Some(&program),
        &natives,
        shell.as_deref(),
        &utilities,
        &ports,
    )?;
    let cmdline = arch_cmdline(arch, args);
    let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, cmdline.as_deref())?;
    Ok((image, kernel.elf))
}

/// The image `run` boots when no program is named: a kernel with nothing in
/// it, and an initramfs with `/sbin/init`, its units, zinc for the getty,
/// and the utilities and ports a shell wants, with `ferrix.init=/sbin/init`
/// on the command line.
fn build_init_image(arch: Arch, args: &Args) -> Result<(PathBuf, PathBuf)> {
    let natives = native::build(arch, args.release)?;
    let (loader, kernel) = build_halves(arch, args)?;
    let shell = zinc::build(arch)?;
    if shell.is_none() {
        return Err(Error::new(format!(
            "zinc is not built for {arch}, so init's getty would have no shell: name one with --init"
        )));
    }
    let utilities = uutils::carried(arch)?;
    let mut carried = rustc::default_links(args);
    carried.extend(init::carried(arch)?);
    // The package manager, which is the system's, not an app.
    carried.extend(pkg::carried(arch)?);
    // `--auth-seed`: authd, its seeds and the accounts it needs.
    carried.extend(auth::with_seeds(arch, args)?);
    // The apps, each installed from its package (`docs/APPS.md` §5).
    carried.extend(apps::installed(arch, args)?);
    // `dmesg`, in busybox's place.
    carried.extend(dmesg::carried(arch)?);
    let initramfs =
        initramfs::build_with_utilities(None, &natives, shell.as_deref(), &utilities, &carried)?;
    let mut cmdline = init::command_line();
    if let Some(extra) = arch_cmdline(arch, args) {
        cmdline = format!("{} {extra}", cmdline.trim_end());
    }
    let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, Some(&cmdline))?;
    Ok((image, kernel.elf))
}

/// Compile what `flash` copies onto a board: the loader, the kernel and the
/// initramfs.
///
/// Given a program the kernel starts `sh -i` and the archive carries busybox,
/// exactly as [`build_image`] arranges for QEMU, so a card boots to the shell
/// an image does. Without one the files are the ones they always were. Either
/// way the archive carries the tree's native programs, as an image's does.
pub(crate) fn build_board_files(arch: Arch, args: &Args) -> Result<flash::BoardFiles> {
    // `--compositor`: the desktop instead, which `compositor` knows how to make.
    if args.compositor {
        return compositor::board_files(arch, args);
    }
    let natives = native::build(arch, args.release)?;
    let Some(program) = optional_program(arch, args)? else {
        let (loader, kernel) = build_halves(arch, args)?;
        return Ok(flash::BoardFiles {
            loader,
            kernel,
            initramfs: initramfs::build(None, &natives, None, &[])?,
            defaults: board_defaults(arch),
        });
    };
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &program, "")?;
    let shell = zinc::build(arch)?;
    let utilities = uutils::carried(arch)?;
    let initramfs = initramfs::build_with_utilities(
        Some(&program),
        &natives,
        shell.as_deref(),
        &utilities,
        &apps::ported(arch, args)?,
    )?;
    Ok(flash::BoardFiles {
        loader,
        kernel,
        initramfs,
        defaults: board_defaults(arch),
    })
}

/// A board card's `FERRIX/DEFAULTS.TXT`: on ARMv7-A (the DK1)
/// [`ARMV7A_DEVELOPMENT_WORD`], as every ARMv7-A image carries it.
fn board_defaults(arch: Arch) -> Option<&'static str> {
    (arch == Arch::Armv7a).then_some("ferrix.unisolated\n")
}

/// The program `init` names for `arch`, with `{arch}` replaced by its name,
/// refused unless it is a file.
///
/// `ferrousli` is not a path: it names the busybox `cargo xtask busybox`
/// installs, built first when it is missing or older than ferrousli. Nor is
/// `blank`, the compositor's first program, which is built here.
pub(crate) fn program_for(init: &str, arch: Arch) -> Result<PathBuf> {
    if init == busybox::INIT_NAME {
        return busybox::program(arch);
    }
    if init == display::INIT_NAME {
        return display::build_blank(arch, false);
    }
    let program = PathBuf::from(init.replace("{arch}", arch.name()));
    if !program.is_file() {
        return Err(Error::new(format!(
            "no program at {} for {arch}",
            program.display()
        )));
    }
    Ok(program)
}

/// The program `build`, `run` and `test-boot` were given, if any: `--init`,
/// or else a non-empty `FERRIX_INIT`, the variable that has always chosen the
/// shell those commands embed.
pub(crate) fn optional_program(arch: Arch, args: &Args) -> Result<Option<PathBuf>> {
    let init = match (&args.init, std::env::var("FERRIX_INIT")) {
        (Some(init), _) => init.clone(),
        (None, Ok(init)) if !init.is_empty() => init,
        (None, Ok(_) | Err(std::env::VarError::NotPresent)) => return Ok(None),
        (None, Err(std::env::VarError::NotUnicode(_))) => {
            return Err(Error::new("FERRIX_INIT is not valid UTF-8"));
        }
    };
    program_for(&init, arch).map(Some)
}

/// Compile both halves for `arch`, without assembling an image.
///
/// A board has its own filesystem already, put there by the vendor's firmware,
/// so what it wants is the two files rather than something to write over the
/// card with.
/// What [`build_image`] assembles for a boot without a program, before it
/// is assembled: `test-powerfail` writes one image per boot from them, each
/// with its own command line.
fn build_parts(arch: Arch, args: &Args) -> Result<powerfail::Built> {
    let natives = native::build(arch, args.release)?;
    let (loader, kernel) = build_halves(arch, args)?;
    let initramfs = initramfs::build(None, &natives, None, &[])?;
    Ok(powerfail::Built {
        loader,
        kernel,
        initramfs,
    })
}

fn build_halves(arch: Arch, args: &Args) -> Result<(PathBuf, cargo::Kernel)> {
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    Ok((loader, kernel))
}
