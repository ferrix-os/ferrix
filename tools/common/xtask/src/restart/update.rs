//! `test-restart --update`: the display's driver put on new images while
//! the machine runs, before the kill rounds (`docs/DEVMGR.md` §4.1).
//!
//! With `/bin/blank` holding the card, the shell runs `/bin/drvupdate` five
//! times, and each answer must be the one its image earns:
//!
//! 1. bytes that are not a program: `refused`, and the card never goes;
//! 2. an image past 8 MiB: `malformed`, refused by `drvupdated` from the
//!    header before it makes any VMO, and the card never goes;
//! 3. `/sbin/pong`, a native program that exits at once on a START it does
//!    not understand: `rolled back`, and `card0` comes back from the old
//!    image (the exit arm);
//! 4. the `gpu` driver built as version `silent`, which says so and then
//!    neither publishes nor exits: `rolled back` once devmgr's 15 s pass
//!    (the deadline arm);
//! 5. the `gpu` driver built as version `next`: `updated`, its version line,
//!    devmgr's line with the carried file's fingerprint, and `card0` back.
//!
//! The kill rounds that follow then require the restarted driver to say
//! `version next` each time: a restart starts the image the update put on
//! the device. The shell must answer after every step.

use std::time::Instant;

use super::{ALIVE, ALIVE_ANSWER, PATIENCE, panicked};
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, init, native, ports, qemu};

/// Where the client is carried.
const DRVUPDATE: &str = "/bin/drvupdate";
/// The `next` build of the card's driver.
const NEXT: &str = "/lib/next/gpu";
/// The `silent` build, which never publishes.
const SILENT: &str = "/lib/next/silent";
/// Bytes that are not a program.
const GARBAGE: &str = "/lib/next/garbage";
/// A native program every image carries, which exits on a START.
const EXITS: &str = "/sbin/pong";
/// Where the shell writes the image past 8 MiB, and removes it after.
const BIG: &str = "/tmp/drvupdate-big";
/// Bytes in it: 9 MiB.
const BIG_BYTES: u32 = 9 << 20;

/// What the `next` driver says once at its start.
pub(super) const VERSION_NEXT: &str = "gpu: version next";
/// What the `silent` one says.
const VERSION_SILENT: &str = "gpu: version silent";
/// What the client's answer line begins with.
const ANSWER: &str = "drvupdate: gpu: ";
/// What the kernel says when the card goes, and, without `gone`, when it
/// is published.
const GONE: &str = "display  card0 is gone";
const CARD: &str = "display  card0 is ";

/// What an update boot carries beyond a restart boot's, and the
/// fingerprint devmgr must name the `next` image by.
pub(super) struct Carried {
    pub(super) files: Vec<ports::File>,
    pub(super) natives: native::Built,
    pub(super) fingerprint: u64,
}

/// Build the helper, the client and the two versions of the card's driver.
pub(super) fn carried(arch: Arch, release: bool) -> Result<Carried> {
    native::allow_updater();
    let helper = native::build_one(arch, release, &paths::target_dir(), &native::UPDATER)?;
    let client = init::built(arch)?
        .ok_or_else(|| Error::new("the init workspace, with drvupdate, is not built for x86-64"))?
        .drvupdate;
    let version = |version: &str| -> Result<Vec<u8>> {
        let target = paths::target_dir().join(format!("drv-{version}"));
        let path =
            cargo::build_native_version(arch, release, "ferrix-gpu", "gpu", &target, version)?;
        std::fs::read(&path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    let next = version("next")?;
    let silent = version("silent")?;
    let fingerprint = fnv64(&next);
    let file = |path: &str, mode: u32, bytes: Vec<u8>| ports::File {
        path: path.trim_start_matches('/').to_owned(),
        mode,
        content: ports::Content::Bytes(bytes),
    };
    let client = std::fs::read(&client)
        .map_err(|error| Error::new(format!("reading {}: {error}", client.display())))?;
    Ok(Carried {
        files: vec![
            file(DRVUPDATE, 0o755, client),
            file(NEXT, 0o755, next),
            file(SILENT, 0o755, silent),
            file(GARBAGE, 0o644, b"this is not a program\n".repeat(64)),
        ],
        natives: helper,
        fingerprint,
    })
}

/// FNV-1a over 64 bits, as devmgr's line gives it
/// (`ferrix_drvupdate_proto::Fingerprint`).
fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Whether the card is to stay up through a step, or to go and come back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Card {
    Stays,
    Back,
}

/// The five updates, in order.
pub(super) fn steps(
    watching: &mut qemu::Watching<'_>,
    fingerprint: u64,
) -> std::result::Result<(), String> {
    let said = format!("fnv64 {fingerprint:016x}");
    let big =
        format!("head -c {BIG_BYTES} /dev/zero > {BIG}; {DRVUPDATE} gpu {BIG}; rm -f {BIG}\n");
    let plan: [(&str, String, &str, Card, Vec<&str>); 5] = [
        (
            "an image that is not a program",
            format!("{DRVUPDATE} gpu {GARBAGE}\n"),
            "refused",
            Card::Stays,
            vec!["devmgr   update of gpu refused"],
        ),
        ("an image past 8 MiB", big, "malformed", Card::Stays, vec![]),
        (
            "a program that exits before it publishes",
            format!("{DRVUPDATE} gpu {EXITS}\n"),
            "rolled back",
            Card::Back,
            vec!["rolled back:"],
        ),
        (
            "a driver that never publishes",
            format!("{DRVUPDATE} gpu {SILENT}\n"),
            "rolled back",
            Card::Back,
            vec![VERSION_SILENT, "rolled back:"],
        ),
        (
            "the next version",
            format!("{DRVUPDATE} gpu {NEXT}\n"),
            "updated",
            Card::Back,
            vec![VERSION_NEXT, "updated:", said.as_str()],
        ),
    ];
    for (what, line, want, card, required) in plan {
        step(watching, what, line.as_bytes(), want, card, &required)?;
    }
    Ok(())
}

/// One update: type `line`, wait for the client's answer (and, when the card
/// is to come back, for it), and require `want`, the card as `card` says,
/// every line of `required`, and a live shell.
fn step(
    watching: &mut qemu::Watching<'_>,
    what: &str,
    line: &[u8],
    want: &str,
    card: Card,
    required: &[&str],
) -> std::result::Result<(), String> {
    let io = |error: Error| format!("updating to {what}: {error}");
    let before = watching.after().len();
    watching.type_in(line).map_err(io)?;
    let _ = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            let lines = lines.get(before..).unwrap_or_default();
            panicked(lines)
                || (answered(lines).is_some() && (card == Card::Stays || card_back(lines)))
        })
        .map_err(io)?;
    let lines = watching.after().get(before..).unwrap_or_default();
    if panicked(lines) {
        return Err(format!("updating to {what}: the kernel panicked"));
    }
    let Some(answer) = answered(lines) else {
        return Err(format!("updating to {what}: drvupdate never answered"));
    };
    if !answer.contains(&format!("{ANSWER}{want} (")) {
        return Err(format!(
            "updating to {what}: the answer was {answer:?}, not {want:?}"
        ));
    }
    match card {
        Card::Stays if lines.iter().any(|line| line.contains(GONE)) => {
            return Err(format!(
                "updating to {what}: card0 went, and an update that is refused must leave \
                 the running driver alone"
            ));
        }
        Card::Back if !card_back(lines) => {
            return Err(format!(
                "updating to {what}: card0 was not published again after it went"
            ));
        }
        _ => {}
    }
    if let Some(missing) = required
        .iter()
        .find(|want| !lines.iter().any(|line| line.contains(**want)))
    {
        return Err(format!("updating to {what}: no line said {missing:?}"));
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
    if alive {
        Ok(())
    } else {
        Err(format!(
            "updating to {what}: the shell did not answer afterwards"
        ))
    }
}

/// The client's answer line, if it has come.
fn answered(lines: &[String]) -> Option<&String> {
    lines.iter().find(|line| line.contains(ANSWER))
}

/// Whether the card went and was published again after it went.
fn card_back(lines: &[String]) -> bool {
    lines
        .iter()
        .rposition(|line| line.contains(GONE))
        .is_some_and(|gone| {
            lines
                .iter()
                .skip(gone + 1)
                .any(|line| line.contains(CARD) && !line.contains(GONE))
        })
}

/// Every `comm` process /proc lists but the newest: the drivers the updates
/// stopped, which may be listed yet, and which the kill rounds must not
/// count. The newest is the `next` driver, started last. An empty list when
/// the search does not finish: the rounds then say what they find.
pub(super) fn stale_drivers(watching: &mut qemu::Watching<'_>, comm: &str) -> Vec<u32> {
    let before = watching.after().len();
    if watching.type_in(&super::find_line(comm)).is_err() {
        return Vec::new();
    }
    let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
        super::listing_ended(lines.get(before..).unwrap_or_default())
    });
    let mut pids = super::pids_in(watching.after().get(before..).unwrap_or_default());
    pids.sort_unstable();
    let _newest = pids.pop();
    pids
}

/// How many times the `next` driver said its version in `lines`: once for
/// the update, and once for each restart after it.
pub(super) fn next_started(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| line.contains(VERSION_NEXT))
        .count()
}

#[cfg(test)]
mod tests {
    use super::{card_back, fnv64};

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn the_fingerprint_is_devmgrs() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn a_card_is_back_only_after_it_went() {
        let published = "  display  card0 is a 2D card";
        let gone = "  display  card0 is gone";
        assert!(!card_back(&lines(&[published])));
        assert!(!card_back(&lines(&[published, gone])));
        assert!(card_back(&lines(&[gone, published])));
        assert!(!card_back(&lines(&[gone, published, gone])));
    }
}
