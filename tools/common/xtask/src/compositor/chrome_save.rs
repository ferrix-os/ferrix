//! `test-compositor --boot chrome-save`: Chrome's Save-as on Yoru's
//! desktop is Ferrix's own file chooser. Chrome asks the desktop portal --
//! `org.freedesktop.portal.FileChooser`, on the session bus -- and portald
//! opens the `chooser` app, tole's `FileDialog` after the customer's Key File
//! Dialog design, rather than GTK's dialog.
//!
//! Chrome shows `test-chrome-window`'s yellow page; the host clicks into
//! it and presses Ctrl+S. Required: a window of the chooser's class came,
//! titled "Save File"; Escape took it away; Chrome went on. QEMU's
//! screendump with the dialog up is kept as `chrome-save.ppm`. Skipped
//! where the machine has no Chrome volume.

use std::time::{Duration, Instant};

use super::boot::{absolute, press, say_the_marker, with_the_transcript};
use super::portal_desktop::CHOOSER_CLASS;
use super::{FAILED, MARKER, Programs};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// `test-compositor --boot chrome-save`.
///
/// # Errors
///
/// Chrome never came, the chooser did not, or Chrome ended.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each step and what it must have done"
)]
pub(super) fn test_chrome_save(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let lines = format!(
        "exec-once = {}\n",
        crate::chrome::window_command(super::browser::CHROME_WINDOW_PAGE)
    );
    let Some((image, kernel, args)) =
        super::portal_desktop::image(arch, programs, args, "chrome-save", &lines, Vec::new())?
    else {
        return Ok(());
    };
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("chrome-save.ppm");
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        if watching.lines().iter().any(|line| line.contains(FAILED)) {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: the compositor failed before `{MARKER}`")),
                watching,
            ));
        }
        say_the_marker(watching, arch);
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let wait = |watching: &mut Watching<'_>, word: &str, from: usize, secs: u64| {
            watching.read_more(Instant::now() + Duration::from_secs(secs), |lines| {
                lines.iter().skip(from).any(|line| line.contains(word))
            })
        };
        if !wait(watching, "title: data:text/html", 0, 240)? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: chrome-save: Chrome's window never came")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(8));
        let (ax, ay) = (0x7fff / 2, 0x7fff / 2);
        qmp.input_send_event(&[absolute("x", ax), absolute("y", ay)])?;
        std::thread::sleep(Duration::from_millis(500));
        let button = |down: bool| {
            format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
        };
        qmp.input_send_event(&[button(true)])?;
        qmp.input_send_event(&[button(false)])?;
        std::thread::sleep(Duration::from_secs(2));
        let from = watching.after().len();
        press(&mut qmp, &["ctrl", "s"])?;
        if !wait(watching, CHOOSER_CLASS, from, 120)? {
            let gtk = watching
                .after()
                .iter()
                .skip(from)
                .any(|line| line.contains("portal-probe") && line.contains("title: Save File"));
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: chrome-save: Ctrl+S opened {}",
                    if gtk {
                        "GTK's Save File dialog, not Ferrix's chooser: the portal was not asked"
                    } else {
                        "no dialog at all"
                    }
                )),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(5));
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        println!(
            "  {arch}: chrome-save: Ferrix's chooser is up (the screen is {})",
            dump.display()
        );
        let from = watching.after().len();
        press(&mut qmp, &["esc"])?;
        let gone = watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
            lines
                .iter()
                .skip(from)
                .any(|line| line.contains("portal-probe: windows") && !line.contains(CHOOSER_CLASS))
        })?;
        let ended = watching
            .lines()
            .iter()
            .chain(watching.after())
            .any(|line| line.contains("app-chrome-") && line.contains("stopped"));
        let still = watching
            .after()
            .iter()
            .skip(from)
            .rev()
            .find(|line| line.contains("portal-probe: windows"))
            .is_some_and(|line| line.contains("title: data:text/html"));
        let mut failures = Vec::new();
        if !gone {
            failures.push("Escape did not take the chooser away".to_owned());
        }
        if ended || !still {
            failures.push("Chrome did not outlast its dialog".to_owned());
        }
        if failures.is_empty() {
            println!(
                "  {arch}: chrome-save: Ctrl+S opened Ferrix's chooser through the portal, Escape closed it, Chrome went on"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: chrome-save: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
