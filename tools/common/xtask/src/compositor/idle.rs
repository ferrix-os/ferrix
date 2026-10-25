//! The two hypridle boots: `idle`, with a configuration of its own whose
//! timeouts are seconds, and `idle-user`, with the user's own
//! `~/.config/hypr/hypridle.conf` read off this machine at build time and
//! carried unchanged.
//!
//! Both boot the compositor with two windows and `/bin/hypridle` started
//! by `exec-once`, and both are judged by what the screen shows and what
//! hypridle said: the screen goes black when a listener's `on-timeout` runs
//! `hyprctl dispatch dpms off`, and comes back when a key -- QEMU's virtio
//! keyboard, the same path a person's typing takes -- ends the idle and the
//! listener's `on-resume` runs `hyprctl dispatch dpms on`.
//!
//! The first also presses a key bound to `loginctl lock-session`, which is
//! how the user's file locks, and requires the whole chain Ferrix has for
//! it: `loginctl` reaching hypridle over its socket, `lock_cmd` starting
//! `/bin/lock`, the compositor telling hypridle through
//! `hyprland-lock-notify-v1` that the session locked and unlocked, and
//! `on_lock_cmd` and `on_unlock_cmd` running.
//!
//! The second cannot wait the ten and fifteen minutes the user's file asks
//! for, so a key bound to `hyprctl dispatch forceidle` says the seat has
//! been idle one second longer than its longest timeout: every listener
//! fires as it would have, and what each one did is printed as the record
//! of the user's file on Ferrix.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{build_image, press, say_the_marker, settle, undithered, with_the_transcript};
use super::picture::{differences, expected};
use super::{Carried, EITHER, EXPECTED, FAILED, MARKER, Programs, gates_busybox};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// Where the boots put hypridle's configuration.
const HYPRIDLE_CONF_PATH: &str = "etc/hypridle.conf";

/// The compositor's configuration for both boots: the tiled pair, hypridle,
/// and three keys.
///
/// `L` is the user's lock key's command, `F` pretends the seat has been
/// idle for `{forced}` seconds, and `N` is a key that does nothing but be
/// input. All three are binds, so the focused window, which draws something
/// else when it is sent a key, never sees them.
fn compositor_config(forced: u64) -> String {
    format!(
        "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
exec-once = /bin/hypridle -c /{HYPRIDLE_CONF_PATH}
bind = , L, exec, /bin/loginctl lock-session
bind = , F, exec, /bin/hyprctl dispatch forceidle {forced}
bind = , N, exec, /bin/busybox true
"
    )
}

/// The `idle` boot's hypridle.conf: the user's file's shape, with timeouts
/// a boot can wait for and a `lock_cmd` the image has.
///
/// The commands are bare names, as the user's are, so `PATH` is part of
/// what is tested. The dpms listener's timeout is long enough that the
/// screen is tiled again, after the lock, before it fires a second time.
const TEST_CONF: &str = "\
# hypridle's test configuration, carried by `cargo xtask test-compositor`.
general {
    lock_cmd = pidof lock || lock 3                  # one locker at a time
    on_lock_cmd = echo hypridle-test: on_lock_cmd ran
    on_unlock_cmd = echo hypridle-test: on_unlock_cmd ran
    before_sleep_cmd = loginctl lock-session         # said to never run
}

listener {
    timeout = 5
    on-timeout = echo hypridle-test: idle for 5 s
    on-resume = echo hypridle-test: back after 5 s
}

listener {
    timeout = 20
    on-timeout = hyprctl dispatch dpms off
    on-resume = hyprctl dispatch dpms on
}
";

/// How long the boots wait for anything: a first idle has to follow the
/// compositor's start by the listener's timeout, under TCG.
const PATIENCE: Duration = Duration::from_secs(90);

/// What hypridle prints once its notifications are made.
const READY: &str = "wayland done, listening for loginctl";

/// What the `idle` boot requires hypridle and its commands to have said,
/// after the key that ends the idle and locks.
const TEST_SAID: [&str; 9] = [
    "hypridle-test: idle for 5 s",
    "Running hyprctl dispatch dpms off",
    "hypridle-test: back after 5 s",
    "Running hyprctl dispatch dpms on",
    "Got lock-session from loginctl",
    "Locking with pidof lock || lock 3",
    "hypridle-test: on_lock_cmd ran",
    "hypridle-test: on_unlock_cmd ran",
    "[WARN] general:before_sleep_cmd: Ferrix has no suspend",
];

/// `idle`: hypridle's own configuration, every part of it carried out.
pub(super) fn test_idle(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(busybox) = gates_busybox(arch) else {
        println!(
            "  {arch}: no busybox at ~/.local/share/ferrix/busybox/{arch}/bin/busybox.static, so \
             hypridle has no /bin/sh to run its commands with; skipped"
        );
        return Ok(());
    };
    let said = boot(
        arch,
        programs,
        args,
        &busybox,
        TEST_CONF,
        &compositor_config(0),
        |run| {
            run.wait_for(&["hypridle-test: idle for 5 s"])?;
            run.black("the screen turned off by the dpms listener, with no input for 20 s")?;
            press(&mut run.qmp, &["l"])?;
            if let Err(error) = run.wait_for(&TEST_SAID) {
                // Which half failed: the key, or the compositor? A second
                // key, bound to a command hyprix says it started, tells.
                press(&mut run.qmp, &["n"])?;
                let reached = run.wait_for(&["started /bin/busybox"]).is_ok();
                println!(
                    "  {arch}: after the lock key went unanswered, a second key {}",
                    if reached {
                        "reached the compositor: the first was lost"
                    } else {
                        "did not reach the compositor either"
                    }
                );
                return Err(error);
            }
            run.wait_for(&["lock: locked 1 screen(s) and unlocked again"])?;
            run.tiled("the windows again, the key having ended the idle and the lock let go")
        },
    )?;
    require(arch, &said, &TEST_SAID)?;
    refuse_failures(arch, &said)?;
    println!(
        "  {arch}: hypridle ran each listener's on-timeout after its timeout and its on-resume at \
         the next key, `loginctl lock-session` reached its lock_cmd, and the compositor's lock \
         and unlock reached on_lock_cmd and on_unlock_cmd"
    );
    Ok(())
}

/// Where the user's file is on this machine.
fn users_file() -> Option<PathBuf> {
    let home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
    let path = home.join("hypr").join("hypridle.conf");
    path.is_file().then_some(path)
}

/// The longest `timeout = <seconds>` in a file, read the simple way: the
/// boot only needs a number of seconds past every listener's.
fn longest_timeout(text: &str) -> u64 {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next()?.trim();
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            (key == "timeout" || key == "listener:timeout")
                .then(|| value.trim().parse::<u64>().ok())
                .flatten()
        })
        .max()
        .unwrap_or(0)
}

/// `idle-user`: the user's own file, carried out as far as Ferrix can.
pub(super) fn test_idle_user(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(path) = users_file() else {
        println!("  {arch}: no ~/.config/hypr/hypridle.conf on this machine; skipped");
        return Ok(());
    };
    let Some(busybox) = gates_busybox(arch) else {
        println!("  {arch}: no busybox for hypridle's /bin/sh; skipped");
        return Ok(());
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    let forced = longest_timeout(&text).saturating_add(1);
    let dims = text.contains("dpms off");
    println!(
        "  {arch}: {} carried unchanged; F will say the seat has been idle {forced} s",
        path.display()
    );
    let said = boot(
        arch,
        programs,
        args,
        &busybox,
        &text,
        &compositor_config(forced),
        |run| {
            run.wait_for(&[READY])?;
            run.tiled("the windows, before anything is idle")?;
            press(&mut run.qmp, &["f"])?;
            if dims {
                run.black("the screen turned off by the user's dpms listener")?;
            } else {
                run.wait_for(&["Idled: rule"])?;
            }
            press(&mut run.qmp, &["n"])?;
            run.wait_for(&["Resumed: rule"])?;
            if dims {
                run.tiled("the windows again, at the first key")?;
            }
            // What the commands said after the resume, before the transcript
            // is taken.
            run.linger(Duration::from_secs(3))
        },
    )?;
    refuse_failures(arch, &said)?;
    // The record: what hypridle said about the file, and what each of its
    // commands did.
    println!("  {arch}: what hypridle and its commands said:");
    for line in said.iter().filter(|line| recorded(line)) {
        println!("    {}", line.trim_start_matches("| ").trim());
    }
    Ok(())
}

/// Whether a line of the transcript belongs in the record of the user's
/// file: hypridle's warnings and what it ran, and anything its commands
/// printed that says they could not.
fn recorded(line: &str) -> bool {
    [
        "[WARN]",
        "[ERR]",
        "Config error",
        "Idled: rule",
        "Resumed: rule",
        "Running ",
        "Locking with",
        "Got lock-session",
        "not found",
        "loginctl:",
    ]
    .iter()
    .any(|wanted| line.contains(wanted))
}

/// Every line in `wanted` must be in `said`.
fn require(arch: Arch, said: &[String], wanted: &[&str]) -> Result<()> {
    for want in wanted {
        if !said.iter().any(|line| line.contains(want)) {
            return Err(Error::new(format!(
                "{arch}: the hypridle boot did not say `{want}`"
            )));
        }
    }
    Ok(())
}

/// hypridle's own failures: a `[CRITICAL]` is the program ending, and an
/// `[ERR]` a command it could not start or a socket it could not take.
fn refuse_failures(arch: Arch, said: &[String]) -> Result<()> {
    if let Some(line) = said
        .iter()
        .find(|line| line.contains("[CRITICAL]") || line.contains("[ERR] Failed run"))
    {
        return Err(Error::new(format!(
            "{arch}: hypridle said `{}`",
            line.trim()
        )));
    }
    Ok(())
}

/// One boot in progress: the machine's monitor and its transcript.
struct Running<'a, 'w> {
    qmp: Qmp,
    watching: &'a mut Watching<'w>,
    dump: PathBuf,
    arch: Arch,
}

impl Running<'_, '_> {
    /// Read the guest until every line of `wanted` has been said.
    fn wait_for(&mut self, wanted: &[&str]) -> Result<()> {
        // What was said before the compositor's marker counts too: a
        // program started early can say its line before the screen is up.
        let before: Vec<String> = self.watching.lines().to_vec();
        let done = |lines: &[String]| {
            wanted
                .iter()
                .all(|want| before.iter().chain(lines).any(|line| line.contains(want)))
        };
        let said = self
            .watching
            .read_more(Instant::now() + PATIENCE, |lines| done(lines))?;
        let every: Vec<String> = self
            .watching
            .lines()
            .iter()
            .chain(self.watching.after())
            .cloned()
            .collect();
        if said || done(&every) {
            return Ok(());
        }
        let missing: Vec<&&str> = wanted
            .iter()
            .filter(|want| !every.iter().any(|line| line.contains(**want)))
            .collect();
        Err(with_the_transcript(
            &Error::new(format!("{}: the guest never said {missing:?}", self.arch)),
            self.watching,
        ))
    }

    /// Keep reading for `time`, for what is still on its way.
    fn linger(&mut self, time: Duration) -> Result<()> {
        let _ = self.watching.read_more(Instant::now() + time, |_| false)?;
        Ok(())
    }

    /// Take screendumps until every pixel is black.
    fn black(&mut self, what: &str) -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let screen = self.dump()?;
            if screen.pixels.iter().all(|byte| *byte == 0) {
                println!(
                    "  {}: {what}: every one of {} pixels black",
                    self.arch,
                    screen.width * screen.height
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                let lit = screen
                    .pixels
                    .chunks_exact(3)
                    .filter(|pixel| pixel.iter().any(|byte| *byte != 0))
                    .count();
                return Err(with_the_transcript(
                    &Error::new(format!(
                        "{}: {what}: {lit} of {} pixels are not black",
                        self.arch,
                        screen.width * screen.height
                    )),
                    self.watching,
                ));
            }
            // Read the guest meanwhile, so its lines are not left queued.
            let _ = self
                .watching
                .read_more(Instant::now() + Duration::from_millis(250), |_| false)?;
        }
    }

    /// Take screendumps until the screen is the tiled pair.
    fn tiled(&mut self, what: &str) -> Result<()> {
        let (_, path) = EXPECTED[0];
        let want = expected(path)?;
        let screen = settle(&mut self.qmp, &self.dump, &want)?;
        let (_, wrong) = differences(&screen, &want);
        if wrong != 0 {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{}: {what}: {wrong} pixels differ from {path}",
                    self.arch
                )),
                self.watching,
            ));
        }
        println!(
            "  {}: {what}, every one of {} pixels as the renderer draws them",
            self.arch,
            screen.width * screen.height
        );
        Ok(())
    }

    fn dump(&mut self) -> Result<Image> {
        self.qmp.screendump(Some(DEVICE_ID), &self.dump)?;
        let bytes = std::fs::read(&self.dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", self.dump.display())))?;
        parse_ppm(&bytes)
    }
}

/// Boot the compositor with hypridle and `conf`, run `steps` once the
/// screen is up, and give the whole transcript.
fn boot(
    arch: Arch,
    programs: &Programs,
    args: &Args,
    busybox: &str,
    conf: &str,
    config: &str,
    mut steps: impl FnMut(&mut Running<'_, '_>) -> Result<()>,
) -> Result<Vec<String>> {
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports: vec![crate::ports::File {
            path: HYPRIDLE_CONF_PATH.to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(conf.as_bytes().to_vec()),
        }],
        ..Carried::none()
    };
    let (image, kernel) = build_image(arch, programs, &undithered(config), carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("compositor.ppm");
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        if let Some(line) = watching
            .lines()
            .iter()
            .rev()
            .find(|line| line.contains(FAILED))
        {
            return Err(Error::new(format!("{arch}: {}", line.trim())));
        }
        let up = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(MARKER))
        })?;
        if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
            return Err(Error::new(format!(
                "{arch}: the compositor never printed `{MARKER}`"
            )));
        }
        say_the_marker(watching, arch);
        let mut running = Running {
            qmp,
            watching,
            dump: dump.clone(),
            arch,
        };
        steps(&mut running)?;
        running
            .watching
            .read_what_was_said(Duration::from_secs(2))?;
        said = running
            .watching
            .lines()
            .iter()
            .chain(running.watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    if let Some(line) = said.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Err(Error::new(format!(
            "{arch}: the kernel stopped while hypridle ran: {}",
            line.trim()
        )));
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::longest_timeout;

    #[test]
    fn the_longest_timeout_is_read_from_either_form() {
        let text = "listener {\n    timeout = 600  # ten minutes\n}\nlistener:timeout = 900\n\
                    general:lock_cmd = x\n";
        assert_eq!(longest_timeout(text), 900);
        assert_eq!(longest_timeout("general { }\n"), 0);
    }
}
