//! `test-compositor --boot gtk-chooser`: a plain GTK 3 program's and a GTK
//! 4 program's file dialogs on Yoru's desktop are Ferrix's own chooser.
//! The session's environment has `GTK_USE_PORTAL=1` and
//! `GDK_DEBUG=portals` (Yoru's theme), so GTK 3's `GtkFileChooserNative`
//! and GTK 4's `GtkFileDialog` ask the desktop portal, and portald opens
//! the `chooser` app.
//!
//! The GTK 3 one is the volume's `gtk3-open` (`fetch-chrome.sh` builds it),
//! the GTK 4 one Debian's zenity (`zenity --file-selection`), one after the
//! other. For each, required: a window of the chooser's class came, and
//! Escape took it away with the program told it was cancelled -- gtk3-open
//! ends 0 having said so, zenity ends 1 -- and neither ended on a fault. Kept as
//! `gtk-chooser-3.ppm` and `gtk-chooser-4.ppm`. Skipped where the machine
//! has no Chrome volume, which carries both.

use std::time::{Duration, Instant};

use super::boot::{press, say_the_marker, with_the_transcript};
use super::portal_desktop::CHOOSER_CLASS;
use super::{FAILED, MARKER, Programs};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The guest's side: GTK 3's dialog, then GTK 4's.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
sleep 4
/data/usr/local/bin/gtk3-open
echo "gtk-chooser: gtk3-open ended $?"
/data/usr/bin/zenity --file-selection --title "Open a file (GTK 4)"
echo "gtk-chooser: zenity ended $?"
"#;

/// `test-compositor --boot gtk-chooser`.
///
/// # Errors
///
/// A dialog that was GTK's, or none, or one Escape did not close.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each program's dialog and what it must have done"
)]
pub(super) fn test_gtk_chooser(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let ports = vec![crate::ports::File {
        path: "etc/gtk-chooser.sh".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(SCRIPT.as_bytes().to_vec()),
    }];
    let Some((image, kernel, args)) = super::portal_desktop::image(
        arch,
        programs,
        args,
        "gtk-chooser",
        "exec-once = /bin/sh /etc/gtk-chooser.sh\n",
        ports,
    )?
    else {
        return Ok(());
    };
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
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
        let mut failures = Vec::new();
        // Cancelled: gtk3-open says so and ends 0, zenity ends 1.
        for (gtk, ended, cancelled) in [
            (
                "3",
                "gtk-chooser: gtk3-open ended",
                "gtk-chooser: gtk3-open ended 0",
            ),
            (
                "4",
                "gtk-chooser: zenity ended",
                "gtk-chooser: zenity ended 1",
            ),
        ] {
            let from = watching.after().len();
            let came = watching.read_more(Instant::now() + Duration::from_secs(120), |lines| {
                lines
                    .iter()
                    .skip(from)
                    .any(|line| line.contains("portal-probe") && line.contains(CHOOSER_CLASS))
            })?;
            if !came {
                let said = watching
                    .after()
                    .iter()
                    .skip(from)
                    .rev()
                    .find(|line| line.contains("portal-probe"))
                    .cloned()
                    .unwrap_or_default();
                failures.push(format!(
                    "GTK {gtk}'s dialog was not Ferrix's chooser: {}",
                    said.trim()
                ));
                break;
            }
            std::thread::sleep(Duration::from_secs(4));
            let dump = paths::build_dir(arch).join(format!("gtk-chooser-{gtk}.ppm"));
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            println!(
                "  {arch}: gtk-chooser: GTK {gtk}'s dialog is Ferrix's chooser (the screen is {})",
                dump.display()
            );
            let from = watching.after().len();
            press(&mut qmp, &["esc"])?;
            let told = watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
                lines.iter().skip(from).any(|line| line.contains(ended))
            })?;
            if !told {
                failures.push(format!("Escape did not end GTK {gtk}'s dialog"));
                break;
            }
            let line = watching
                .after()
                .iter()
                .skip(from)
                .find(|line| line.contains(ended))
                .cloned()
                .unwrap_or_default();
            println!("  {arch}: gtk-chooser: {}", line.trim());
            if !line.contains(cancelled) {
                failures.push(format!(
                    "GTK {gtk}'s program was not told it was cancelled: {}",
                    line.trim()
                ));
                break;
            }
        }
        if failures.is_empty() {
            println!(
                "  {arch}: gtk-chooser: GTK 3's and GTK 4's file dialogs were Ferrix's chooser, through the portal"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: gtk-chooser: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
