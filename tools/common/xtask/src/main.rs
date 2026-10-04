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
//! cargo xtask test-procfs --arch all [--i686] [--timeout SECONDS]
//! cargo xtask test-uvm  [--arch x86_64] [--timeout SECONDS]
//! cargo xtask test-nvrm [--arch x86_64] [--accel kvm|tcg] [--timeout SECONDS]
//! cargo xtask test-nvrm-link [--arch x86_64] [--accel kvm|tcg] [--timeout SECONDS]
//! cargo xtask run-nvidia [--timeout SECONDS]
//! cargo xtask test-rustc [--accel kvm] [--memory M] [--timeout SECONDS]
//! cargo xtask test-selfhost [--accel kvm] [--release] [--smp N] [--memory M] [--timeout SECONDS] [--plan DIR]
//! cargo xtask test-selfhost --arch x86_64|aarch64 --volume IMG [--release]
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

mod adbd;
mod apps;
mod args;
mod audio;
mod auth;
mod badapple;
mod btrfs_check;
mod btrfs_disk;
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
mod compositor;
mod console;
mod coverage;
mod display;
mod dma_faults;
mod dotfiles;
mod edid;
mod everything;
mod fat;
mod ferrousli;
mod flash;
mod fuzzel;
mod gate_rows;
mod gateway;
mod init;
mod init_file;
mod initramfs;
mod input;
mod installer;
mod ipc;
mod jobs;
mod kaslr;
mod keyboard;
mod native;
mod net;
mod noise;
mod nvidia;
mod nvrm;
mod nvrm_link;
mod omz;
mod orphans;
mod parallel;
mod paths;
mod pe;
mod persistent;
mod pkg;
mod ports;
mod powerfail;
mod procfs;
mod pty;
mod qemu;
mod remote;
mod restart;
mod rustc;
mod seam;
mod seat;
mod selfhost;
mod sem;
mod serial;
mod session;
mod sha256;
mod shell;
mod shm;
mod ssh;
mod start_page;
mod steam;
mod steamcmd;
mod symbolize;
mod sysfs;
mod test_disk;
mod threads;
mod uboot_env;
mod uefi_vars;
mod uutils;
mod uvm;
mod vfs;
mod vnc;
mod wallpaper;
mod waybar;
mod window;
mod workspace;
mod wsl;
mod yserver;
mod zinc;

use std::path::PathBuf;
use std::process::ExitCode;

use args::Args;
use paths::Arch;

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

const USAGE: &str = "\
Ferrix build driver

USAGE:
    cargo xtask <COMMAND> [OPTIONS]

COMMANDS:
    build         Compile the loader and kernel and write a bootable image
    run           Boot the image under QEMU, attached to the terminal
    run-compositor  Boot src/user/system/linux/compositor/hyprix as init with a virtio-gpu, on a screen this host can show
    run-badapple  Boot the badapple app's player as init: all of Bad Apple!! in a window, heard on this host's sound server (tools/common/fetch/fetch-badapple.sh first)
    remote-desktop  Send this tree to another machine, boot the desktop there and watch it here over VNC
    wallpapers    Convert pictures for run-compositor's desktop and keep them on this machine
    everything-volume  Make run-compositor --everything's volume from the fetched ones (on Windows, in WSL)
    test-btrfs    Boot, write a tree on the blank btrfs disk, and require host btrfs check to find nothing
    test-powerfail  Kill QEMU while it writes btrfs, replay at the next boot, and require btrfs check to pass, --seeds times
    test-boot     Boot the image under QEMU and assert the kernel came up, moved (KASLR) unless built
                  --mitigations off or told nokaslr
    test-kaslr    Boot the image twice and require the loader to have put the kernel somewhere new
    test-shell    Boot with a static busybox built in and require its script's output; again with it
                  started from a file by ferrix.init=; under busybox, require reboot(2) to commit /data
    test-apps     Boot once with every app in src/user/apps that has [[smoke]] checks, and require each
                  check's line
    test-pkg      Boot with pkg, the package manager, and require it to install, run and remove the stat
                  service, and to refuse a changed package, a missing dependency and a needed removal
    test-vfs      Boot with busybox in the initramfs and require stage 8's exit programs and applets
    test-net      Boot with a network device and require busybox to configure it and fetch a file
    test-clipboard  Boot the desktop with its clipboard port on a socket xtask speaks vdagent over, and carry text both ways
    test-adb      Boot adbd with a network, and drive it with this machine's adb: shell, push, pull, forward, reboot
    test-display  Boot src/user/system/linux/compositor/blank as init with a virtio-gpu, and require its colour on every pixel
    test-compositor  Boot src/user/system/linux/compositor/hyprix as init with a virtio-gpu, and require its background on every pixel
    test-video    Boot a wallpaper that moves and require the screen to show its frames in turn
    test-input    Boot src/user/system/linux/compositor/evecho as init with virtio-input, send a key and a touch over QMP, and require them back
    test-audio    Boot src/user/system/linux/compositor/tone as init with virtio-snd, play a second of a counter, and require every frame back from QEMU's wav file
    test-badapple Boot the badapple app's player as init, play 30 s of Bad Apple!!, and require the held frame on the screen, the song in QEMU's wav file, and the two in step
    test-seat     Boot the compositor with a client, type into it over QMP, and require the key and the keybind to land
    test-pty      Boot the term app as init, run a program on a pseudoterminal, and require its output back
    test-foot     Boot the compositor with foot, the ported Wayland terminal, and require its font and its text on screen
    test-vkgears  Boot the compositor with vkgears and the Venus card, and require it drew frames on the host's GPU (Linux hosts)
    test-jobs     Boot an interactive shell on the console, type a session with jobs at it, and require the answers
    test-init     Boot /sbin/init as pid 1, type at the shell its getty gives, and require its session, a failing
                  service's restart budget, a service's cgroup, and a shutdown btrfs check finds clean
    bench-ipc     Boot --init's shell and time a channel round trip between two native processes
                  (/sbin/ipc-bench): the floor of a native call and the trip, in nanoseconds
    bench-seam    Boot a stock Linux kernel on the same QEMU machine and time a 4 KiB O_DIRECT read of the pattern disk at depths 1 and 32: the in-kernel reference for the seam boot line (tools/common/fetch/fetch-linux-reference.sh first)
    test-auth     Boot init with authd, type at the shell its getty gives, and require each refusal of
                  docs/AUTH.md: a wrong password, an unknown account, a user naming another, the throttle
                  (with --sabotage NAME, against an authd with that refusal turned off, which must fail)
    test-restart  Boot a shell beside a device, kill -9 its driver twice, and require it started again each time (--boot gpu|input|net|blk|all; gpu if not given)
    test-install  Install the live image on a blank disk with ferrix-install, then boot that disk alone (x86_64)
    test-sysfs    Boot a shell beside a card, input devices and a network adapter, read sysfs, and unbind and bind the card through it
    test-threads  Boot threads-test as init and require std::thread, Mutex, mpsc and /proc's thread count (--i686: the x86-64
                  image runs a 32-bit x86 build of it)
    test-sem      Boot sem-test as init and require System V semaphores across forks: a SEM_UNDO mutex, undo at a kill,
                  EINTR, EIDRM and timeouts (--i686: 32-bit x86, through ipc(117), as Steam calls them)
    test-shm      Boot shm-test as init and require System V shared memory as Chromium's MIT-SHM uses it: a segment
                  attached by two processes, removed while attached, gone at the last detach (--i686: 32-bit x86, through
                  ipc(117) and 395-398)
    test-procfs   Boot procfs-test as init and require /proc/self/fd links to stat as fstat (sockets, anonymous files, a pipe,
                  a memfd), /proc/net/tcp's inode to match, and every /proc inode number to fit 32 bits, and mincore (--i686: 32-bit x86)
    test-uvm      Build NVIDIA's UVM against uvm-kpi and ferrousli from the tree tools/common/fetch/fetch-nvidia.sh fetches
                  (or FERRIX_NVIDIA_SRC's), boot it as init, and require its 15 GPU-free self-tests to pass; then a krealloc
                  control that must fail one (x86_64)
    test-nvrm     Build nvrm and its core, put the core on a volume, and boot nvrm as devmgr's Gpu driver on QEMU's
                  pci-testdev: handed over (marked, interrupts isolated, budget set, the core loaded, nvrm up with its
                  device), and refused for a budget too small
                  and for interrupts not isolated (x86_64)
    test-nvrm-link
                  Build nvrm-link-test and its core from fetch-nvidia.sh's objects, run RM's no-GPU path (init,
                  root client, NV01_DEVICE_0 refused) on the host and on Ferrix from a volume, and each control
                  the core's loader must refuse (x86_64)
    run-nvidia    Boot Ferrix on the RTX 3060 in libvirt's ferrix-3060 domain (the patched QEMU as root, the sharing
                  domains shut off), nvrm as its driver with its core and the GSP firmware on the NVIDIA volume;
                  print the serial port until nvrm is up or stops, then destroy the domain (x86_64)
    test-rustc    Attach the rustc volume tools/common/fetch/fetch-rustc-sysroot.sh makes, run `rustc hello.rs && ./hello`
    test-chrome   Attach the volume tools/common/fetch/fetch-chrome.sh makes, and require headless Chrome to run a page's script and draw it
    test-claude-code  Attach the volume tools/common/fetch/fetch-claude-code.sh makes, and require Claude Code to start and,
                  against a stub Messages API xtask serves, run a Bash command on Ferrix for its model
                  (--everything: as `claude` on run-compositor --everything's merged volume, on ferrousli's
                  loader and libc.so.6 as that desktop runs it, unless --interpreter glibc)
    test-steamcmd Attach the volume tools/common/fetch/fetch-steamcmd.sh makes, and require Valve's 32-bit steamcmd to update itself
                  and log in to Steam anonymously, over the network
    test-steam-bootstrap  Attach the volume tools/common/fetch/fetch-steam.sh makes, and require Valve's steam.sh to update the
                  client and run it until it asks for an X display, over the network
    test-bwrap    Boot with / in memory and require Debian's bubblewrap, which tools/common/fetch/fetch-bwrap.sh fetches,
                  to run Steam's requirements check and a pressure-vessel-shaped container as root (docs/NAMESPACES.md §8)
    test-steam-window  Attach the volume tools/common/fetch/fetch-steam-window.sh makes, and require Steam's sign-in window
                  on hyprix, drawn through yserver, over the network (docs/STEAM.md)
    run-steam     The same boot, the screen dumped into build/x86_64/steam/ every few seconds until the timeout
    test-steam-store  Steam as run-compositor --everything starts it, its 64-bit side on ferrousli: require its sign-in
                  window, then, with the test account in ~/.config/ferrix/steam-test-account (FERRIX_STEAM_ACCOUNT_FILE),
                  sign in and require its store on the screen; without the file the store step is skipped (docs/STEAM.md)
    test-steam-game  The same, then require Teeworlds installed from Steam, started by it and drawing in its window;
                  needs the account file
    test-yserver  Attach the volume tools/common/fetch/fetch-yserver.sh makes, start yserver headless on lavapipe and require xdpyinfo
                  to reach it
    test-xwindow  The same volume, yserver as a client of the compositor: its root must be the screen's size, and xev's window
                  one of the compositor's, by title and class and on the screen, the server started as
                  run-compositor --everything starts it (--everything: on that desktop's merged volume)
    test-chrome-window  The same volume, and require Chrome in a window on the compositor, its page on the screen
    test-chrome-audio   The same window on a page playing 440 Hz, and require the tone in QEMU's wav file of the virtio-snd card
    bench-chrome  Chrome in a window, left alone, scrolled and pointed at: processor time, frames and memory per phase
    bench-chrome-video  Chrome playing a video with its sound: processor time, frames shown and dropped, underruns, and gaps in what the card played
    test-selfhost  Run `cargo xtask build` on Ferrix from that toolchain and this checkout, and boot the image it made;
                  with --plan DIR, have Ferrix make every build a FERRIX_BUILDS=record:DIR run wrote down;
                  with --volume IMG, judge a volume Ferrix built on elsewhere (the Pixel 7's selfhost.sh):
                  btrfs check, then boot the image it holds
    builds-execute  Make every build in --plan DIR here, keeping the outputs in DIR/store (see tools/common/xtask/src/builds.rs)
    coverage      Run every boot gate under QEMU's drcov plugin (FERRIX_DRCOV) with --init, and
                  require the certified item's statement coverage to hold its recorded floor
    check         Run every quality gate (fmt, clippy, layering, audits, tests, docs)
    host-clippy   check's host clippy step alone, as CI runs it
    host-test     check's host test step alone, as CI runs it
    host-doctest  check's doc test step alone, as CI runs it
    host-doc      check's documentation step alone, as CI runs it
    check-docs    check's commit hooks, audits and generated-document checks alone, no cargo
                  step: the gate of a change to docs/ and top-level Markdown only
    gate-rows --since <ref> [--moved <old main>]
                  The gate rows docs/BACKLOG.md asks of the changes since <ref>; with
                  --moved, whether a rebase from <old main> asks for a gate again
    check-ferrousli
                  check --ferrousli's ferrousli steps alone, as CI runs them
    miri          CI's Miri steps alone, --jobs crates at a time, building nothing else
                  (needs a nightly toolchain with miri)
    loom          check's loom models alone (src/tests/loom): the wait's and the way out's
                  orderings, each with a control that must fail
    native-clippy check's clippy of the native programs for --arch, as CI runs it
    model-doc     Regenerate docs/generated/ from the SysML model
    busybox       Build busybox against ferrousli (x86_64) for --init ferrousli
    uutils        Build uutils/coreutils against ferrousli (x86_64), the utilities replacing busybox's
    ports         Build the libraries ported onto ferrousli that apps build against (x86_64: libcxx, zlib; Arm: zlib); the ported programs are apps
    apps          List the apps in src/user/apps, each checked against its app.toml (docs/APPS.md)
    check-apps    check's steps for the apps alone: each app's formatting, clippy, tests and folder
    build-apps    Build every app's package, or --app's, for --arch: scripts too, which run and
                  run-compositor never start, and take the last of
    new-app       Write a new app's folder: --app NAME, --abi native (the default) or linux
    components    The parts of the tree in repositories of their own (components.toml): each checkout against its pin
    pin-components  Pin every clean component checkout that moved past its pin, once its commit is pushed
    flash         Copy the loader and kernel onto a board's boot partition
    watch-serial  Watch a real serial port for the kernel's boot report
    deploy        flash, then watch-serial: one command for a board

OPTIONS:
    --arch <x86_64|aarch64|armv7a|all>   Target architecture   [default: host]
    --release                            Build with optimisations
    --iterate                            With --release: the kernel with thin LTO, relinked in
                                         seconds; for working on it, never for a gate
                                         (docs/TEST-TIME.md, C4)
    --strip-kernel                       Images carry the kernel without its debug information, as
                                         flash writes it; the loader reads the whole file into
                                         memory, so a small --memory needs it [default: off]
    --mitigations <on|off>              The kernel's side-channel defences [default: on, the
                                         certified setting]. off builds with
                                         --cfg ferrix_mitigations_off into target/mitigations-off:
                                         no index clamps, no speculation controls, no barriers
                                         (docs/certification/SPECULATION.md)
    --smp <N>                            Virtual CPUs          [default: 4; 1 under whpx]
    --memory <MiB>                       Guest memory          [default: 512]
    --timeout <SECONDS>                  test-boot patience    [default: 120]
    --seeds <N>                          test-powerfail cuts   [default: 8]
    --accel <auto|tcg|whpx|kvm|hvf>      QEMU accelerator      [default: auto for run
                                         without --gdb, tcg otherwise]
    --gdb                                Wait for a debugger on :1234, with nokaslr in CMDLINE.TXT so the
                                         kernel runs where its ELF's symbols say
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
    --everything                         run-compositor: all of it at once -- --gl, --release,
                                         --clipboard, --chrome, and rustc and cargo in the shell,
                                         from one volume made of the rustc and Chrome ones, with
                                         claude too once fetch-claude-code.sh has run; and
                                         this machine's ~/.config/hypr/hyprland.conf with its
                                         dotfiles unless --config or --no-dotfiles says otherwise
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
    --scale <N>                          run-compositor, flash --compositor: the monitor's scale, 1 by default
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
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nxtask: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `build`: an image for each architecture asked for.
fn build(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let (image, _) = build_image(arch, args)?;
        println!("built {}", image.display());
    }
    Ok(())
}

/// `builds-execute`: carry out the plan in `--plan DIR`.
fn builds_execute(args: &Args) -> Result<()> {
    let plan = args
        .plan
        .as_deref()
        .ok_or_else(|| Error::new("builds-execute needs --plan DIR"))?;
    builds::execute(std::path::Path::new(plan)).map(|_| ())
}

/// The command `args` name, with the parts of the tree that live in
/// repositories of their own at their pins first: before anything reaches
/// for them, and before `--arch all` starts children that would each clone
/// them (`crate::components`).
fn command_of(args: &Args) -> Result<&str> {
    let Some(command) = args.command.as_deref() else {
        println!("{USAGE}");
        return Err(Error::new("no command given"));
    };
    components::ensure()?;
    Ok(command)
}

fn run() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    // Every kernel and image this run builds, whichever command builds it:
    // set once, here, rather than threaded through each of them.
    cargo::set_kernel_build(args.mitigations, args.iterate, args.release)?;
    fat::set_strip_kernel(args.strip_kernel);

    if args.help {
        println!("{USAGE}");
        return Ok(());
    }

    let command = command_of(&args)?;

    // `--arch all` of a command whose architectures are independent: one
    // child an architecture, all at once (`crate::parallel`).
    if parallel::wanted(command, &args) {
        return parallel::run(command, &args);
    }

    match command {
        "build" => build(&args),
        "run" => run_machine(args),
        "test-btrfs" => btrfs_check::test_btrfs(&args, |arch| build_image(arch, &args)),
        "test-powerfail" => powerfail::test_powerfail(&args, |arch| build_parts(arch, &args)),
        "test-boot" => test_boot(&args),
        "test-kaslr" => test_kaslr(&args),
        "test-shell" => test_shell(&args),
        "test-vfs" => test_vfs(&args),
        "test-net" => test_net(&args),
        "test-adb" => adbd::test_adb(&args, program_for),
        #[cfg(unix)]
        "test-clipboard" => clipboard::test_clipboard(&args),
        #[cfg(not(unix))]
        "test-clipboard" => Err(Error::new(
            "test-clipboard serves the clipboard port on a Unix socket: run it on the Linux host",
        )),
        "test-display" => display::test_display(&args),
        "run-compositor" => compositor::run_compositor(&args),
        "run-badapple" => badapple::run_badapple(&args),
        "remote-desktop" => remote::remote_desktop(&args),
        "wallpapers" => wallpaper::import(&args),
        "everything-volume" => everything::volume().map(drop),
        "test-compositor" => compositor::test_compositor(&args),
        "test-video" => compositor::test_video(&args),
        "test-foot" => compositor::test_foot(&args),
        "test-vkgears" => compositor::test_vkgears(&args),
        "test-input" => input::test_input(&args),
        "test-audio" => audio::test_audio(&args),
        "test-badapple" => badapple::test_badapple(&args),
        "test-seat" => seat::test_seat(&args),
        "test-pty" => pty::test_pty(&args),
        "test-jobs" => jobs::test_jobs(&args),
        "test-init" => init::test_init(&args),
        "test-auth" => auth::test_auth(&args),
        "test-restart" => restart::test_restart(&args),
        "bench-seam" | "bench-ipc" => bench(command, &args),
        "test-sysfs" => sysfs::test_sysfs(&args),
        "test-install" => installer::test_install(&args),
        "test-threads" | "test-sem" | "test-shm" => sem::run(command, &args),
        "test-procfs" | "test-uvm" | "test-nvrm" | "test-nvrm-link" => sem::run(command, &args),
        "test-apps" => apps::test_apps(&args),
        "test-pkg" => pkg::test_pkg(&args),
        "coverage" => coverage::run(&args),
        "test-rustc" => rustc::test_rustc(&args),
        "test-chrome" | "test-chrome-window" | "test-chrome-audio" => chrome::run(command, &args),
        "test-claude-code" => claude_code::test_claude_code(&args),
        "test-steamcmd" | "test-steam-bootstrap" => steamcmd::run(command, &args),
        "test-bwrap" => bwrap::test_bwrap(&args),
        "test-yserver" | "test-xwindow" => yserver::run(command, &args),
        "run-nvidia" => nvidia::run_nvidia(&args),
        "run-steam" => compositor::steam_window::run(&args, false),
        "test-steam-window" => compositor::steam_window::run(&args, true),
        "test-steam-store" | "test-steam-game" => compositor::test_steam_store(command, &args),
        "bench-chrome" => compositor::bench_chrome(&args),
        "bench-chrome-video" => compositor::bench_chrome_video(&args),
        "test-selfhost" => selfhost::test_selfhost(&args),
        "builds-execute" => builds_execute(&args),
        name if check::COMMANDS.contains(&name) => check::command(command, &args),
        "native-clippy" => args
            .arches()?
            .into_iter()
            .try_for_each(check::native_clippy),
        "model-doc" => check::model_doc(),
        "busybox" => busybox::build(args.single_arch()?).map(|_| ()),
        "uutils" => uutils::build(args.single_arch()?).map(|_| ()),
        "ports" => ports::build(args.single_arch()?),
        "apps" => apps::list(),
        "build-apps" => apps::build_apps(&args),
        "new-app" => apps::new_app(&args),
        "omz" => omz::install(args.from.as_deref()),
        "zsh-functions" => omz::install_functions(args.from.as_deref()),
        "flash" => {
            let arch = args.single_arch()?;
            let files = build_board_files(arch, &args)?;
            flash::run(arch, &files, &args)
        }
        "watch-serial" => serial::watch(None, &args),
        // The whole of a board round-trip. Separate commands exist because
        // each half is useful alone — reflashing without watching, watching a
        // board someone else reset — but the common case is both, and a
        // command per step is a command per step to forget.
        "deploy" => {
            let arch = args.single_arch()?;
            let files = build_board_files(arch, &args)?;
            flash::run(arch, &files, &args)?;
            serial::watch(Some(&files.kernel), &args)
        }
        other => Err(Error::new(format!("unknown command `{other}`\n\n{USAGE}"))),
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

fn test_boot(args: &Args) -> Result<()> {
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
        let initramfs = initramfs::build_with_utilities(
            Some(&program),
            &natives,
            shell.as_deref(),
            &utilities,
            &apps::ported(arch, args)?,
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
        let ports = apps::ported(arch, &args)?;
        let curl = ports.iter().any(|file| file.path == "bin/curl");
        let git = ports.iter().any(|file| file.path == "usr/bin/git");
        let programs = net::commands(&servers, curl, git);
        let commands = vfs::encode(&programs)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let list = paths::build_dir(arch).join("net-commands");
        vfs::write_if_changed(&list, &commands)?;
        let kernel = cargo::build_kernel_with_commands(arch, args.release, &list)?;
        let natives = native::build(arch, args.release)?;
        let initramfs = initramfs::build(Some(&program), &natives, None, &ports)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        if let Err(error) = qemu::test_net(arch, &image, &kernel, &programs, &args) {
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
fn image_cmdline(args: &Args) -> Option<String> {
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
fn build_image(arch: Arch, args: &Args) -> Result<(PathBuf, PathBuf)> {
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
        let cmdline = image_cmdline(args);
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
    let initramfs = initramfs::build_with_utilities(
        Some(&program),
        &natives,
        shell.as_deref(),
        &utilities,
        &ports,
    )?;
    let cmdline = image_cmdline(args);
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
    let initramfs =
        initramfs::build_with_utilities(None, &natives, shell.as_deref(), &utilities, &carried)?;
    let mut cmdline = init::command_line();
    if let Some(extra) = image_cmdline(args) {
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
fn build_board_files(arch: Arch, args: &Args) -> Result<flash::BoardFiles> {
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
            defaults: None,
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
        defaults: None,
    })
}

/// The boot benchmarks: the seam's disk read against Linux's (`bench-seam`),
/// and a channel round trip between two native processes (`bench-ipc`).
fn bench(command: &str, args: &Args) -> Result<()> {
    if command == "bench-ipc" {
        ipc::bench_ipc(args)
    } else {
        seam::bench_seam(args)
    }
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
