//! The boot of the Settings app and its game-mode page (`docs/SETTINGS.md`).
//!
//! The desktop comes up with Settings open. The image carries the app as its
//! package installs it -- the program, its launcher entry, its icon -- with
//! fuzzel, the system's own entries, and one more of the boot's own: **Game
//! Probe**, a zinc script that says how it was started and what the user's
//! game-mode file holds. Keys reach the guest as a person's would: QEMU
//! presses a function key, the key is bound to `/bin/vkbd`, and vkbd types
//! into whatever has the keyboard.
//!
//! What is required, in order:
//!
//! 1. Settings lists the image's applications, none in game mode, and its
//!    window is up.
//! 2. `probe` typed into it and Return pressed turns Game Probe's switch
//!    on: Settings says it wrote the file, and a second process
//!    (`settings --list`) reads the switch back as on from the file.
//! 3. Game Probe started from the launcher has the mark: it prints
//!    `FERRIX_GAME_MODE=1` and its own ID, and reads its ID out of the file
//!    with the shell's own `read`.
//! 4. Return in Settings again turns the switch off, and Game Probe started
//!    from the launcher again has no mark and finds the file without its
//!    ID.
//!
//! Step 4 is half of the negative control -- the same probe, the same
//! launcher, and no mark once the switch is off. The other half is the
//! launcher itself: with the one line that gives the mark taken out of the
//! fuzzel app (`launch.game.apply`), step 3 fails here with "was not started
//! in game mode" (`docs/SETTINGS.md` §6 has the run).
//!
//! The screen after step 2 is kept in the build directory as
//! `settings.ppm`: Settings is drawn with fonts, not compared with a
//! picture, since what is asked of it is what it wrote.

use std::path::Path;
use std::time::{Duration, Instant};

use super::boot::{judge_still_running, judged_image, press, say_the_marker, with_the_transcript};
use super::{Carried, EITHER, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The boot's `hyprland.conf`: Settings as the desktop comes up, and the
/// keys that type into it, open the launcher and ask what the file holds.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot settings`.
exec-once = /bin/settings
bind = , F1, exec, /bin/vkbd p r o b e
bind = , F2, exec, /bin/vkbd Return
bind = , F3, exec, /bin/fuzzel --log-level=info
bind = , F4, exec, /bin/settings --list
";

/// The probe's launcher entry. Its ID is `probe.desktop`.
const PROBE_ENTRY: &str = "\
[Desktop Entry]
Type=Application
Name=Game Probe
GenericName=Game Mode Probe
Comment=Says how it was started
Exec=/bin/zinc /bin/game-probe
Icon=ferrix-pattern
Terminal=false
";

/// The probe: what it was started with, then each line of the user's file
/// that is an ID, then that it is done. `HOME` is `/` on this desktop.
const PROBE: &str = "\
echo \"game-probe: FERRIX_GAME_MODE=${FERRIX_GAME_MODE} FERRIX_GAME_MODE_APP=${FERRIX_GAME_MODE_APP} .\"
while read -r line; do
  case \"$line\" in
    \\#*|\"\") ;;
    *) echo \"game-probe: the file has $line\" ;;
  esac
done < /.config/ferrix/game-mode.conf
echo \"game-probe: done\"
";

/// The user's file, as Settings names it when it writes.
const FILE: &str = "/.config/ferrix/game-mode.conf";

/// The fonts Settings draws with: the brand's Inter, as an image a person
/// runs carries it.
const FONTS: [&str; 1] = ["inter/InterVariable.ttf"];

/// How long one step of the boot may take under emulation on a busy host.
const PATIENCE: Duration = Duration::from_secs(150);

/// Everything the boot carries: Settings as its package installs it, fuzzel
/// and the system's entries, the probe, and zinc to run it.
fn carried(arch: Arch) -> Result<Carried> {
    let fuzzel = crate::apps::program(arch, "fuzzel", "fuzzel")?;
    let mut ports = crate::fuzzel::boot_files(&fuzzel)?;
    ports.extend(crate::apps::taken(arch, &["settings"])?);
    ports.push(File {
        path: "usr/share/applications/probe.desktop".to_owned(),
        mode: 0o644,
        content: Content::Bytes(PROBE_ENTRY.as_bytes().to_vec()),
    });
    ports.push(File {
        path: "bin/game-probe".to_owned(),
        mode: 0o755,
        content: Content::Bytes(PROBE.as_bytes().to_vec()),
    });
    let root = paths::workspace_root();
    for font in FONTS {
        let path = root.join("assets/fonts").join(font);
        let bytes = std::fs::read(&path)
            .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
        ports.push(File {
            path: format!("usr/share/ferrix/fonts/{font}"),
            mode: 0o644,
            content: Content::Bytes(bytes),
        });
    }
    let zinc = crate::zinc::build(arch)?;
    if zinc.is_none() {
        return Err(Error::new(format!(
            "{arch}: zinc is not built for this architecture, and the probe is a zinc script"
        )));
    }
    Ok(Carried {
        zinc,
        ports,
        ..Carried::none()
    })
}

/// Build what the boot carries beyond the compositor's own programs, ahead
/// of the first boot (`build_ahead`).
pub(super) fn build_ahead(arch: Arch) -> Result<()> {
    let _ = crate::apps::program(arch, "fuzzel", "fuzzel")?;
    let _ = crate::apps::program(arch, "settings", "settings")?;
    let _ = crate::zinc::build(arch)?;
    Ok(())
}

/// The Settings boot.
pub(super) fn test_settings(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (image, kernel) = judged_image(arch, programs, CONFIG, carried(arch)?, None, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("settings.ppm");
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        said = drive(arch, port, &dump, watching)?;
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_still_running(arch, &said)?;
    judge(arch, &said)?;
    println!(
        "  {arch}: Settings listed the image's applications, a switch turned in it was written \
         to {FILE}, the launcher started the application with FERRIX_GAME_MODE=1 while the \
         switch was on and without it once it was off"
    );
    Ok(())
}

/// How many of `lines` have `wanted` in them.
fn count(lines: &[String], wanted: &str) -> usize {
    lines.iter().filter(|line| line.contains(wanted)).count()
}

/// Wait until `wanted` has been said `times` times, or fail saying `what`
/// never happened, with the guest's last lines.
fn wait(
    watching: &mut Watching<'_>,
    arch: Arch,
    (wanted, times): (&str, usize),
    what: &str,
) -> Result<()> {
    let came = watching.read_more(Instant::now() + PATIENCE, |lines| {
        count(lines, wanted) >= times
    })?;
    if came {
        return Ok(());
    }
    Err(with_the_transcript(
        &Error::new(format!(
            "{arch}: {what}: the guest never said `{wanted}`{}",
            if times > 1 {
                format!(" a {times}. time")
            } else {
                String::new()
            }
        )),
        watching,
    ))
}

/// Start Game Probe from the launcher, for the `nth` time: open fuzzel,
/// type its name, press Return, and wait until the probe is done.
fn launch_probe(
    qmp: &mut Qmp,
    watching: &mut Watching<'_>,
    arch: Arch,
    nth: usize,
    typed_before: usize,
) -> Result<()> {
    press(qmp, &["f3"])?;
    wait(
        watching,
        arch,
        ("namespace launcher", nth),
        "F3 did not open the launcher",
    )?;
    press(qmp, &["f1"])?;
    wait(
        watching,
        arch,
        ("vkbd: typed p r o b e", typed_before + 1),
        "F1 did not type into the launcher",
    )?;
    // The launcher has the keys once it has drawn them; a moment for that.
    std::thread::sleep(Duration::from_secs(2));
    press(qmp, &["f2"])?;
    wait(
        watching,
        arch,
        ("executing probe.desktop", nth),
        "Return in the launcher did not start Game Probe",
    )?;
    wait(
        watching,
        arch,
        ("game-probe: done", nth),
        "Game Probe did not run to its end",
    )?;
    println!("  {arch}: the launcher started Game Probe ({nth})");
    Ok(())
}

/// Drive the boot, and answer everything the guest said.
fn drive(arch: Arch, port: u16, dump: &Path, watching: &mut Watching<'_>) -> Result<Vec<String>> {
    let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
    let up = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines.iter().any(|line| line.contains(MARKER))
    })?;
    if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
        return Err(Error::new(format!(
            "{arch}: the compositor never printed `{MARKER}`"
        )));
    }
    say_the_marker(watching, arch);
    wait(
        watching,
        arch,
        ("settings: the window is up", 1),
        "Settings did not open its window",
    )?;
    println!("  {arch}: Settings is up");

    // On: the name typed into Settings' filter, and Return.
    press(&mut qmp, &["f1"])?;
    wait(
        watching,
        arch,
        ("vkbd: typed p r o b e", 1),
        "F1 did not type into Settings",
    )?;
    std::thread::sleep(Duration::from_secs(2));
    press(&mut qmp, &["f2"])?;
    wait(
        watching,
        arch,
        ("settings: game mode on for probe.desktop", 1),
        "Return in Settings did not turn Game Probe's switch on",
    )?;
    std::thread::sleep(Duration::from_secs(2));
    qmp.screendump(Some(DEVICE_ID), dump)?;
    println!(
        "  {arch}: Game Probe's switch is on; the screen is {}",
        dump.display()
    );
    press(&mut qmp, &["f4"])?;
    wait(
        watching,
        arch,
        ("on probe.desktop Game Probe", 1),
        "a second process did not read the switch back as on",
    )?;
    launch_probe(&mut qmp, watching, arch, 1, 1)?;

    // Off: Settings has the keyboard again, its filter still `probe`.
    std::thread::sleep(Duration::from_secs(2));
    press(&mut qmp, &["f2"])?;
    wait(
        watching,
        arch,
        ("settings: game mode off for probe.desktop", 1),
        "Return in Settings did not turn Game Probe's switch off",
    )?;
    println!("  {arch}: Game Probe's switch is off");
    launch_probe(&mut qmp, watching, arch, 2, 2)?;
    watching.read_what_was_said(Duration::from_secs(2))?;
    Ok(watching
        .lines()
        .iter()
        .chain(watching.after())
        .cloned()
        .collect())
}

/// What the boot requires of its transcript.
fn judge(arch: Arch, said: &[String]) -> Result<()> {
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    let refuse = |why: String| Err(Error::new(format!("{arch}: {why}")));
    // Settings listed something, with nothing in game mode on a new system.
    let Some(listed) = said
        .iter()
        .find(|line| line.contains("settings: ") && line.contains(" applications listed"))
    else {
        return refuse("Settings never said what it listed".to_owned());
    };
    if !listed.contains(", 0 in game mode") || listed.contains("settings: 0 applications") {
        return refuse(format!(
            "Settings did not list the image's applications with none in game mode: {}",
            listed.trim()
        ));
    }
    for wanted in [
        // Its own entry and the system's are in the list a launcher has.
        "off settings.desktop Settings",
        "off terminal.desktop Terminal",
        "on probe.desktop Game Probe",
        &format!("settings: game mode on for probe.desktop; wrote {FILE}"),
        &format!("settings: game mode off for probe.desktop; wrote {FILE}"),
    ] {
        if !has(wanted) {
            return refuse(format!("the Settings boot never said `{wanted}`"));
        }
    }
    // The probe ran twice, in this order: marked, then not.
    let started: Vec<&str> = said
        .iter()
        .filter(|line| line.contains("game-probe: FERRIX_GAME_MODE="))
        .map(|line| super::boot::said_on_its_own(line))
        .collect();
    let (Some(first), Some(second), 2) = (started.first(), started.get(1), started.len()) else {
        return refuse(format!(
            "Game Probe was to be started twice and said how {} times",
            started.len()
        ));
    };
    if !first.contains("FERRIX_GAME_MODE=1 FERRIX_GAME_MODE_APP=probe.desktop .") {
        return refuse(format!(
            "with its switch on, Game Probe was not started in game mode by the launcher: `{first}`"
        ));
    }
    if !second.contains("FERRIX_GAME_MODE= FERRIX_GAME_MODE_APP= .") {
        return refuse(format!(
            "with its switch off again, Game Probe was still started with a mark: `{second}`"
        ));
    }
    // The launcher said so itself, once: for the start that was marked.
    let marked = count(
        said,
        "game mode: probe.desktop is started with FERRIX_GAME_MODE=1",
    );
    if marked != 1 {
        return refuse(format!(
            "the launcher said it started Game Probe in game mode {marked} times, not once"
        ));
    }
    // The file, read by the shell: the ID while the switch was on, and not
    // after.
    let in_file = count(said, "game-probe: the file has probe.desktop");
    if in_file != 1 {
        return refuse(format!(
            "{FILE} held probe.desktop for {in_file} of the probe's two runs, not for the first \
             alone"
        ));
    }
    if let Some(line) = said
        .iter()
        .find(|line| line.contains("settings: err: ") || line.contains(" err: "))
    {
        return refuse(format!("a program complained: {}", line.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CONFIG, PROBE, PROBE_ENTRY, judge};
    use crate::paths::Arch;

    /// A transcript of a boot that passed, as the guest prints one.
    fn passed() -> Vec<String> {
        [
            "settings: 5 applications listed with 5 icons, 0 in game mode (/.config/ferrix/game-mode.conf)",
            "settings: the window is up, 1264x704 at scale 1, 6 rows in view",
            "vkbd: typed p r o b e",
            "settings: game mode on for probe.desktop; wrote /.config/ferrix/game-mode.conf",
            "on probe.desktop Game Probe",
            "off settings.desktop Settings",
            "off terminal.desktop Terminal",
            " info: fuzzel: 5 entries, window 400x300 on layer Overlay, namespace launcher",
            " info: executing probe.desktop: \"/bin/zinc /bin/game-probe\"",
            " info: game mode: probe.desktop is started with FERRIX_GAME_MODE=1",
            "game-probe: FERRIX_GAME_MODE=1 FERRIX_GAME_MODE_APP=probe.desktop .",
            "game-probe: the file has probe.desktop",
            "game-probe: done",
            "settings: game mode off for probe.desktop; wrote /.config/ferrix/game-mode.conf",
            " info: executing probe.desktop: \"/bin/zinc /bin/game-probe\"",
            "game-probe: FERRIX_GAME_MODE= FERRIX_GAME_MODE_APP= .",
            "game-probe: done",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    /// `said` with each line that has `wanted` changed by `change`.
    fn with(wanted: &str, change: impl Fn(&str) -> Option<String>) -> Vec<String> {
        passed()
            .into_iter()
            .filter_map(|line| {
                if line.contains(wanted) {
                    change(&line)
                } else {
                    Some(line)
                }
            })
            .collect()
    }

    fn refusal(said: &[String]) -> String {
        judge(Arch::X86_64, said)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn a_boot_that_did_everything_passes() {
        assert_eq!(refusal(&passed()), "");
    }

    #[test]
    fn a_launcher_that_gives_no_mark_fails() {
        // The negative control's transcript: the switch is on, the file has
        // the ID, and the program was started without the mark.
        let unmarked = with(
            "FERRIX_GAME_MODE=1 FERRIX_GAME_MODE_APP=probe.desktop",
            |_| Some("game-probe: FERRIX_GAME_MODE= FERRIX_GAME_MODE_APP= .".to_owned()),
        );
        assert!(
            refusal(&unmarked).contains("was not started in game mode by the launcher"),
            "{}",
            refusal(&unmarked)
        );
    }

    #[test]
    fn a_mark_that_outlives_its_switch_fails() {
        let kept = with("game-probe: FERRIX_GAME_MODE= ", |_| {
            Some("game-probe: FERRIX_GAME_MODE=1 FERRIX_GAME_MODE_APP=probe.desktop .".to_owned())
        });
        assert!(
            refusal(&kept).contains("still started with a mark"),
            "{}",
            refusal(&kept)
        );
    }

    #[test]
    fn a_file_that_did_not_change_fails() {
        let never_written = with("game-probe: the file has probe.desktop", |_| None);
        assert!(
            refusal(&never_written).contains("held probe.desktop for 0 of the probe's two runs"),
            "{}",
            refusal(&never_written)
        );
        let never_cleared: Vec<String> = passed()
            .into_iter()
            .chain(["game-probe: the file has probe.desktop".to_owned()])
            .collect();
        assert!(
            refusal(&never_cleared).contains("for 2 of the probe's two runs"),
            "{}",
            refusal(&never_cleared)
        );
        let not_read_back = with("on probe.desktop Game Probe", |_| None);
        assert!(
            refusal(&not_read_back).contains("never said `on probe.desktop Game Probe`"),
            "{}",
            refusal(&not_read_back)
        );
    }

    #[test]
    fn a_complaint_or_an_empty_list_fails() {
        let complained: Vec<String> = passed()
            .into_iter()
            .chain(["settings: err: no font was found".to_owned()])
            .collect();
        assert!(refusal(&complained).contains("a program complained"));
        let empty = with(" applications listed", |_| {
            Some(
                "settings: 0 applications listed with 0 icons, 0 in game mode (no file)".to_owned(),
            )
        });
        assert!(refusal(&empty).contains("did not list the image's applications"));
    }

    #[test]
    fn the_boots_own_files_are_what_they_say() {
        assert!(PROBE_ENTRY.starts_with("[Desktop Entry]\n"));
        assert!(PROBE_ENTRY.contains("Exec=/bin/zinc /bin/game-probe\n"));
        assert!(PROBE.contains("FERRIX_GAME_MODE=${FERRIX_GAME_MODE} "));
        assert!(PROBE.contains("/.config/ferrix/game-mode.conf"));
        for key in ["F1", "F2", "F3", "F4"] {
            assert!(CONFIG.contains(&format!("bind = , {key}, exec, ")), "{key}");
        }
    }
}
