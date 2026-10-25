//! `test-compositor --boot chrome-save`: Chrome's Save-as dialog on Yoru's
//! desktop, GTK's file chooser. It needs what Debian's maintainer scripts
//! make and the volume's build now makes itself (`fetch-chrome.sh`):
//! `GSettings`' compiled schemas, without which `GLib` ends the browser when
//! the chooser asks for its settings, and gdk-pixbuf's loaders' cache, by
//! which the chooser draws its SVG icons.
//!
//! Chrome shows `test-chrome-window`'s yellow page; the host clicks into
//! it and presses Ctrl+S; a script in the guest says the windows'
//! titles every two seconds. Required: a window titled "Save File" came,
//! Escape took it away, and Chrome was still running after. QEMU's
//! screendump with the dialog up is kept as `chrome-save.ppm`. Skipped
//! where the machine has no Chrome volume.

use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, press, say_the_marker, with_the_transcript};
use super::{FAILED, MARKER, Programs};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The guest's side: the windows' titles.
const SCRIPT: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
while :; do
  echo "save-probe: titles $(hyprctl clients | grep -E '^[[:space:]]*title:' | tr '\n' ';')"
  sleep 2
done
"#;

/// The dialog's title, GTK's for a file chooser that saves.
const DIALOG: &str = "title: Save File";

/// `test-compositor --boot chrome-save`.
///
/// # Errors
///
/// Chrome never came, no dialog came, or Chrome ended.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each step and what it must have done"
)]
pub(super) fn test_chrome_save(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    if arch != Arch::X86_64 {
        println!("  {arch}: chrome-save skipped: Chrome for Testing is linux64 alone");
        return Ok(());
    }
    let Ok(volume) = crate::chrome::volume() else {
        println!(
            "  {arch}: chrome-save skipped: no Chrome volume (tools/common/fetch/fetch-chrome.sh)"
        );
        return Ok(());
    };
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str(crate::chrome::WINDOW_ENV);
    config.push_str(&crate::chrome::window_library_path(false));
    config.push_str(&format!(
        "exec-once = {}\n",
        crate::chrome::window_command(super::browser::CHROME_WINDOW_PAGE)
    ));
    config.push_str("exec-once = /bin/sh /etc/save-probe.sh\n");
    let links = super::desktop::chrome_links(arch, &carried.ports);
    carried.ports.extend(links);
    carried.ports.extend(crate::chrome::window_files());
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    carried.ports.push(crate::ports::File {
        path: "etc/save-probe.sh".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(SCRIPT.as_bytes().to_vec()),
    });
    let mut args = args.clone();
    args.data_image = Some(volume.clone());
    if !args.memory_given {
        args.memory = crate::chrome::MEMORY;
    }
    println!("  {arch}: chrome-save: Chrome from {}", volume.display());
    let (image, kernel) = build_image(arch, programs, &config, carried, &args)?;
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
        // Chrome's window, by its title in the probe's lines.
        if !wait(watching, "title: data:text/html", 0, 240)? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: chrome-save: Chrome's window never came")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(8));
        // Into the page, then Ctrl+S.
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
        if !wait(watching, DIALOG, from, 120)? {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: chrome-save: Ctrl+S opened no `Save File` window"
                )),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(5));
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        println!(
            "  {arch}: chrome-save: the dialog is up (the screen is {})",
            dump.display()
        );
        let from = watching.after().len();
        press(&mut qmp, &["esc"])?;
        let gone = watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
            lines
                .iter()
                .skip(from)
                .any(|line| line.contains("save-probe: titles") && !line.contains(DIALOG))
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
            .find(|line| line.contains("save-probe: titles"))
            .is_some_and(|line| line.contains("title: data:text/html"));
        let mut failures = Vec::new();
        if !gone {
            failures.push("Escape did not take the dialog away".to_owned());
        }
        if ended || !still {
            failures.push("Chrome did not outlast its dialog".to_owned());
        }
        if failures.is_empty() {
            println!(
                "  {arch}: chrome-save: Ctrl+S opened GTK's Save File dialog, Escape closed it, Chrome went on"
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
