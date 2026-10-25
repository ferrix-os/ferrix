//! `cargo xtask test-usb-serial`: an FTDI serial adapter driven from ring
//! 3, end to end, against QEMU's emulated one (`docs/USB-SERIAL.md` §5).
//!
//! The boot has a `qemu-xhci` controller with QEMU's `usb-serial` on it --
//! an FT232BM, `0403:6001`, `bcdDevice` 0x400 -- whose line is a Unix
//! socket this gate holds the other end of. Nothing of the host's is
//! attached. Init is a busybox shell running [`script`], and the gate
//! requires, in order:
//!
//! 1. `usbhid`'s line for the adapter, naming its terminal `/tmp/ttyUSB0`;
//! 2. `stty -F /tmp/ttyUSB0 115200 raw -echo` reaching the chip: the
//!    driver's line for the new rate, and QEMU's own trace of the divisor
//!    it was sent, decoded by QEMU back to 115200;
//! 3. a line the guest wrote to `/tmp/ttyUSB0` arriving on the socket, with
//!    this run's nonce in it;
//! 4. a line the gate wrote to the socket read whole from `/tmp/ttyUSB0`,
//!    with another nonce, which the guest prints.
//!
//! Each nonce is new for each run and the guest's is built into its
//! script, so no transcript of an earlier run, and no echo of what the
//! gate itself sent, passes 3 or 4.
//!
//! What QEMU cannot show is the FT232H itself: its emulation is the B
//! family's, so the H divisor, the 512-byte high-speed packets and the
//! USB-Blaster are held by `ferrix-usb-host`'s tests against its model
//! instead (`src/lib/drivers/usb/usb-host/src/tests/serial.rs`).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::args::Args;
use crate::paths::Arch;
use crate::qemu::Watching;
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-usb-serial",
    help_at: 485,
    help: &[
        "test-usb-serial Boot busybox beside QEMU's FTDI usb-serial: set its rate with stty, send a line each way",
        "              through /tmp/ttyUSB0 and a host socket, and require QEMU's trace of the rate (x86_64)",
    ],
    run: |_, args| test(args),
}];

/// What the script prints once its terminal is set and it waits for the
/// gate: the boot's marker.
const READY: &str = "serial-gate: ready";
/// What it prints before what it read.
const GOT: &str = "serial-gate-in=";
/// What it prints when something it needs is missing.
const FAILED: &str = "serial-gate: failed";
/// What the gate sends first, so the guest's line goes out only once the
/// socket is connected: QEMU drops what its unconnected socket is given.
const GO: &[u8] = b"go\n";
/// The rate the guest sets.
const BAUD: u32 = 115_200;
/// How long each step may take.
const PATIENCE: Duration = Duration::from_secs(30);

/// The guest's half: wait for the terminal, set it, say so, wait for the
/// gate's go, write `out`, read `len` bytes, print them.
fn script(out: &str, len: usize) -> String {
    format!(
        "i=0\n\
         while [ ! -e /tmp/ttyUSB0 ]; do i=$((i+1)); \
         if [ $i -gt 300 ]; then echo '{FAILED}: no /tmp/ttyUSB0'; exit 1; fi; sleep 0.1; done\n\
         stty -F /tmp/ttyUSB0 {BAUD} raw -echo || echo '{FAILED}: stty'\n\
         echo '{READY}'\n\
         head -c {go} /tmp/ttyUSB0 > /tmp/go\n\
         echo {out} > /tmp/ttyUSB0\n\
         head -c {len} /tmp/ttyUSB0 > /tmp/in\n\
         echo \"{GOT}$(cat /tmp/in)\"\n\
         sleep 3\n",
        go = GO.len(),
    )
}

/// A number no earlier run had.
fn nonce() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_nanos() as u64) ^ u64::from(std::process::id()).rotate_left(32)
}

/// Busybox for `arch`: `--init`'s, or the static one `test-shell` boots.
fn busybox(arch: Arch, args: &Args) -> Result<PathBuf> {
    if let Some(init) = &args.init {
        return Ok(PathBuf::from(init.replace("{arch}", arch.name())));
    }
    if let Some(built) = crate::busybox::installed_program(arch) {
        return Ok(built);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let alpine = home
        .join(".local/share/ferrix/busybox")
        .join(arch.name())
        .join("bin/busybox.static");
    if alpine.is_file() {
        Ok(alpine)
    } else {
        Err(Error::new(format!(
            "no busybox for {arch}: `cargo xtask busybox`, or --init <path> naming a static one"
        )))
    }
}

/// The gate's half, once the guest is ready: connect, send go, read the
/// guest's line, send ours, and wait for the guest to print it.
fn exchange(
    watching: &mut Watching<'_>,
    socket: &Path,
    from_guest: &str,
    to_guest: &str,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + PATIENCE;
    let mut stream = loop {
        match UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() >= deadline => {
                return Err(format!("the socket {} did not connect: {error}", socket.display()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    stream.write_all(GO).map_err(|error| format!("sending go: {error}"))?;
    let wanted = format!("{from_guest}\n");
    let mut heard = Vec::new();
    while !String::from_utf8_lossy(&heard).contains(&wanted) {
        if Instant::now() >= deadline {
            return Err(format!(
                "the guest's line {from_guest:?} never came out of the adapter; the socket heard {:?}",
                String::from_utf8_lossy(&heard)
            ));
        }
        let mut buffer = [0_u8; 256];
        match stream.read(&mut buffer) {
            Ok(0) => return Err("QEMU closed the socket".to_owned()),
            Ok(n) => heard.extend_from_slice(buffer.get(..n).unwrap_or_default()),
            Err(_) => {
                // A timeout: let the console move, so its lines are kept.
                let _ = watching.read_more(Instant::now() + Duration::from_millis(10), |_| false);
            }
        }
    }
    stream
        .write_all(to_guest.as_bytes())
        .map_err(|error| format!("sending the gate's line: {error}"))?;
    let want = format!("{GOT}{to_guest}");
    let found = watching
        .read_more(deadline, |lines| {
            lines
                .iter()
                .any(|line| line.contains(GOT) || line.contains(FAILED))
        })
        .map_err(|error| error.to_string())?;
    let said = watching.lines().iter().chain(watching.after());
    if found && said.clone().any(|line| line.contains(&want)) {
        Ok(())
    } else {
        Err(format!(
            "the guest did not read {to_guest:?} from /tmp/ttyUSB0; it said {:?}",
            said.filter(|line| line.contains("serial-gate")).collect::<Vec<_>>()
        ))
    }
}

/// Every rate QEMU's trace says the emulated chip was set to, in order.
fn baud_rates(trace: &str) -> Vec<u32> {
    trace
        .lines()
        .filter(|line| line.contains("usb_serial_set_baud"))
        .filter_map(|line| line.rsplit("baud rate ").next())
        .filter_map(|rate| rate.trim().parse().ok())
        .collect()
}

/// `test-usb-serial`.
///
/// # Errors
///
/// A boot that did not carry both lines, or set the rate.
pub(crate) fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-usb-serial runs on x86-64 only: under QEMU's AArch64 firmware the xHCI \
             controller's BAR is left unplaced (docs/XHCI.md §8)",
        ));
    }
    let seed = nonce();
    let from_guest = format!("ferrix-to-host-{seed:016x}");
    let to_guest = format!("host-to-ferrix-{:016x}", seed.rotate_left(17) ^ 0x5A5A);
    let shell = busybox(arch, args)?;
    println!("  {arch}: busybox {} as init, with the gate's script", shell.display());
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel_with_init(
        arch,
        args.release,
        &shell,
        &script(&from_guest, to_guest.len()),
    )?;
    let natives = crate::native::build(arch, args.release)?;
    // Busybox as /bin/busybox too, with its applets linked in /bin: the
    // script runs stty, head and cat.
    let initramfs = crate::initramfs::build(Some(&shell), &natives, None, &[])?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;

    // A Unix socket's path is at most 107 bytes, which a worktree's build
    // directory is past: the socket and its trace go in the temporary
    // directory, named for this process.
    let socket = std::env::temp_dir().join(format!("ferrix-usb-serial-{}.sock", std::process::id()));
    let trace = crate::qemu::usb_serial_trace(&socket);
    for stale in [&socket, &trace] {
        if stale.exists() {
            std::fs::remove_file(stale)
                .map_err(|error| Error::new(format!("{}: {error}", stale.display())))?;
        }
    }
    let mut booted = args.clone();
    booted.usb_serial = Some(socket.clone());

    let mut outcome = Err("the boot never reached the script's ready line".to_owned());
    let lines = crate::qemu::watch_then(arch, &image, &kernel, &booted, READY, |watching| {
        watching.stop_when_done();
        outcome = exchange(watching, &socket, &from_guest, &to_guest);
        Ok(())
    })?;
    let mut failures = Vec::new();
    if let Err(why) = outcome {
        failures.push(why);
    }
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    let found = "usbhid: serial 0 0403:6001 (FT232B UART, 64-byte packets) is /tmp/ttyUSB0";
    if !has(found) {
        failures.push(format!("the driver never said `{found}`"));
    }
    let set = format!("usbhid: serial 0 set to {BAUD} baud, 8 data bits, parity None, 1 stop bits");
    if !has(&set) {
        failures.push(format!("the driver never said `{set}`"));
    }
    // QEMU decodes the divisor itself: hw/usb/dev-serial.c's
    // `usb_serial_set_baud`, "dev %d:%u baud rate %d". The rate is the
    // chip's, which a divisor of 26 makes 115384: within the 3% a UART
    // allows, as the driver's own arithmetic is held to.
    let traced = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&trace);
    let rates = baud_rates(&traced);
    let near =
        |rate: u32, wanted: u32| u64::from(rate.abs_diff(wanted)) * 100 <= u64::from(wanted) * 3;
    let opened = rates.iter().position(|&rate| near(rate, 9600));
    let set = rates.iter().rposition(|&rate| near(rate, BAUD));
    if !matches!((opened, set), (Some(first), Some(last)) if first < last) {
        failures.push(format!(
            "QEMU's trace {} does not show the chip set to 9600 baud and then to {BAUD}, \
             within 3%: it traced the rates {rates:?}",
            trace.display(),
        ));
    }
    if !failures.is_empty() {
        let mut message = format!("{arch}: the FTDI adapter did not carry the run:\n");
        for failure in &failures {
            message.push_str("    - ");
            message.push_str(failure);
            message.push('\n');
        }
        message.push_str("  The whole transcript is above and in the serial log.");
        return Err(Error::new(message));
    }
    println!(
        "  {arch}: stty set the FT232BM to {BAUD} baud (QEMU decoded the divisor to it), \
         {from_guest:?} came out of /tmp/ttyUSB0 onto the socket, and {to_guest:?} went the \
         other way"
    );
    Ok(())
}
