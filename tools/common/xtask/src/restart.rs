//! `test-restart`: a driver killed from a shell is started again, and the
//! machine stays up.
//!
//! `docs/DEVMGR.md` §4. The gate boots zinc as an interactive shell, finds
//! the driver's processes through `/proc`, and types `kill -9` at them --
//! what a person at the console would do, and what once took the whole
//! machine down. Then it requires:
//!
//! 1. devmgr's DIED for every driver it killed, printed by the kernel;
//! 2. after them, devmgr's word that it started each one again, and
//!    whatever the kind publishes back under the name it had;
//! 3. the shell still answering, so the kernel is alive and the console
//!    works, and the kind's own proof that the device serves again.
//!
//! It kills the restarted drivers a second time and requires the same again,
//! so a restart is shown to be repeatable rather than a one-off.
//!
//! `--boot` names the kind; it is the display driver's when not given:
//!
//! * `gpu`: a virtio-gpu, held by `/bin/blank` as a compositor holds a card;
//!   `card0` is published again.
//! * `input`: a virtio keyboard and tablet, one driver each, both killed;
//!   `event0` and `event1` are published again, and a key pressed through QMP
//!   reaches the keyboard's node.
//! * `net`: a virtio-net; the interface comes back under the name and index
//!   it had, with its link up (`docs/NET-RING.md`, parking).
//! * `blk`: every disk's driver on a machine whose `/` is a btrfs root made
//!   fresh for the run; a file is written and read back on `/` after each
//!   round, and host `btrfs check` reads the volume afterwards.
//! * `all`: each of those, one boot each.
//!
//! The drivers it kills are looked up afresh each round, and must be ones it
//! has not killed already: a driver killed in round one may still be listed
//! in `/proc` when round two looks, beside the one that replaced it, and
//! procfs says `S` for both. The kill line checks every pid is still that
//! driver in the same line, and the gate fails naming the pids if one is
//! not, rather than kill whatever has the number by then.
//!
//! `--update` first puts the display's driver on new images while the
//! machine runs (`docs/DEVMGR.md` §4.1, [`update`]), then kills it as
//! above, and requires each restart to start the updated image.
//!
//! x86-64 only, for the reason `test-jobs` is: `kill` and `cat` are uutils',
//! built for x86-64 alone (`docs/UUTILS.md` D3).

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, btrfs_check, btrfs_disk, cargo, fat, initramfs, native, ports, qemu};
use crate::{uutils, zinc};

mod update;

/// How long to wait for the answer to one line.
const PATIENCE: Duration = Duration::from_secs(30);

/// What the pid search prints before each pid. Typed with a quote in the
/// middle, so the echo of the typed line never matches it.
const PID_TAG: &str = "restart-gate-pid=";

/// What the pid search prints once it has looked at every process, so the
/// gate reads the whole list and not only its first line.
const LISTED: &str = "restart-gate-listed";

/// What the kill line prints when it killed the pids it was given, which it
/// does only if every one is still the driver's process.
const KILLED_TAG: &str = "restart-gate-killed=";

/// What the kill line prints when a pid was not the driver's any more, or
/// the kill was refused, and nothing was killed.
const SPARED_TAG: &str = "restart-gate-spared=";

/// What the kernel prints for devmgr's DIED.
const DIED: &str = "devmgr   the driver of";

/// What the kernel prints when devmgr says it started a driver again.
const RESTARTED: &str = "was started again";

/// Where the card's holder is carried.
const BLANK: &str = "/bin/blank";

/// A line whose answer only a live shell can compute.
const ALIVE: &[u8] = b"echo restart-gate: $((6 * 7)) alive\n";
const ALIVE_ANSWER: &str = "restart-gate: 42 alive";

/// What the network listing prints before each `name:index:operstate`.
const LINK_TAG: &str = "restart-gate-link=";

/// Lists every network interface but the loopback as
/// `name:ifindex:operstate`, then [`LISTED`].
const LINKS: &[u8] = b"for n in /sys/class/net/*; do [ \"${n##*/}\" = lo ] && continue; \
    read i < $n/ifindex; read s < $n/operstate; \
    echo restart-gate-'link='${n##*/}:$i:$s; done; echo restart-gate-'listed'\n";

/// What the file line prints once it has read back what it wrote on `/`.
const FILE_TAG: &str = "restart-gate-file=";

/// Where `src/user/system/linux/compositor/evecho` is carried, which prints every event
/// the input devices deliver.
const EVECHO: &str = "/bin/evecho";

/// What evecho prints once it has opened every device.
const EVECHO_READY: &str = "evecho: ready";

/// The keyboard's name, as evecho prints it beside the node it opened.
const KEYBOARD: &str = "QEMU Virtio Keyboard";

/// The kinds of driver the gate kills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// The display's `gpu`.
    Gpu,
    /// The keyboard's and tablet's `input`.
    Input,
    /// The network's `net`.
    Net,
    /// Every disk's `blk`, `/` among them.
    Blk,
}

impl Kind {
    /// The kinds `--boot` names.
    fn asked(boot: Option<&str>) -> Result<Vec<Kind>> {
        Ok(match boot {
            None | Some("gpu") => vec![Kind::Gpu],
            Some("input") => vec![Kind::Input],
            Some("net") => vec![Kind::Net],
            Some("blk") => vec![Kind::Blk],
            Some("all") => vec![Kind::Gpu, Kind::Input, Kind::Net, Kind::Blk],
            Some(other) => {
                return Err(Error::new(format!(
                    "test-restart --boot takes gpu, input, net, blk or all, not `{other}`"
                )));
            }
        })
    }

    /// The driver's `comm`, which is its program's name.
    const fn comm(self) -> &'static str {
        match self {
            Kind::Gpu => "gpu",
            Kind::Input => "input",
            Kind::Net => "net",
            Kind::Blk => "blk",
        }
    }

    /// What the report calls it.
    const fn what(self) -> &'static str {
        match self {
            Kind::Gpu => "display driver",
            Kind::Input => "input drivers",
            Kind::Net => "network driver",
            Kind::Blk => "disk drivers",
        }
    }

    /// What the kernel prints once each device is published again, all of
    /// which must follow the deaths.
    const fn published(self) -> &'static [&'static str] {
        match self {
            Kind::Gpu => &["display  card0 is"],
            Kind::Input => &["input    event0 ", "input    event1 "],
            Kind::Net | Kind::Blk => &[],
        }
    }
}

/// Boot a shell beside each kind's devices, kill the drivers twice, and
/// require them back each time.
///
/// # Errors
///
/// When the image cannot be built, when the boot fails or panics, or when a
/// driver is not found, not restarted, or the shell stops answering.
pub(crate) fn test_restart(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-restart runs on x86-64 only: `kill` comes from uutils, which is built \
             for x86-64 alone (docs/UUTILS.md D3)",
        ));
    }
    let kinds = Kind::asked(args.boot.as_deref())?;
    if args.update && kinds != [Kind::Gpu] {
        return Err(Error::new(
            "test-restart --update updates the display's driver so far: --boot gpu, or no --boot",
        ));
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose init is an interactive shell");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, "")?;
    let mut natives = native::build(arch, args.release)?;
    let utilities = uutils::carried(arch)?;
    if utilities.is_empty() {
        return Err(Error::new(
            "the image carries no utilities: `cargo xtask uutils` builds the ones this \
             gate types",
        ));
    }
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    // A program holding the card when its driver dies: `src/user/system/linux/compositor/blank`
    // sets a mode, shows a buffer and waits, as a compositor's card stays open.
    let blank = crate::display::build_blank(arch, false)?;
    let mut carried = crate::apps::ported(arch, args)?;
    carried.push(carry(BLANK, &blank)?);
    // What shows a key arriving on a restarted keyboard.
    if kinds.contains(&Kind::Input) {
        let evecho = crate::input::build_evecho(arch, false)?;
        carried.push(carry(EVECHO, &evecho)?);
    }
    // The helper, the client and the images it updates the card's driver to.
    let fingerprint = if args.update {
        let update = update::carried(arch, args.release)?;
        natives.push(update.natives);
        carried.extend(update.files);
        Some(update.fingerprint)
    } else {
        None
    };
    let archive = initramfs::build_with_utilities(
        Some(&shell),
        &natives,
        Some(&bytes),
        &utilities,
        &carried,
    )?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    for kind in kinds {
        restart_one(arch, &image, &kernel, args, kind, fingerprint)?;
    }
    Ok(())
}

/// `program`, carried at `path` in the image.
fn carry(path: &str, program: &std::path::Path) -> Result<ports::File> {
    Ok(ports::File {
        path: path.trim_start_matches('/').to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(
            std::fs::read(program)
                .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?,
        ),
    })
}

/// One kind's boot: its devices on the bus, two rounds, and the verdict.
fn restart_one(
    arch: Arch,
    image: &std::path::Path,
    kernel: &std::path::Path,
    args: &Args,
    kind: Kind,
    update: Option<u64>,
) -> Result<()> {
    let mut booted = args.clone();
    match kind {
        // A virtio-gpu on the bus, and nothing drawn to a window.
        Kind::Gpu => booted.display = true,
        Kind::Input => {
            booted.input = true;
            // QMP, to press a key on the restarted keyboard.
            booted.qmp_port = Some(crate::display::free_port()?);
        }
        Kind::Net => booted.net = true,
        // `/` on a btrfs root made fresh for the run (`qemu::attach_root_disk`).
        Kind::Blk => booted.btrfs_root = true,
    }
    let checker = match kind {
        Kind::Blk => Some(btrfs_check::Checker::required()?),
        _ => None,
    };
    println!(
        "  {arch}: killing the {} from the shell (timeout {}s)",
        kind.what(),
        args.timeout
    );
    let mut failures: Vec<String> = Vec::new();
    let lines = qemu::watch_then(
        arch,
        image,
        kernel,
        &booted,
        qemu::SUCCESS_MARKER,
        |watching| {
            // The shell at its prompt is left there: nothing powers off.
            watching.stop_when_done();
            if let Err(failure) = ready(watching, kind) {
                failures.push(failure);
                return Ok(());
            }
            let before = match prepare(watching, kind) {
                Ok(before) => before,
                Err(failure) => {
                    failures.push(failure);
                    return Ok(());
                }
            };
            let mut killed = Vec::new();
            if let Some(fingerprint) = update {
                match update::steps(watching, fingerprint) {
                    // Every driver the updates started but the last is
                    // gone, though /proc may list it yet: the rounds kill
                    // the newest alone.
                    Ok(()) => killed = update::stale_drivers(watching, kind.comm()),
                    Err(failure) => {
                        failures.push(failure);
                        return Ok(());
                    }
                }
            }
            for round in 1..=2 {
                let outcome = kill_and_expect_back(watching, kind, round, &mut killed)
                    .and_then(|()| serves_again(watching, kind, round, &before, booted.qmp_port));
                if let Err(failure) = outcome {
                    failures.push(failure);
                    break;
                }
            }
            Ok(())
        },
    )?;
    if let Some(panic) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        failures.push(format!("the kernel panicked: {}", panic.trim()));
    }
    // The update's start, then one for each of the two rounds' restarts.
    let started = update::next_started(&lines);
    if update.is_some() && failures.is_empty() && started < 3 {
        failures.push(format!(
            "{:?} came {started} times, not 3: a restart after the update must start the \
             image the update put on the card",
            update::VERSION_NEXT
        ));
    }
    if failures.is_empty()
        && let Some(checker) = checker
    {
        let root = btrfs_disk::test_root_path(arch);
        if let Err(error) = checker.run(&root, arch) {
            failures.push(error.to_string());
        }
    }
    if !failures.is_empty() {
        let mut message = format!("{arch}: the {} did not come back:\n", kind.what());
        for failure in &failures {
            message.push_str("    - ");
            message.push_str(failure);
            message.push('\n');
        }
        message.push_str("  The whole transcript is above and in the serial log.");
        return Err(Error::new(message));
    }
    println!(
        "  {arch}: the {}, killed twice, came back each time, and the machine stayed up",
        kind.what()
    );
    Ok(())
}

/// Wait for what the first keystroke needs: the kind's devices published,
/// which the kernel says as each driver's READY goes out, and the shell
/// reading the console.
fn ready(watching: &mut qemu::Watching<'_>, kind: Kind) -> std::result::Result<(), String> {
    let deadline = Instant::now() + PATIENCE;
    let said = |lines: &[String], want: &str| lines.iter().any(|line| line.contains(want));
    for want in kind.published() {
        if said(watching.lines(), want) {
            continue;
        }
        let published = watching
            .read_more(deadline, |lines| said(lines, want))
            .map_err(|error| error.to_string())?;
        if !published {
            return Err(format!(
                "the {} were never published: no `{want}`",
                kind.what()
            ));
        }
    }
    if watching
        .wait_for_shell(deadline)
        .map_err(|error| error.to_string())?
    {
        Ok(())
    } else {
        Err("the shell never answered at the console".to_owned())
    }
}

/// What a kind needs before the first kill, and what it must find again
/// after each: the card held, or the interfaces as they were.
fn prepare(
    watching: &mut qemu::Watching<'_>,
    kind: Kind,
) -> std::result::Result<Vec<String>, String> {
    match kind {
        Kind::Gpu => hold_the_card(watching).map(|()| Vec::new()),
        Kind::Net => {
            let links = links(watching, 0)?;
            if links.is_empty() {
                return Err("no network interface in /sys/class/net to watch".to_owned());
            }
            Ok(links)
        }
        Kind::Input | Kind::Blk => Ok(Vec::new()),
    }
}

/// Start [`BLANK`] in the background and wait for its buffer on the screen.
fn hold_the_card(watching: &mut qemu::Watching<'_>) -> std::result::Result<(), String> {
    let before = watching.after().len();
    watching
        .type_in(format!("{BLANK} &\n").as_bytes())
        .map_err(|error| error.to_string())?;
    let shown = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            lines
                .get(before..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(crate::display::MARKER))
        })
        .map_err(|error| error.to_string())?;
    if shown {
        Ok(())
    } else {
        Err(format!(
            "{BLANK} never showed its buffer, so nothing held the card"
        ))
    }
}

/// One round: find the drivers, kill them, and require a DIED for each, a
/// restart for each and what the kind publishes, then a live shell. `killed`
/// is the pids earlier rounds killed, which this one adds to.
fn kill_and_expect_back(
    watching: &mut qemu::Watching<'_>,
    kind: Kind,
    round: u32,
    killed: &mut Vec<u32>,
) -> std::result::Result<(), String> {
    let comm = kind.comm();
    let io = |error: Error| format!("round {round}: {error}");
    let before = watching.after().len();
    watching.type_in(&find_line(comm)).map_err(io)?;
    let listed = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            listing_ended(lines.get(before..).unwrap_or_default())
        })
        .map_err(io)?;
    if !listed {
        return Err(format!(
            "round {round}: the search for the {comm} drivers never finished"
        ));
    }
    let found = pids_in(watching.after().get(before..).unwrap_or_default());
    let pids = the_drivers(comm, &found, killed).map_err(|why| format!("round {round}: {why}"))?;

    // Checked again in the line that kills, so what is killed is the drivers
    // the list named or nothing. One line, so that killing a disk's driver
    // leaves no second command waiting to be read from that disk.
    let before = watching.after().len();
    watching.type_in(&kill_line(comm, &pids)).map_err(io)?;
    let _ = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            kill_answer(lines.get(before..).unwrap_or_default(), &pids).is_some()
        })
        .map_err(io)?;
    match kill_answer(watching.after().get(before..).unwrap_or_default(), &pids) {
        Some(true) => killed.extend_from_slice(&pids),
        Some(false) => {
            return Err(format!(
                "round {round}: one of {pids:?} was no longer a {comm} process when the gate \
                 came to kill it, so nothing was killed"
            ));
        }
        None => {
            return Err(format!(
                "round {round}: the line that kills {pids:?} never said whether it did"
            ));
        }
    }
    let back = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            let lines = lines.get(before..).unwrap_or_default();
            came_back(lines, pids.len(), kind.published()) || panicked(lines)
        })
        .map_err(io)?;
    let after = watching.after().get(before..).unwrap_or_default();
    if panicked(after) {
        return Err(format!(
            "round {round}: the kernel panicked after `kill -9` of {pids:?}"
        ));
    }
    if !back {
        return Err(format!(
            "round {round}: after `kill -9` of {pids:?} the guest never said {DIED:?} and \
             {RESTARTED:?} {} times, then {:?}",
            pids.len(),
            kind.published()
        ));
    }

    let before = watching.after().len();
    watching.type_in(ALIVE).map_err(io)?;
    let alive = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            lines
                .get(before..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(ALIVE_ANSWER))
        })
        .map_err(io)?;
    if !alive {
        return Err(format!(
            "round {round}: the shell did not answer after the drivers came back"
        ));
    }
    Ok(())
}

/// The kind's own proof that its device serves again after a round: a key
/// pressed arriving, the interfaces as they were, or a file written and read
/// back on `/`.
fn serves_again(
    watching: &mut qemu::Watching<'_>,
    kind: Kind,
    round: u32,
    before: &[String],
    qmp_port: Option<u16>,
) -> std::result::Result<(), String> {
    match kind {
        Kind::Net => {
            let after = links(watching, round)?;
            if after == before {
                Ok(())
            } else {
                Err(format!(
                    "round {round}: the interfaces were {before:?} before the kill and \
                     {after:?} after it: a restarted driver must take its interface back \
                     under the same name and index, with its link up"
                ))
            }
        }
        Kind::Blk => write_and_read_back(watching, round),
        Kind::Input => a_key_arrives(watching, round, qmp_port),
        Kind::Gpu => Ok(()),
    }
}

/// Start [`EVECHO`], press and release `a` on the keyboard through QMP, and
/// require the press on the node evecho found the keyboard on. Not always
/// `event0`: numbers are the lowest free, and when both drivers die at once
/// the one that comes back first takes the lower.
fn a_key_arrives(
    watching: &mut qemu::Watching<'_>,
    round: u32,
    qmp_port: Option<u16>,
) -> std::result::Result<(), String> {
    let io = |error: Error| format!("round {round}: {error}");
    let port = qmp_port.ok_or_else(|| format!("round {round}: the boot has no QMP port"))?;
    let before = watching.after().len();
    watching
        .type_in(format!("{EVECHO} &\n").as_bytes())
        .map_err(io)?;
    let ready = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            seen(lines.get(before..).unwrap_or_default(), EVECHO_READY)
        })
        .map_err(io)?;
    if !ready {
        return Err(format!(
            "round {round}: {EVECHO} never opened the restarted devices"
        ));
    }
    let keyboard =
        crate::input::node_of(watching.after().get(before..).unwrap_or_default(), KEYBOARD)
            .ok_or_else(|| format!("round {round}: {EVECHO} opened no `{KEYBOARD}`"))?;
    let arrived_line = format!("evecho: {keyboard} EV_KEY KEY_A 1");
    let mut qmp =
        crate::display::Qmp::connect(port, Instant::now() + Duration::from_secs(10)).map_err(io)?;
    for down in [true, false] {
        qmp.input_send_event(&[crate::input::key("a", down)])
            .map_err(io)?;
    }
    let arrived = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            seen(lines.get(before..).unwrap_or_default(), &arrived_line)
        })
        .map_err(io)?;
    if arrived {
        Ok(())
    } else {
        Err(format!(
            "round {round}: `a` pressed through QMP never reached {keyboard} after the \
             keyboard's driver came back"
        ))
    }
}

/// Whether any of `lines` holds `want`.
fn seen(lines: &[String], want: &str) -> bool {
    lines.iter().any(|line| line.contains(want))
}

/// Every interface but the loopback, as `name:index:operstate`, waiting for
/// the link to come up again for as long as the patience allows.
fn links(
    watching: &mut qemu::Watching<'_>,
    round: u32,
) -> std::result::Result<Vec<String>, String> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let before = watching.after().len();
        watching
            .type_in(LINKS)
            .map_err(|error| format!("round {round}: {error}"))?;
        let listed = watching
            .read_more(Instant::now() + PATIENCE, |lines| {
                listing_ended(lines.get(before..).unwrap_or_default())
            })
            .map_err(|error| format!("round {round}: {error}"))?;
        if !listed {
            return Err(format!(
                "round {round}: the listing of /sys/class/net never finished"
            ));
        }
        let found = tagged(watching.after().get(before..).unwrap_or_default(), LINK_TAG);
        if found.iter().all(|link| link.ends_with(":up")) || Instant::now() >= deadline {
            return Ok(found);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Write a line to a file on `/`, sync it, and read it back.
fn write_and_read_back(
    watching: &mut qemu::Watching<'_>,
    round: u32,
) -> std::result::Result<(), String> {
    let want = format!("round{round}");
    let line = format!(
        "echo {want} > /restart-gate && sync && read l < /restart-gate && \
         echo restart-gate-'file='$l\n"
    );
    let before = watching.after().len();
    watching
        .type_in(line.as_bytes())
        .map_err(|error| format!("round {round}: {error}"))?;
    let _ = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            !tagged(lines.get(before..).unwrap_or_default(), FILE_TAG).is_empty()
        })
        .map_err(|error| format!("round {round}: {error}"))?;
    let read = tagged(watching.after().get(before..).unwrap_or_default(), FILE_TAG);
    if read.first() == Some(&want) {
        Ok(())
    } else {
        Err(format!(
            "round {round}: writing {want:?} to /restart-gate on the btrfs root and reading \
             it back gave {read:?}"
        ))
    }
}

fn panicked(lines: &[String]) -> bool {
    lines.iter().any(|line| line.contains(qemu::PANIC_MARKER))
}

/// The number after `tag` in `line`, if the tag is there.
fn number_after(line: &str, tag: &str) -> Option<u32> {
    let (_, rest) = line.split_once(tag)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The word after `tag` on every line that has it, up to the listing's end.
fn tagged(lines: &[String], tag: &str) -> Vec<String> {
    lines
        .iter()
        .take_while(|line| !line.contains(LISTED))
        .filter_map(|line| {
            let (_, rest) = line.split_once(tag)?;
            Some(
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

/// The line that finds the drivers: every process whose `comm` is `comm`,
/// then [`LISTED`].
///
/// A process that ends between the glob and the `read` is skipped: the
/// failed `read` leaves `n` holding the previous process's name, so testing
/// it would list a pid that was never a driver.
fn find_line(comm: &str) -> Vec<u8> {
    format!(
        "for p in /proc/[0-9]*; do read n < $p/comm || continue; \
         [ \"$n\" = {comm} ] && echo restart-gate-'pid='${{p#/proc/}}; done; \
         echo restart-gate-'listed'\n"
    )
    .into_bytes()
}

/// Whether the search has printed [`LISTED`]: its end, and not the echo of
/// the typed line, which has a quote in the middle of the tag.
fn listing_ended(lines: &[String]) -> bool {
    lines.iter().any(|line| line.contains(LISTED))
}

/// Every pid the search printed, in order, up to its end.
fn pids_in(lines: &[String]) -> Vec<u32> {
    lines
        .iter()
        .take_while(|line| !line.contains(LISTED))
        .filter_map(|line| number_after(line, PID_TAG))
        .collect()
}

/// The drivers to kill: every `comm` process listed that no earlier round
/// killed. A killed driver may still be listed beside the one that replaced
/// it. None is a failure, named, rather than a guess.
fn the_drivers(comm: &str, found: &[u32], killed: &[u32]) -> std::result::Result<Vec<u32>, String> {
    let fresh: Vec<u32> = found
        .iter()
        .copied()
        .filter(|pid| !killed.contains(pid))
        .collect();
    match fresh.as_slice() {
        [] if found.is_empty() => Err(format!("no process named {comm} in /proc")),
        [] => Err(format!(
            "the only {comm} processes in /proc, {found:?}, are ones the gate already killed"
        )),
        _ => Ok(fresh),
    }
}

/// The line that kills `pids`, if every one is still a `comm` process when
/// the shell runs the line, and says which it did. Its tags have a quote in
/// the middle, as [`find_line`]'s have.
fn kill_line(comm: &str, pids: &[u32]) -> Vec<u8> {
    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let list = list.join(" ");
    let first = pids.first().copied().unwrap_or(0);
    format!(
        "ok=1; for p in {list}; do read n < /proc/$p/comm && [ \"$n\" = {comm} ] || ok=0; done; \
         if [ $ok = 1 ] && kill -9 {list}; \
         then echo restart-gate-'killed='{first}; else echo restart-gate-'spared='{first}; fi\n"
    )
    .into_bytes()
}

/// What the kill line said about `pids`: `Some(true)` killed, `Some(false)`
/// spared, `None` nothing yet. The line names the first pid.
fn kill_answer(lines: &[String], pids: &[u32]) -> Option<bool> {
    let first = pids.first().copied()?;
    lines.iter().find_map(|line| {
        if number_after(line, KILLED_TAG) == Some(first) {
            Some(true)
        } else if number_after(line, SPARED_TAG) == Some(first) {
            Some(false)
        } else {
            None
        }
    })
}

/// Whether `lines` say `count` drivers died and then came back: a DIED for
/// each, and after the first of them a RESTARTED for each and every line in
/// `published`, in any order. The kernel prints a device as its driver's
/// READY goes out and devmgr sends RESTARTED once it has read PUBLISHED, so
/// the two race.
fn came_back(lines: &[String], count: usize, published: &[&str]) -> bool {
    let Some(died) = lines.iter().position(|line| line.contains(DIED)) else {
        return false;
    };
    let after = lines.get(died..).unwrap_or_default();
    let deaths = after.iter().filter(|line| line.contains(DIED)).count();
    let restarts = after.iter().filter(|line| line.contains(RESTARTED)).count();
    deaths >= count
        && restarts >= count
        && published
            .iter()
            .all(|want| after.iter().any(|line| line.contains(want)))
}

#[cfg(test)]
mod tests {
    use super::{
        KILLED_TAG, LINK_TAG, LISTED, PID_TAG, SPARED_TAG, came_back, find_line, kill_answer,
        kill_line, listing_ended, pids_in, tagged, the_drivers,
    };

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    const DIED: &str =
        "  devmgr   the driver of 0x18 ended with status 137; the device is quiesced";
    const RESTARTED: &str =
        "  devmgr   the driver of 0x18 was started again and published (restart 1)";

    #[test]
    fn the_typed_lines_are_not_answers() {
        // The console echoes what was typed; every tag is split by a quote.
        let typed = String::from_utf8_lossy(&find_line("gpu")).into_owned();
        assert!(!typed.contains(PID_TAG));
        assert!(!typed.contains(LISTED));
        assert!(!listing_ended(&lines(&[&typed])));
        let kill = String::from_utf8_lossy(&kill_line("gpu", &[273, 274])).into_owned();
        assert!(!kill.contains(KILLED_TAG));
        assert!(!kill.contains(SPARED_TAG));
        assert_eq!(kill_answer(&lines(&[&kill]), &[273, 274]), None);
        let links = String::from_utf8_lossy(super::LINKS).into_owned();
        assert!(!links.contains(LINK_TAG));
        assert!(!links.contains(LISTED));
    }

    #[test]
    fn every_printed_pid_is_found() {
        let printed = lines(&[
            "x",
            "restart-gate-pid=222",
            "restart-gate-pid=273",
            "restart-gate-listed",
            "restart-gate-pid=9",
        ]);
        assert!(listing_ended(&printed));
        assert_eq!(pids_in(&printed), [222, 273]);
    }

    #[test]
    fn a_driver_killed_already_is_not_killed_again() {
        // Round two of the run that killed 222: 222 was still listed, first,
        // beside the 273 that replaced it.
        assert_eq!(the_drivers("gpu", &[222, 273], &[222]), Ok(vec![273]));
        assert_eq!(the_drivers("gpu", &[273], &[222]), Ok(vec![273]));
        // Two input drivers, both fresh, are both killed.
        assert_eq!(the_drivers("input", &[40, 41], &[]), Ok(vec![40, 41]));
    }

    #[test]
    fn no_fresh_driver_is_a_failure_not_a_guess() {
        assert!(the_drivers("gpu", &[], &[]).is_err());
        assert!(the_drivers("gpu", &[222], &[222]).is_err());
    }

    #[test]
    fn the_kill_line_says_whether_it_killed_or_spared() {
        let killed = lines(&["restart-gate-killed=273"]);
        assert_eq!(kill_answer(&killed, &[273, 274]), Some(true));
        assert_eq!(kill_answer(&killed, &[27]), None);
        assert_eq!(
            kill_answer(&lines(&["restart-gate-spared=273"]), &[273]),
            Some(false)
        );
    }

    #[test]
    fn the_card_and_the_restart_may_come_in_either_order() {
        // The kernel's line glued onto a shell's prompt, as a console has it.
        let card = "\x1b[J  display  card0 is a scanout: no 3D";
        let wanted = ["display  card0 is"];
        assert!(came_back(&lines(&[DIED, card, RESTARTED]), 1, &wanted));
        assert!(came_back(&lines(&[DIED, RESTARTED, card]), 1, &wanted));
    }

    #[test]
    fn a_card_from_before_the_death_does_not_count() {
        let card = "  display  card0 is a scanout: no 3D";
        assert!(!came_back(
            &lines(&[card, DIED, RESTARTED]),
            1,
            &["display  card0 is"]
        ));
    }

    #[test]
    fn every_killed_driver_must_die_and_come_back() {
        assert!(!came_back(&lines(&[DIED, RESTARTED]), 2, &[]));
        assert!(!came_back(&lines(&[DIED, DIED, RESTARTED]), 2, &[]));
        assert!(came_back(
            &lines(&[DIED, DIED, RESTARTED, RESTARTED]),
            2,
            &[]
        ));
    }

    #[test]
    fn the_interfaces_are_read_as_listed() {
        let printed = lines(&[
            "restart-gate-link=eth0:2:up",
            "restart-gate-listed",
            "restart-gate-link=eth1:3:up",
        ]);
        assert_eq!(tagged(&printed, LINK_TAG), ["eth0:2:up"]);
    }
}
