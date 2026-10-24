//! `cargo xtask test-log-remote`: a Ferrix machine's log sent over TLS to
//! `lode-receive` on this host, and every record kept there once, through
//! the receiver's outage (`docs/LOGGING.md` §10.2, §10.3).
//!
//! xtask builds `lode-receive` for this host and pairs a machine named
//! `guest` with it for `10.0.2.2` -- the gateway's address, which xtask's
//! own gateway carries to this host's loopback. The image carries the CA,
//! the machine's certificate and key, `lode-upload` with a line sending the
//! records of `remote` to `https://10.0.2.2:PORT/upload`, and a unit that
//! brings the network up and writes forty lines in two halves.
//!
//! After the first twenty, xtask waits until the receiver holds them, then
//! stops it; the second twenty are written while it is down, and the
//! uploader's tries fail and back off; xtask starts it again. It requires
//! the receiver to hold the forty, each once, in order, with the machine's
//! host name, and the uploader to have said its failures.
//!
//! x86-64 by default (KVM): what is tested is the protocol, TLS and the
//! cursor, not the processor; `--arch` runs another.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, init, initramfs, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-log-remote",
    help_at: 403,
    help: &[
        "test-log-remote Send a guest's log over TLS to lode-receive on this host, take the receiver",
        "              away and back, and require every record kept there once",
    ],
    run: |_, args| test_remote(args),
}];

/// The lines in each half.
const HALF: usize = 20;

/// The machine's name at the receiver.
const MACHINE: &str = "guest";

/// The guest's unit.
const UNIT: &str = "\
# test-log-remote's workload (tools/common/xtask/src/lode_remote.rs).
[Unit]
Description=Bring the network up and write lines for the receiver
After=lode.service

[Service]
Type=oneshot
TimeoutStartSec=600
ExecStart=/bin/busybox sh /lib/lode-remote.sh
";

/// Folding off for the lines, which differ only in their numbers.
const RULES: &str = "\
# test-log-remote's rules (tools/common/xtask/src/lode_remote.rs).
fold 0s where id = remote
";

/// The workload.
const SCRIPT: &str = r#"bb=/bin/busybox
say() { echo "remote: $*"; }
say "setting up"
$bb ip link set eth0 up
/bin/dhcpc -i eth0 -n -q -t 5 -T 2 || say "FAIL no lease"
i=0
until [ -n "$($bb pidof lode-upload)" ]; do
  i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode-upload never ran"; break; }
  $bb usleep 100000
done
n=0; while [ $n -lt 20 ]; do echo "<14>remote: line $n" > /dev/kmsg; n=$((n+1)); done
say "first half written"
# xtask takes the receiver away once it holds the first half: the second
# is written when nothing answers at its port any more.
i=0
while $bb timeout 2 $bb nc 10.0.2.2 PORT < /dev/null > /dev/null 2>&1; do
  i=$((i+1)); [ $i -gt 120 ] && { say "FAIL the receiver never went away"; break; }
  $bb usleep 500000
done
n=20; while [ $n -lt 40 ]; do echo "<14>remote: line $n" > /dev/kmsg; n=$((n+1)); done
say "second half written"
$bb sleep 120
say done
"#;

/// `cargo xtask test-log-remote`.
fn test_remote(args: &Args) -> Result<()> {
    let receiver = crate::lode_net::build_receiver()?;
    for arch in args.arches()? {
        test_arch(arch, args, &receiver)?;
    }
    Ok(())
}

/// The receiver, run as a child of xtask.
struct Running {
    /// It.
    child: Child,
}

impl Running {
    /// Start `program` keeping `root`, on `port` of this host's loopback.
    fn start(program: &Path, root: &Path, port: u16) -> Result<Running> {
        let child = Command::new(program)
            .args(["run", "--listen", &format!("127.0.0.1:{port}"), "--root"])
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| Error::new(format!("starting lode-receive: {error}")))?;
        // Until it holds its lock, it is not serving.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if check(program, root).is_some_and(|text| text.contains("\"running\":true")) {
                return Ok(Running { child });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(Error::new("lode-receive did not start"))
    }

    /// Stop it, by its own pid.
    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `lode-receive check --json`'s output.
fn check(program: &Path, root: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(["check", "--json", "--root"])
        .arg(root)
        .output()
        .ok()?;
    String::from_utf8(output.stdout).ok()
}

/// The `MESSAGE`s of `remote`'s records the receiver holds, in order.
fn held(program: &Path, root: &Path) -> Vec<String> {
    let Some(listing) = check(program, root) else {
        return Vec::new();
    };
    // The machine's directory: the one whose hostname is not empty; there is
    // only the guest.
    let Some(dir) = listing
        .split("\"dir\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
    else {
        return Vec::new();
    };
    let Ok(output) = Command::new(program)
        .args(["show", "--machine", dir, "--root"])
        .arg(root)
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let rest = line.split("\"MESSAGE\":\"").nth(1)?;
            Some(rest.split('"').next()?.to_owned())
        })
        .collect()
}

/// One architecture's boot.
fn test_arch(arch: Arch, args: &Args, receiver: &Path) -> Result<()> {
    let root = crate::paths::build_dir(arch).join("lode-receive");
    let _ = std::fs::remove_dir_all(&root);
    let status = Command::new(receiver)
        .args([
            "pair",
            "--machine",
            MACHINE,
            "--server-name",
            "10.0.2.2",
            "--root",
        ])
        .arg(&root)
        .stdout(Stdio::null())
        .status()
        .map_err(|error| Error::new(format!("lode-receive pair: {error}")))?;
    if !status.success() {
        return Err(Error::new("lode-receive pair failed"));
    }
    let port = crate::display::free_port()?;
    let files = image_files(arch, &root, port)?;
    println!("  {arch}: building an image whose lode sends its log to this host over TLS");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let image = fat::write_image_with(
        arch,
        &loader,
        &kernel,
        &archive,
        Some(&init::command_line()),
    )?;
    let mut booted = args.clone();
    booted.net = true;
    booted.timeout = booted.timeout.max(300);
    let running = Running::start(receiver, &root, port)?;
    println!(
        "  {arch}: lode-receive on 127.0.0.1:{port}, keeping {}",
        root.display()
    );
    let mut verdict: std::result::Result<(), String> = Err(String::from("never began"));
    let mut running = Some(running);
    let watched = qemu::watch_then(
        arch,
        &image,
        &kernel,
        &booted,
        "remote: first half written",
        |at| {
            at.stop_when_done();
            verdict = halves(at, receiver, &root, port, &mut running);
            Ok(())
        },
    );
    if let Some(running) = running.take() {
        running.stop();
    }
    let _ = watched?;
    verdict.map_err(|why| {
        Error::new(format!(
            "{arch}: {why}\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    println!(
        "  {arch}: {} records sent over TLS, each kept once, through the receiver's outage",
        2 * HALF
    );
    Ok(())
}

/// The two halves: the first kept, the receiver away for the second and
/// back, then all forty kept once.
fn halves(
    at: &mut qemu::Watching<'_>,
    program: &Path,
    root: &Path,
    port: u16,
    running: &mut Option<Running>,
) -> std::result::Result<(), String> {
    let want = |count: usize| -> Vec<String> { (0..count).map(|n| format!("line {n}")).collect() };
    let wait_for = |count: usize, seconds: u64| {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let got = held(program, root);
            if got.len() >= count || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    };
    let first = wait_for(HALF, 60);
    if first != want(HALF) {
        return Err(format!(
            "the first {HALF} lines were not kept once, in order: {first:?}"
        ));
    }
    if let Some(running) = running.take() {
        running.stop();
    }
    // The second half, written while nothing answers; the uploader's tries
    // fail and say so.
    let saw = |at: &mut qemu::Watching<'_>, what: &str, seconds: u64| {
        at.read_more(Instant::now() + Duration::from_secs(seconds), |lines| {
            lines.iter().any(|line| line.contains(what))
        })
        .map_err(|error| error.to_string())
    };
    if !saw(at, "remote: second half written", 90)? {
        return Err(String::from("the second half was never written"));
    }
    if let Some(line) = at.after().iter().find(|line| line.contains("remote: FAIL")) {
        return Err(line.trim().to_owned());
    }
    if !saw(at, "trying again in", 60)? {
        return Err(String::from(
            "the uploader never said the receiver was gone",
        ));
    }
    *running = Some(Running::start(program, root, port).map_err(|error| error.to_string())?);
    let all = wait_for(2 * HALF, 120);
    if all != want(2 * HALF) {
        return Err(format!(
            "after the receiver came back, the {} lines were not each kept once, in order: {all:?}",
            2 * HALF
        ));
    }
    let listing = check(program, root).unwrap_or_default();
    if !listing.contains("\"hostname\":\"ferrix\"") {
        return Err(format!("the machine's host name was not kept: {listing}"));
    }
    Ok(())
}

/// What the image carries: init, lode, `lode-upload` and its line, the CA
/// and the machine's certificate and key from `root`, DHCP, the workload.
fn image_files(arch: Arch, root: &Path, port: u16) -> Result<Vec<File>> {
    let pki = root.join(".pki");
    let read = |path: PathBuf| {
        std::fs::read(&path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    let relative = format!("etc/ferrix/lode-upload.d/{MACHINE}");
    let remote = format!("/{relative}");
    let script = SCRIPT.replace("PORT", &port.to_string());
    let config = format!(
        "# test-log-remote's collector (tools/common/xtask/src/lode_remote.rs).\n\
         {MACHINE} https://10.0.2.2:{port}/upload export ca={remote}/ca.pem \
         cert={remote}/client.pem key={remote}/client.key where id = \"remote\"\n"
    );
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = std::fs::read(crate::busybox::program(arch)?)
        .map_err(|error| Error::new(format!("reading busybox: {error}")))?;
    let bytes = |path: &str, mode: u32, content: Vec<u8>| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(content),
    };
    let link = |path: &str, to: &str| File {
        path: path.to_owned(),
        mode: 0o777,
        content: Content::Link(to.to_owned()),
    };
    files.extend([
        bytes("bin/busybox", 0o755, busybox),
        crate::lode::enabled(),
        link("etc/ferrix/units/getty@.service", "/dev/null"),
        link(
            "etc/ferrix/units/multi-user.target.wants/lode-upload.service",
            "/lib/ferrix/units/lode-upload.service",
        ),
        bytes("etc/ferrix/lode-upload.conf", 0o644, config.into_bytes()),
        bytes(
            &format!("{relative}/ca.pem"),
            0o644,
            read(pki.join("ca.pem"))?,
        ),
        bytes(
            &format!("{relative}/client.pem"),
            0o644,
            read(pki.join(format!("clients/{MACHINE}.pem")))?,
        ),
        bytes(
            &format!("{relative}/client.key"),
            0o600,
            read(pki.join(format!("clients/{MACHINE}.key")))?,
        ),
        bytes(
            "etc/ferrix/lode.d/remote.rules",
            0o644,
            RULES.as_bytes().to_vec(),
        ),
        bytes("lib/lode-remote.sh", 0o755, script.into_bytes()),
        bytes(
            "etc/ferrix/units/lode-remote.service",
            0o644,
            UNIT.as_bytes().to_vec(),
        ),
        link(
            "etc/ferrix/units/multi-user.target.wants/lode-remote.service",
            "/etc/ferrix/units/lode-remote.service",
        ),
    ]);
    files.extend(crate::dhcp::carried(arch)?);
    files.extend(crate::lode_net::carried(arch)?);
    Ok(files)
}
