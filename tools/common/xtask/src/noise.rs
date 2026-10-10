//! What QEMU writes to its standard error that is not about this guest.
//!
//! A boot's serial output is QEMU's *stdout*; its stderr is QEMU's own
//! remarks, and those are worth reading — an argument it refuses, a device it
//! has not got, the reason a run ended before the guest said anything. So
//! stderr is passed through rather than thrown away, and only one message is
//! taken out of it.
//!
//! # The one message, and why it is not ours to fix
//!
//! `run-compositor --gl` on a Wayland host fills the terminal with
//!
//! ```text
//! (qemu:65917): Gdk-WARNING **: 00:53:50.302: eglMakeCurrent failed
//! ```
//!
//! four times for every frame the guest draws, which at sixty frames a second
//! is a serial log nobody can read.
//!
//! The warning is GDK's, from `gdk_wayland_display_make_gl_context_current`
//! in `gdk/wayland/gdkglcontext-wayland.c`, and the EGL error behind it is
//! `EGL_BAD_ACCESS`: *the context is already current to some other thread*.
//! The other thread is QEMU's vCPU thread. When the guest resets the
//! virtio-gpu — a one-byte write to the device-status register, which every
//! virtio driver does as it starts and as it stops — `virtio_gpu_gl_reset` in
//! `hw/display/virtio-gpu-gl.c` calls `virtio_gpu_virgl_reset_scanout`
//! directly, under a comment saying that GL functions must be called on the
//! main thread. That reaches `gtk_gl_area_make_current` from the vCPU thread
//! and binds the GL area's EGL context there; afterwards GTK's own render
//! callback can never bind it again, and warns once per attempt until the
//! context comes back.
//!
//! It is QEMU's bug, not the guest's — it is reached before the compositor
//! starts, while the loader is still drawing — and it is unfixed upstream as
//! of v11.1.2. Nothing Ferrix can do from inside the guest avoids it, short of
//! never resetting the card. The QEMU `tools/common/fetch/fetch-qemu-linux.sh`
//! builds carries the fix (`tools/common/data/qemu/0003-*.patch`, see
//! `docs/GPU.md` §3.12), so a count here means a QEMU without it. The line is
//! still hidden and counted for any other QEMU, and [`report`] says how many
//! went.
//!
//! # Why the blank lines go too
//!
//! glib writes a blank line before every message, so dropping the warning on
//! its own would leave the terminal just as long and rather more puzzling. A
//! blank line is therefore held back until the line after it is known: it is
//! printed if that line is printed, and dropped with it if not.
//!
//! # And a run's DMA faults, when it is judged
//!
//! A run this tool judges also has QEMU trace every VT-d fault, and the sieve
//! takes those lines, and QEMU's own remarks about the faults, out of stderr
//! and hands them to [`crate::dma_faults`], which holds them to what the
//! kernel said it provoked. `crate::dma_faults` says why they cannot simply be
//! shown.

use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStderr, Command, Stdio};
use std::thread::JoinHandle;

use crate::dma_faults::{self, Seen};
use crate::{Error, Result};

/// The domain glib stamps the message with. Matched as well as the text
/// below, so that a line quoting the warning — QEMU's own `egl:
/// eglMakeCurrent failed`, from the X11 path, which is a real error report —
/// is not mistaken for it.
const DOMAIN: &str = "Gdk-WARNING";

/// The text of the message this hides.
const MESSAGE: &str = "eglMakeCurrent failed";

/// Whether `line` is the message this hides.
fn hidden(line: &str) -> bool {
    line.contains(DOMAIN) && line.contains(MESSAGE)
}

/// What a line puts on the screen.
#[derive(Debug, PartialEq, Eq)]
enum Show {
    /// Nothing: it was hidden, or it is a blank being held for the next one.
    Nothing,
    /// The line itself.
    Line,
    /// A blank line that was held back, and then the line itself.
    BlankThenLine,
}

/// Reads stderr a line at a time and says what to print.
#[derive(Debug)]
struct Sieve {
    /// A blank line seen and not yet printed, waiting on the line after it.
    held: bool,
    /// How many lines have been hidden.
    hidden: u64,
    /// Whether DMA fault lines are taken out, for a run that is judged.
    judged: bool,
    /// The DMA fault lines taken out.
    dma: Seen,
}

impl Sieve {
    const fn new(judged: bool) -> Self {
        Self {
            held: false,
            hidden: 0,
            judged,
            dma: Seen {
                faults: Vec::new(),
                remarks: Vec::new(),
            },
        }
    }

    /// What `line` should put on the screen.
    fn take(&mut self, line: &str) -> Show {
        if self.judged {
            if let Some(fault) = dma_faults::traced(line) {
                self.dma.faults.push(fault);
                return self.set_aside();
            }
            if dma_faults::remark(line) {
                self.dma.remarks.push(line.to_owned());
                return self.set_aside();
            }
        }
        if line.trim().is_empty() {
            // Two blanks in a row: the first was not the one glib writes
            // before a message, so it is a blank line of somebody's own.
            let show = if self.held { Show::Line } else { Show::Nothing };
            self.held = true;
            return show;
        }
        if hidden(line) {
            self.hidden += 1;
            self.held = false;
            return Show::Nothing;
        }
        let show = if self.held {
            Show::BlankThenLine
        } else {
            Show::Line
        };
        self.held = false;
        show
    }

    /// What is left to print once the stream has ended: the held blank, if
    /// the last line was one, since nothing came to drop it with.
    const fn flush(&self) -> Show {
        if self.held { Show::Line } else { Show::Nothing }
    }

    /// Take a DMA fault line out of the stream. A blank held before it is not
    /// glib's, so it is printed rather than dropped.
    const fn set_aside(&mut self) -> Show {
        let show = if self.held { Show::Line } else { Show::Nothing };
        self.held = false;
        show
    }
}

/// Lines a gate asked to have kept for it rather than shown: the prefix
/// they begin with, and the file they are appended to ([`keep`]).
static KEPT: std::sync::Mutex<Option<(&'static str, std::path::PathBuf)>> =
    std::sync::Mutex::new(None);

/// From now on, append every line of a child's standard error that begins
/// with `prefix` to `file`, and keep it off the terminal: a trace QEMU
/// writes there that a gate counts afterwards, as `test-nvme` counts the
/// NVMe model's Flush commands. It shares the stream with the DMA fault
/// trace, which is why it is not sent to a log file of QEMU's own.
pub(crate) fn keep(prefix: &'static str, file: &std::path::Path) {
    if let Ok(mut kept) = KEPT.lock() {
        *kept = Some((prefix, file.to_path_buf()));
    }
}

/// Whether `line` is one [`keep`] asked for, in which case it is in the
/// file now.
fn kept(line: &str) -> bool {
    let Ok(kept) = KEPT.lock() else {
        return false;
    };
    let Some((prefix, file)) = kept.as_ref() else {
        return false;
    };
    if !line.starts_with(prefix) {
        return false;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = writeln!(file, "{line}");
    }
    true
}

/// What a sieve took out of a stream.
#[derive(Debug, Default)]
pub(crate) struct Sieved {
    /// How many GDK warnings were hidden.
    pub(crate) hidden: u64,
    /// The DMA fault lines taken out of a judged run's stream.
    pub(crate) dma: Seen,
}

/// A thread carrying a child's standard error to this one's, sieved.
#[derive(Debug)]
pub(crate) struct Filter {
    thread: JoinHandle<Sieved>,
}

impl Filter {
    /// Start carrying `stderr`.
    pub(crate) fn start(stderr: ChildStderr) -> Self {
        Self::carry(stderr, false)
    }

    /// Start carrying the stderr of a run this tool judges, taking out the
    /// DMA fault lines [`crate::dma_faults`] judges it by.
    pub(crate) fn start_judged(stderr: ChildStderr) -> Self {
        Self::carry(stderr, true)
    }

    fn carry(stderr: ChildStderr, judged: bool) -> Self {
        let thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut sieve = Sieve::new(judged);
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                // Bytes rather than `lines()`: what QEMU writes here is a
                // guest's doing as often as not, and a stream that is not
                // UTF-8 must still reach the terminal rather than end the
                // thread and take the rest of stderr with it.
                match reader.read_until(b'\n', &mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&bytes);
                let line = line.trim_end_matches(['\n', '\r']);
                if kept(line) {
                    continue;
                }
                match sieve.take(line) {
                    Show::Nothing => {}
                    Show::Line => eprintln!("{line}"),
                    Show::BlankThenLine => eprintln!("\n{line}"),
                }
            }
            if sieve.flush() == Show::Line {
                eprintln!();
            }
            let _ = std::io::stderr().flush();
            Sieved {
                hidden: sieve.hidden,
                dma: sieve.dma,
            }
        });
        Self { thread }
    }

    /// Wait for the stream to end, and say what was taken out of it.
    ///
    /// A thread that panicked hid nothing worth saying, and its panic has
    /// already been printed, so what it had taken is lost rather than a
    /// failure of the boot it was watching.
    pub(crate) fn finish(self) -> Sieved {
        self.thread.join().unwrap_or_default()
    }
}

/// Say how many lines were hidden, if any were.
///
/// Hiding output without saying so is how a person comes to distrust a log,
/// so the count is printed and names what it counted.
pub(crate) fn report(hidden: u64) {
    if hidden == 0 {
        return;
    }
    println!(
        "  {hidden} `{MESSAGE}` warnings from GDK hidden; QEMU resets the card's \
         GL context on the wrong thread (tools/common/xtask/src/noise.rs says why)"
    );
}

/// Run `command` with its standard error sieved, and wait for it to finish.
///
/// Its standard output and input are left alone: the guest's serial port is
/// on them, and a person typing at it needs the terminal it always had.
///
/// # Errors
///
/// When QEMU cannot be started, or exits unsuccessfully.
pub(crate) fn run(mut command: Command, description: &str) -> Result<()> {
    let _ = command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| Error::new(format!("could not run {description}: {error}")))?;
    let filter = child.stderr.take().map(Filter::start);
    let status = child
        .wait()
        .map_err(|error| Error::new(format!("could not wait for {description}: {error}")))?;
    report(filter.map_or(0, |filter| filter.finish().hidden));
    crate::cargo::finished(status, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The warning as glib writes it, blank line and all.
    const WARNING: &str = "(qemu:65917): Gdk-WARNING **: 00:53:50.302: eglMakeCurrent failed";

    /// Run `lines` through a sieve and return what reached the screen, with a
    /// blank line as an empty string, and how many were hidden.
    fn sieved(lines: &[&str]) -> (Vec<String>, u64) {
        let mut sieve = Sieve::new(false);
        let mut shown = Vec::new();
        for line in lines {
            match sieve.take(line) {
                Show::Nothing => {}
                Show::Line => shown.push((*line).to_owned()),
                Show::BlankThenLine => {
                    shown.push(String::new());
                    shown.push((*line).to_owned());
                }
            }
        }
        if sieve.flush() == Show::Line {
            shown.push(String::new());
        }
        (shown, sieve.hidden)
    }

    #[test]
    fn the_warning_goes_and_takes_its_blank_line_with_it() {
        let (shown, hidden) = sieved(&["", WARNING, "", WARNING, "", WARNING]);
        assert_eq!(shown, Vec::<String>::new());
        assert_eq!(hidden, 3);
    }

    #[test]
    fn everything_else_qemu_says_still_reaches_the_terminal() {
        let complaint = "qemu-system-x86_64: -device ide-hd: Failed to get \"write\" lock";
        let (shown, hidden) = sieved(&["", WARNING, "", complaint]);
        assert_eq!(shown, ["", complaint]);
        assert_eq!(hidden, 1);
    }

    /// The X11 path's message is QEMU's own `error_report`, which is a real
    /// failure and says so without glib's domain. It must survive.
    #[test]
    fn qemus_own_egl_error_is_not_the_warning() {
        let reported = "qemu: egl: eglMakeCurrent failed: EGL_BAD_ACCESS";
        assert!(!hidden(reported));
        let (shown, count) = sieved(&[reported]);
        assert_eq!(shown, [reported]);
        assert_eq!(count, 0);
    }

    /// A judged run's DMA fault lines leave the stream for `dma_faults`;
    /// anything else, and every line of a run that is not judged, stays.
    #[test]
    fn a_judged_run_sets_its_dma_fault_lines_aside() {
        let trace = "1@1.0:vtd_dmar_fault sid 0x10 fault 5 addr 0x1000 write 1";
        let remark = "qemu-system-x86_64: vtd_iommu_translate: detected translation failure \
                      (dev=00:02:00, iova=0x1000)";
        let other = "qemu-system-x86_64: warning: something else";
        let mut sieve = Sieve::new(true);
        assert_eq!(sieve.take(trace), Show::Nothing);
        assert_eq!(sieve.take(remark), Show::Nothing);
        assert_eq!(sieve.take(other), Show::Line);
        assert_eq!(sieve.dma.faults.len(), 1);
        assert_eq!(sieve.dma.remarks, [remark]);

        let (shown, _) = sieved(&[trace, remark]);
        assert_eq!(shown, [trace, remark]);
    }

    #[test]
    fn a_blank_line_of_somebody_elses_is_kept() {
        // Two in a row: the first cannot be the one glib writes before a
        // message, because the second is not a message.
        let (shown, hidden) = sieved(&["first", "", "", "second"]);
        assert_eq!(shown, ["first", "", "", "second"]);
        assert_eq!(hidden, 0);
        // And one at the very end, with nothing after it to drop it with.
        let (shown, hidden) = sieved(&["first", ""]);
        assert_eq!(shown, ["first", ""]);
        assert_eq!(hidden, 0);
    }
}
