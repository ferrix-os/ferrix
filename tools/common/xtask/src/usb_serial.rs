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
/// What it prints before what it read, each round.
const GOT: &str = "serial-gate-in=";
const GOT_AGAIN: &str = "serial-gate-in-again=";
/// What it prints once the killed driver's successor linked the terminal
/// again.
const REBOUND: &str = "serial-gate: rebound";
/// What it prints once the host controller's driver, killed, is back and
/// the terminal with it.
const HOST_REBOUND: &str = "serial-gate: host rebound";
const GOT_THIRD: &str = "serial-gate-in-third=";
/// What it prints before how many bytes the keyboard's event node gave.
const KEY: &str = "serial-gate-key=";
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
/// gate's go, write `lines[0]`, read `len` bytes, print them. Then kill
/// `usbserial` with SIGKILL, wait for its successor and set the terminal
/// again, read the keyboard's event node meanwhile, and do the round again
/// with `lines[1]`. Then kill `usbhid` itself, the controller's driver,
/// and do the round a third time with `lines[2]` once devmgr has started it
/// again and it has exported the interface again.
fn script(lines: [&str; 3], len: usize) -> String {
    let round = |line: &str, got: &str| {
        format!(
            "head -c {go} /tmp/ttyUSB0 > /tmp/go\n\
             echo {line} > /tmp/ttyUSB0\n\
             head -c {len} /tmp/ttyUSB0 > /tmp/in\n\
             echo \"{got}$(cat /tmp/in)\"\n",
            go = GO.len(),
        )
    };
    // Kill `victim`, then wait for a `usbserial` that is not the one that
    // was running, and set its terminal.
    let kill = |victim: &str| {
        format!(
            "old=\n\
             for p in /proc/[0-9]*; do read c < $p/comm; \
             if [ \"$c\" = usbserial ]; then old=${{p##*/}}; fi; done\n\
             for p in /proc/[0-9]*; do read c < $p/comm; \
             if [ \"$c\" = {victim} ]; then kill -9 ${{p##*/}} && echo \"serial-gate: killed {victim} ${{p##*/}}\"; fi; done\n\
             new=\n\
             i=0\n\
             while [ -z \"$new\" ]; do i=$((i+1)); \
             if [ $i -gt 300 ]; then echo '{FAILED}: no usbserial after {victim}'; exit 1; fi; sleep 0.1; \
             for p in /proc/[0-9]*; do read c < $p/comm; \
             if [ \"$c\" = usbserial ] && [ \"${{p##*/}}\" != \"$old\" ]; then new=${{p##*/}}; fi; done; done\n\
             i=0\n\
             until stty -F /tmp/ttyUSB0 {BAUD} raw -echo 2>/dev/null; do i=$((i+1)); \
             if [ $i -gt 100 ]; then echo '{FAILED}: stty after {victim}'; exit 1; fi; sleep 0.1; done\n"
        )
    };
    format!(
        "i=0\n\
         while [ ! -e /tmp/ttyUSB0 ]; do i=$((i+1)); \
         if [ $i -gt 300 ]; then echo '{FAILED}: no /tmp/ttyUSB0'; exit 1; fi; sleep 0.1; done\n\
         stty -F /tmp/ttyUSB0 {BAUD} raw -echo || echo '{FAILED}: stty'\n\
         echo '{READY}'\n\
         {first}\
         {kill_driver}\
         dd if=/dev/input/event0 of=/tmp/key bs=24 count=1 2>/dev/null &\n\
         echo '{REBOUND}'\n\
         {second}\
         wait\n\
         echo \"{KEY}$(wc -c < /tmp/key)\"\n\
         {kill_host}\
         echo '{HOST_REBOUND}'\n\
         {third}\
         sleep 3\n",
        first = round(lines[0], GOT),
        kill_driver = kill("usbserial"),
        second = round(lines[1], GOT_AGAIN),
        kill_host = kill("usbhid"),
        third = round(lines[2], GOT_THIRD),
    )
}

/// The rate an FT232H runs at for `baud`, by the driver's own divisor.
pub(crate) fn h_rate(baud: u32) -> u32 {
    use ferrix_usb_serial::ftdi::{Chip, divisor, rate};
    divisor(Chip::H, baud)
        .and_then(|value| rate(Chip::H, value))
        .unwrap_or(0)
}

/// A number no earlier run had.
fn nonce() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_nanos() as u64) ^ u64::from(std::process::id()).rotate_left(32)
}

/// Busybox for `arch`: `--init`'s, or the static one `test-shell` boots.
pub(crate) fn busybox(arch: Arch, args: &Args) -> Result<PathBuf> {
    if let Some(init) = &args.init {
        return Ok(PathBuf::from(init.replace("{arch}", arch.name())));
    }
    if let Some(built) = crate::busybox::installed_program(arch) {
        return Ok(built);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
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
fn connect(socket: &Path) -> std::result::Result<UnixStream, String> {
    let deadline = Instant::now() + PATIENCE;
    let stream = loop {
        match UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() >= deadline => {
                return Err(format!(
                    "the socket {} did not connect: {error}",
                    socket.display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    Ok(stream)
}

/// The whole run, from the gate's side: both rounds, the kill between
/// them and the key pressed after it.
fn exchange(
    watching: &mut Watching<'_>,
    socket: &Path,
    qmp_port: u16,
    lines: [&str; 6],
) -> std::result::Result<(), String> {
    let [
        from_guest,
        to_guest,
        from_again,
        to_again,
        from_third,
        to_third,
    ] = lines;
    let mut stream = connect(socket)?;
    round(watching, &mut stream, from_guest, to_guest, GOT)?;
    // The driver is killed; its successor links the terminal again.
    let rebound = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            lines
                .iter()
                .any(|line| line.contains(REBOUND) || line.contains(FAILED))
        })
        .map_err(|error| error.to_string())?;
    if !rebound || !watching.after().iter().any(|line| line.contains(REBOUND)) {
        return Err("no usbserial linked /tmp/ttyUSB0 again after the kill".to_owned());
    }
    // A key on the keyboard behind the same controller: it must arrive
    // whatever became of the serial driver.
    let mut qmp = crate::display::Qmp::connect(qmp_port, Instant::now() + PATIENCE)
        .map_err(|error| error.to_string())?;
    for down in [true, false] {
        qmp.input_send_event(&[crate::input::key("a", down)])
            .map_err(|error| error.to_string())?;
    }
    round(watching, &mut stream, from_again, to_again, GOT_AGAIN)?;
    let keyed = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(KEY))
        })
        .map_err(|error| error.to_string())?;
    let key_line = watching
        .lines()
        .iter()
        .chain(watching.after())
        .find(|line| line.contains(KEY))
        .cloned()
        .unwrap_or_default();
    if !keyed || !key_line.trim_end().ends_with(&format!("{KEY}24")) {
        return Err(format!(
            "the keyboard's key never reached /dev/input/event0 after usbserial was killed: `{}`",
            key_line.trim()
        ));
    }
    // The controller's driver is killed; devmgr starts it again, it
    // exports the interface again, and a new usbserial takes it.
    let back = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            lines
                .iter()
                .any(|line| line.contains(HOST_REBOUND) || line.contains(FAILED))
        })
        .map_err(|error| error.to_string())?;
    if !back
        || !watching
            .after()
            .iter()
            .any(|line| line.contains(HOST_REBOUND))
    {
        return Err("no usbserial linked /tmp/ttyUSB0 again after usbhid was killed".to_owned());
    }
    round(watching, &mut stream, from_third, to_third, GOT_THIRD)
}

/// One round: go, the guest's line in, the gate's line out and read back.
fn round(
    watching: &mut Watching<'_>,
    stream: &mut UnixStream,
    from_guest: &str,
    to_guest: &str,
    got: &str,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + PATIENCE;
    stream
        .write_all(GO)
        .map_err(|error| format!("sending go: {error}"))?;
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
    let want = format!("{got}{to_guest}");
    let found = watching
        .read_more(deadline, |lines| {
            lines
                .iter()
                .any(|line| line.contains(got) || line.contains(FAILED))
        })
        .map_err(|error| error.to_string())?;
    let said = watching.lines().iter().chain(watching.after());
    if found && said.clone().any(|line| line.contains(&want)) {
        Ok(())
    } else {
        Err(format!(
            "the guest did not read {to_guest:?} from /tmp/ttyUSB0; it said {:?}",
            said.filter(|line| line.contains("serial-gate"))
                .collect::<Vec<_>>()
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

/// What the boot's lines and QEMU's trace at `trace` must show, each
/// failure said.
fn judge(lines: &[String], trace: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let count = |want: &str| lines.iter().filter(|line| line.contains(want)).count();
    // usbhid exported the interface, devmgr started usbserial for it, and
    // it made the terminal: twice, the second time after the kill, under
    // the same number.
    // usbhid exported the interface, devmgr started usbserial for it, and
    // it made the terminal: three times -- at boot, after usbserial's kill
    // and after usbhid's -- always under the same number.
    for (want, times) in [
        (
            "usbhid: interface 0 of 0403:6001 exported for its class driver",
            3,
        ),
        (
            "devmgr   usbserial for usb 0403:6001 interface 0: started (serial 0)",
            3,
        ),
        (
            "usbserial: serial 0 0403:6001 (FT232B UART, 64-byte packets, serial 1-",
            3,
        ),
        ("serial-gate: killed usbserial ", 1),
        (
            "devmgr   usbserial for usb 0403:6001 interface 0 (serial 0) died with status 137; started again (1 of 8)",
            1,
        ),
        ("serial-gate: killed usbhid ", 1),
        (&format!("usbserial: serial 0 set to {BAUD} baud"), 3),
    ] {
        let seen = count(want);
        if seen != times {
            failures.push(format!("`{want}` came {seen} times, not {times}"));
        }
    }
    // The controller's driver lived through usbserial's death, and was
    // started again, once, only after its own.
    let host_killed = lines
        .iter()
        .position(|line| line.contains("serial-gate: killed usbhid "))
        .unwrap_or(lines.len());
    let (before, after) = lines.split_at(host_killed);
    let in_part =
        |part: &[String], want: &str| part.iter().filter(|line| line.contains(want)).count();
    if in_part(before, "usbhid: xHCI running") != 1 || in_part(before, "was started again") != 0 {
        failures.push("usbhid died or was started again before it was killed".to_owned());
    }
    if in_part(after, "usbhid: xHCI running") != 1 || in_part(after, "was started again") != 1 {
        failures.push("usbhid was not started again, once, after it was killed".to_owned());
    }
    // QEMU decodes the divisor itself: hw/usb/dev-serial.c's
    // `usb_serial_set_baud`, "dev %d:%u baud rate %d". The rate is the
    // chip's, which a divisor of 26 makes 115384: within the 3% a UART
    // allows, as the driver's own arithmetic is held to.
    let traced = std::fs::read_to_string(trace).unwrap_or_default();
    let rates = baud_rates(&traced);
    let near =
        |rate: u32, wanted: u32| u64::from(rate.abs_diff(wanted)) * 100 <= u64::from(wanted) * 3;
    // Each driver opens the chip at 9600, and stty sets it to BAUD.
    let wanted = [9600, BAUD, 9600, BAUD, 9600, BAUD];
    let shaped = rates.len() == wanted.len()
        && rates
            .iter()
            .zip(wanted)
            .all(|(&rate, wanted)| near(rate, wanted));
    if !shaped {
        failures.push(format!(
            "QEMU's trace {} does not show the chip set to 9600 baud and then {BAUD}, by \
             each of the two drivers, within 3%: it traced the rates {rates:?}",
            trace.display(),
        ));
    }
    failures
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
    let from_again = format!("ferrix-to-host-{:016x}", seed.rotate_left(29) ^ 0xA5A5);
    let to_again = format!("host-to-ferrix-{:016x}", seed.rotate_left(41) ^ 0x3C3C);
    let from_third = format!("ferrix-to-host-{:016x}", seed.rotate_left(7) ^ 0x1E1E);
    let to_third = format!("host-to-ferrix-{:016x}", seed.rotate_left(53) ^ 0xE1E1);
    let shell = busybox(arch, args)?;
    println!(
        "  {arch}: busybox {} as init, with the gate's script",
        shell.display()
    );
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel_with_init(
        arch,
        args.release,
        &shell,
        &script([&from_guest, &from_again, &from_third], to_guest.len()),
    )?;
    let natives = crate::native::build(arch, args.release)?;
    // Busybox as /bin/busybox too, with its applets linked in /bin: the
    // script runs stty, head and cat.
    let initramfs = crate::initramfs::build(Some(&shell), &natives, None, &[])?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;

    // A Unix socket's path is at most 107 bytes, which a worktree's build
    // directory is past: the socket and its trace go in the temporary
    // directory, named for this process.
    let socket =
        std::env::temp_dir().join(format!("ferrix-usb-serial-{}.sock", std::process::id()));
    let trace = crate::qemu::usb_serial_trace(&socket);
    for stale in [&socket, &trace] {
        if stale.exists() {
            std::fs::remove_file(stale)
                .map_err(|error| Error::new(format!("{}: {error}", stale.display())))?;
        }
    }
    let mut booted = args.clone();
    booted.usb_serial = Some(socket.clone());
    let qmp_port = crate::display::free_port()?;
    booted.qmp_port = Some(qmp_port);

    let mut outcome = Err("the boot never reached the script's ready line".to_owned());
    let lines = crate::qemu::watch_then(arch, &image, &kernel, &booted, READY, |watching| {
        watching.stop_when_done();
        outcome = exchange(
            watching,
            &socket,
            qmp_port,
            [
                &from_guest,
                &to_guest,
                &from_again,
                &to_again,
                &from_third,
                &to_third,
            ],
        );
        Ok(())
    })?;
    let mut failures = Vec::new();
    if let Err(why) = outcome {
        failures.push(why);
    }
    failures.extend(judge(&lines, &trace));
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&trace);
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
         other way; usbserial killed with SIGKILL was started again by devmgr under the same \
         number, carried {from_again:?} and {to_again:?}, and a key typed meanwhile reached \
         the keyboard's node with usbhid never restarted; usbhid killed was started again, \
         exported the interface again, and a new usbserial carried {from_third:?} and {to_third:?}"
    );
    Ok(())
}
