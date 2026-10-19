//! adbd, the device end of the Android Debug Bridge (`src/user/system/linux/adbd`,
//! `docs/ADB.md`): building it for the initramfs, and `test-adb`, which
//! drives it with this machine's own `adb`.
//!
//! Built as `zinc.rs` builds the shell: a static Linux program with
//! std against the target's musl, from a workspace of its own into a target
//! directory of its own. `--adbd` puts it in an image at `/bin/adbd`, where
//! nothing starts it: anyone who reaches its port gets a shell, so running
//! it is always something somebody asked for.
//!
//! # `test-adb`
//!
//! The kernel runs three programs, as `test-net` runs its own: `ip` brings
//! `eth0` up, `dhcpc` configures it, and `adbd --test` listens on 5555,
//! which a `--forward` on the host's loopback reaches. Once adbd says it is
//! listening, this machine's `adb` connects and runs what a person would:
//! a command in the shell, a push and a pull of the same bytes compared
//! byte for byte, a listing, a forward to a port inside, and a reboot,
//! which `--test` turns into adbd ending so the boot can finish. Without an
//! `adb` on this machine there is nothing to drive it with, and the gate
//! says so and passes as skipped, as the other steps that need a host tool
//! do.

use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::gateway::Forward;
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::vfs::{self, Expect};
use crate::{Error, Result, cargo, fat, initramfs, native, ports, qemu, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-adb",
    help_at: 280,
    help: &[
        "test-adb      Boot adbd with a network, and drive it with this machine's adb: shell, push, pull, forward, reboot",
    ],
    run: |_, args| test_adb(args, crate::program_for),
}];

/// Where adbd goes in the initramfs: in `/bin`, where the kernel's command
/// list finds its programs.
pub(crate) const PATH: &str = "bin/adbd";

/// The port adbd listens on in the guest.
const GUEST_PORT: u16 = 5555;

/// What adbd prints once it listens, which the gate waits for.
const LISTENING: &str = "adbd: listening on";

/// What adbd prints when `reboot:` ends it under `--test`.
const ENDED: &str = "adbd: the host asked for a reboot; ending (--test)";

/// How long one `adb` command may take.
const ADB_PATIENCE: Duration = Duration::from_secs(60);

/// Build adbd for `arch` and return it as a file for the initramfs, or
/// `None` on an architecture it is not built for yet.
pub(crate) fn file(arch: Arch) -> Result<Option<ports::File>> {
    let Some(target) = zinc::target(arch) else {
        println!("  adbd is not built for {} yet", arch.name());
        return Ok(None);
    };
    println!("  building adbd for {target}");
    let target_dir = paths::target_dir().join("adbd");
    let program = target_dir.join(target).join("release").join("adbd");
    crate::builds::Build::cargo(
        format!("cargo build (adbd) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/adbd"),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    // For zinc's reason: RUSTFLAGS replaces the flags every config file up
    // the tree would otherwise merge in.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .output(&program)
    .run()?;
    let bytes = std::fs::read(&program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    Ok(Some(ports::File {
        path: PATH.to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(bytes),
    }))
}

/// The programs the kernel runs for the gate: the network, then adbd until
/// the host's reboot ends it.
fn commands() -> Vec<vfs::Command> {
    vec![
        vfs::Command {
            argv: &["ip", "link", "set", "eth0", "up"],
            status: 0,
            expect: Expect::Nothing,
        },
        vfs::Command {
            argv: &["dhcpc", "-i", "eth0", "-n", "-q", "-t", "5", "-T", "2"],
            status: 0,
            expect: Expect::Shaped(&["eth0: 10.0.2.15/24 by DHCP*"]),
        },
        vfs::Command {
            argv: &["adbd", "--test"],
            status: 0,
            expect: Expect::Lines(&[ENDED]),
        },
    ]
}

/// `cargo xtask test-adb --init <busybox>`.
///
/// # Errors
///
/// A build that failed, or an architecture where adbd did not answer the
/// host as it should.
pub(crate) fn test_adb(
    args: &Args,
    program_for: impl Fn(&str, Arch) -> Result<PathBuf>,
) -> Result<()> {
    if !have_adb() {
        println!("  test-adb: no `adb` on this machine to drive adbd with; skipped");
        return Ok(());
    }
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new(
            "test-adb needs --init PATH, a static busybox for each architecture, for the \
             network's `ip` and the shell adbd runs",
        )
    })?;
    let mut failed = Vec::new();
    for arch in args.arches()? {
        let Some(adbd) = file(arch)? else { continue };
        let port = free_port()?;
        let run = Args {
            net: true,
            forwards: vec![Forward {
                host: port,
                guest: GUEST_PORT,
            }],
            ..args.clone()
        };
        let program = program_for(init, arch)?;
        let programs = commands();
        let list = paths::build_dir(arch).join("adb-commands");
        vfs::write_if_changed(&list, &vfs::encode(&programs)?)?;
        let loader = cargo::build_loader(arch, run.release)?;
        let kernel = cargo::build_kernel_with_commands(arch, run.release, &list)?;
        let natives = native::build(arch, run.release)?;
        let mut carried = vec![adbd];
        carried.extend(crate::dhcp::carried(arch)?);
        let initramfs = initramfs::build(Some(&program), &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        if let Err(error) = run_one(arch, &image, &kernel, &run, port) {
            eprintln!("\n  {error}");
            failed.push(arch.name());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!("adb failed on {}", failed.join(", "))))
    }
}

/// Boot one image, drive its adbd from this machine, and require it ended
/// as `--test` ends it.
fn run_one(arch: Arch, image: &Path, kernel: &Path, args: &Args, port: u16) -> Result<()> {
    println!(
        "  {arch}: adbd under QEMU, reached at 127.0.0.1:{port} (timeout {}s)",
        args.timeout
    );
    let serial = format!("127.0.0.1:{port}");
    let lines = qemu::watch_then(arch, image, kernel, args, LISTENING, |watching| {
        let driven = drive(arch, &serial);
        let _ = adb(&["disconnect", &serial]);
        driven?;
        let deadline = Instant::now() + Duration::from_secs(args.timeout);
        let ended = watching.read_more(deadline, |after| {
            after.iter().any(|line| line.contains(vfs::DONE))
        })?;
        if ended {
            Ok(())
        } else {
            Err(Error::new(format!(
                "{arch}: the command list did not finish after the reboot"
            )))
        }
    })?;
    if !lines.iter().any(|line| line.contains(ENDED)) {
        return Err(Error::new(format!(
            "{arch}: adbd did not end on the host's reboot as --test makes it"
        )));
    }
    println!("  {arch}: adbd answered adb's shell, push, pull, ls, forward and reboot");
    Ok(())
}

/// What a person would do with a device, from this machine's `adb`.
fn drive(arch: Arch, serial: &str) -> Result<()> {
    let connected = adb(&["connect", serial])?;
    if !connected.contains("connected to") {
        return Err(Error::new(format!("adb connect {serial}: {connected}")));
    }
    let echoed = adb(&["-s", serial, "shell", "echo", "adb-on-ferrix"])?;
    if echoed.trim() != "adb-on-ferrix" {
        return Err(Error::new(format!("adb shell echo gave {echoed:?}")));
    }
    push_and_pull(arch, serial)?;
    let listed = adb(&["-s", serial, "ls", "/bin"])?;
    if !listed.contains("adbd") {
        return Err(Error::new(format!(
            "adb ls /bin did not list adbd: {listed:?}"
        )));
    }
    // A forward to adbd's own port inside, spoken through: a CNXN in, and
    // adbd's CNXN back proves the stream reached the guest, where a bare
    // connect would only have reached this machine's adb.
    let local = free_port()?;
    let _ = adb(&[
        "-s",
        serial,
        "forward",
        &format!("tcp:{local}"),
        &format!("tcp:{GUEST_PORT}"),
    ])?;
    let answered = cnxn_through(local);
    let _ = adb(&["-s", serial, "forward", "--remove", &format!("tcp:{local}")]);
    answered?;
    let _ = adb(&["-s", serial, "reboot"])?;
    Ok(())
}

/// Say `CNXN` to whatever `127.0.0.1:port` leads to and require a `CNXN`
/// back.
fn cnxn_through(port: u16) -> Result<()> {
    use std::io::{Read, Write};

    use ferrix_adb::message::{self, CNXN, HEADER_BYTES, Header, MAX_PAYLOAD, VERSION};

    let fail = |why: String| Error::new(format!("adb forward tcp:{port}: {why}"));
    let mut socket = std::net::TcpStream::connect_timeout(
        &(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_secs(10),
    )
    .map_err(|error| fail(error.to_string()))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| fail(error.to_string()))?;
    socket
        .write_all(&message::message(CNXN, VERSION, MAX_PAYLOAD, b"host::\0"))
        .map_err(|error| fail(error.to_string()))?;
    let mut head = [0; HEADER_BYTES];
    socket
        .read_exact(&mut head)
        .map_err(|error| fail(format!("no answer through it: {error}")))?;
    match Header::decode(&head, u32::MAX) {
        Ok(header) if header.command == CNXN => Ok(()),
        other => Err(fail(format!("answered {other:?}, not CNXN"))),
    }
}

/// Push bytes that are not text, pull them back, and require them equal.
fn push_and_pull(arch: Arch, serial: &str) -> Result<()> {
    let dir = paths::build_dir(arch).join("adb");
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("{}: {error}", dir.display())))?;
    let sent = dir.join("pushed.bin");
    let back = dir.join("pulled.bin");
    // 300 000 bytes: several WRTEs and several DATA packets each way.
    let bytes: Vec<u8> = (0..300_000_u32)
        .map(|at| (at.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(&sent, &bytes)
        .map_err(|error| Error::new(format!("{}: {error}", sent.display())))?;
    let sent_text = sent.display().to_string();
    let back_text = back.display().to_string();
    let _ = adb(&["-s", serial, "push", &sent_text, "/adb-pushed.bin"])?;
    let _ = std::fs::remove_file(&back);
    let _ = adb(&["-s", serial, "pull", "/adb-pushed.bin", &back_text])?;
    let returned =
        std::fs::read(&back).map_err(|error| Error::new(format!("{back_text}: {error}")))?;
    if returned != bytes {
        return Err(Error::new(format!(
            "adb pull gave {} bytes back, not the {} pushed, or different ones",
            returned.len(),
            bytes.len()
        )));
    }
    Ok(())
}

/// Whether this machine has an `adb`.
fn have_adb() -> bool {
    Command::new("adb")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Run `adb` with `args`, within [`ADB_PATIENCE`], and return what it
/// printed, both streams.
fn adb(args: &[&str]) -> Result<String> {
    let mut child = Command::new("adb")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::new(format!("adb {}: {error}", args.join(" "))))?;
    let deadline = Instant::now() + ADB_PATIENCE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::new(format!(
                    "adb {} did not finish within {}s",
                    args.join(" "),
                    ADB_PATIENCE.as_secs()
                )));
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| Error::new(format!("adb {}: {error}", args.join(" "))))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(text)
    } else {
        Err(Error::new(format!(
            "adb {} failed: {}",
            args.join(" "),
            text.trim()
        )))
    }
}

/// A port on this machine's loopback that nothing listens on now.
fn free_port() -> Result<u16> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| Error::new(format!("finding a free port: {error}")))
}
