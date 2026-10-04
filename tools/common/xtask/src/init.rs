//! `/sbin/init` for the image, and `test-init` (`docs/INIT.md` §15).
//!
//! # Building it
//!
//! `src/user/system/linux/init/` is a workspace of its own, built as zinc is: a static program
//! against the target's own musl, linked by rust-lld, so any host builds it.
//! [`carried`] returns what an image carries for it: `/sbin/init`,
//! `/sbin/getty`, the getty generator in `/lib/ferrix/generators`, and the
//! units of `src/user/system/linux/init/units` in `/lib/ferrix/units`, with `default.target` a link
//! to `multi-user.target`.
//!
//! Only images that name `/sbin/init` carry it. With nothing named and
//! nothing built in, the kernel starts `/sbin/init` (§8.1), so an image with
//! no program of its own -- `test-boot`'s, `test-btrfs`'s -- would boot into
//! a manager that never ends, where today it ends at the marker.
//!
//! # The test
//!
//! One boot per architecture, of a kernel with no program in it,
//! `ferrix.init=/sbin/init` on the command line, zinc at `/bin/sh`, the
//! test's own units in `/etc/ferrix/units`, and a fresh blank btrfs volume at
//! `/data`. It types at the console the getty gives, as `test-jobs` does, and
//! everything it judges is either the manager's own lines or a line the
//! guest printed in answer to what was typed. A marker is built from a shell
//! variable (`echo "$m-self"`), because the console echoes the typed line:
//! the echo holds `$m-self`, and only the answer holds `stat-self`.
//!
//! **Stage one, boot and a terminal.** `multi-user.target` becomes active
//! and the getty on the console prints its banner. The shell reads its own
//! `/proc/self/stat`: it must be pid 1's child, lead its own session and
//! process group, and have the console (5:1) as its controlling terminal
//! with its own group in the foreground -- read from the kernel, not taken
//! from what was typed. `flaky.service` exits 1 each time it is started; it
//! must be restarted by `Restart=on-failure` and then reported
//! `failed (start-limit-hit)` once its budget of three is spent. Then
//! `kill -TERM 1` from the prompt: every unit stops, `multi-user.target`
//! before `getty@console.service` before `basic.target` before
//! `sysinit.target`, and the machine powers off through `reboot(2)`. `btrfs
//! check` of the volume must then find it clean.
//!
//! **Stage two, groups.** `forker.service` is a oneshot that remains after
//! its main process exits, having left a grandchild behind that writes its
//! own pid to a file and blocks for good. That pid must be in the service's
//! `cgroup.procs`. It is `BindsTo=anchor.service`, whose main process writes
//! its pid too; killing that process from the prompt makes the manager stop
//! `forker.service`, and stopping it must end the grandchild. The negative
//! control is `KillMode=process` on `forker.service`, which leaves the
//! grandchild alive and fails the last check.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::btrfs_check::Checker;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::{self, SUCCESS_MARKER, Watching};
use crate::{Error, Result, btrfs_disk, cargo, fat, initramfs, native, zinc};

/// Where the init is, and what `ferrix.init=` names.
pub(crate) const PATH: &str = "/sbin/init";

/// The shipped units, beside this module in the tree.
const UNITS: &str = "src/user/system/linux/init/units";

/// How long to wait for the answer to one line typed at the prompt.
const PATIENCE: Duration = Duration::from_secs(30);

/// How long to wait between the checks that retry.
const POLL: Duration = Duration::from_millis(500);

/// How many times the grandchild's end is looked for, [`POLL`] apart: half a
/// minute in all, as it has always been given.
const GONE_TRIES: usize = 60;

/// The volume the boot writes and `btrfs check` reads, under `build/`.
const VOLUME: &str = "init-data.img";

/// What the getty prints once it holds the terminal: `Ferrix <host> on
/// /dev/console`.
const BANNER: &str = " on /dev/console";

/// What the kernel says as `reboot(2)` powers off, Linux's line.
const POWER_DOWN: &str = "reboot: Power down";

/// The console's device number as `stat`'s `tty_nr` encodes it: 5:1.
const CONSOLE_TTY_NR: u32 = 5 << 8 | 1;

/// What init says after each read-only remount of §8.2's step 2 that a write
/// then found refused with `EROFS`: `/`, the root volume, and `/data`
/// (finding F-53, which the kernel's `MS_REMOUNT` closed).
const READ_ONLY_AT_SHUTDOWN: [&str; 2] = ["init     / is read-only", "init     /data is read-only"];

/// The units shutdown must stop, in this order (§15, stage one).
const STOP_ORDER: [&str; 4] = [
    "multi-user.target: stopped",
    "getty@console.service: stopped",
    "basic.target: stopped",
    "sysinit.target: stopped",
];

/// The test's own units, in `/etc/ferrix/units`.
const TEST_UNITS: &[(&str, &str)] = &[
    (
        "flaky.service",
        "[Unit]\n\
         Description=Fails every time, for its restart budget\n\
         StartLimitBurst=3\n\
         StartLimitIntervalSec=60s\n\
         \n\
         [Service]\n\
         ExecStart=/bin/sh -c \"exit 1\"\n\
         Restart=on-failure\n\
         RestartSec=100ms\n",
    ),
    (
        "anchor.service",
        "[Unit]\n\
         Description=Blocks for good, and says its pid\n\
         \n\
         [Service]\n\
         ExecStart=:/bin/sh -c 'echo $$ > /run/anchor.pid; read x < /dev/ptmx'\n",
    ),
    (
        "forker.service",
        "[Unit]\n\
         Description=Leaves a grandchild behind, bound to anchor.service\n\
         BindsTo=anchor.service\n\
         After=anchor.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=:/bin/sh -c \"(sh -c 'echo $$ > /run/forker.pid; read x < /dev/ptmx' &)\"\n",
    ),
    (
        "echoer.service",
        "[Unit]\n\
         Description=Says its pid, then blocks\n\
         \n\
         [Service]\n\
         ExecStart=:/bin/sh -c 'echo \"echoer-up $$\"; read x < /dev/ptmx'\n",
    ),
    (
        "notifier.service",
        "[Unit]\n\
         Description=Says READY=1 on its readiness pipe\n\
         \n\
         [Service]\n\
         Type=notify\n\
         NotifyFd=3\n\
         ExecStart=:/bin/sh -c 'echo STATUS=warming >&3; echo READY=1 >&3; echo STATUS=serving >&3; read x < /dev/ptmx'\n",
    ),
    (
        "lazy.service",
        "[Unit]\n\
         Description=Never says READY=1\n\
         \n\
         [Service]\n\
         Type=notify\n\
         TimeoutStartSec=infinity\n\
         ExecStart=:/bin/sh -c 'echo STATUS=still-starting >&$NOTIFY_FD; read x < /dev/ptmx'\n",
    ),
    (
        "daemon.service",
        "[Unit]\n\
         Description=Forks, and says its daemon's pid in a file\n\
         \n\
         [Service]\n\
         Type=forking\n\
         PIDFile=/run/daemon.pid\n\
         ExecStart=:/bin/sh -c 'sh -c \"read x < /dev/ptmx\" & echo $! > /run/daemon.pid'\n",
    ),
    (
        "echo.socket",
        "[Unit]\n\
         Description=Answers each connection with an instance\n\
         \n\
         [Socket]\n\
         ListenStream=127.0.0.1:7777\n\
         Accept=yes\n",
    ),
    (
        "echo@.service",
        "[Unit]\n\
         Description=Echoes one connection's line\n\
         \n\
         [Service]\n\
         ExecStart=:/bin/sh -c 'read l; echo \"echoed $l\"'\n\
         StandardInput=socket\n\
         StandardOutput=socket\n",
    ),
    (
        "hello.socket",
        "[Unit]\n\
         Description=Starts hello.service on the first connection\n\
         \n\
         [Socket]\n\
         ListenStream=127.0.0.1:7778\n",
    ),
    (
        "hello.service",
        "[Unit]\n\
         Description=Says what the socket passed it\n\
         \n\
         [Service]\n\
         ExecStart=:/bin/sh -c 'echo \"hello-fds $LISTEN_FDS $LISTEN_FDNAMES pid-ok-$(( LISTEN_PID == $$ ))\"; read x < /dev/ptmx'\n",
    ),
    (
        "test.slice",
        "[Unit]\n\
         Description=The test's own slice\n\
         \n\
         [Slice]\n\
         TasksMax=64\n",
    ),
    (
        "hog.service",
        "[Unit]\n\
         Description=Grows past its MemoryMax=\n\
         \n\
         [Service]\n\
         Slice=test.slice\n\
         MemoryMax=16M\n\
         ExecStart=:/bin/sh -c 'x=0123456789abcdef; while :; do x=\"$x$x\"; done'\n",
    ),
    (
        "tasks.service",
        "[Unit]\n\
         Description=Forks past its TasksMax=\n\
         \n\
         [Service]\n\
         TasksMax=3\n\
         ExecStart=:/bin/sh -c 'for i in 1 2 3 4 5; do read x < /dev/ptmx & done; echo forked; read x < /dev/ptmx'\n",
    ),
    (
        "deleg.service",
        "[Unit]\n\
         Description=Manages its own cgroup as uid 1000\n\
         \n\
         [Service]\n\
         User=ferrix\n\
         Delegate=yes\n\
         ExecStart=:/bin/sh -c 'mkdir /sys/fs/cgroup/system.slice/deleg.service/sub && echo delegated-ok; read x < /dev/ptmx'\n",
    ),
    (
        "spare.service",
        "[Unit]\n\
         Description=Enabled and disabled from the prompt\n\
         \n\
         [Service]\n\
         ExecStart=:/bin/sh -c 'read x < /dev/ptmx'\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
    ),
    (
        "pong.service",
        "[Unit]\n\
         Description=A native service that answers what the directory routes to it\n\
         \n\
         [Service]\n\
         Type=native\n\
         ExecStart=/sbin/pong\n\
         Offers=ferrix.test\n",
    ),
    (
        "pong-as-user.service",
        "[Unit]\n\
         Description=A native service run as ferrix, by a helper that became it\n\
         \n\
         [Service]\n\
         Type=native\n\
         ExecStart=/sbin/pong\n\
         User=ferrix\n",
    ),
    (
        "asker.service",
        "[Unit]\n\
         Description=Opens ferrix.test, which it declares\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         Uses=ferrix.test\n\
         ExecStart=/bin/dirclient ferrix.test\n",
    ),
    (
        "rogue.service",
        "[Unit]\n\
         Description=Opens ferrix.test, having declared only ferrix.other\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         Uses=ferrix.other\n\
         ExecStart=/bin/dirclient ferrix.test\n",
    ),
    (
        "boxed.service",
        "[Unit]\n\
         Description=Checks its own sandbox: NoNewPrivileges=, PrivateTmp=, ProtectSystem=strict\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=boxed\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         ProtectSystem=strict\n\
         ExecStart=:/bin/suid-sh -c 'echo \"$P-ids $UID $EUID\"; while read k v; do [ \"$k\" = NoNewPrivs: ] && echo \"$P-nnp $v\"; done < /proc/self/status; [ -e /tmp/l13-host ] && echo \"$P-tmp-shared\" || echo \"$P-tmp-private\"; echo in > /tmp/l13-$P && echo \"$P-tmp-writable\"; echo in > /run/l13-open/$P && echo \"$P-run-writable\" || echo \"$P-run-refused\"; echo \"$P-done\"'\n",
    ),
    (
        "open.service",
        "[Unit]\n\
         Description=The same checks with no sandbox, to show each would see the difference\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=open\n\
         ExecStart=:/bin/suid-sh -c 'echo \"$P-ids $UID $EUID\"; while read k v; do [ \"$k\" = NoNewPrivs: ] && echo \"$P-nnp $v\"; done < /proc/self/status; [ -e /tmp/l13-host ] && echo \"$P-tmp-shared\" || echo \"$P-tmp-private\"; echo in > /tmp/l13-$P && echo \"$P-tmp-writable\"; echo in > /run/l13-open/$P && echo \"$P-run-writable\" || echo \"$P-run-refused\"; echo \"$P-done\"'\n",
    ),
    (
        "netns.service",
        "[Unit]\n\
         Description=Looks at its network from inside PrivateNetwork=\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=netns\n\
         PrivateNetwork=yes\n\
         ExecStart=:/bin/sh -c 'n=0; while read i rest; do [ \"$i\" = lo ] && n=1; done < /proc/net/route; echo \"$P-lo-route $n\"; while read i rest; do case $i in *:) echo \"$P-dev $i\";; esac; done < /proc/net/dev; echo ping | nc 127.0.0.1 7777; echo \"$P-net-done\"'\n",
    ),
    (
        "netopen.service",
        "[Unit]\n\
         Description=The same look at the network with no PrivateNetwork=\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=netopen\n\
         ExecStart=:/bin/sh -c 'n=0; while read i rest; do [ \"$i\" = lo ] && n=1; done < /proc/net/route; echo \"$P-lo-route $n\"; while read i rest; do case $i in *:) echo \"$P-dev $i\";; esac; done < /proc/net/dev; echo ping | nc 127.0.0.1 7777; echo \"$P-net-done\"'\n",
    ),
    (
        "sc-plain.service",
        "[Unit]\n\
         Description=The filter checks' script with no filter\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=plain\n\
         ExecStart=:/bin/sh -c 'mkdir /tmp/sc-$P; echo \"$P-mkdir $?\"; hostname l13; echo \"$P-hostname $?\"; while read k v; do case $k in NoNewPrivs:|Seccomp:) echo \"$P-$k $v\";; esac; done < /proc/self/status; echo \"$P-done\"'\n",
    ),
    (
        "sc-kill.service",
        "[Unit]\n\
         Description=A deny-list that kills mkdir\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=kill\n\
         SystemCallFilter=~mkdir mkdirat\n\
         ExecStart=:/bin/sh -c 'mkdir /tmp/sc-$P; echo \"$P-mkdir $?\"; hostname l13; echo \"$P-hostname $?\"; while read k v; do case $k in NoNewPrivs:|Seccomp:) echo \"$P-$k $v\";; esac; done < /proc/self/status; echo \"$P-done\"'\n",
    ),
    (
        "sc-eperm.service",
        "[Unit]\n\
         Description=A deny-list whose words answer EPERM\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=eperm\n\
         SystemCallFilter=~mkdir:EPERM mkdirat:EPERM\n\
         ExecStart=:/bin/sh -c 'mkdir /tmp/sc-$P; echo \"$P-mkdir $?\"; hostname l13; echo \"$P-hostname $?\"; while read k v; do case $k in NoNewPrivs:|Seccomp:) echo \"$P-$k $v\";; esac; done < /proc/self/status; echo \"$P-done\"'\n",
    ),
    (
        "sc-errno.service",
        "[Unit]\n\
         Description=A deny-list answering SystemCallErrorNumber=\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=errno\n\
         SystemCallFilter=~mkdir mkdirat\n\
         SystemCallErrorNumber=EACCES\n\
         ExecStart=:/bin/sh -c 'mkdir /tmp/sc-$P; echo \"$P-mkdir $?\"; hostname l13; echo \"$P-hostname $?\"; while read k v; do case $k in NoNewPrivs:|Seccomp:) echo \"$P-$k $v\";; esac; done < /proc/self/status; echo \"$P-done\"'\n",
    ),
    (
        "sc-allow.service",
        "[Unit]\n\
         Description=An allow-list of @system-service, native ABI only\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         User=ferrix\n\
         Environment=P=allow\n\
         SystemCallFilter=@system-service\n\
         SystemCallArchitectures=native\n\
         ExecStart=:/bin/sh -c 'mkdir /tmp/sc-$P; echo \"$P-mkdir $?\"; hostname l13; echo \"$P-hostname $?\"; while read k v; do case $k in NoNewPrivs:|Seccomp:) echo \"$P-$k $v\";; esac; done < /proc/self/status; echo \"$P-done\"'\n",
    ),
    (
        "getty@.service.d/test.conf",
        "[Service]\n\
         Environment=TERM=dumb \"PS1=init-test%%# \"\n",
    ),
];

/// The units the test's `multi-user.target` wants besides the getty.
const WANTED: [&str; 13] = [
    "flaky.service",
    "anchor.service",
    "forker.service",
    "echoer.service",
    "notifier.service",
    "daemon.service",
    "echo.socket",
    "hello.socket",
    "hog.service",
    "tasks.service",
    "deleg.service",
    "asker.service",
    "rogue.service",
];

/// Who the test's user is: root, and `ferrix`, whom `su` becomes to be
/// refused what only root may do.
const PASSWD: &str = "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/:/bin/sh\n\
                      plain:x:1001:1001:plain:/:/bin/sh\n\
                      auth:x:90:90:authd:/var/lib/ferrix/auth:/sbin/nologin\n";

/// Their groups.
const GROUP: &str = "root:x:0:\nferrix:x:1000:\nplain:x:1001:\nauth:x:90:\nwheel:x:10:ferrix\n";

/// The three programs of `src/user/system/linux/init/` for one architecture.
#[derive(Debug)]
pub(crate) struct Built {
    init: PathBuf,
    getty: PathBuf,
    generator: PathBuf,
    svc: PathBuf,
    /// The directory's test client, carried by `test-init` alone.
    dirclient: PathBuf,
}

/// Build `src/user/system/linux/init/` for `arch`, or `None` on an architecture it is not built
/// for yet.
pub(crate) fn built(arch: Arch) -> Result<Option<Built>> {
    let Some(target) = zinc::target(arch) else {
        println!("  init is not built for {} yet", arch.name());
        return Ok(None);
    };
    println!("  building init for {target}");
    let target_dir = paths::target_dir().join("init");
    let release = target_dir.join(target).join("release");
    let built = Built {
        init: release.join("init"),
        getty: release.join("getty"),
        generator: release.join("getty-generator"),
        svc: release.join("svc"),
        dirclient: release.join("dirclient"),
    };
    crate::builds::Build::cargo(
        format!("cargo build (init) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/init"),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    // As for zinc: RUSTFLAGS replaces the flags every config file up the
    // tree would merge, the root's ARM linker script among them.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .output(&built.init)
    .output(&built.getty)
    .output(&built.generator)
    .output(&built.svc)
    .output(&built.dirclient)
    .run()?;
    Ok(Some(built))
}

/// A file's bytes, or an error naming it.
fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// The names linked to `/bin/svc`, each of which it takes as its verb.
pub(crate) const SVC_NAMES: [&str; 3] = ["poweroff", "reboot", "shutdown"];

/// What an image carries for init on `arch`: the programs and the shipped
/// units. Empty on an architecture init is not built for.
pub(crate) fn carried(arch: Arch) -> Result<Vec<File>> {
    let Some(built) = built(arch)? else {
        return Ok(Vec::new());
    };
    let program = |path: &str, from: &Path| -> Result<File> {
        Ok(File {
            path: path.to_owned(),
            mode: 0o755,
            content: Content::Bytes(read(from)?),
        })
    };
    let mut files = vec![
        program("sbin/init", &built.init)?,
        program("sbin/getty", &built.getty)?,
        program("lib/ferrix/generators/getty-generator", &built.generator)?,
        program("bin/svc", &built.svc)?,
    ];
    // The commands a person types to turn the machine off, which `svc`
    // answers to as systemctl does. busybox's would signal pid 1 as its own
    // init reads signals: `poweroff`'s SIGUSR2 is one init ignores.
    for name in SVC_NAMES {
        files.push(File {
            path: format!("bin/{name}"),
            mode: 0o777,
            content: Content::Link("svc".to_owned()),
        });
    }
    let units = paths::workspace_root().join(UNITS);
    let mut names: Vec<PathBuf> = std::fs::read_dir(&units)
        .map_err(|error| Error::new(format!("reading {}: {error}", units.display())))?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    names.sort();
    for path in names {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        files.push(File {
            path: format!("lib/ferrix/units/{name}"),
            mode: 0o644,
            content: Content::Bytes(read(&path)?),
        });
    }
    files.push(File {
        path: "lib/ferrix/units/default.target".to_owned(),
        mode: 0o777,
        content: Content::Link("multi-user.target".to_owned()),
    });
    Ok(files)
}

/// The command line an image that boots init carries (§8.1): init as pid
/// 1, and `devmgr` started by it (§7.3, L12), which is outside the certified
/// configuration and the reason those images skip the kernel's disk checks.
pub(crate) fn command_line() -> String {
    format!("{} {DEVMGR_OPTION}\n", qemu::init_option(PATH))
}

/// The kernel option that has pid 1 start `devmgr`.
const DEVMGR_OPTION: &str = "ferrix.devmgr=init";

/// A word of `ExecStart=`, quoted as systemd reads one.
fn exec_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
    {
        return word.to_owned();
    }
    let escaped = word.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// The compositor unit's restart: a session's compositor is never started
/// again, since a client that killed it would get a fresh, unlocked desktop
/// (`docs/AUTH.md` §6.4); the session ends, and the seat is the console's
/// login. A desktop that is root's keeps its restart, since every client
/// there is root already.
const fn restart_line(session: bool) -> &'static str {
    if session {
        "# Not restarted: the session ends with its compositor (docs/AUTH.md §6.4).\n"
    } else {
        "Restart=on-failure\n"
    }
}

/// The console's getty on a desktop's image: on a session's, with
/// `--login`, so the seat a session that ended goes back to is a login
/// (`docs/AUTH.md` §6.4; `authd` and `/bin/login` come with the desktop's
/// authentication); masked on a root desktop's that wants no shell.
fn console_getty(session: bool, shell: bool) -> Vec<File> {
    if session {
        vec![File {
            path: "etc/ferrix/units/getty@.service.d/login.conf".to_owned(),
            mode: 0o644,
            content: Content::Bytes(crate::auth::LOGIN_DROP_IN.as_bytes().to_vec()),
        }]
    } else if shell {
        Vec::new()
    } else {
        vec![File {
            path: "etc/ferrix/units/getty@.service".to_owned(),
            mode: 0o777,
            content: Content::Link("/dev/null".to_owned()),
        }]
    }
}

/// What a desktop image carries so that init is pid 1 and the compositor a
/// service of `graphical.target` (landing L10): init and its units, the
/// compositor at `/bin/hyprix` with `hyprix.service` running it with
/// `arguments`, and `default.target` pointed at `graphical.target`. Without
/// a shell in the image the console's getty is masked, since it would only
/// fail and be restarted. With `pulsed` the sound server is a service of
/// `graphical.target` too, started before the compositor, and every client
/// the compositor starts is told where it listens (`docs/AUDIO.md`, U2d).
///
/// # Errors
///
/// When init cannot be built for `arch`.
pub(crate) fn desktop_files(
    arch: Arch,
    hyprix: &[u8],
    arguments: &[&str],
    shell: bool,
    pulsed: Option<&[u8]>,
    session_user: Option<&str>,
) -> Result<Vec<File>> {
    let mut files = carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    // With a session user, `sessiond` starts the compositor as that user
    // and hands it seat0's devices (`crate::session`); it sets the user's
    // own `HOME`.
    let mut command = match session_user {
        Some(user) => vec![
            "/bin/sessiond".to_owned(),
            "--user".to_owned(),
            exec_word(user),
            "--".to_owned(),
            "/bin/hyprix".to_owned(),
        ],
        None => vec!["/bin/hyprix".to_owned()],
    };
    command.extend(arguments.iter().map(|word| exec_word(word)));
    let home = if session_user.is_some() {
        // sessiond, the unit's own process, takes the seat channel and arms
        // each lock at authd (docs/AUTH.md §3.7): init routes the name to
        // this unit alone.
        "Uses=ferrix.auth.seat\n"
    } else {
        "# Root's home, which the kernel gave hyprix as pid 1 and a unit with\n\
         # no User= is not given; the clients find ~/.config through it.\n\
         Environment=HOME=/\n"
    };
    let restart = restart_line(session_user.is_some());
    let unit = format!(
        "# The compositor, a service of graphical.target (docs/INIT.md, L10).\n\
         [Unit]\n\
         Description=The compositor\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         {home}\
         {}\
         {restart}\
         StandardOutput=console\n\
         StandardError=console\n",
        command.join(" "),
        if pulsed.is_some() {
            format!(
                "# Where pulsed listens, for every client it starts.\n\
                 Environment=PULSE_SERVER=unix:{PULSE_SOCKET}\n"
            )
        } else {
            String::new()
        }
    );
    let unit_file = |path: &str, text: &str| File {
        path: path.to_owned(),
        mode: 0o644,
        content: Content::Bytes(text.as_bytes().to_vec()),
    };
    let link = |path: &str, target: &str| File {
        path: path.to_owned(),
        mode: 0o777,
        content: Content::Link(target.to_owned()),
    };
    files.push(File {
        path: "bin/hyprix".to_owned(),
        mode: 0o755,
        content: Content::Bytes(hyprix.to_vec()),
    });
    files.push(unit_file("etc/ferrix/units/hyprix.service", &unit));
    files.push(link(
        "etc/ferrix/units/graphical.target.wants/hyprix.service",
        "/etc/ferrix/units/hyprix.service",
    ));
    files.push(link(
        "etc/ferrix/units/default.target",
        "/lib/ferrix/units/graphical.target",
    ));
    files.extend(console_getty(session_user.is_some(), shell));
    if let Some(pulsed) = pulsed {
        files.push(File {
            path: "bin/pulsed".to_owned(),
            mode: 0o755,
            content: Content::Bytes(pulsed.to_vec()),
        });
        files.push(unit_file(
            "etc/ferrix/units/pulsed.service",
            &format!(
                "# The sound server, a service of graphical.target (docs/AUDIO.md, U2d).\n\
                 [Unit]\n\
                 Description=The PulseAudio-protocol server\n\
                 Before=hyprix.service\n\
                 \n\
                 [Service]\n\
                 ExecStart=/bin/pulsed {PULSE_SOCKET}\n\
                 Restart=on-failure\n\
                 StandardOutput=console\n\
                 StandardError=console\n"
            ),
        ));
        files.push(link(
            "etc/ferrix/units/graphical.target.wants/pulsed.service",
            "/etc/ferrix/units/pulsed.service",
        ));
    }
    Ok(files)
}

/// Where the desktop's `pulsed` listens: libpulse's clients are told with
/// `PULSE_SERVER`, since the compositor gives them no `XDG_RUNTIME_DIR` of
/// a session.
pub(crate) const PULSE_SOCKET: &str = "/run/pulse/native";

/// The test's own units, zinc, and busybox for its `su`, as carried files.
fn test_files(shell: &[u8], busybox: &[u8], dirclient: &[u8]) -> Vec<File> {
    let mut files: Vec<File> = ["bin/sh", "bin/zinc"]
        .into_iter()
        .map(|path| File {
            path: path.to_owned(),
            mode: 0o755,
            content: Content::Bytes(shell.to_vec()),
        })
        .collect();
    for (name, text) in TEST_UNITS {
        files.push(File {
            path: format!("etc/ferrix/units/{name}"),
            mode: 0o644,
            content: Content::Bytes(text.as_bytes().to_vec()),
        });
    }
    for name in WANTED {
        files.push(File {
            path: format!("etc/ferrix/units/multi-user.target.wants/{name}"),
            mode: 0o777,
            content: Content::Link(format!("/etc/ferrix/units/{name}")),
        });
    }
    // The sandboxing stage's set-uid shell: zinc, set-uid root, which a
    // uid-1000 service runs to see whether `execve` gave it root.
    files.push(File {
        path: "bin/suid-sh".to_owned(),
        mode: 0o4755,
        content: Content::Bytes(shell.to_vec()),
    });
    files.push(File {
        path: "bin/dirclient".to_owned(),
        mode: 0o755,
        content: Content::Bytes(dirclient.to_vec()),
    });
    files.push(File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: Content::Bytes(busybox.to_vec()),
    });
    // `cat` and `setsid` for the `login` stage: `/proc/self/*` read back, and
    // a `login` with no controlling terminal.
    // `stat` for the `su` stage: `/bin/su`'s mode as the image has it.
    // `timeout` for the `/dev/tty` probe's read.
    // `hostname` for the filter checks: `sethostname`, outside
    // `@system-service`.
    for applet in [
        "su", "nc", "mkdir", "cat", "setsid", "stat", "timeout", "hostname",
    ] {
        files.push(File {
            path: format!("bin/{applet}"),
            mode: 0o777,
            content: Content::Link("busybox".to_owned()),
        });
    }
    for (path, text) in [
        ("bin/revoke-reader", REVOKE_READER),
        ("bin/revoke-steal", REVOKE_STEAL),
    ] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o755,
            content: Content::Bytes(text.as_bytes().to_vec()),
        });
    }
    for (path, text) in [("etc/passwd", PASSWD), ("etc/group", GROUP)] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o644,
            content: Content::Bytes(text.as_bytes().to_vec()),
        });
    }
    files
}

/// `cargo xtask test-init`.
///
/// # Errors
///
/// When an image cannot be built, a boot fails, or anything §15's stages
/// one and two require is missing, with the serial log's path.
///
/// Verifies: L.init.1
pub(crate) fn test_init(args: &Args) -> Result<()> {
    let checker = Checker::required()?;
    for arch in args.arches()? {
        test_arch(arch, args, &checker)?;
    }
    Ok(())
}

/// One architecture's boot.
fn test_arch(arch: Arch, args: &Args, checker: &Checker) -> Result<()> {
    let shell = zinc::built(arch)?
        .ok_or_else(|| Error::new(format!("zinc could not be built for {arch}")))?;
    let shell = read(&shell)?;
    let mut files = carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = read(&crate::busybox::program(arch)?)?;
    let dirclient = built(arch)?
        .map(|built| read(&built.dirclient))
        .transpose()?
        .unwrap_or_default();
    files.extend(test_files(&shell, &busybox, &dirclient));
    // authd, `/bin/login` and the `login` service, with no password for
    // anyone: the `login` stage sets ferrix's first one at the console.
    files.extend(crate::auth::carried(arch, None)?);
    // L9's gate as designed: real sshd under socket activation, where
    // `sshdt` is built, which is x86-64 alone.
    let sshd = arch == Arch::X86_64;
    if sshd {
        files.extend(sshd_files()?);
    }
    println!("  {arch}: building an image whose init is {PATH}");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let options = command_line();
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;

    let volume = btrfs_disk::blank_copy(arch, VOLUME)?;
    let mut with_volume = args.clone();
    with_volume.data_image = Some(volume.clone());
    with_volume.data_image_kept = true;

    println!(
        "  {arch}: typing at the console the getty gives (timeout {}s)",
        args.timeout
    );
    let mut failures: Vec<String> = Vec::new();
    let lines = qemu::watch_then(arch, &image, &kernel, &with_volume, SUCCESS_MARKER, |at| {
        session(at, &mut failures, sshd)
    })?;
    let after = after_marker(&lines);
    failures.extend(judge_boot(after).err());
    failures.extend(judge_units(after).err());
    failures.extend(judge_flaky(after).err());
    failures.extend(judge_shutdown(after).err());
    failures.extend(judge_audit_power(after).err());
    if !failures.is_empty() {
        let mut message = format!("{arch}: init did not do what §15 requires:\n");
        for failure in &failures {
            message.push_str("    - ");
            message.push_str(failure);
            message.push('\n');
        }
        message.push_str(&format!(
            "  Serial output is in {}",
            paths::build_dir(arch).join("serial.log").display()
        ));
        return Err(Error::new(message));
    }
    checker.run(&volume, arch)?;
    // The boot that skips its self-checks records that it did, which only a
    // reader outside it can see (AUDIT.md §6): once, on x86-64, since the
    // record is the same code on every architecture.
    if arch == Arch::X86_64 {
        checks_skipped(arch, args, &loader, &kernel, &archive)?;
    }
    println!(
        "  {arch}: init booted multi-user.target, gave the console a session, spent a failing \
         service's budget, ended a service's grandchild with its cgroup, answered svc, waited for \
         readiness, activated sockets, OOM-killed a service in its own slice, routed a native \
         service through the directory, and powered off clean"
    );
    Ok(())
}

/// What is typed, and what it is judged by. Every failure goes into
/// `failures`, so one run reports everything that was wrong.
fn session(at: &mut Watching<'_>, failures: &mut Vec<String>, sshd: bool) -> Result<()> {
    // The getty's banner, and the target it is part of: the prompt follows.
    let deadline = Instant::now() + PATIENCE * 2;
    let up = at.read_more(deadline, |lines| {
        has(lines, "multi-user.target: active") && has(lines, BANNER)
    })?;
    if !up {
        failures.push("multi-user.target never became active with a getty on the console".into());
        return Ok(());
    }
    if !at.wait_for_shell(Instant::now() + PATIENCE)? {
        failures.push("the getty's shell never answered at the console".into());
        return Ok(());
    }

    // Stage one: the shell's own session, read from the kernel.
    match ask(
        at,
        "m=stat; read s < /proc/self/stat; echo \"$m-self $s\"\n",
        "stat-self ",
    )? {
        Some(line) => failures.extend(judge_stat(&line).err()),
        None => failures.push("the shell never printed its /proc/self/stat".into()),
    }

    // The getty's drop-in reached the shell: a template's drop-ins apply to
    // its instances.
    match ask(at, "m=env; echo \"$m-term $TERM\"\n", "env-term ")? {
        Some(line) if line.trim() == "env-term dumb" => {}
        Some(line) => failures.push(format!(
            "getty@.service.d/test.conf did not reach the shell: `{}`",
            line.trim()
        )),
        None => failures.push("the shell never said its TERM".into()),
    }

    // Stage two: the grandchild is in its service's cgroup, and goes with it.
    let pid = ask(
        at,
        "m=grp; read g < /run/forker.pid; echo \"$m-pid $g\"\n",
        "grp-pid ",
    )?
    .and_then(|line| line.trim().strip_prefix("grp-pid ").map(str::to_owned))
    .filter(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()));
    let Some(pid) = pid else {
        failures.push("forker.service's grandchild never wrote its pid to /run/forker.pid".into());
        return power_off(at, failures);
    };
    let listed = ask(
        at,
        "while read p; do [ \"$p\" = \"$g\" ] && echo \"$m-in $p\"; done \
         < /sys/fs/cgroup/system.slice/forker.service/cgroup.procs; echo \"$m-listed\"\n",
        "grp-listed",
    )?;
    let member = format!("grp-in {pid}");
    if listed.is_none() || !has(at.after(), &member) {
        failures.push(format!(
            "the grandchild {pid} is not in forker.service's cgroup.procs"
        ));
    }
    if ask(
        at,
        "read a < /run/anchor.pid; kill $a\n",
        "forker.service: stopped",
    )?
    .is_none()
    {
        failures.push(
            "killing anchor.service's main process did not stop forker.service, which is \
             BindsTo= it"
                .into(),
        );
    } else {
        let mut gone = false;
        for _ in 0..GONE_TRIES {
            let check = "[ -d /proc/$g ] && echo \"$m-alive\" || echo \"$m-gone\"\n";
            let before = at.after().len();
            at.type_in(check.as_bytes())?;
            let deadline = Instant::now() + PATIENCE;
            let _ = at.read_more(deadline, |lines| {
                lines
                    .get(before..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| matches!(line.trim(), "grp-alive" | "grp-gone"))
            })?;
            if at
                .after()
                .iter()
                .skip(before)
                .any(|line| line.trim() == "grp-gone")
            {
                gone = true;
                break;
            }
            thread::sleep(POLL);
        }
        if !gone {
            failures.push(format!(
                "stopping forker.service left its grandchild {pid} alive: its cgroup was not \
                 emptied"
            ));
        }
    }
    after_stage_one(at, failures, sshd)
}

/// The stages after the first, in order, and the power-off that ends them.
fn after_stage_one(at: &mut Watching<'_>, failures: &mut Vec<String>, sshd: bool) -> Result<()> {
    control(at, failures)?;
    readiness(at, failures)?;
    sockets(at, failures)?;
    resources(at, failures)?;
    directory(at, failures)?;
    sandboxing(at, failures)?;
    devmgr_by_init(at, failures)?;
    audit_read_back(at, failures)?;
    login(at, failures)?;
    su(at, failures)?;
    dev_tty(at, failures)?;
    console_revoke(at, failures)?;
    if sshd {
        sshd_activated(at, failures)?;
    }
    power_off(at, failures)
}

/// ferrix's first password, chosen at the console by the `login` stage.
const FIRST_PASSWORD: &str = "chosen at the console";

/// What `/proc/self/status` says of a process whose ids are all ferrix's.
const FERRIX_IDS: &str = "Uid:\t1000\t1000\t1000\t1000";

/// The `login` stage (`docs/AUTH.md` P2.3): from root's shell on the
/// console, `/bin/login` against `authd`.
///
/// First the console's own `tty_nr`, read back, so the number `authd` takes
/// for the console is the target's and not only Linux's formula. Then, with
/// no controlling terminal (`setsid`), `login ferrix` is not offered a first
/// password -- the certification consultant's F4, before any is set. Then at
/// the console it is: chosen, typed twice, and the shell that follows is
/// ferrix's in every id and in `session-1.scope`. Last, a wrong password is
/// refused and the chosen one lets ferrix in again, in `session-2.scope`.
fn login(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    at.redact(FIRST_PASSWORD);
    match ask(
        at,
        "m=login; echo \"$m-tty $(cat /proc/self/stat)\"\n",
        "login-tty ",
    )? {
        Some(line) => {
            let tty_nr = line.rsplit_once(')').and_then(|(_, rest)| {
                rest.split_ascii_whitespace()
                    .nth(4)
                    .and_then(|field| field.parse::<u32>().ok())
            });
            if tty_nr == Some(CONSOLE_TTY_NR) {
                println!("  the console's tty_nr, read on the target: {CONSOLE_TTY_NR} (5:1)");
            } else {
                failures.push(format!(
                    "the console's tty_nr is {tty_nr:?}, not {CONSOLE_TTY_NR}: `{}`",
                    line.trim()
                ));
            }
        }
        None => failures.push("the console's /proc/self/stat was never read".into()),
    }
    // busybox's `setsid` has no `-w`: it forks, and `login` goes on in a
    // session of its own while the shell carries on, so its answer is waited
    // for as a line.
    let before = at.after().len();
    at.type_in(b"setsid login ferrix </dev/null\n")?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        lines.get(before..).unwrap_or_default().iter().any(|line| {
            line.contains("login: no password is set for ferrix") || line.contains("Choose one now")
        })
    })?;
    let since = at.after().get(before..).unwrap_or_default().to_vec();
    if has(&since, "Choose one now") || !has(&since, "login: no password is set for ferrix") {
        failures.push(
            "login with no controlling terminal was not refused with `no password is set`: a first \
             password was offered off the console"
                .into(),
        );
    } else {
        println!("  login: with no controlling terminal, ferrix was not offered a first password");
    }
    // At the console: the offer, the password twice, the session.
    let first = log_in(
        at,
        failures,
        &[FIRST_PASSWORD, FIRST_PASSWORD],
        "session-1.scope",
        Some("ferrix has no password. Choose one now:"),
    )?;
    if first && !has(at.after(), "first-password,tty_nr=1281") {
        failures.push(
            "authd's audit did not record the first password and the console's tty_nr".into(),
        );
    }
    // A wrong password, then the chosen one.
    let _ = log_in(
        at,
        failures,
        &["a wrong guess", FIRST_PASSWORD],
        "session-2.scope",
        None,
    )?;
    Ok(())
}

/// `login ferrix` at the console, the `answers` given at its prompts in
/// turn ([`answer_prompts`]), and the shell that follows checked and left:
/// whether it ran as ferrix in `scope`.
fn log_in(
    at: &mut Watching<'_>,
    failures: &mut Vec<String>,
    answers: &[&str],
    scope: &str,
    offer: Option<&str>,
) -> Result<bool> {
    let before = at.after().len();
    let _ = ask(
        at,
        "m=login; echo \"$m-starts\"; login ferrix\n",
        "login-starts",
    )?;
    if let Some(offer) = offer {
        let deadline = Instant::now() + PATIENCE;
        let offered = at.read_more(deadline, |lines| {
            lines
                .get(before..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(offer))
        })?;
        if !offered {
            failures.push(format!("login at the console did not say `{offer}`"));
            return Ok(false);
        }
    }
    for (index, answer) in answers.iter().enumerate() {
        let _ = answer_prompt(at, before, index, answer)?;
        if *answer != FIRST_PASSWORD {
            // A refusal is held for the policy's two seconds.
            let deadline = Instant::now() + PATIENCE;
            let refused = at.read_more(deadline, |lines| {
                lines
                    .get(before..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| line.contains("Login incorrect"))
            })?;
            if !refused {
                failures.push(
                    "a wrong password at login was not refused with `Login incorrect`".into(),
                );
                return Ok(false);
            }
        }
    }
    let deadline = Instant::now() + PATIENCE;
    let wanted = format!("login: ferrix in user-1000.slice/{scope}");
    let _ = at.read_more(deadline, |lines| {
        lines
            .get(before..)
            .unwrap_or_default()
            .iter()
            .any(|line| line.contains(&wanted))
    })?;
    if !has(at.after().get(before..).unwrap_or_default(), &wanted) {
        failures.push(format!("login at the console never said `{wanted}`"));
        return Ok(false);
    }
    let shell = at.after().len();
    let _ = ask(
        at,
        "cat /proc/self/status /proc/self/cgroup; m=login; echo \"$m-user\"\n",
        "login-user",
    )?;
    let said = at.after().get(shell..).unwrap_or_default().to_vec();
    let mut good = true;
    if !has(&said, FERRIX_IDS) {
        failures.push(format!(
            "the shell login started is not ferrix's in every id (`{FERRIX_IDS}`)"
        ));
        good = false;
    }
    if !has(&said, &format!("user-1000.slice/{scope}")) {
        failures.push(format!(
            "the shell login started is not in user-1000.slice/{scope}"
        ));
        good = false;
    }
    match ask(
        at,
        "exit\nm=login; echo \"$m-back $(cat /proc/self/status)\"\n",
        "login-back",
    )? {
        Some(_) => {}
        None => failures.push("the shell login started did not exit back to root's".into()),
    }
    if good {
        println!(
            "  login: ferrix logged in at the console, every id 1000, in user-1000.slice/{scope}"
        );
    }
    Ok(good)
}

/// `plain`'s first password, chosen at the console by the `su` stage.
const PLAIN_PASSWORD: &str = "plain chose this";

/// What `/proc/self/status` says of a process whose ids are all root's.
const ROOT_IDS: &str = "Uid:\t0\t0\t0\t0";

/// The `su` stage (`docs/AUTH.md` P2.6, decision 5), after `login`: in
/// ferrix's own session, `/bin/su` is set-uid root in the image (U6); with
/// ferrix's password the shell `su` starts is root's in every id; a wrong
/// one is refused; ferrix may not become another user; with no
/// controlling terminal `su` asks nothing (U5); and `plain`, not in wheel,
/// is refused before any prompt.
fn su(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    at.redact(PLAIN_PASSWORD);
    if !enter(at, failures, "ferrix", &[FIRST_PASSWORD])? {
        return Ok(());
    }
    match ask(
        at,
        "m=su; echo \"$m-mode $(stat -c '%a %u' /bin/su)\"\n",
        "su-mode ",
    )? {
        Some(line) if line.trim() == "su-mode 4755 0" => {
            println!("  su: /bin/su is set-uid root in the image (4755, uid 0)");
        }
        Some(line) => failures.push(format!("/bin/su is not 4755 root's: `{}`", line.trim())),
        None => failures.push("/bin/su's mode was never read".into()),
    }
    // The right password: root in every id.
    let before = at.after().len();
    let _ = ask(
        at,
        "m=su; echo \"$m-starts\"; su -c 'cat /proc/self/status; m=su; echo \"$m-root\"'\n",
        "su-starts",
    )?;
    let _ = answer_prompts(at, before, &[FIRST_PASSWORD])?;
    let _ = wait_for(at, before, "su-root")?;
    let said = at.after().get(before..).unwrap_or_default().to_vec();
    if has(&said, ROOT_IDS) && has(&said, "su-root") {
        println!("  su: ferrix, in wheel, became root with ferrix's own password");
    } else {
        failures.push(format!(
            "su with ferrix's password did not give root in every id (`{ROOT_IDS}`)"
        ));
    }
    // A wrong one.
    let before = at.after().len();
    let _ = ask(
        at,
        "m=su; echo \"$m-starts\"; su -c 'm=su; echo \"$m-wrong-root\"'; m=su; echo \"$m-wrong\"\n",
        "su-starts",
    )?;
    let _ = answer_prompts(at, before, &["not the password"])?;
    let _ = wait_for(at, before, "su-wrong")?;
    let said = at.after().get(before..).unwrap_or_default().to_vec();
    if has(&said, "su: Authentication failure") && !has(&said, "su-wrong-root") {
        println!("  su: a wrong password was refused");
    } else {
        failures
            .push("su took a wrong password, or did not say `su: Authentication failure`".into());
    }
    // Another user, not root: refused before any prompt.
    // Statuses are not looked at, only what `su` says: the shell's `$?`
    // after it is not to be relied on here.
    let before = at.after().len();
    match ask(
        at,
        "su plain -c 'm=su; echo \"$m-other-plain\"'; m=su; echo \"$m-other\"\n",
        "su-other",
    )? {
        Some(_)
            if has(since(at, before), "only root may become another user")
                && !has(since(at, before), "su-other-plain") =>
        {
            println!("  su: ferrix may not become plain");
        }
        _ => failures.push("su let ferrix become another user than root".into()),
    }
    // No controlling terminal: refused before connecting (U5).
    let before = at.after().len();
    at.type_in(b"setsid su -c 'm=su; echo \"$m-notty-root\"' </dev/null\n")?;
    let _ = wait_for(at, before, "su: no terminal to ask the password on")?;
    let said = at.after().get(before..).unwrap_or_default().to_vec();
    if has(&said, "su: no terminal to ask the password on") && !has(&said, "su-notty-root") {
        println!("  su: with no controlling terminal, su asked nothing and gave nothing");
    } else {
        failures.push("su with no controlling terminal was not refused before asking".into());
    }
    let _ = ask(at, "exit\nm=su; echo \"$m-back\"\n", "su-back")?;
    // `plain`, not in wheel: its first password at the console, then `su`.
    if !enter(at, failures, "plain", &[PLAIN_PASSWORD, PLAIN_PASSWORD])? {
        return Ok(());
    }
    let before = at.after().len();
    match ask(
        at,
        "su -c 'm=su; echo \"$m-plain-root\"'; m=su; echo \"$m-plain\"\n",
        "su-plain",
    )? {
        Some(_)
            if has(since(at, before), "su: plain is not in wheel")
                && !has(since(at, before), "su-plain-root") =>
        {
            println!("  su: plain, not in wheel, was refused before any password was asked");
        }
        _ => failures.push(
            "su did not refuse plain, who is not in wheel, with `plain is not in wheel`".into(),
        ),
    }
    let _ = ask(at, "exit\nm=su; echo \"$m-back\"\n", "su-back")?;
    Ok(())
}

/// The `/dev/tty` stage (`docs/AUTH.md` §1; the certification consultant's
/// D4): it is the caller's controlling terminal. With none -- a uid-1000
/// process in a session of its own -- it is refused, and nothing reaches the
/// console; from root's shell on the console it opens. Which terminal it is
/// for a pty's session is the kernel's self-check's to show.
fn dev_tty(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    // The markers are also in the echo of what was typed: only a line that
    // begins with one is the probe's.
    let said_by_probe = |lines: &[String], marker: &str| {
        lines
            .iter()
            .any(|line| line.trim_start().starts_with(marker))
    };
    let probe = "su ferrix -c 'setsid sh -c \"exec 3<>/dev/tty || { echo probe-tty-open-refused; exit; }; \
                 echo probe-tty-opened; echo probe-tty-written-by-ferrix >&3; timeout 2 cat <&3 >/dev/null; \
                 echo probe-tty-read-status \\$?\"' </dev/null\n";
    let before = at.after().len();
    at.type_in(probe.as_bytes())?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        let lines = lines.get(before..).unwrap_or_default();
        said_by_probe(lines, "probe-tty-read-status")
            || said_by_probe(lines, "probe-tty-open-refused")
    })?;
    let said = since(at, before).to_vec();
    if said_by_probe(&said, "probe-tty-open-refused")
        && !said_by_probe(&said, "probe-tty-opened")
        && !said_by_probe(&said, "probe-tty-written-by-ferrix")
    {
        println!("  /dev/tty: refused to a uid-1000 process with no controlling terminal");
    } else {
        failures.push("/dev/tty opened for a uid-1000 process with no controlling terminal".into());
    }
    // Root's shell on the console: the console is its session's terminal,
    // so `/dev/tty` opens. Which object it is -- the console, or a pty for a
    // session whose terminal is one -- the kernel's self-check asks of the
    // rule itself: `stat` here would follow `/proc/self/fd`'s plain link
    // back to the node, and this image has no program that makes a pty its
    // controlling terminal (busybox has no `script` here).
    match ask(
        at,
        "exec 3<>/dev/tty && { m=devtty; echo \"$m-console-opened\"; exec 3<&-; }\n",
        "devtty-console-opened",
    )? {
        Some(_) => {
            println!("  /dev/tty: opens for root's shell, whose session's terminal is the console");
        }
        None => failures.push("/dev/tty did not open for the shell on the console".into()),
    }
    Ok(())
}

/// The console revoke stage's reader: a program a console session leaves
/// running, reading the console. It waits until root has moved it into a
/// scope of its own, so that nothing but the hangup can end it, then becomes
/// a `cat` reading the console, which says how its read ended.
const REVOKE_READER: &str = "#!/bin/sh
echo $$ > /tmp/revoke-reader
until [ -e /tmp/revoke-moved ]; do :; done
exec cat >> /tmp/stolen
";

/// What busybox's `cat` says when its read is refused with `EIO`: the
/// reader's read ended by the hangup, not at an end of file. Looked for
/// anywhere in a line: the shell getty starts next prints its prompt on the
/// same console at the same moment, and may print it first (2026-10-05, on
/// main 634c7ed0e). Nothing typed in the stage says it.
const REVOKED_READ: &str = "cat: read error: I/O error";

/// The console revoke stage's thief: run by ferrix under `setsid -c`, which
/// tries to steal the console with `TIOCSCTTY` 1 through a descriptor that
/// can read it, then asks whether `/dev/tty` is now its.
const REVOKE_STEAL: &str = "#!/bin/sh
if (exec 3<>/dev/tty) 2>/dev/null; then
    echo revoke-steal-took
else
    echo revoke-steal-refused
fi
";

/// The console revoke stage (`docs/AUTH.md` §1; the certification
/// consultant's R0 to R4, ledger line 317). A uid-1000 reader holding root's
/// shell's console descriptors is moved into a scope of its own and seen
/// blocked in `read`; root's shell exits, and getty, restarting, hangs the
/// console up. The reader must end, its read refused, and a line typed at
/// the new shell must not reach it. Then ferrix, in a session of its own,
/// may not steal the console from root's live session.
///
/// The reader is judged on the one look at its `stat` that ended the wait
/// for it, and that look is the line echoed. A console read sleeps two
/// milliseconds at a time between looks for a keystroke
/// (`fs::terminal::POLL_NANOS`), so a reader waiting in it reads `R` for as
/// long as it waits its turn after each, which on a loaded host is often: a
/// second look, after the one that saw `S`, failed the stage with a reader
/// that was waiting all along (2026-10-04, `batch-20261004T190141Z-b0-2`).
/// A reader that never waits ends the wait after 500 looks, and the look
/// echoed then says what it was doing instead.
fn console_revoke(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let said_by_probe = |lines: &[String], marker: &str| {
        lines
            .iter()
            .any(|line| line.trim_start().starts_with(marker))
    };
    let before = at.after().len();
    let start = "su ferrix -c 'setsid /bin/revoke-reader' & \
                 until [ -s /tmp/revoke-reader ]; do :; done; \
                 p=$(cat /tmp/revoke-reader); \
                 svc scope --unit revoke-reader.scope $p && : > /tmp/revoke-moved; \
                 n=0; until s=$(cat /proc/$p/stat); case \"$s\" in *revoke-reader*|*'(sh)'*) false;; *') S '*) true;; *) n=$((n+1)); [ $n -ge 500 ];; esac; do :; done; \
                 m=revoke; echo \"$m-reader $s\"; exit\n";
    match ask(at, start, "revoke-reader ")? {
        Some(line) => {
            let state = line
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_ascii_whitespace().next().map(str::to_owned));
            let script = line.contains("(revoke-reader)") || line.contains("(sh)");
            if state.as_deref() == Some("S") && !script {
                println!(
                    "  revoke: ferrix's reader, in a scope of its own, waits in read on the console"
                );
            } else {
                failures.push(format!(
                    "the revoke stage's reader was not waiting (state {state:?}): `{}`",
                    line.trim()
                ));
            }
        }
        None => {
            failures.push("the revoke stage's reader never started".into());
            return Ok(());
        }
    }
    // Root's shell has exited; getty restarts and hangs the console up.
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        has(lines.get(before..).unwrap_or_default(), REVOKED_READ)
    })?;
    if has(since(at, before), REVOKED_READ) {
        println!("  revoke: getty's hangup ended the reader's read of the console with EIO");
    } else {
        failures.push(
            "the console was not revoked: a reader from the last session still reads it after getty \
             restarted"
                .into(),
        );
    }
    // A line typed at the new shell, as a password would be.
    let _ = ask(
        at,
        ": revoke-typed-after-the-hangup\nm=revoke; echo \"$m-typed\"\n",
        "revoke-typed",
    )?;
    match ask(
        at,
        "m=revoke; echo \"$m-stolen $(stat -c %s /tmp/stolen)\"\n",
        "revoke-stolen ",
    )? {
        Some(line) if line.trim() == "revoke-stolen 0" => {
            println!("  revoke: nothing typed after the hangup reached the reader");
        }
        Some(line) => failures.push(format!(
            "what was typed after getty's hangup reached the last session's reader: `{}`",
            line.trim()
        )),
        None => failures.push("the revoke stage's /tmp/stolen was never measured".into()),
    }
    // R0: ferrix, a session leader holding a console descriptor that can
    // read, may not take the console from root's live session.
    let before = at.after().len();
    at.type_in(b"su ferrix -c 'setsid -c /bin/revoke-steal'\n")?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        let lines = lines.get(before..).unwrap_or_default();
        said_by_probe(lines, "revoke-steal-took") || said_by_probe(lines, "revoke-steal-refused")
    })?;
    let said = since(at, before).to_vec();
    if said_by_probe(&said, "revoke-steal-refused") && !said_by_probe(&said, "revoke-steal-took") {
        println!("  revoke: ferrix could not steal the console from root's session");
    } else {
        failures.push("a uid-1000 process stole the console from a live session".into());
    }
    Ok(())
}

/// `login NAME` at the console with `answers` given at its prompts in turn
/// ([`answer_prompts`]): whether its shell started.
fn enter(
    at: &mut Watching<'_>,
    failures: &mut Vec<String>,
    name: &str,
    answers: &[&str],
) -> Result<bool> {
    let before = at.after().len();
    let _ = ask(
        at,
        &format!("m=login; echo \"$m-starts\"; login {name}\n"),
        "login-starts",
    )?;
    let _ = answer_prompts(at, before, answers)?;
    let wanted = format!("login: {name} in user-");
    if wait_for(at, before, &wanted)? {
        Ok(true)
    } else {
        failures.push(format!("`login {name}` did not log {name} in"));
        Ok(false)
    }
}

/// How often [`answer_prompt`] looks at the console's unfinished line for
/// the prompt it waits on.
const PROMPT_LOOK: Duration = Duration::from_millis(10);

/// What every password prompt says -- `Password: `, `New password: `,
/// `Retype new password: ` -- and the end of its line once it has taken an
/// answer, trailing blanks aside.
const PROMPT: &str = "assword: ";

/// Give `answers` to the password prompts of a `login` or `su` started after
/// line `before`, one per prompt, each typed once its prompt is on the
/// console. Says whether every one was taken within [`PATIENCE`] of the
/// last.
///
/// An answer may not be typed ahead. The prompt flushes what is waiting
/// (`TCSAFLUSH`, as `getpass` does, so nothing typed before the question is
/// taken as its answer), and so does turning the echo back on after. An
/// answer typed before its prompt's flush is thrown away whole if it all
/// came before the flush (`login plain`, 2026-10-04,
/// `batch-20261004T190141Z-5`: typed blind two seconds apart, one answer was
/// lost, the next question took the next line the gate typed), and **cut**
/// if the flush came while it arrived: a QEMU serial port takes 16 bytes
/// at a time, so `chosen at the console` came in two, the flush threw away
/// the first and `su` read ` console` as the password (2026-10-05,
/// `po5-red-tip-init-all-1`, the echo showing `chosen at the` before
/// `Password: `). Typing it again could not mend that, since `su` asks once.
///
/// So each answer is typed once, when its prompt is the console's unfinished
/// line ([`Watching::unfinished_line`]): the program echoes nothing and
/// flushes before it prints the prompt (`ferrix-auth-client`'s `Terminal`),
/// so what is typed after the prompt is all read. A password prompt's line
/// ends once its answer has been read -- the echo is off, and the program
/// writes the newline itself -- so a prompt has taken its answer when a
/// line ends with it.
fn answer_prompts(at: &mut Watching<'_>, before: usize, answers: &[&str]) -> Result<bool> {
    for (index, answer) in answers.iter().enumerate() {
        if !answer_prompt(at, before, index, answer)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Give `answer` to the password prompt numbered `index` from 0 since line
/// `before`, as [`answer_prompts`] does: typed once, when `index` prompts
/// have taken an answer and one more is on the console, finished line or
/// not. Says whether it was taken within [`PATIENCE`].
///
/// A prompt is counted as shown wherever it is, and as answered only at the
/// end of a line: a line the console's log wrote into while the prompt
/// waited shows it without its answer.
fn answer_prompt(at: &mut Watching<'_>, before: usize, index: usize, answer: &str) -> Result<bool> {
    let answered = |lines: &[String]| {
        lines
            .get(before..)
            .unwrap_or_default()
            .iter()
            .filter(|line| line.trim_end().ends_with(PROMPT.trim_end()))
            .count()
    };
    let taken = |lines: &[String]| answered(lines) > index;
    let deadline = Instant::now() + PATIENCE;
    let mut typed = false;
    while !taken(at.after()) {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        // The lines first, then the unfinished one: a line leaves the
        // unfinished one before it is sent, so it is never counted twice.
        let shown = since(at, before)
            .iter()
            .map(|line| line.matches(PROMPT).count())
            .sum::<usize>()
            + at.unfinished_line().matches(PROMPT).count();
        if !typed && shown > index && answered(at.after()) == index {
            at.type_in(format!("{answer}\n").as_bytes())?;
            typed = true;
        }
        let look = (Instant::now() + PROMPT_LOOK).min(deadline);
        let _ = at.read_more(look, taken)?;
    }
    Ok(true)
}

/// The lines from `before` on.
fn since<'a>(at: &'a Watching<'_>, before: usize) -> &'a [String] {
    at.after().get(before..).unwrap_or_default()
}

/// Wait for a line holding `text` among those from `before` on.
fn wait_for(at: &mut Watching<'_>, before: usize, text: &str) -> Result<bool> {
    let deadline = Instant::now() + PATIENCE;
    at.read_more(deadline, |lines| {
        lines
            .get(before..)
            .unwrap_or_default()
            .iter()
            .any(|line| line.contains(text))
    })
}

/// `svc audit` and what it prints, one record a line.
fn audit_lines(at: &mut Watching<'_>) -> Result<Vec<String>> {
    let before = at.after().len();
    // The marker is built from a variable, as everywhere here: the typed
    // line's echo can wrap so that a line of it reads `audit-done` alone,
    // and the wait then ended before `svc audit` had printed anything
    // (armv7a, l13init-init, 2026-10-04).
    at.type_in(b"m=audit; svc audit; echo \"$m-done\"\n")?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        lines
            .get(before..)
            .unwrap_or_default()
            .iter()
            .any(|line| line.trim() == "audit-done")
    })?;
    Ok(at.after().get(before..).unwrap_or_default().to_vec())
}

/// Whether a line of `svc audit` is a record of `event` saying `with`.
fn recorded(lines: &[String], event: &str, with: &str) -> bool {
    let named = format!(" {event} ");
    lines
        .iter()
        .any(|line| line.contains(&named) && line.contains(with))
}

/// The audit record (`docs/certification/AUDIT.md` §4, slice 3): init keeps
/// it in `/var/log/audit/<id>.bin` on the volume, and `svc audit` reads it
/// back: the start-up record with the boot's id, the configuration as this
/// boot has it (the checks run), and the decisions only a boot with init
/// makes -- the starter and the reader's handle given to pid 1, devmgr
/// started by it, and `/` switched with pid 1 moved -- each in the file.
///
/// Verifies: H.AUD.10
fn audit_read_back(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let lines = audit_lines(at)?;
    let id = lines
        .iter()
        .find(|line| line.contains(" START "))
        .and_then(|line| line.split(" id ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_owned);
    let Some(id) = id else {
        failures.push("svc audit showed no start-up record with the boot's audit id".into());
        return Ok(());
    };
    let file = format!("/var/log/audit/{id}.bin");
    if !has(&everything(at), &file) {
        failures.push(format!(
            "init never said it keeps the audit record in {file}, the start-up record's id"
        ));
    }
    for (event, with, what) in [
        (
            "CONFIG",
            "detail 1 1 0",
            "the checks' configuration, as run",
        ),
        (
            "STARTER_GIVEN",
            "target 2:1",
            "devmgr's starter given to pid 1",
        ),
        (
            "READER_GIVEN",
            "target 2:1",
            "the audit record's handle given to pid 1",
        ),
        ("DEVMGR_STARTED", "pid 1 ", "devmgr started by pid 1"),
        (
            "ROOT_SWITCHED",
            "detail 1 0 0",
            "/ switched with pid 1 moved",
        ),
    ] {
        if !recorded(&lines, event, with) {
            failures.push(format!("the audit record read back has no {event}: {what}"));
        }
    }
    Ok(())
}

/// The power action is the last record, and nothing made before it is
/// lost: init reads the audit record a last time as it goes down, and the
/// kernel, recording the power action, says how far it saw the reader read
/// and prints every high-value record past that on the console -- those a
/// driver made after the last read, the power action's own among them, and
/// as one line a run the ring overwrote. So init's last read must be where
/// the kernel saw it stop, and every record from there to the power action
/// must be on the console.
///
/// Verifies: H.AUD.11
fn judge_audit_power(after: &[String]) -> std::result::Result<(), String> {
    const TOLD: &str = "audit    power action 1 recorded as record ";
    let Some(at) = after.iter().rposition(|line| line.contains(TOLD)) else {
        return Err("the kernel never said it recorded the power-off".into());
    };
    let told = after
        .get(at)
        .and_then(|line| line.split(TOLD).nth(1))
        .unwrap_or_default();
    let mut numbers = told
        .split(|c: char| !c.is_ascii_digit())
        .filter(|word| !word.is_empty())
        .filter_map(|word| word.parse::<u64>().ok());
    let (Some(record), Some(seen)) = (numbers.next(), numbers.next()) else {
        return Err(format!(
            "the power-off's audit line does not parse: `{told}`"
        ));
    };
    let read = after
        .get(..at)
        .unwrap_or_default()
        .iter()
        .rev()
        .find_map(|line| line.split("high-value ring next ").nth(1))
        .and_then(|rest| rest.split(',').next())
        .and_then(|next| next.trim().parse::<u64>().ok())
        .ok_or("init never said how far it read the audit record before power-off")?;
    if read != seen {
        return Err(format!(
            "init said it read the audit record to {read}, and the kernel saw it read to {seen}"
        ));
    }
    // Every record from where the reader stopped to the power action is on
    // the console: printed as unread, or in a run the ring overwrote.
    let mut shown: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for line in after.iter().skip(at + 1) {
        if let Some(rest) = line.split("audit    unread #").nth(1) {
            let number = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok());
            shown.extend(number);
        } else if let Some(rest) = line.split("audit    lost #").nth(1) {
            let mut ends = rest
                .trim()
                .split("..#")
                .filter_map(|n| n.parse::<u64>().ok());
            if let (Some(first), Some(last)) = (ends.next(), ends.next()) {
                shown.extend(first..=last);
            }
        }
    }
    if let Some(missing) = (seen..=record).find(|number| !shown.contains(number)) {
        return Err(format!(
            "the power-off was record {record} and the reader read to {seen}, but the kernel \
             never printed record {missing}"
        ));
    }
    Ok(())
}

/// A boot with `ferrix.checks=skip` records that its self-checks were
/// skipped (AUDIT.md §6), which no check inside that boot can see: its own
/// `svc audit` shows the checks' configuration record saying 0.
///
/// Verifies: H.AUD.12
fn checks_skipped(
    arch: Arch,
    args: &Args,
    loader: &Path,
    kernel: &cargo::Kernel,
    archive: &[u8],
) -> Result<()> {
    let options = format!("{} ferrix.checks=skip\n", command_line().trim_end());
    let image = fat::write_image_with(arch, loader, kernel, archive, Some(&options))?;
    let volume = btrfs_disk::blank_copy(arch, VOLUME)?;
    let mut with_volume = args.clone();
    with_volume.data_image = Some(volume);
    with_volume.data_image_kept = true;
    println!("  {arch}: booting again with ferrix.checks=skip, to read that it was recorded");
    let mut failures: Vec<String> = Vec::new();
    let _ = qemu::watch_then(
        arch,
        &image,
        kernel,
        &with_volume,
        qemu::UNCHECKED_MARKER,
        |at| {
            let deadline = Instant::now() + PATIENCE * 2;
            let up = at.read_more(deadline, |lines| {
                has(lines, "multi-user.target: active") && has(lines, BANNER)
            })?;
            if !up {
                failures.push("the unchecked boot never reached a getty".into());
                return Ok(());
            }
            if !at.wait_for_shell(Instant::now() + PATIENCE)? {
                failures.push("the unchecked boot's shell never answered at the console".into());
                return Ok(());
            }
            let lines = audit_lines(at)?;
            if !recorded(&lines, "CONFIG", "detail 1 0 0") {
                failures.push(
                    "a boot with ferrix.checks=skip did not record that its checks were skipped"
                        .into(),
                );
            }
            at.type_in(b"svc poweroff\n")?;
            let deadline = Instant::now() + PATIENCE * 4;
            let _ = at.read_more(deadline, |lines| has(lines, POWER_DOWN))?;
            Ok(())
        },
    )?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!("{arch}: {}", failures.join("; "))))
    }
}

/// The directory (L8, stage five): the kernel greeted init on its
/// bootstrap channel; `asker.service`, which declares `Uses=ferrix.test`,
/// opened it and was answered by `pong.service`, a native service started
/// by that OPEN in its own cgroup; `rogue.service`, which does not declare
/// it, was REFUSED; and `pong-as-user.service`, `Type=native` with
/// `User=ferrix`, runs as uid and gid 1000 in every role: made by a helper
/// that became the user, since a native process runs as its maker (P0b).
fn directory(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    if !has(
        &everything(at),
        "init     the kernel greeted init, version 1",
    ) {
        failures.push("init did not read the kernel's hello on its bootstrap channel".into());
    }
    let deadline = Instant::now() + PATIENCE;
    let both = |lines: &[String]| {
        let said = |unit: &str, what: &str| {
            lines
                .iter()
                .any(|line| line.contains(&format!("{unit}[")) && line.contains(what))
        };
        said("asker.service", "dir-") && said("rogue.service", "dir-")
    };
    let _ = at.read_more(deadline, both)?;
    let all = everything(at);
    let line_of = |unit: &str| {
        all.iter()
            .find(|line| line.contains(&format!("{unit}[")) && line.contains("dir-"))
            .map(|line| line.trim().to_owned())
    };
    match line_of("asker.service") {
        Some(line) if line.ends_with("dir-answer: pong ferrix.test to asker.service") => {}
        other => failures.push(format!(
            "asker.service's OPEN of ferrix.test was not answered by pong.service: {other:?}"
        )),
    }
    match line_of("rogue.service") {
        Some(line) if line.contains("dir-refused: ferrix.test") => {}
        other => failures.push(format!(
            "rogue.service's OPEN of a name it does not declare was not REFUSED: {other:?}"
        )),
    }
    let main = ask(at, "svc status pong.service\n", "Main PID: ")?
        .and_then(|line| line.trim().strip_prefix("Main PID: ").map(str::to_owned));
    let listed = ask(
        at,
        "read p < /sys/fs/cgroup/system.slice/pong.service/cgroup.procs; m=pong; echo \"$m-proc $p\"\n",
        "pong-proc ",
    )?
    .and_then(|line| line.trim().strip_prefix("pong-proc ").map(str::to_owned));
    match (main, listed) {
        (Some(main), Some(listed)) if main == listed && !main.is_empty() => {}
        (main, listed) => failures.push(format!(
            "pong.service's native process is not the one in its cgroup: main {main:?}, \
             cgroup.procs {listed:?}"
        )),
    }
    let _ = ask(
        at,
        "svc start pong-as-user.service; m=started; echo \"$m-as\"\n",
        "started-as",
    )?;
    for (field, name) in [("Uid:", "uid"), ("Gid:", "gid")] {
        let asked = format!(
            "read p < /sys/fs/cgroup/system.slice/pong-as-user.service/cgroup.procs; \
             m={name}; while read k r e s f; do [ \"$k\" = {field} ] && \
             echo \"$m-of $r $e $s $f\"; done < /proc/$p/status\n"
        );
        let answer = format!("{name}-of ");
        match ask(at, &asked, &answer)? {
            Some(line) if line.trim() == format!("{name}-of 1000 1000 1000 1000") => {}
            other => failures.push(format!(
                "pong-as-user.service, Type=native with User=ferrix, does not run as 1000 in \
                 every {name} role: {other:?}"
            )),
        }
    }
    Ok(())
}

/// What each run of the sandboxing stage's script prints last.
const SANDBOX_DONE: [&str; 2] = ["boxed-done", "open-done"];

/// The sandboxing keys (L13, §4.5). `boxed.service`, uid 1000 with
/// `NoNewPrivileges=yes`, `PrivateTmp=yes` and `ProtectSystem=strict`, and
/// `open.service`, the same script with none of them, each run a
/// set-uid root shell and look from inside:
///
/// * its ids: through the set-uid bit `open.service` must become
///   effective root, so that `boxed.service` staying 1000 in both shows
///   `no_new_privs`, and `NoNewPrivs:` in its `status` must be 1 where the
///   kernel has the line (S3 adds it);
/// * `/tmp`: a file the prompt put in the machine's `/tmp` must be missing
///   from the sandboxed one's, which it can write, and what it writes there
///   must not reach the machine's;
/// * `/run/l13-open`, a directory the prompt made mode 0777, and so a
///   write only a read-only mount can refuse: the sandboxed one's write
///   must be refused, and must not reach the machine.
///
/// Then `netns.service` and `filtered.service`, which ask for the two keys
/// whose kernel half has not landed, must be refused with the reason, and
/// must not run.
fn sandboxing(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let before = at.after().len();
    at.type_in(
        b"echo host > /tmp/l13-host; mkdir -m 777 /run/l13-open; \
          svc start open.service; svc start boxed.service\n",
    )?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        SANDBOX_DONE.iter().all(|done| {
            lines
                .get(before..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(".service[") && line.trim_end().ends_with(done))
        })
    })?;
    let said = since(at, before).to_vec();
    let judged = judge_sandbox(&said);
    if judged.is_empty() {
        println!(
            "  sandboxing: NoNewPrivileges= kept a set-uid shell at uid 1000, PrivateTmp= gave \
             a /tmp of its own, ProtectSystem=strict refused a write to a 0777 directory"
        );
    }
    failures.extend(judged);

    let leaked = ask(
        at,
        "m=l13; for f in /tmp/l13-boxed /run/l13-open/boxed /tmp/l13-open /run/l13-open/open; do \
         [ -e $f ] && echo \"$m-there $f\"; done; echo \"$m-looked\"\n",
        "l13-looked",
    )?;
    let there = since(at, before)
        .iter()
        .filter_map(|line| line.trim().strip_prefix("l13-there "))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if leaked.is_none() {
        failures.push("the machine's /tmp and /run/l13-open were never looked at".into());
    }
    for (path, wanted) in [
        ("/tmp/l13-boxed", false),
        ("/run/l13-open/boxed", false),
        ("/tmp/l13-open", true),
        ("/run/l13-open/open", true),
    ] {
        if there.iter().any(|seen| seen == path) != wanted {
            failures.push(format!(
                "{path} is {} the machine after the sandboxing stage, where it should {}be",
                if wanted { "missing from" } else { "on" },
                if wanted { "" } else { "not " }
            ));
        }
    }

    let before = at.after().len();
    at.type_in(b"svc start netopen.service; svc start netns.service\n")?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        ["netns-net-done", "netopen-net-done"].iter().all(|done| {
            lines
                .get(before..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(".service[") && line.trim_end().ends_with(done))
        })
    })?;
    let judged = judge_network(since(at, before));
    if judged.is_empty() {
        println!(
            "  sandboxing: PrivateNetwork= gave a namespace with only lo, up, where the \
             machine's loopback services cannot be reached"
        );
    }
    failures.extend(judged);

    let before = at.after().len();
    let starts: Vec<String> = FILTERED
        .iter()
        .map(|(unit, _)| format!("svc start {unit}"))
        .collect();
    at.type_in(format!("{}\n", starts.join("; ")).as_bytes())?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        FILTERED.iter().all(|(unit, prefix)| {
            lines.get(before..).unwrap_or_default().iter().any(|line| {
                line.contains(&format!("{unit}[")) && line.contains(&format!("{prefix}-done"))
            })
        })
    })?;
    let judged = judge_filters(since(at, before));
    if judged.is_empty() {
        println!(
            "  sandboxing: SystemCallFilter= killed a denied mkdir, answered EPERM and \
             SystemCallErrorNumber='s EACCES, and an allow-list of @system-service ran a \
             shell and killed sethostname"
        );
    }
    failures.extend(judged);
    Ok(())
}

/// What `netns.service` (`PrivateNetwork=yes`) and `netopen.service` (the
/// same script without it) said, judged; one line per thing wrong. Inside
/// the namespace `lo` must be up (its route is there), it must be the only
/// interface, and `echo.socket` on the machine's 127.0.0.1:7777 must not
/// answer; outside, the same connection must be answered, which shows the
/// check could see the difference.
fn judge_network(said: &[String]) -> Vec<String> {
    let lines_of = |unit: &str| -> Vec<String> {
        said.iter()
            .filter_map(|line| {
                let (_, after) = line.split_once(&format!("{unit}["))?;
                let (_, text) = after.split_once("]: ")?;
                Some(text.trim().to_owned())
            })
            .collect()
    };
    let inside = lines_of("netns.service");
    let outside = lines_of("netopen.service");
    let says = |lines: &[String], text: &str| lines.iter().any(|line| line == text);
    let mut wrong = Vec::new();
    for (unit, lines, prefix) in [
        ("netns.service", &inside, "netns"),
        ("netopen.service", &outside, "netopen"),
    ] {
        if !says(lines, &format!("{prefix}-net-done")) {
            wrong.push(format!("{unit} never finished its checks: {lines:?}"));
        }
    }
    if !says(&inside, "netns-lo-route 1") {
        wrong.push(format!(
            "netns.service's lo is not up in its network namespace: {inside:?}"
        ));
    }
    let devices: Vec<&String> = inside
        .iter()
        .filter(|line| line.starts_with("netns-dev "))
        .collect();
    if devices.len() != 1 || !says(&inside, "netns-dev lo:") {
        wrong.push(format!(
            "netns.service has other interfaces than lo, so it is not in a namespace of its \
             own: {devices:?}"
        ));
    }
    if says(&inside, "echoed ping") {
        wrong.push(
            "netns.service, with PrivateNetwork=yes, reached the machine's echo.socket on \
             127.0.0.1:7777"
                .into(),
        );
    }
    if !says(&outside, "echoed ping") {
        wrong.push(format!(
            "netopen.service could not reach echo.socket, so nothing shows PrivateNetwork= \
             at work: {outside:?}"
        ));
    }
    wrong
}

/// The filter checks' units and the prefix each prints.
const FILTERED: [(&str, &str); 5] = [
    ("sc-plain.service", "plain"),
    ("sc-kill.service", "kill"),
    ("sc-eperm.service", "eperm"),
    ("sc-errno.service", "errno"),
    ("sc-allow.service", "allow"),
];

/// What the filter checks' units said, judged; one line per thing wrong.
/// Each runs `mkdir` and `hostname` (`sethostname`, refused `EPERM` to uid
/// 1000) as uid 1000 and prints their statuses, 159 being death by
/// `SIGSYS`, and its `NoNewPrivs:` and `Seccomp:` lines. `sc-plain.service`
/// has no filter and shows what each would be without one.
fn judge_filters(said: &[String]) -> Vec<String> {
    let lines_of = |unit: &str| -> Vec<String> {
        said.iter()
            .filter_map(|line| {
                let (_, after) = line.split_once(&format!("{unit}["))?;
                let (_, text) = after.split_once("]: ")?;
                Some(text.trim().to_owned())
            })
            .collect()
    };
    // unit, prefix, mkdir, hostname, Seccomp, NoNewPrivs
    let wanted: [[&str; 6]; 5] = [
        ["sc-plain.service", "plain", "0", "1", "0", "0"],
        ["sc-kill.service", "kill", "159", "1", "2", "1"],
        ["sc-eperm.service", "eperm", "1", "1", "2", "1"],
        ["sc-errno.service", "errno", "1", "1", "2", "1"],
        ["sc-allow.service", "allow", "0", "159", "2", "1"],
    ];
    let mut wrong = Vec::new();
    for [unit, prefix, mkdir, hostname, seccomp, nnp] in wanted {
        let message = match prefix {
            "eperm" => Some("Operation not permitted"),
            "errno" => Some("Permission denied"),
            _ => None,
        };
        let lines = lines_of(unit);
        let expect = [
            (format!("{prefix}-done"), "finished its checks"),
            (
                format!("{prefix}-mkdir {mkdir}"),
                "had mkdir end as expected",
            ),
            (
                format!("{prefix}-hostname {hostname}"),
                "had hostname end as expected",
            ),
            (
                format!("{prefix}-Seccomp: {seccomp}"),
                "showed the expected Seccomp: mode",
            ),
            (
                format!("{prefix}-NoNewPrivs: {nnp}"),
                "showed the expected NoNewPrivs:",
            ),
        ];
        for (text, what) in expect {
            if !lines.contains(&text) {
                wrong.push(format!("{unit} never {what} (`{text}`): {lines:?}"));
            }
        }
        if let Some(message) = message
            && !lines.iter().any(|line| line.contains(message))
        {
            wrong.push(format!("{unit}'s mkdir did not say `{message}`: {lines:?}"));
        }
    }
    wrong
}

/// What the two runs of the sandboxing script said, judged; one line per
/// thing wrong.
fn judge_sandbox(said: &[String]) -> Vec<String> {
    let lines_of = |unit: &str| -> Vec<String> {
        said.iter()
            .filter_map(|line| {
                let (_, after) = line.split_once(&format!("{unit}["))?;
                let (_, text) = after.split_once("]: ")?;
                Some(text.trim().to_owned())
            })
            .collect()
    };
    let value = |lines: &[String], key: &str| {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(key).map(str::to_owned))
    };
    let says = |lines: &[String], text: &str| lines.iter().any(|line| line == text);
    let boxed = lines_of("boxed.service");
    let open = lines_of("open.service");
    let mut wrong = Vec::new();
    for (unit, lines, prefix) in [
        ("boxed.service", &boxed, "boxed"),
        ("open.service", &open, "open"),
    ] {
        if !says(lines, &format!("{prefix}-done")) {
            wrong.push(format!("{unit} never finished its checks: {lines:?}"));
        }
        if !says(lines, &format!("{prefix}-tmp-writable")) {
            wrong.push(format!("{unit} could not write to its /tmp"));
        }
    }
    match value(&open, "open-ids ").as_deref() {
        Some("1000 0") => {}
        other => wrong.push(format!(
            "open.service's set-uid shell did not become effective root, so nothing shows \
             NoNewPrivileges= at work: ids {other:?}"
        )),
    }
    match value(&boxed, "boxed-ids ").as_deref() {
        Some("1000 1000") => {}
        other => wrong.push(format!(
            "boxed.service, with NoNewPrivileges=yes, gained a privilege through a set-uid \
             program: ids {other:?}, not 1000 1000"
        )),
    }
    for (unit, lines, key, wanted) in [
        ("boxed.service", &boxed, "boxed-nnp ", "1"),
        ("open.service", &open, "open-nnp ", "0"),
    ] {
        if let Some(flag) = value(lines, key)
            && flag != wanted
        {
            wrong.push(format!(
                "{unit}'s status says NoNewPrivs: {flag}, not {wanted}"
            ));
        }
    }
    for (lines, text, why) in [
        (
            &boxed,
            "boxed-tmp-private",
            "boxed.service's /tmp is not private",
        ),
        (
            &open,
            "open-tmp-shared",
            "open.service did not see the machine's /tmp",
        ),
        (
            &boxed,
            "boxed-run-refused",
            "boxed.service, under ProtectSystem=strict, wrote to a 0777 directory in /run",
        ),
        (
            &open,
            "open-run-writable",
            "open.service could not write to /run/l13-open",
        ),
    ] {
        if !says(lines, text) {
            wrong.push(format!("{why}: {lines:?}"));
        }
    }
    wrong
}

/// `devmgr` started by pid 1 (L12, §7.3), under `ferrix.devmgr=init`: the
/// kernel gave init the starter and init only a process handle back; `/`
/// moved onto the fresh root disk with pid 1 once `devmgr` had reported;
/// `devmgr.service` is active, and a `svc restart` of it has the kernel start
/// a second `devmgr` once the first and its drivers have gone, with init
/// removing the first's cgroup and not saying it could not.
///
/// Verifies: L.quiesce.4
fn devmgr_by_init(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let all = everything(at);
    for (line, what) in [
        (
            "init     the kernel greeted init, version 1, and gave it devmgr's starter",
            "init was not given devmgr's starter",
        ),
        (
            "devmgr   pid 1 got only a process handle",
            "the kernel did not check that pid 1 got only a process handle to devmgr",
        ),
        (
            "devmgr   started by pid 1:",
            "the devmgr pid 1 started did not report",
        ),
        (
            "root     pid 1 moved onto the volume with the switch",
            "pid 1 was not moved onto the root volume",
        ),
        (
            "init     / is the root volume now",
            "init was not told / is the root volume",
        ),
    ] {
        if !has(&all, line) {
            failures.push(what.into());
        }
    }
    match ask(at, "svc status devmgr.service\n", "Active: ")? {
        Some(line) if line.contains("Active: active") => {}
        other => failures.push(format!("devmgr.service is not active: {other:?}")),
    }
    let started = |lines: &[String]| {
        lines
            .iter()
            .filter(|line| line.contains("devmgr   started by pid 1:"))
            .count()
    };
    let before = started(at.after());
    let restart = at.after().len();
    at.type_in(b"svc restart devmgr.service\n")?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| started(lines) > before)?;
    if started(at.after()) <= before {
        failures.push("svc restart devmgr.service did not start a second devmgr".into());
    }
    match ask(at, "svc status devmgr.service\n", "Active: ")? {
        Some(line) if line.contains("Active: active") => {}
        other => failures.push(format!(
            "devmgr.service is not active after its restart: {other:?}"
        )),
    }
    // The old devmgr's drivers' jobs are held a moment past its end, and
    // init waits for them to go rather than failing to remove its cgroup.
    let since = at.after().get(restart..).unwrap_or_default();
    if has(since, "devmgr.service: removing its cgroup") {
        failures.push("init could not remove devmgr.service's cgroup on its restart".into());
    }
    Ok(())
}

/// Resources (L5, stage three): `hog.service`, in `test.slice` under a
/// 16 MiB `MemoryMax=`, grows until the kernel's OOM kill takes it, and is
/// reported `failed (oom-kill)` while its siblings run on; `tasks.service`'s
/// forks past `TasksMax=3` are refused and counted in its `pids.events`; a
/// process started at the prompt is grouped into a scope by `svc scope`;
/// and `deleg.service`, uid 1000 with `Delegate=yes`, makes a cgroup in its
/// own.
fn resources(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let deadline = Instant::now() + PATIENCE;
    let killed = at.read_more(deadline, |lines| {
        has(lines, "hog.service: failed (oom-kill)")
    })?;
    if !killed {
        failures
            .push("hog.service, past its MemoryMax=, was not reported failed (oom-kill)".into());
    }
    match ask(at, "svc status hog.service\n", "CGroup: ")? {
        Some(line) if line.trim() == "CGroup: /test.slice/hog.service" => {}
        other => failures.push(format!(
            "hog.service's cgroup is not under test.slice: {other:?}"
        )),
    }
    let (_, running) = status_active(at, "echoer.service")?;
    if !running {
        failures.push("the OOM kill in hog.service reached echoer.service too".into());
    }

    let counted = ask(
        at,
        "m=tasks; while read k v; do [ \"$k\" = max ] && echo \"$m-max $v\"; done \
         < /sys/fs/cgroup/system.slice/tasks.service/pids.events\n",
        "tasks-max ",
    )?;
    let refused = counted
        .as_deref()
        .and_then(|line| line.trim().strip_prefix("tasks-max "))
        .and_then(|count| count.trim().parse::<u64>().ok())
        .is_some_and(|count| count >= 1);
    if !refused {
        failures.push(format!(
            "tasks.service's forks past TasksMax=3 were not refused and counted: {counted:?}"
        ));
    }

    let scoped = ask(
        at,
        "read x < /dev/ptmx & p=$!; svc scope --unit probe.scope $p; read c < /proc/$p/cgroup; \
         m=scope; echo \"$m-cg $c\"\n",
        "scope-cg ",
    )?;
    match scoped {
        Some(line) if line.trim() == "scope-cg 0::/system.slice/probe.scope" => {}
        other => failures.push(format!(
            "svc scope did not move the process into probe.scope: {other:?}"
        )),
    }

    let deadline = Instant::now() + PATIENCE;
    let delegated = at.read_more(deadline, |lines| {
        lines
            .iter()
            .any(|line| line.contains("deleg.service[") && line.contains("delegated-ok"))
    })?;
    if !delegated {
        failures.push(
            "deleg.service, uid 1000 with Delegate=yes, could not make a cgroup in its own".into(),
        );
    }
    Ok(())
}

/// Whether `svc status` says `unit` is active and running, with the lines
/// it printed.
fn status_active(at: &mut Watching<'_>, unit: &str) -> Result<(Vec<String>, bool)> {
    let before = at.after().len();
    let _ = ask(at, &format!("svc status {unit}\n"), "Active: ")?;
    let shown: Vec<String> = at
        .after()
        .iter()
        .skip(before)
        .map(|l| l.trim().to_owned())
        .collect();
    let running = shown.iter().any(|line| line == "Active: active (running)");
    Ok((shown, running))
}

/// Socket activation (L9): nothing of `hello.service` runs until a
/// connection reaches `hello.socket`, and then it runs with the listening
/// socket as descriptor 3 (`LISTEN_FDS=1`, its name, and `LISTEN_PID` its
/// own pid); `echo.socket` (`Accept=yes`) answers each connection with an
/// instance of `echo@.service` whose standard streams are the connection.
fn sockets(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let before = at.after().len();
    let _ = ask(at, "svc status hello.service\n", "Active: ")?;
    if !at
        .after()
        .iter()
        .skip(before)
        .any(|line| line.trim() == "Active: inactive (dead)")
    {
        failures.push("hello.service ran before any connection reached hello.socket".into());
    }
    let wanted = "hello.service[";
    let before = at.after().len();
    at.type_in(b"nc 127.0.0.1 7778 < /dev/null &\n")?;
    let deadline = Instant::now() + PATIENCE;
    let _ = at.read_more(deadline, |lines| {
        lines
            .get(before..)
            .unwrap_or_default()
            .iter()
            .any(|line| line.contains(wanted) && line.contains("hello-fds"))
    })?;
    let said = at
        .after()
        .iter()
        .skip(before)
        .find(|line| line.contains(wanted) && line.contains("hello-fds"))
        .cloned();
    match said {
        Some(line) if line.contains("hello-fds 1 hello.socket pid-ok-1") => {}
        Some(line) => failures.push(format!(
            "hello.service was started without the socket as sd_listen_fds says it: `{}`",
            line.trim()
        )),
        None => failures.push("a connection to hello.socket did not start hello.service".into()),
    }
    match ask(at, "echo ping | nc 127.0.0.1 7777\n", "echoed ")? {
        Some(line) if line.trim() == "echoed ping" => {}
        Some(line) => failures.push(format!("echo.socket's instance said `{}`", line.trim())),
        None => failures.push(
            "a connection to echo.socket was not answered by an echo@.service instance".into(),
        ),
    }
    Ok(())
}

/// What `svc status lazy.service` shows once lazy.service has said its
/// STATUS=.
const LAZY_STATUS: &str = "Status: \"still-starting\"";

/// Readiness (L7): `notifier.service` became active on its `READY=1` and
/// shows its last `STATUS=`; `lazy.service`, which never says `READY=1`,
/// stays activating with its status shown; `daemon.service` forked, and its
/// main pid is the one its `PIDFile=` names.
fn readiness(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let before = at.after().len();
    let _ = ask(at, "svc status notifier.service\n", "Status: ")?;
    let shown: Vec<String> = at
        .after()
        .iter()
        .skip(before)
        .map(|l| l.trim().to_owned())
        .collect();
    if !shown.iter().any(|line| line == "Active: active (running)") {
        failures.push("notifier.service did not become active on READY=1".into());
    }
    if !shown.iter().any(|line| line == "Status: \"serving\"") {
        failures.push("svc status did not show notifier.service's last STATUS=, serving".into());
    }

    at.type_in(b"svc start lazy.service &\n")?;
    // Asked until lazy.service has said its STATUS=, which is what it is
    // judged at: it never says READY=1, so it must still be activating then.
    let deadline = Instant::now() + PATIENCE;
    let shown = loop {
        let before = at.after().len();
        let _ = ask(
            at,
            "svc status lazy.service; m=lazy; echo \"$m-shown\"\n",
            "lazy-shown",
        )?;
        let shown: Vec<String> = at
            .after()
            .iter()
            .skip(before)
            .map(|l| l.trim().to_owned())
            .collect();
        if shown.iter().any(|line| line == LAZY_STATUS) || Instant::now() >= deadline {
            break shown;
        }
        thread::sleep(POLL);
    };
    if !shown
        .iter()
        .any(|line| line == "Active: activating (start)")
    {
        failures.push(
            "lazy.service, which never says READY=1, was not left activating: readiness is \
             not waited for"
                .into(),
        );
    }
    if !shown.iter().any(|line| line == LAZY_STATUS) {
        failures.push("svc status did not show lazy.service's STATUS= before readiness".into());
    }

    let written = ask(
        at,
        "m=daemon; read d < /run/daemon.pid; echo \"$m-pid $d\"\n",
        "daemon-pid ",
    )?
    .and_then(|line| line.trim().strip_prefix("daemon-pid ").map(str::to_owned));
    let before = at.after().len();
    let main = ask(at, "svc status daemon.service\n", "Main PID: ")?
        .and_then(|line| line.trim().strip_prefix("Main PID: ").map(str::to_owned));
    let active = at
        .after()
        .iter()
        .skip(before)
        .any(|line| line.trim() == "Active: active (running)");
    match (written, main) {
        (Some(written), Some(main)) if written == main && active => {}
        (written, main) => failures.push(format!(
            "daemon.service's main pid is {main:?} and active is {active}, where its PIDFile= \
             says {written:?}"
        )),
    }
    Ok(())
}

/// Stage four: `svc` at the prompt. `status` gives `echoer.service`'s main
/// pid, `log` the line that process wrote, `restart` a new main pid, and
/// the same `svc stop` made as uid 1000 through `su` is refused and leaves
/// the service running.
fn control(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let first = main_pid(at, failures, "before the restart")?;
    if let Some(pid) = first {
        let wanted = format!("[{pid}] echoer-up {pid}");
        if ask(at, "svc log echoer.service\n", &wanted)?.is_none() {
            failures.push(format!(
                "svc log echoer.service did not show `{wanted}`, the line its main process wrote"
            ));
        }
    }
    match ask(
        at,
        "svc restart echoer.service; r=$?; m=restart; echo \"$m-status $r\"\n",
        "restart-status ",
    )? {
        Some(line) if line.trim() == "restart-status 0" => {}
        Some(line) => failures.push(format!("svc restart failed: `{}`", line.trim())),
        None => failures.push("svc restart never returned".into()),
    }
    let second = main_pid(at, failures, "after the restart")?;
    if first.is_some() && first == second {
        failures.push(format!(
            "svc restart left echoer.service's main pid at {first:?}"
        ));
    }
    let refused = ask(
        at,
        "su ferrix -c 'svc stop echoer.service'; r=$?; m=su; echo \"$m-status $r\"\n",
        "su-status ",
    )?;
    match refused {
        Some(line) if line.trim() == "su-status 1" => {}
        Some(line) => failures.push(format!(
            "svc stop as uid 1000 was not refused: `{}`",
            line.trim()
        )),
        None => failures.push("svc stop as uid 1000 never returned".into()),
    }
    if !has(at.after(), "Permission denied") {
        failures.push("the refusal of uid 1000's svc stop did not say why".into());
    }
    if main_pid(at, failures, "after uid 1000's svc stop")? != second {
        failures.push("uid 1000's refused svc stop changed echoer.service anyway".into());
    }
    administer(at, failures)
}

/// The rest of stage four: `set-property` writes a limit to the running
/// cgroup, `enable` and `disable` make and remove the link `[Install]`
/// names, and `top` lists the units with a cgroup.
fn administer(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let limit = ask(
        at,
        "svc set-property echoer.service TasksMax=7; \
         read n < /sys/fs/cgroup/system.slice/echoer.service/pids.max; m=prop; echo \"$m-max $n\"\n",
        "prop-max ",
    )?;
    match limit {
        Some(line) if line.trim() == "prop-max 7" => {}
        other => failures.push(format!(
            "svc set-property TasksMax=7 did not reach echoer.service's pids.max: {other:?}"
        )),
    }
    let link = "/etc/ferrix/units/multi-user.target.wants/spare.service";
    let enabled = ask(
        at,
        &format!("svc enable spare.service; [ -L {link} ] && m=on || m=off; echo \"enable-$m\"\n"),
        "enable-",
    )?;
    if enabled.as_deref().map(str::trim) != Some("enable-on") {
        failures.push(format!(
            "svc enable spare.service did not make {link}: {enabled:?}"
        ));
    }
    let disabled = ask(
        at,
        &format!(
            "svc disable spare.service; [ -L {link} ] && m=on || m=off; echo \"disable-$m\"\n"
        ),
        "disable-",
    )?;
    if disabled.as_deref().map(str::trim) != Some("disable-off") {
        failures.push(format!(
            "svc disable spare.service left {link}: {disabled:?}"
        ));
    }
    if ask(at, "svc top\n", "echoer.service ")?.is_none() {
        failures.push("svc top did not list echoer.service".into());
    }
    Ok(())
}

/// Every line of the boot so far, the marker's and what came after it.
fn everything(at: &Watching<'_>) -> Vec<String> {
    at.lines().iter().chain(at.after()).cloned().collect()
}

/// `sshdt`, the units that run it under socket activation on port 2200, and
/// the link that has `multi-user.target` listen for it.
fn sshd_files() -> Result<Vec<File>> {
    let (mut files, exec) = crate::ssh::test_server(Arch::X86_64)?;
    let units = [
        (
            "sshd.socket",
            "[Unit]\n\
             Description=Starts sshd on the first connection to port 2200\n\
             \n\
             [Socket]\n\
             ListenStream=0.0.0.0:2200\n"
                .to_owned(),
        ),
        (
            "sshd.service",
            format!(
                "[Unit]\n\
                 Description=sshdt, given its listening socket by init\n\
                 \n\
                 [Service]\n\
                 ExecStart={exec}\n"
            ),
        ),
    ];
    for (name, text) in units {
        files.push(File {
            path: format!("etc/ferrix/units/{name}"),
            mode: 0o644,
            content: Content::Bytes(text.into_bytes()),
        });
    }
    files.push(File {
        path: "etc/ferrix/units/multi-user.target.wants/sshd.socket".to_owned(),
        mode: 0o777,
        content: Content::Link("/etc/ferrix/units/sshd.socket".to_owned()),
    });
    Ok(files)
}

/// L9's gate as designed (§15): `sshd.service` does not run until a
/// connection reaches `sshd.socket`; the first one starts it with the socket
/// passed as `LISTEN_FDS`, and it answers on that same connection with its
/// SSH banner. The socket is on port 2200 and `sshdt` would bind 127.0.0.1:2222 of its
/// own accord, so only an `sshdt` that took the passed socket can answer.
fn sshd_activated(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    match sshd_state(at)? {
        Some(line) if line.contains("inactive") => {}
        other => failures.push(format!(
            "sshd.service ran before any connection reached sshd.socket: {other:?}"
        )),
    }
    let banner = ask(
        at,
        "echo | nc -w 5 127.0.0.1 2200 | { read b; m=sshd; echo \"$m-said $b\"; }\n",
        "sshd-said ",
    )?;
    match banner {
        Some(line) if line.trim().starts_with("sshd-said SSH-2.0-") => {}
        other => failures.push(format!(
            "a connection to sshd.socket's port 2200 was not answered by sshd's banner: {other:?}"
        )),
    }
    match sshd_state(at)? {
        Some(line) if line.contains("active (running)") => {}
        other => failures.push(format!(
            "sshd.service is not running after the connection that started it: {other:?}"
        )),
    }
    Ok(())
}

/// `svc status sshd.service`'s `Active:` line, read once the report's last
/// line, its own `CGroup:`, has come, so none of it is left for the next
/// question and no earlier report's is taken for it.
/// It may follow the prompt on one serial line, as init's own lines can.
fn sshd_state(at: &mut Watching<'_>) -> Result<Option<String>> {
    let before = at.after().len();
    let _ = ask(
        at,
        "svc status sshd.service\n",
        "CGroup: /system.slice/sshd.service",
    )?;
    Ok(at.after().iter().skip(before).find_map(|line| {
        let at = line.find("Active: ")?;
        line.get(at..).map(|active| active.trim().to_owned())
    }))
}

/// `echoer.service`'s main pid, from `svc status`, which must also say it
/// is active and running.
fn main_pid(at: &mut Watching<'_>, failures: &mut Vec<String>, when: &str) -> Result<Option<u32>> {
    let before = at.after().len();
    let Some(line) = ask(at, "svc status echoer.service\n", "Main PID: ")? else {
        failures.push(format!("svc status echoer.service gave no main pid {when}"));
        return Ok(None);
    };
    let active = at
        .after()
        .iter()
        .skip(before)
        .any(|line| line.trim() == "Active: active (running)");
    if !active {
        failures.push(format!(
            "svc status did not say echoer.service was active {when}"
        ));
    }
    Ok(line
        .trim()
        .strip_prefix("Main PID: ")
        .and_then(|pid| pid.trim().parse().ok()))
}

/// Write to `/data`, so `btrfs check` reads a volume that was written, then
/// `poweroff` and wait for the power to go: the name a person types, which is
/// `svc poweroff` through its link (the unchecked boot types `svc`'s own).
fn power_off(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    if ask(
        at,
        "m=data; echo \"$m kept\" > /data/init-test; echo \"$m-written\"\n",
        "data-written",
    )?
    .is_none()
    {
        failures.push("the shell could not write /data/init-test".into());
    }
    at.type_in(b"poweroff\n")?;
    let deadline = Instant::now() + PATIENCE * 4;
    let _ = at.read_more(deadline, |lines| has(lines, POWER_DOWN))?;
    Ok(())
}

/// Type `keys` and wait for a line starting with `answer` after them;
/// return that line.
fn ask(at: &mut Watching<'_>, keys: &str, answer: &str) -> Result<Option<String>> {
    let before = at.after().len();
    at.type_in(keys.as_bytes())?;
    let deadline = Instant::now() + PATIENCE;
    let found = |lines: &[String]| {
        lines
            .get(before..)
            .unwrap_or_default()
            .iter()
            .find(|line| starts(line, answer))
            .cloned()
    };
    let _ = at.read_more(deadline, |lines| found(lines).is_some())?;
    Ok(found(at.after()))
}

/// Whether `line` answers with `answer`: the guest's own line starting with
/// it, or one of init's lines saying it. Init's line may follow a prompt on
/// the same line, since the console interleaves init's output with the
/// shell's.
fn starts(line: &str, answer: &str) -> bool {
    line.trim_start().starts_with(answer) || line.contains(&format!("init     {answer}"))
}

/// Whether any line holds `text`.
fn has(lines: &[String], text: &str) -> bool {
    lines.iter().any(|line| line.contains(text))
}

/// The lines from the boot marker on.
fn after_marker(lines: &[String]) -> &[String] {
    lines
        .iter()
        .position(|line| line.contains(SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default()
}

/// The kernel started init from the file, and init booted.
fn judge_boot(after: &[String]) -> std::result::Result<(), String> {
    let started = format!("init     starting {PATH}");
    if !after.iter().any(|line| line.trim() == started) {
        return Err(format!("the kernel never said `{started}`"));
    }
    if !has(after, "init     booting ") {
        return Err("init never said which target it was booting".into());
    }
    Ok(())
}

/// No unit file, shipped or the test's, drew a warning as it loaded.
fn judge_units(after: &[String]) -> std::result::Result<(), String> {
    let warned: Vec<&str> = after
        .iter()
        .map(|line| line.trim())
        .filter(|line| {
            [
                "/lib/ferrix/units/",
                "/etc/ferrix/units/",
                "/run/ferrix/units/",
            ]
            .iter()
            .any(|directory| line.starts_with(&format!("init     {directory}")))
        })
        .collect();
    if warned.is_empty() {
        Ok(())
    } else {
        Err(format!("unit files drew warnings: {warned:?}"))
    }
}

/// The shell's `/proc/self/stat`, as `stat-self <the file>`: its session
/// and controlling terminal are its own.
fn judge_stat(line: &str) -> std::result::Result<(), String> {
    let text = line.trim().strip_prefix("stat-self ").unwrap_or_default();
    // The fields after `comm`, which may hold spaces, start after its `)`.
    let (Some(pid), Some(rest)) = (
        text.split_whitespace().next(),
        text.rsplit_once(')').map(|(_, rest)| rest),
    ) else {
        return Err(format!(
            "the shell's /proc/self/stat did not parse: `{text}`"
        ));
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let number = |at: usize| fields.get(at).and_then(|field| field.parse::<i64>().ok());
    let Ok(pid) = pid.parse::<i64>() else {
        return Err(format!(
            "the shell's /proc/self/stat did not parse: `{text}`"
        ));
    };
    // state ppid pgrp session tty_nr tpgid, after `comm`.
    let (ppid, pgrp, session, tty, foreground) =
        (number(1), number(2), number(3), number(4), number(5));
    let mut wrong = Vec::new();
    if pid == 1 {
        wrong.push("the shell is pid 1, so init did not start it".to_owned());
    }
    if ppid != Some(1) {
        wrong.push(format!("its parent is {ppid:?}, not init"));
    }
    if session != Some(pid) {
        wrong.push(format!("its session is {session:?}, not its own ({pid})"));
    }
    if pgrp != Some(pid) {
        wrong.push(format!("its process group is {pgrp:?}, not its own"));
    }
    if tty != Some(i64::from(CONSOLE_TTY_NR)) {
        wrong.push(format!(
            "its controlling terminal is {tty:?}, not the console ({CONSOLE_TTY_NR})"
        ));
    }
    if foreground != Some(pid) {
        wrong.push(format!(
            "the console's foreground group is {foreground:?}, not the shell's"
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!("the shell's /proc/self/stat: {}", wrong.join("; ")))
    }
}

/// `flaky.service` was restarted, then failed on its start limit.
fn judge_flaky(after: &[String]) -> std::result::Result<(), String> {
    if !after
        .iter()
        .any(|line| line.contains("flaky.service: ") && line.contains("; restarting at "))
    {
        return Err("flaky.service was never restarted".into());
    }
    if !has(after, "flaky.service: failed (start-limit-hit)") {
        return Err("flaky.service was not failed by its start limit".into());
    }
    Ok(())
}

/// `poweroff` stopped everything in reverse order and powered off.
fn judge_shutdown(after: &[String]) -> std::result::Result<(), String> {
    let Some(from) = after
        .iter()
        .position(|line| line.contains("init     going down: poweroff.target"))
    else {
        return Err("poweroff (svc through its link) did not start poweroff.target".into());
    };
    let mut rest = after.iter().skip(from);
    for want in STOP_ORDER {
        if !rest.any(|line| line.contains(want)) {
            return Err(format!(
                "shutdown did not say `{want}`, or not in the order {STOP_ORDER:?}"
            ));
        }
    }
    let remaining: Vec<&String> = rest.collect();
    for want in READ_ONLY_AT_SHUTDOWN {
        if !remaining.iter().any(|line| line.contains(want)) {
            let said: Vec<&str> = remaining
                .iter()
                .filter(|line| line.contains("read-only"))
                .map(|line| line.trim())
                .collect();
            return Err(format!(
                "shutdown did not say `{want}` after its remount (F-53); it said {said:?}"
            ));
        }
    }
    if !remaining.iter().any(|line| line.trim() == POWER_DOWN) {
        return Err(format!("the machine never said `{POWER_DOWN}`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    /// What a passing sandboxing stage prints, before S3 adds `NoNewPrivs:`.
    const SANDBOXED: [&str; 10] = [
        "  open.service[301]: open-ids 1000 0",
        "  open.service[301]: open-tmp-shared",
        "  open.service[301]: open-tmp-writable",
        "  open.service[301]: open-run-writable",
        "  open.service[301]: open-done",
        "  boxed.service[302]: boxed-ids 1000 1000",
        "  boxed.service[302]: boxed-tmp-private",
        "  boxed.service[302]: boxed-tmp-writable",
        "init-test# boxed.service[302]: boxed-run-refused",
        "  boxed.service[302]: boxed-done",
    ];

    #[test]
    fn a_sandbox_that_holds_passes_with_or_without_the_status_line() {
        assert_eq!(judge_sandbox(&lines(&SANDBOXED)), Vec::<String>::new());
        let mut with = SANDBOXED.to_vec();
        with.push("  boxed.service[302]: boxed-nnp 1");
        with.push("  open.service[301]: open-nnp 0");
        assert_eq!(judge_sandbox(&lines(&with)), Vec::<String>::new());
    }

    /// What a passing `PrivateNetwork=` check prints.
    const NETWORKED: [&str; 9] = [
        "  netopen.service[401]: netopen-lo-route 1",
        "  netopen.service[401]: netopen-dev lo:",
        "  netopen.service[401]: netopen-dev eth0:",
        "  netopen.service[401]: echoed ping",
        "  netopen.service[401]: netopen-net-done",
        "  netns.service[402]: netns-lo-route 1",
        "  netns.service[402]: netns-dev lo:",
        "  netns.service[402]: nc: can't connect to remote host (127.0.0.1): Connection refused",
        "  netns.service[402]: netns-net-done",
    ];

    #[test]
    fn a_private_network_passes_and_each_leak_fails_alone() {
        assert_eq!(judge_network(&lines(&NETWORKED)), Vec::<String>::new());
        let with = |extra: &str, from: &str, to: &str| {
            let mut changed: Vec<String> = NETWORKED
                .iter()
                .map(|line| line.replace(from, to))
                .collect();
            if !extra.is_empty() {
                changed.push(extra.to_owned());
            }
            judge_network(&changed)
        };
        let down = with("", "netns-lo-route 1", "netns-lo-route 0");
        assert_eq!(down.len(), 1, "{down:?}");
        assert!(down[0].starts_with("netns.service's lo is not up"));
        let shared = with("  netns.service[402]: netns-dev eth0:", "", "");
        assert_eq!(shared.len(), 1, "{shared:?}");
        assert!(shared[0].starts_with("netns.service has other interfaces"));
        let reached = with("  netns.service[402]: echoed ping", "", "");
        assert_eq!(reached.len(), 1, "{reached:?}");
        assert!(reached[0].contains("reached the machine's echo.socket"));
    }

    /// What passing filter checks print (abridged to the judged lines).
    fn filtered() -> Vec<String> {
        let mut out = Vec::new();
        for (unit, prefix, mkdir, hostname, seccomp, nnp) in [
            ("sc-plain.service", "plain", "0", "1", "0", "0"),
            ("sc-kill.service", "kill", "159", "1", "2", "1"),
            ("sc-eperm.service", "eperm", "1", "1", "2", "1"),
            ("sc-errno.service", "errno", "1", "1", "2", "1"),
            ("sc-allow.service", "allow", "0", "159", "2", "1"),
        ] {
            for text in [
                format!("{prefix}-mkdir {mkdir}"),
                format!("{prefix}-hostname {hostname}"),
                format!("{prefix}-NoNewPrivs: {nnp}"),
                format!("{prefix}-Seccomp: {seccomp}"),
                format!("{prefix}-done"),
            ] {
                out.push(format!("  {unit}[500]: {text}"));
            }
        }
        out.push(
            "  sc-eperm.service[501]: mkdir: can't create directory '/tmp/sc-eperm': \
             Operation not permitted"
                .to_owned(),
        );
        out.push(
            "  sc-errno.service[502]: mkdir: can't create directory '/tmp/sc-errno': \
             Permission denied"
                .to_owned(),
        );
        out
    }

    #[test]
    fn filters_that_hold_pass_and_each_one_off_fails_alone() {
        assert_eq!(judge_filters(&filtered()), Vec::<String>::new());
        let swap = |from: &str, to: &str| {
            let changed: Vec<String> = filtered()
                .iter()
                .map(|line| line.replace(from, to))
                .collect();
            judge_filters(&changed)
        };
        let unkilled = swap("kill-mkdir 159", "kill-mkdir 0");
        assert_eq!(unkilled.len(), 1, "{unkilled:?}");
        assert!(unkilled[0].starts_with("sc-kill.service never had mkdir end"));
        let open = swap("allow-hostname 159", "allow-hostname 1");
        assert_eq!(open.len(), 1, "{open:?}");
        let errno = swap("Permission denied", "Operation not permitted");
        assert_eq!(errno.len(), 1, "{errno:?}");
        assert!(errno[0].starts_with("sc-errno.service's mkdir did not say"));
    }

    #[test]
    fn each_key_that_did_nothing_fails_on_its_own_line() {
        let swap = |from: &str, to: &str| {
            let changed: Vec<String> = SANDBOXED
                .iter()
                .map(|line| line.replace(from, to))
                .collect();
            judge_sandbox(&changed)
        };
        let nnp = swap("boxed-ids 1000 1000", "boxed-ids 1000 0");
        assert_eq!(nnp.len(), 1, "{nnp:?}");
        assert!(nnp[0].starts_with("boxed.service, with NoNewPrivileges=yes, gained"));
        let tmp = swap("boxed-tmp-private", "boxed-tmp-shared");
        assert_eq!(tmp.len(), 1, "{tmp:?}");
        assert!(tmp[0].starts_with("boxed.service's /tmp is not private"));
        let ro = swap("boxed-run-refused", "boxed-run-writable");
        assert_eq!(ro.len(), 1, "{ro:?}");
        assert!(ro[0].starts_with("boxed.service, under ProtectSystem=strict, wrote"));
        let control = swap("open-ids 1000 0", "open-ids 1000 1000");
        assert!(control[0].starts_with("open.service's set-uid shell did not become"));
        let mut flagged = SANDBOXED.to_vec();
        flagged.push("  boxed.service[302]: boxed-nnp 0");
        assert_eq!(judge_sandbox(&lines(&flagged)).len(), 1);
    }

    #[test]
    fn exec_words_are_quoted_only_when_they_must_be() {
        assert_eq!(exec_word("--config"), "--config");
        assert_eq!(exec_word("/hypr/hyprland.conf"), "/hypr/hyprland.conf");
        assert_eq!(exec_word("two words"), "\"two words\"");
        assert_eq!(exec_word("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(exec_word(""), "\"\"");
    }

    #[test]
    fn a_shell_that_leads_its_session_on_the_console_passes() {
        let line = "stat-self 57 (sh) S 1 57 57 1281 57 0 0";
        assert_eq!(judge_stat(line), Ok(()));
    }

    #[test]
    fn a_shell_without_the_console_fails_on_each_field() {
        let why = judge_stat("stat-self 57 (sh) S 1 57 57 0 -1 0").unwrap_err();
        assert!(why.contains("controlling terminal"), "{why}");
        assert!(why.contains("foreground"), "{why}");
        let why = judge_stat("stat-self 57 (sh) S 12 12 12 1281 12 0").unwrap_err();
        assert!(why.contains("parent"), "{why}");
        assert!(why.contains("session"), "{why}");
    }

    #[test]
    fn a_command_name_with_spaces_parses() {
        assert_eq!(judge_stat("stat-self 9 (a b) S 1 9 9 1281 9"), Ok(()));
    }

    #[test]
    fn shutdown_must_stop_in_reverse_order() {
        let good = lines(&[
            "  init     going down: poweroff.target",
            "  init     multi-user.target: stopped",
            "  init     getty@console.service: stopped",
            "  init     basic.target: stopped",
            "  init     sysinit.target: stopped",
            "  init     / is read-only",
            "  init     /data is read-only",
            "reboot: Power down",
        ]);
        assert_eq!(judge_shutdown(&good), Ok(()));
        let swapped = lines(&[
            "  init     going down: poweroff.target",
            "  init     getty@console.service: stopped",
            "  init     multi-user.target: stopped",
            "  init     basic.target: stopped",
            "  init     sysinit.target: stopped",
            "  init     / is read-only",
            "  init     /data is read-only",
            "reboot: Power down",
        ]);
        assert!(judge_shutdown(&swapped).is_err());
        let no_power = lines(&good.iter().take(7).map(String::as_str).collect::<Vec<_>>());
        assert!(judge_shutdown(&no_power).unwrap_err().contains(POWER_DOWN));
        // F-53's negative control, as the kernel answered before N1.
        let refused = lines(&[
            "  init     going down: poweroff.target",
            "  init     multi-user.target: stopped",
            "  init     getty@console.service: stopped",
            "  init     basic.target: stopped",
            "  init     sysinit.target: stopped",
            "  init     remounting / read-only: Invalid argument (os error 22)",
            "  init     remounting /data read-only: Invalid argument (os error 22)",
            "reboot: Power down",
        ]);
        assert!(judge_shutdown(&refused).unwrap_err().contains("F-53"));
    }

    #[test]
    fn a_warning_about_a_unit_file_fails() {
        let clean = lines(&["  init     booting default.target"]);
        assert_eq!(judge_units(&clean), Ok(()));
        let warned = lines(&[
            "  init     /etc/ferrix/units/getty@.service.d/test.conf:2: Failed to resolve",
        ]);
        assert!(judge_units(&warned).unwrap_err().contains("test.conf"));
    }

    #[test]
    fn flaky_must_be_restarted_and_then_fail_on_its_limit() {
        let good = lines(&[
            "  init     flaky.service: exit-code; restarting at 3.100s",
            "  init     flaky.service: failed (start-limit-hit)",
        ]);
        assert_eq!(judge_flaky(&good), Ok(()));
        let never = lines(&["  init     flaky.service: failed (exit-code)"]);
        assert!(judge_flaky(&never).is_err());
    }

    #[test]
    fn an_answer_is_found_after_inits_prefix_but_not_in_the_echo() {
        assert!(starts(
            "  init     forker.service: stopped",
            "forker.service: stopped"
        ));
        assert!(starts(
            "ferrix# \u{1b}[J\u{1b}[8C  init     forker.service: stopped",
            "forker.service: stopped"
        ));
        assert!(starts("stat-self 5 (sh)", "stat-self "));
        assert!(!starts(
            "init-test# m=stat; echo \"$m-self $s\"",
            "stat-self "
        ));
    }
}
