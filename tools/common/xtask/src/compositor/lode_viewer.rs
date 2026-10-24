//! The boot of the log viewer app (`docs/LOGGING.md`, the viewer).
//!
//! The desktop comes up with the viewer open on its Logs page over a JSONL
//! export the image carries: the app's own test records, 801 of the N150's
//! first boot under lode. Keys reach the guest as a person's would: QEMU
//! presses a function key, the key is bound to `/bin/vkbd`, and vkbd types
//! into the window that has the keyboard.
//!
//! What is required, in order:
//!
//! 1. The viewer read the file's 801 records and its window is up, drawn
//!    with the design's fonts -- on the CPU, as every QEMU guest draws.
//! 2. `/`, `id=msr` and Return typed into it run lode's filter language
//!    over the records: the viewer says it found the 13 lines `msr` wrote,
//!    of 801. The number is the file's, counted on the host when the file
//!    was taken; a viewer that did not run the query, or ran it on other
//!    records, says another.
//!
//! 3. A query pasted over it: `clip copy id=usbhid` puts text on the
//!    compositor's clipboard, `/`, Ctrl+A and Ctrl+V in the viewer select
//!    the query and paste over it through `wl_data_device` (Ctrl never
//!    types its letter), and Return runs it: the viewer says it pasted 9
//!    characters and found usbhid's 14 lines.
//!
//! The screen after step 3 is kept in the build directory as
//! `lode-viewer.ppm`.
//!
//! `lode-viewer-ssh` is the other side: the viewer on this machine following
//! a Ferrix guest's lode over ssh, as its Machines page does for a box. The
//! guest runs `sshdt`, its port 22 forwarded to a free port here, and the
//! viewer's `--pull` (the link the Machines page starts, with no window)
//! logs in with the test boots' client key and keeps what comes in a
//! store of its own. Required: records kept, among them lode's own "lode
//! started for this boot"; then a second pull resumes from the store's last
//! record, so the store holds the two pulls' records once each.

use std::time::{Duration, Instant};

use super::boot::{judge_still_running, judged_image, press, say_the_marker, with_the_transcript};
use super::{Carried, EITHER, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// Where the image carries the records.
const RECORDS: &str = "/usr/share/ferrix/lode-viewer-test.jsonl";

/// The app's own test records, in its folder.
const FIXTURE: &str = "testdata/n150-boot-excerpt.jsonl";

/// The boot's `hyprland.conf`: the viewer as the desktop comes up, and the
/// key that types a query into it.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot lode-viewer`.
exec-once = /bin/lode-viewer /usr/share/ferrix/lode-viewer-test.jsonl --page logs --zoom 0
bind = , F1, exec, /bin/vkbd slash i d equal m s r Return
bind = , F2, exec, /bin/clip copy id=usbhid
bind = , F3, exec, /bin/vkbd slash
bind = , F4, exec, /bin/vkbd CTRL A
bind = , F5, exec, /bin/vkbd CTRL V
bind = , F6, exec, /bin/vkbd Return
";

/// The records the file holds, and how many of them `msr` wrote.
const RECORDS_IN_FILE: usize = 801;
/// The lines `id=msr` finds.
const MSR_LINES: usize = 13;
/// The query pasted, and the lines it finds.
const PASTED: &str = "id=usbhid";
/// The lines `id=usbhid` finds.
const USBHID_LINES: usize = 14;

/// How long one step may take under emulation on a busy host.
const PATIENCE: Duration = Duration::from_secs(150);

/// Everything the boot carries: the viewer as its package installs it, and
/// its test records.
fn carried(arch: Arch) -> Result<Carried> {
    let mut ports = crate::apps::taken(arch, &["lode-viewer"])?;
    let path = crate::apps::folder("lode-viewer")?.join(FIXTURE);
    let bytes =
        std::fs::read(&path).map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    ports.push(File {
        path: RECORDS.trim_start_matches('/').to_owned(),
        mode: 0o644,
        content: Content::Bytes(bytes),
    });
    Ok(Carried {
        ports,
        ..Carried::none()
    })
}

/// Build what the boot carries ahead of the first boot (`build_ahead`).
pub(super) fn build_ahead(arch: Arch) -> Result<()> {
    let _ = crate::apps::program(arch, "lode-viewer", "lode-viewer")?;
    let _ = crate::apps::program(Arch::X86_64, "lode-viewer", "lode-viewer")?;
    Ok(())
}

/// The ssh boot's `hyprland.conf`: an address from the gateway, as
/// run-compositor's network takes one, and sshdt on every address, port 22.
fn ssh_config(exec: &str) -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-compositor --boot lode-viewer-ssh`.\n\
         exec-once = /bin/dhcpc -i eth0 -n -t 5 -T 2\n\
         exec-once = {exec} -b 0.0.0.0 -p 22\n"
    )
}

/// The viewer on this machine following the guest over ssh.
pub(super) fn test_lode_viewer_ssh(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut files, exec) = crate::ssh::test_server(arch)?;
    files.extend(crate::dhcp::carried(arch)?);
    // sshdt runs a command with /bin/sh: busybox's, as a person's image has.
    let busybox = crate::busybox::installed_program(arch).ok_or_else(|| {
        Error::new(format!(
            "{arch}: no busybox for sshdt's /bin/sh: `cargo xtask busybox` builds one"
        ))
    })?;
    let carried = Carried {
        ports: files,
        busybox: Some(busybox),
        ..Carried::none()
    };
    let (image, kernel) = judged_image(arch, programs, &ssh_config(&exec), carried, None, args)?;
    // The viewer runs here, so it is this machine's build: a static x86-64
    // program, whatever the guest's architecture.
    let viewer = crate::apps::program(Arch::X86_64, "lode-viewer", "lode-viewer")?;
    let key = crate::ssh::test_client_key()?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.net = true;
    qemu_args.forwards.push(crate::gateway::Forward {
        host: port,
        guest: crate::ssh::GUEST_PORT,
    });
    let work = paths::build_dir(arch).join("lode-viewer-ssh");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)
        .map_err(|error| Error::new(format!("{}: {error}", work.display())))?;
    let mut pulls = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let up = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|line| line.contains("sshdt is ready"))
                && lines.iter().any(|line| line.contains("10.0.2.15"))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: sshdt never said it was ready, or the guest took no address"
                )),
                watching,
            ));
        }
        say_the_marker(watching, arch);
        for round in 1..=2 {
            // The guest's console is read while the pull runs: a console
            // nobody reads stops the guest that writes to it.
            let (viewer, key, work) = (viewer.clone(), key.clone(), work.clone());
            let running = std::thread::spawn(move || pull(&viewer, &key, port, &work));
            while !running.is_finished() {
                let _ =
                    watching.read_more(Instant::now() + Duration::from_millis(500), |_| false)?;
            }
            let said = running
                .join()
                .map_err(|_| Error::new("the pull's thread panicked"))??;
            println!(
                "  {arch}: pull {round}: {}",
                said.lines().last().unwrap_or("nothing")
            );
            pulls.push(said);
        }
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    let counted = count_store(&viewer, &work)?;
    judge_ssh(arch, &pulls, counted)?;
    println!(
        "  {arch}: the viewer followed the guest's lode over ssh into a store of its own, \
         and a second pull went on from where the first stopped"
    );
    Ok(())
}

/// One `--pull` of the guest, its output.
fn pull(
    viewer: &std::path::Path,
    key: &std::path::Path,
    port: u16,
    work: &std::path::Path,
) -> Result<String> {
    let output = std::process::Command::new(viewer)
        .args(["--pull", &format!("root@127.0.0.1:{port}"), "--key"])
        .arg(key)
        .arg("--into")
        .arg(work.join("root"))
        .args(["--seconds", "15"])
        .env("LODE_VIEWER_KNOWN_HOSTS", work.join("known_hosts"))
        .output()
        .map_err(|error| Error::new(format!("{}: {error}", viewer.display())))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// How many records the store the pulls made holds, and whether lode's
/// start is among them: the viewer's own reading of it.
fn count_store(viewer: &std::path::Path, work: &std::path::Path) -> Result<(usize, bool)> {
    let root = work.join("root");
    let dir = std::fs::read_dir(&root)
        .map_err(|error| Error::new(format!("{}: {error}", root.display())))?
        .flatten()
        .map(|item| item.path())
        .find(|path| {
            path.is_dir()
                && !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        })
        .ok_or_else(|| Error::new(format!("{}: the pulls kept no store", root.display())))?;
    let output = std::process::Command::new(viewer)
        .arg("-D")
        .arg(&dir)
        .args(["--no-remotes", "--render"])
        .arg(work.join("frame.png"))
        .output()
        .map_err(|error| Error::new(format!("{}: {error}", viewer.display())))?;
    let all = String::from_utf8_lossy(&output.stdout).into_owned();
    let records = all
        .lines()
        .find_map(|line| {
            line.strip_prefix("lode-viewer: read ")?
                .split(' ')
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0);
    let started = std::fs::read_dir(&dir)
        .map_err(|error| Error::new(format!("{}: {error}", dir.display())))?
        .flatten()
        .filter(|item| item.file_name().to_string_lossy().ends_with(".fxj"))
        .any(|item| {
            std::fs::read(item.path()).is_ok_and(|bytes| {
                bytes
                    .windows(26)
                    .any(|window| window == b"lode started for this boot")
            })
        });
    Ok((records, started))
}

/// The number at `kept` in a pull's last line: `pull …: N records kept in …`.
fn kept(said: &str) -> Option<usize> {
    let line = said
        .lines()
        .rev()
        .find(|line| line.contains(" records kept in "))?;
    let before = line.split(" records kept in ").next()?;
    before.rsplit(' ').next()?.parse().ok()
}

/// What the ssh boot requires.
fn judge_ssh(arch: Arch, pulls: &[String], (stored, started): (usize, bool)) -> Result<()> {
    let refuse = |why: String| Err(Error::new(format!("{arch}: {why}")));
    let (Some(first), Some(second)) = (
        pulls.first().and_then(|said| kept(said)),
        pulls.get(1).and_then(|said| kept(said)),
    ) else {
        return refuse(format!("a pull did not say what it kept: {pulls:?}"));
    };
    if first == 0 {
        return refuse(format!(
            "the first pull kept no record: {}",
            pulls.first().map_or("", String::as_str)
        ));
    }
    if !started {
        return refuse(
            "the store has no \"lode started for this boot\": the pull did not start at the boot"
                .to_owned(),
        );
    }
    if stored != first + second {
        return refuse(format!(
            "the store holds {stored} records, and the pulls kept {first} and {second}: the second did not go on from where the first stopped"
        ));
    }
    Ok(())
}

/// The viewer's boot.
pub(super) fn test_lode_viewer(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (image, kernel) = judged_image(arch, programs, CONFIG, carried(arch)?, None, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("lode-viewer.ppm");
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
        "  {arch}: the log viewer read {RECORDS_IN_FILE} records, drew its window, and a query \
         typed into it found the {MSR_LINES} lines msr wrote, and one pasted over it found usbhid's {USBHID_LINES}"
    );
    Ok(())
}

/// Wait until a line has `wanted` in it, or fail saying `what` never
/// happened, with the guest's last lines.
fn wait(watching: &mut Watching<'_>, arch: Arch, wanted: &str, what: &str) -> Result<()> {
    let came = watching.read_more(Instant::now() + PATIENCE, |lines| {
        lines.iter().any(|line| line.contains(wanted))
    })?;
    if came {
        return Ok(());
    }
    Err(with_the_transcript(
        &Error::new(format!("{arch}: {what}: the guest never said `{wanted}`")),
        watching,
    ))
}

/// Drive the boot, and answer everything the guest said.
fn drive(
    arch: Arch,
    port: u16,
    dump: &std::path::Path,
    watching: &mut Watching<'_>,
) -> Result<Vec<String>> {
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
        "lode-viewer: the window is up",
        "the log viewer did not open its window",
    )?;
    println!("  {arch}: the log viewer is up");
    // A moment for the window to have the keyboard.
    std::thread::sleep(Duration::from_secs(2));
    press(&mut qmp, &["f1"])?;
    wait(
        watching,
        arch,
        "lode-viewer: where `id=msr`",
        "the query typed into the log viewer was not run",
    )?;
    println!("  {arch}: the typed query ran");
    // The clipboard: clip offers the text, the viewer pastes it over the
    // selected query.
    press(&mut qmp, &["f2"])?;
    std::thread::sleep(Duration::from_secs(2));
    for key in ["f3", "f4", "f5"] {
        press(&mut qmp, &[key])?;
        std::thread::sleep(Duration::from_millis(800));
    }
    wait(
        watching,
        arch,
        &format!("lode-viewer: pasted {} characters", PASTED.len()),
        "Ctrl+V did not paste the clipboard into the query",
    )?;
    press(&mut qmp, &["f6"])?;
    wait(
        watching,
        arch,
        &format!("lode-viewer: where `{PASTED}`"),
        "the pasted query was not run",
    )?;
    std::thread::sleep(Duration::from_secs(2));
    qmp.screendump(Some(DEVICE_ID), dump)?;
    println!(
        "  {arch}: the pasted query ran; the screen is {}",
        dump.display()
    );
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
    let read = format!("lode-viewer: read {RECORDS_IN_FILE} records from {RECORDS}");
    if !has(&read) {
        return refuse(format!("the log viewer never said `{read}`"));
    }
    if !has("lode-viewer: the window is up") {
        return refuse("the log viewer never drew its window".to_owned());
    }
    let found = format!("lode-viewer: where `id=msr`: {MSR_LINES} of {RECORDS_IN_FILE} records");
    if !has(&found) {
        let said_instead = said
            .iter()
            .find(|line| line.contains("lode-viewer: where "))
            .map_or("nothing", |line| line.trim());
        return refuse(format!(
            "the query typed into the log viewer did not find msr's {MSR_LINES} lines: it said {said_instead}"
        ));
    }
    let pasted = format!("lode-viewer: pasted {} characters", PASTED.len());
    if !has(&pasted) {
        return refuse(format!("Ctrl+V never pasted the clipboard: no `{pasted}`"));
    }
    let found =
        format!("lode-viewer: where `{PASTED}`: {USBHID_LINES} of {RECORDS_IN_FILE} records");
    if !has(&found) {
        let said_instead = said
            .iter()
            .rev()
            .find(|line| line.contains("lode-viewer: where "))
            .map_or("nothing", |line| line.trim());
        return refuse(format!(
            "the pasted query did not replace the typed one and find usbhid's {USBHID_LINES} lines: it said {said_instead}"
        ));
    }
    if let Some(line) = said.iter().find(|line| line.contains("lode-viewer: err: ")) {
        return refuse(format!("the log viewer complained: {}", line.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CONFIG, judge};
    use crate::paths::Arch;

    fn passed() -> Vec<String> {
        [
            "lode-viewer: read 801 records from /usr/share/ferrix/lode-viewer-test.jsonl (0 other lines)",
            "lode-viewer: 801 records in 1 boots, 19 sources",
            "lode-viewer: the window is up, 1264x704 at scale 1, 754 lines in the table, first frame in 80 ms",
            "vkbd: typed slash i d equal m s r Return",
            "lode-viewer: where `id=msr`: 13 of 801 records",
            "lode-viewer: pasted 9 characters",
            "lode-viewer: where `id=usbhid`: 14 of 801 records",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn refusal(said: &[String]) -> String {
        judge(Arch::X86_64, said)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn a_boot_that_drew_and_queried_passes() {
        assert_eq!(refusal(&passed()), "");
    }

    #[test]
    fn a_query_that_found_other_lines_or_none_fails() {
        let wrong: Vec<String> = passed()
            .into_iter()
            .map(|line| line.replace("13 of 801", "801 of 801"))
            .collect();
        assert!(
            refusal(&wrong).contains("did not find msr's 13 lines"),
            "{}",
            refusal(&wrong)
        );
        let none: Vec<String> = passed()
            .into_iter()
            .filter(|line| !line.contains("where "))
            .collect();
        assert!(
            refusal(&none).contains("it said nothing"),
            "{}",
            refusal(&none)
        );
        let unread: Vec<String> = passed()
            .into_iter()
            .filter(|line| !line.contains("read 801"))
            .collect();
        assert!(refusal(&unread).contains("never said"));
        let complained: Vec<String> = passed()
            .into_iter()
            .chain(["lode-viewer: err: no font was found".to_owned()])
            .collect();
        assert!(refusal(&complained).contains("complained"));
        let unpasted: Vec<String> = passed()
            .into_iter()
            .filter(|line| !line.contains("pasted"))
            .collect();
        assert!(refusal(&unpasted).contains("never pasted"));
        let typed_v: Vec<String> = passed()
            .into_iter()
            .map(|line| line.replace("`id=usbhid`: 14", "`id=msrvid=usbhid`: 0"))
            .collect();
        assert!(
            refusal(&typed_v).contains("did not replace"),
            "{}",
            refusal(&typed_v)
        );
    }

    #[test]
    fn a_second_pull_must_go_on_where_the_first_stopped() {
        use super::judge_ssh;
        let pulls = vec![
            "lode-viewer: pull root@127.0.0.1:5555: 175 records kept in /x/host-ferrix\n"
                .to_owned(),
            "lode-viewer: pull root@127.0.0.1:5555: 2 records kept in /x/host-ferrix\n".to_owned(),
        ];
        assert!(judge_ssh(Arch::X86_64, &pulls, (177, true)).is_ok());
        let again = judge_ssh(Arch::X86_64, &pulls, (352, true)).expect_err("repeated");
        assert!(again.to_string().contains("did not go on"), "{again}");
        assert!(judge_ssh(Arch::X86_64, &pulls, (177, false)).is_err());
        let none = vec![pulls[0].replace("175", "0"), pulls[1].clone()];
        assert!(judge_ssh(Arch::X86_64, &none, (2, true)).is_err());
    }

    #[test]
    fn the_key_types_the_query_with_no_key_twice() {
        let bind = CONFIG
            .lines()
            .find(|line| line.contains("vkbd"))
            .expect("the bind");
        let keys: Vec<&str> = bind
            .split("vkbd ")
            .nth(1)
            .unwrap_or("")
            .split(' ')
            .collect();
        let mut seen = std::collections::HashSet::new();
        // vkbd releases its keys in reverse, so a key named twice would be
        // pressed while it is held.
        assert!(keys.iter().all(|key| seen.insert(*key)), "{keys:?}");
    }
}
