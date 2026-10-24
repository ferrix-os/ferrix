//! `test-compositor --boot settings-prompt`: the trusted prompt
//! (`docs/SETTINGS-WRITER.md` §12) on a desktop that is `ferrix`'s, with
//! authd, settingsd and sessiond.
//!
//! The person's keys are QEMU's (`input-send-event`): the seat's own
//! devices. A program of `ferrix`'s, started by a bind, asks settingsd for
//! a change and then, as an attacker of the same uid would, types the
//! password and Return with a virtual keyboard and takes a screenshot while
//! the prompt is up. What is required, each with the sentence its control
//! (ledger 3636 (11)) must make:
//!
//! | What | Control |
//! |---|---|
//! | the prompt comes up for a request from the desktop's account | T2: settingsd's seat check skipped |
//! | the injected password and Return answer nothing | T1: hyprix takes injected input |
//! | a bind does not fire while it is up | T8: input goes the usual way while it is up |
//! | QEMU's own screendump shows no client while it is up | T7: windows drawn under the prompt |
//! | a screenshot taken while it is up is black | T4: no blackout |
//! | a wrong password typed in it is refused, nothing changes; the right one renames the machine | |
//! | an Allow-only prompt (`Methods=none` for `settings.services`) ignores Return in its first second, and takes it after | T9: P5's delay removed |
//! | Escape in an Allow-only prompt makes nothing | T10: settingsd applies after a Cancel |
//! | a program off the list is refused with no prompt | T3: settingsd's list check skipped |
//! | Ctrl+Alt+Delete with no prompt up is the compositor's | |

use std::time::{Duration, Instant};

use super::boot::{judge_still_running, judged_image, press};
use super::{Carried, EITHER, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// `ferrix`'s password, seeded by this boot alone, typed on the US layout.
const PASSWORD: &str = "prompt";

/// The boot's configuration: a window under everything, and a bind for each
/// request.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot settings-prompt`.
exec-once = /bin/pattern checkerboard one
bind = , F1, exec, /bin/sh /etc/ask.sh s1 name boxa
bind = , F2, exec, /bin/sh /etc/ask.sh s2 name box
bind = , F3, exec, /bin/sh /etc/ask.sh s3 sshd on
bind = , F4, exec, /bin/sh /etc/ask.sh s4 sshd off
bind = , F5, exec, /bin/sh /etc/ask.sh s5 unlisted
bind = , F9, exec, /bin/sh -c 'echo probe: bind-fired'
";

/// The program of `ferrix`'s that asks: the request in the background, and
/// while its prompt is up, an attacker's injected keys and a screenshot.
const ASK: &str = r#"#!/bin/sh
exec 2>&1
tag=$1
shift
case "$1" in
  name) request="set-hostname $2" ;;
  sshd) request="service-enable-persistent sshd.service $2" ;;
  unlisted)
    cp /bin/settingsctl /tmp/unlisted
    /tmp/unlisted set-hostname unlisted </dev/null >/tmp/answer 2>&1
    # The compositor's prompt channel cannot be opened through /proc.
    for dir in /proc/[0-9]*; do
      if [ "$(cat $dir/comm 2>/dev/null)" = hyprix ]; then
        cat $dir/fd/5 >/dev/null 2>/tmp/fd5 && echo "probe: $tag fd5 opened" \
          || echo "probe: $tag fd5 refused $(cat /tmp/fd5)"
      fi
    done ;;
esac
if [ -n "$request" ]; then
  settingsctl $request </dev/null >/tmp/answer 2>&1 &
  asked=$!
  if [ "$tag" = s1 ]; then
    sleep 3
    /bin/vkbd p r o m p t Return
    echo "probe: $tag injected"
    echo "probe: $tag during $(/bin/shot)"
  fi
  wait $asked
fi
while read -r line; do echo "probe: $tag $line"; done </tmp/answer
link=no
ls /etc/ferrix/units/multi-user.target.wants/sshd.service >/dev/null 2>&1 && link=yes
echo "probe: $tag end names $(cat /etc/hostname) $(hostname) sshd-linked $link"
"#;

/// A stand-in `sshd.service` the services page switches.
const SSHD_UNIT: &str = "[Unit]\nDescription=settings-prompt's stand-in for sshd\n\n\
                         [Service]\nExecStart=/bin/sleep 100000\n\n\
                         [Install]\nWantedBy=multi-user.target\n";

/// `settings.services` asks no password on this image: its prompt is
/// Allow and Cancel alone.
const NO_PASSWORD: &str = "[Service]\nMethods=none\n";

/// FNV-1a, as `shot` prints its digest (`compositor_shot::digest`).
fn digest(bytes: impl Iterator<Item = u8>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Whether `line` (`shot: WxH DIGEST`) is a picture all black.
fn black(line: &str) -> bool {
    let Some(rest) = line.split("shot: ").nth(1) else {
        return false;
    };
    let mut words = rest.split_whitespace();
    let (Some(size), Some(said)) = (words.next(), words.next()) else {
        return false;
    };
    let Some((width, height)) = size.split_once('x') else {
        return false;
    };
    let (Ok(width), Ok(height)) = (width.parse::<usize>(), height.parse::<usize>()) else {
        return false;
    };
    format!(
        "{:016x}",
        digest(std::iter::repeat_n(0_u8, width * height * 3))
    ) == said
}

/// `test-compositor --boot settings-prompt`.
///
/// # Errors
///
/// A step the module documentation lists, with its sentence.
pub(super) fn test_settings_prompt(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(busybox) = crate::busybox::installed_program(arch) else {
        println!("  {arch}: no busybox on this machine for the boot's probes; skipped");
        return Ok(());
    };
    let file = |path: &str, mode: u32, text: &str| crate::ports::File {
        path: path.to_owned(),
        mode,
        content: crate::ports::Content::Bytes(text.as_bytes().to_vec()),
    };
    let mut ports = super::hyprlock::hyprlock_files(arch, Some(("ferrix", PASSWORD)))?;
    let settings = crate::settingsd::carried(arch)?;
    if settings.is_empty() {
        return Err(Error::new(format!("{arch}: settingsd is not built for it")));
    }
    ports.extend(settings);
    ports.extend([
        file("etc/ask.sh", 0o755, ASK),
        file("lib/ferrix/units/sshd.service", 0o644, SSHD_UNIT),
        file(
            "etc/ferrix/auth/services/settings.services.d/10-none.conf",
            0o644,
            NO_PASSWORD,
        ),
        file("etc/hostname", 0o644, "ferrix\n"),
    ]);
    let carried = Carried {
        busybox: Some(busybox),
        ports,
        ..Carried::none()
    };
    let session = Args {
        session_user: Some(crate::session::USER.to_owned()),
        ..args.clone()
    };
    let (image, kernel) = judged_image(arch, programs, CONFIG, carried, None, &session)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let mut failures = Vec::new();
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        said = drive(arch, port, watching, &mut failures)?;
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    if !failures.is_empty() {
        let mut message = format!(
            "{arch}: the trusted prompt did not do what docs/SETTINGS-WRITER.md §12 requires:\n"
        );
        for failure in &failures {
            message.push_str(&format!("    - {failure}\n"));
        }
        message.push_str(&format!(
            "  Serial output is in {}",
            paths::build_dir(arch).join("serial.log").display()
        ));
        return Err(Error::new(message));
    }
    judge_still_running(arch, &said)?;
    println!(
        "  {arch}: as ferrix, settingsd asked on the seat; injected keys, a bind and a \
         screenshot reached nothing while the prompt was up and QEMU's screen showed no \
         client; a wrong password changed nothing and the right one renamed the machine; an \
         Allow-only prompt waited its second; Escape made nothing; a program off the list was \
         refused; Ctrl+Alt+Delete was the compositor's"
    );
    Ok(())
}

/// The lines since `from` that hold `word`.
fn since<'a>(watching: &'a Watching<'_>, from: usize, word: &str) -> Vec<&'a str> {
    let after = watching.after();
    after
        .get(from.min(after.len())..)
        .unwrap_or_default()
        .iter()
        .filter(|line| line.contains(word))
        .map(String::as_str)
        .collect()
}

/// Wait for step `tag`'s last line, and give its lines.
fn ended(watching: &mut Watching<'_>, from: usize, tag: &str) -> Result<Vec<String>> {
    let last = format!("probe: {tag} end ");
    let _ = wait_for(watching, from, &last, 90)?;
    Ok(since(watching, from, &format!("probe: {tag} "))
        .into_iter()
        .map(str::to_owned)
        .collect())
}

/// Whether any of `lines` holds `word`.
fn has(lines: &[String], word: &str) -> bool {
    lines.iter().any(|line| line.contains(word))
}

/// Wait up to `seconds` for a line holding `word` after `from`.
fn wait_for(watching: &mut Watching<'_>, from: usize, word: &str, seconds: u64) -> Result<bool> {
    watching.read_more(Instant::now() + Duration::from_secs(seconds), |lines| {
        lines.iter().skip(from).any(|line| line.contains(word))
    })
}

/// Type `text` key by key, then Return.
fn type_in(qmp: &mut Qmp, text: &str) -> Result<()> {
    for c in text.chars() {
        press(qmp, &[&c.to_string()])?;
        std::thread::sleep(Duration::from_millis(120));
    }
    press(qmp, &["ret"])
}

/// Whether QEMU's own screendump shows the prompt's background at all four
/// corners, where the window under it would otherwise be (T7).
fn corners_are_the_prompt_s(qmp: &mut Qmp, arch: Arch) -> Result<bool> {
    let path = paths::build_dir(arch).join("settings-prompt.ppm");
    qmp.screendump(Some(DEVICE_ID), &path)?;
    let bytes = std::fs::read(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    let image = crate::display::parse_ppm(&bytes)?;
    let at = |x: usize, y: usize| {
        let i = (y * image.width + x) * 3;
        image.pixels.get(i..i + 3).map(<[u8]>::to_vec)
    };
    let (w, h) = (
        image.width.saturating_sub(4),
        image.height.saturating_sub(4),
    );
    Ok([(4, 4), (w, 4), (4, h), (w, h)]
        .iter()
        .all(|&(x, y)| at(x, y).as_deref() == Some(&[0x10, 0x14, 0x18][..])))
}

#[expect(
    clippy::too_many_lines,
    reason = "the boot's steps, one after the other, each with its sentence"
)]
fn drive(
    arch: Arch,
    port: u16,
    watching: &mut Watching<'_>,
    failures: &mut Vec<String>,
) -> Result<Vec<String>> {
    let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
    if !watching.read_more(Instant::now() + SETTLE, |lines| {
        lines.iter().any(|line| line.contains(MARKER))
    })? {
        return Err(Error::new(format!(
            "{arch}: the compositor never printed `{MARKER}`"
        )));
    }
    let _ = wait_for(
        watching,
        0,
        "settingsd's trusted prompts come through sessiond",
        30,
    )?;
    std::thread::sleep(Duration::from_secs(2));
    let up = "hyprix: a trusted prompt is up";

    // 1. The request with the attacker beside it, and a wrong password.
    let from = watching.after().len();
    press(&mut qmp, &["f1"])?;
    if !wait_for(watching, from, up, 60)? {
        failures
            .push("a request from the desktop was answered without the trusted prompt".to_owned());
        return Ok(watching.after().to_vec());
    }
    // While it is up, before the attacker's keys: a bind, and QEMU's own
    // picture of the screen.
    std::thread::sleep(Duration::from_millis(1000));
    press(&mut qmp, &["f9"])?;
    // A bind's program takes a moment to start and say so.
    if wait_for(watching, from, "probe: bind-fired", 4)? {
        failures.push("a bind fired while the trusted prompt was up".to_owned());
    }
    if !corners_are_the_prompt_s(&mut qmp, arch)? {
        failures.push("a client was drawn while the trusted prompt was up".to_owned());
    }
    // The attacker's password and Return: given the time a right password
    // takes to be checked, nothing may come of them.
    let _ = wait_for(watching, from, "probe: s1 injected", 30)?;
    if wait_for(watching, from, "probe: s1 settingsctl: ", 10)?
        || !since(watching, from, "hyprix: the trusted prompt closed").is_empty()
    {
        failures.push(format!(
            "an injected key answered the trusted prompt: {:?}",
            since(watching, from, "probe: s1 settingsctl: ")
        ));
    }
    let _ = wait_for(watching, from, "probe: s1 during", 20)?;
    let during = since(watching, from, "probe: s1 during");
    if !during.iter().any(|line| black(line)) {
        failures.push(format!(
            "a screenshot showed the trusted prompt: {during:?}"
        ));
    }
    type_in(&mut qmp, "wrong")?;
    let s1 = ended(watching, from, "s1")?;
    if !has(&s1, "settingsctl: refused: authentication-failed")
        || !has(&s1, "end names ferrix ferrix")
    {
        failures.push(format!(
            "a wrong password in the prompt was not refused: {s1:?}"
        ));
    }

    // 2. The right password renames the machine. The last prompt's closing
    // words go first: a key while they show is still the prompt's.
    std::thread::sleep(Duration::from_secs(3));
    let from = watching.after().len();
    press(&mut qmp, &["f2"])?;
    if wait_for(watching, from, up, 60)? {
        std::thread::sleep(Duration::from_millis(1500));
        type_in(&mut qmp, PASSWORD)?;
    }
    let s2 = ended(watching, from, "s2")?;
    if !has(
        &s2,
        "settingsctl: done: the computer is called box (was ferrix)",
    ) || !has(&s2, "end names box box")
    {
        failures.push(format!(
            "the right password in the prompt did not rename the machine: {s2:?}"
        ));
    }

    // 3. Allow-only: Return in its first second is nothing, after it is Allow.
    std::thread::sleep(Duration::from_secs(3));
    let from = watching.after().len();
    press(&mut qmp, &["f3"])?;
    // Return the moment the prompt can take Allow, inside its first second.
    if wait_for(watching, from, "hyprix: the trusted prompt asks", 60)? {
        press(&mut qmp, &["ret"])?;
        std::thread::sleep(Duration::from_millis(1500));
        if !since(watching, from, "probe: s3 settingsctl: ").is_empty() {
            failures.push("a key in the prompt's first second answered it".to_owned());
        }
        press(&mut qmp, &["ret"])?;
    }
    let s3 = ended(watching, from, "s3")?;
    if !has(&s3, "settingsctl: done: sshd.service is on for every boot")
        || !has(&s3, "sshd-linked yes")
    {
        failures.push(format!(
            "Allow in an Allow-only prompt did not turn sshd on: {s3:?}"
        ));
    }

    // 4. Escape in an Allow-only prompt makes nothing.
    std::thread::sleep(Duration::from_secs(3));
    let from = watching.after().len();
    press(&mut qmp, &["f4"])?;
    if wait_for(watching, from, up, 60)? {
        std::thread::sleep(Duration::from_millis(1500));
        press(&mut qmp, &["esc"])?;
    }
    let s4 = ended(watching, from, "s4")?;
    if !has(&s4, "settingsctl: refused: cancelled") || !has(&s4, "sshd-linked yes") {
        failures.push(format!("a cancelled prompt changed something: {s4:?}"));
    }

    // 5. A program off the list: refused, and no prompt.
    std::thread::sleep(Duration::from_secs(3));
    let from = watching.after().len();
    press(&mut qmp, &["f5"])?;
    let s5 = ended(watching, from, "s5")?;
    if !has(
        &s5,
        "settingsctl: refused: not-permitted: /tmp/unlisted may not ask",
    ) || !since(watching, from, up).is_empty()
    {
        failures.push(format!("a program off the list was not refused: {s5:?}"));
    }
    if has(&s5, "fd5 opened") || !has(&s5, "No such device or address") {
        failures.push(format!(
            "the compositor's prompt channel opened through /proc: {s5:?}"
        ));
    }

    // 6. The trusted key, with no prompt up.
    std::thread::sleep(Duration::from_secs(1));
    let from = watching.after().len();
    press(&mut qmp, &["ctrl", "alt", "delete"])?;
    if !wait_for(
        watching,
        from,
        "hyprix: the trusted key: no prompt is waiting",
        20,
    )? {
        failures.push("Ctrl+Alt+Delete was not the compositor's".to_owned());
    }
    Ok(watching
        .lines()
        .iter()
        .chain(watching.after())
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{black, digest};

    #[test]
    fn a_black_picture_is_known_by_its_digest() {
        let line = format!(
            "probe: during shot: 2x1 {:016x}",
            digest(std::iter::repeat_n(0, 6))
        );
        assert!(black(&line));
        assert!(!black("probe: during shot: 2x1 0000000000000000"));
        assert!(!black("probe: during shot: failed"));
    }
}
