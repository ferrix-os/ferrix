//! Watching a real serial port for the kernel's report.
//!
//! The counterpart to `qemu::test_boot` for hardware. QEMU has no STM32MP1
//! model, so the only way to run this kernel on that board is to put it on an
//! SD card and watch what comes out of the debug UART — and once a human is
//! watching a terminal by eye, the boot test stops being a check and becomes
//! an impression. This makes it a check again: the same two markers, the same
//! verdict, the same exit status.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::qemu::{PANIC_MARKER, SUCCESS_MARKER, UNCHECKED_MARKER};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "watch-serial",
    help_at: 1210,
    help: &["watch-serial  Watch a real serial port for the kernel's boot report"],
    run: |_, args| watch(None, args),
}];

/// The line speed every board this targets uses, and what the STM32MP157-DK's
/// device tree asks for in `stdout-path`: `serial0:115200n8`.
const BAUD: &str = "115200";

/// Where USB-serial adapters and the ST-LINK's virtual port appear.
///
/// The DK's ST-LINK presents a CDC-ACM device, so `ttyACM0` is the usual
/// answer; an FTDI cable on the same header is `ttyUSB0`. Both are searched
/// because which one a given board and cable produce is not worth remembering.
/// Windows numbers every serial port `COM<n>` instead, and lists the ones
/// present in the registry; see [`windows_ports`].
const CANDIDATE_PREFIXES: [&str; 2] = ["ttyACM", "ttyUSB"];

/// Watch `port` until the kernel reports, or the patience runs out.
pub(crate) fn watch(kernel: Option<&Path>, args: &Args) -> Result<()> {
    // `deploy` knows which kernel it just flashed; `watch-serial` on its own
    // is watching whatever is already on the board, and says so by passing
    // none rather than guessing at a build in this tree.
    let symbols = kernel.and_then(crate::symbolize::Symbolizer::open);
    let port = match args.port.as_deref() {
        Some(given) => PathBuf::from(given),
        None => discover()?,
    };

    let opened = open(&port)?;
    println!(
        "  watching {} at {BAUD} baud (timeout {}s, Ctrl-C to stop)",
        port.display(),
        args.timeout
    );

    // Read on a thread, for `qemu::test_boot`'s reason: a board may say
    // nothing for seconds and the deadline is for the boot as a whole. The
    // port stays open whether or not anything is driving it, so a timeout here
    // means "nothing arrived", never "the thing exited".
    //
    // Lines are read as bytes and made text afterwards: what the ST-LINK has
    // buffered from before the port was opened can be anything, and a line
    // that is not UTF-8 is a line to print with a replacement character, not a
    // reason to stop reading and report the board unplugged.
    let (sender, receiver) = mpsc::channel();
    let _reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(opened.reader);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&bytes)
                .trim_end_matches(['\r', '\n'])
                .to_owned();
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    // Held to the end of this function: on Windows it is the process holding
    // the port, and dropping it closes the port.
    let _holder = opened.holder;

    let log_path = crate::paths::build_dir(crate::paths::Arch::Armv7a).join("board-serial.log");
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut log = std::fs::File::create(&log_path)?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout);

    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(Error::new(format!(
                "nothing said `{SUCCESS_MARKER}` on {} within {}s.\n  \
                 What arrived is in {}.\n  \
                 If the log is empty the board is not talking: check the cable, the \
                 jumper on the boot pins, and that {BAUD} is the right speed.",
                port.display(),
                args.timeout,
                log_path.display()
            )));
        };
        match receiver.recv_timeout(remaining) {
            Ok(line) => {
                println!("    | {line}");
                writeln!(log, "{line}")?;
                log.flush()?;
                if line.contains(SUCCESS_MARKER) {
                    println!("  board: boot ok");
                    return Ok(());
                }
                // A desktop's card skips the checks on purpose (`flash
                // --compositor` writes `ferrix.checks=skip` among the image's
                // defaults), and `deploy --compositor` watching it is waiting
                // for the desktop, not for a boot test. Anywhere else a boot
                // that skipped them is not the verdict that was asked for.
                if line.contains(UNCHECKED_MARKER) {
                    if args.compositor {
                        println!(
                            "  board: booted with its self-checks skipped, as the desktop's image asks"
                        );
                        return Ok(());
                    }
                    return Err(Error::new(format!(
                        "the board skipped its self-checks (`{UNCHECKED_MARKER}`): its command \
                         line says ferrix.checks=skip, from the card's CMDLINE.TXT or the image's \
                         DEFAULTS.TXT. Flash without --compositor, or put ferrix.checks=run in \
                         CMDLINE.TXT, for a boot test.\n  What arrived is in {}",
                        log_path.display()
                    )));
                }
                if line.contains(PANIC_MARKER) {
                    crate::qemu::take_panic_report(&receiver, &mut log, symbols.as_ref())?;
                    return Err(Error::new(format!(
                        "the kernel panicked on the board; see {}",
                        log_path.display()
                    )));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::new(format!(
                    "{} closed while waiting; was it unplugged?",
                    port.display()
                )));
            }
            // The deadline is rechecked at the top of the loop.
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// The one serial port on this machine, if there is exactly one.
///
/// Refuses to choose between several rather than picking the lowest number: a
/// developer with a board and a USB-serial cable plugged in has two, and
/// guessing wrong means watching a port nothing is driving and concluding the
/// board is dead.
fn discover() -> Result<PathBuf> {
    if cfg!(windows) {
        return match windows_ports().as_slice() {
            [one] => Ok(PathBuf::from(one)),
            [] => Err(Error::new(
                "no serial port found (looked in HKLM\\HARDWARE\\DEVICEMAP\\SERIALCOMM).\n  \
                 Plug the board's USB debug port in, or pass --port COM<n>.",
            )),
            several => Err(Error::new(format!(
                "several serial ports are present; say which with --port:\n    {}",
                several.join("\n    ")
            ))),
        };
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Err(Error::new(
            "cannot read /dev to find a serial port; pass --port",
        ));
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if CANDIDATE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            found.push(entry.path());
        }
    }
    found.sort();

    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Error::new(
            "no serial port found (looked for /dev/ttyACM* and /dev/ttyUSB*).\n  \
             Plug the board's USB debug port in, or pass --port.",
        )),
        several => Err(Error::new(format!(
            "several serial ports are present; say which with --port:\n    {}",
            several
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n    ")
        ))),
    }
}

/// An open port: something to read the board's bytes from, and on Windows the
/// process that holds the port for as long as it is kept.
struct Opened {
    /// The port's bytes, as they arrive.
    reader: Box<dyn Read + Send>,
    /// Killed and reaped on drop.
    holder: Option<Holder>,
}

/// A child process that must not outlive the watch.
struct Holder(Child);

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Configure `port` and open it for reading.
fn open(port: &Path) -> Result<Opened> {
    if cfg!(windows) {
        return open_windows(port);
    }
    configure(port)?;
    let file = std::fs::File::open(port)
        .map_err(|error| Error::new(format!("opening {}: {error}", port.display())))?;
    Ok(Opened {
        reader: Box::new(file),
        holder: None,
    })
}

/// The serial ports Windows says are present: the values under
/// `HKLM\HARDWARE\DEVICEMAP\SERIALCOMM`, which the serial drivers write as
/// each port appears and remove as it goes. `reg`'s line for each reads
/// `    \Device\USBSER000    REG_SZ    COM8` in every display language.
fn windows_ports() -> Vec<String> {
    let Ok(output) = Command::new("reg")
        .args(["query", r"HKLM\HARDWARE\DEVICEMAP\SERIALCOMM"])
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let mut ports: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                [_, "REG_SZ", port] => Some((*port).to_owned()),
                _ => None,
            }
        })
        .collect();
    ports.sort();
    ports
}

/// The `COM<n>` a `--port` names, whether given bare or as `\\.\COM<n>`.
///
/// Checked to be exactly that, because it goes into a PowerShell script.
fn com_name(port: &Path) -> Option<String> {
    let text = port.to_str()?;
    let name = text.strip_prefix(r"\\.\").unwrap_or(text);
    let digits = name
        .get(..3)
        .filter(|prefix| prefix.eq_ignore_ascii_case("COM"))
        .and_then(|_| name.get(3..))?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("COM{digits}"))
}

/// Open a Windows serial port through .NET's `SerialPort`, in a PowerShell
/// that copies what arrives to its standard output.
///
/// Not as a file. A COM port opened with `CreateFile` reads by the rules of
/// its `COMMTIMEOUTS`, which belong to the handle and which `std` cannot set:
/// left as they are, a read can wait until the whole buffer is full, and a
/// boot log of a few hundred bytes a second arrives in lumps or not at all.
/// `SerialPort` sets the line and the timeouts itself, and a read from its
/// stream returns as soon as there is a byte. The process holds the port, so
/// it is killed when the watch ends, and only one program can hold a port:
/// a terminal left open on it makes this fail to open, with the reason.
fn open_windows(port: &Path) -> Result<Opened> {
    let Some(name) = com_name(port) else {
        return Err(Error::new(format!(
            "{} is not a Windows serial port; pass --port COM<n>.",
            port.display()
        )));
    };
    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         $p = [System.IO.Ports.SerialPort]::new('{name}', {BAUD}, 'None', 8, 'One'); \
         $p.Handshake = 'None'; $p.ReadTimeout = -1; $p.Open(); \
         $out = [Console]::OpenStandardOutput(); $buffer = [byte[]]::new(4096); \
         while ($true) {{ $n = $p.BaseStream.Read($buffer, 0, $buffer.Length); \
         if ($n -le 0) {{ break }}; $out.Write($buffer, 0, $n); $out.Flush() }}"
    );
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| {
            Error::new(format!(
                "could not start PowerShell to open {name}: {error}"
            ))
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::new("PowerShell gave no output to read"))?;
    Ok(Opened {
        reader: Box::new(stdout),
        holder: Some(Holder(child)),
    })
}

/// Put the port in raw mode at the right speed.
///
/// Through `stty` rather than `tcsetattr`, because the alternative is either a
/// dependency or a hand-written `termios` binding per platform, and this is a
/// build tool whose dependency list is meant to stay readable. The cost is one
/// process and a Unix-only command, which is the same platform the device node
/// it is configuring only exists on.
fn configure(port: &Path) -> Result<()> {
    if !port.exists() {
        return Err(Error::new(format!(
            "{} does not exist.\n  \
             The board's USB debug port creates it when plugged in; `ls /dev/ttyACM*` \
             after connecting.",
            port.display()
        )));
    }

    if cfg!(not(unix)) {
        println!("  (not Unix: leaving the port's settings alone)");
        return Ok(());
    }

    let Some(stty) = crate::paths::which("stty") else {
        return Err(Error::new(
            "stty is not on PATH, so the port cannot be set up",
        ));
    };

    let output = Command::new(stty)
        .arg("-F")
        .arg(port)
        .args([
            BAUD, "raw", "-echo", "-echoe", "-echok", "-crtscts", "-ixon",
        ])
        .output()
        .map_err(|error| Error::new(format!("running stty: {error}")))?;

    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        let why = why.trim();
        return Err(Error::new(format!(
            "could not configure {}: {why}\n  \
             A permission error here means your user is not in the group owning the \
             port — usually `dialout`: `sudo usermod -aG dialout $USER`, then log in again.",
            port.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::com_name;

    #[test]
    fn a_windows_port_is_com_and_digits_and_nothing_else() {
        assert_eq!(com_name(Path::new("COM8")).as_deref(), Some("COM8"));
        assert_eq!(com_name(Path::new("com12")).as_deref(), Some("COM12"));
        assert_eq!(com_name(Path::new(r"\\.\COM3")).as_deref(), Some("COM3"));
        // It goes into a PowerShell script, so nothing that could end a quote.
        assert_eq!(com_name(Path::new("COM8'; Remove-Item x; '")), None);
        assert_eq!(com_name(Path::new("COM")), None);
        assert_eq!(com_name(Path::new("/dev/ttyACM0")), None);
    }
}
