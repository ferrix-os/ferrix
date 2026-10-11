//! `bootctl`: the running system's half of the second slot
//! (`docs/DEV-ON-FERRIX.md` §4.4, D8; the rules are `docs/AB-BOOT.md` §5).
//!
//! ```text
//! bootctl [--disk /dev/vdX] status
//! bootctl [--disk /dev/vdX] try <image> [--tries N] [--cmdline "words"]
//! bootctl [--disk /dev/vdX] verify <a|b>
//! bootctl [--disk /dev/vdX] good
//! bootctl [--disk /dev/vdX] rollback
//! bootctl [--disk /dev/vdX] init
//! ```
//!
//! The disk is the one with partitions of Ferrix's boot-control and
//! system-slot types (`layout`); `bootctl` writes no other partition. The
//! slot this system booted from is what the loader put first on the
//! command line: `ferrix.slot=a|b ferrix.slot.boot=trial|good|fallback
//! ferrix.slot.seq=<n>`.
//!
//! `try` writes an image -- a slot image (`cargo xtask slot-image`), an
//! `xtask` FAT image such as `build/x86_64/ferrix.img`, or a directory
//! with `KERNEL.ELF`, `INITRD.IMG` and `CMDLINE.TXT` -- into the *idle*
//! slot, the one that is not good. It refuses while this system runs a trial
//! (the idle slot is then the one running) or when the command line and the
//! record disagree. It first takes back any trial armed on the idle slot,
//! so a slot half written is never tried; then writes the slot, flushes,
//! verifies it as the loader does, and only then arms the trial.
//!
//! Root only: the control record decides what the machine boots.

use std::process::ExitCode;

use std::fs::File;

use ferrix_install::disk::{self, SECTOR};
use ferrix_install::layout::{self, AbDisk, Slot};
use ferrix_install::record::{self, Blocks};
use ferrix_install::slotio;
use ferrix_partition::Guid;
use ferrix_slots::record::{Current, MAX_TRIES, Record};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("bootctl: {why}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "usage: bootctl [--disk /dev/vdX] status | try <image> [--tries N] \
                     [--cmdline \"words\"] | verify <a|b> | good | rollback | init";

impl Command {
    /// Whether it writes the record or a slot on a slot boot's behalf.
    fn changes_what_boots(&self) -> bool {
        matches!(
            self,
            Command::Try { .. } | Command::Good | Command::Rollback
        )
    }
}

enum Command {
    Status,
    Try {
        image: String,
        tries: u8,
        cmdline: Option<String>,
    },
    Verify(Slot),
    Good,
    Rollback,
    Init,
}

fn parse() -> Result<(Option<String>, Command), String> {
    let mut disk = None;
    let mut words = Vec::new();
    let mut tries = 1_u8;
    let mut cmdline = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--disk" => disk = Some(args.next().ok_or("--disk needs a disk")?),
            "--tries" => {
                tries = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|&n| (1..=MAX_TRIES).contains(&n))
                    .ok_or(format!("--tries needs a number from 1 to {MAX_TRIES}"))?;
            }
            "--cmdline" => cmdline = Some(args.next().ok_or("--cmdline needs its words")?),
            "--help" | "-h" => return Err(USAGE.into()),
            _ => words.push(arg),
        }
    }
    let command = match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["status"] | [] => Command::Status,
        ["try", image] => Command::Try {
            image: (*image).to_owned(),
            tries,
            cmdline,
        },
        ["verify", slot] => Command::Verify(Slot::from_letter(slot).ok_or("verify a or b")?),
        ["good"] => Command::Good,
        ["rollback"] => Command::Rollback,
        ["init"] => Command::Init,
        _ => return Err(USAGE.into()),
    };
    Ok((disk, command))
}

fn run() -> Result<(), String> {
    let (disk, command) = parse()?;
    if !is_root() {
        return Err("only root may read or change what this machine boots".into());
    }
    let booted = Booted::read()?;
    // A boot that did not come from a slot (the ESP's rescue files) may look
    // and repair, never change what boots (the consultant's S2).
    if booted.slot.is_none() && command.changes_what_boots() {
        return Err(
            "this system did not boot from a slot (no ferrix.slot= on the command line): try, \
             good and rollback refuse; status, verify and init work"
                .into(),
        );
    }
    let ab = layout::locate(disk.as_deref(), booted.disk)?;
    let block = disk::logical_block_size(&ab.disk)?;
    if u64::from(block) != SECTOR {
        return Err(format!(
            "{} has {block}-byte logical blocks; bootctl writes only disks of 512-byte blocks \
             until 4 KiB ones are supported",
            ab.disk
        ));
    }
    match command {
        Command::Status => status(&ab, &booted),
        Command::Try {
            image,
            tries,
            cmdline,
        } => try_image(&ab, &booted, &image, tries, cmdline.as_deref()),
        Command::Verify(slot) => {
            let header = verify(&ab, slot)?;
            println!(
                "bootctl: slot {} verifies ({}, {} MiB)",
                slot.letter(),
                header.label_text(),
                header.needed() >> 20
            );
            Ok(())
        }
        Command::Good => good(&ab, &booted),
        Command::Rollback => rollback(&ab, &booted),
        Command::Init => init(&ab),
    }
}

/// Whether this process's effective user is root, from `/proc/self/status`.
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
        status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().nth(1).map(|euid| euid == "0"))
            .unwrap_or(false)
    })
}

/// What the loader said about this boot.
struct Booted {
    slot: Option<Slot>,
    /// The GUID of the disk the loader booted from.
    disk: Option<Guid>,
    /// `trial`, `good` or `fallback`.
    how: Option<String>,
    sequence: Option<u64>,
}

impl Booted {
    /// From `/proc/cmdline`: the first word with each key, as the kernel
    /// takes them, so a slot's own command line cannot override the
    /// loader's.
    fn read() -> Result<Booted, String> {
        let line =
            std::fs::read_to_string("/proc/cmdline").map_err(|e| format!("/proc/cmdline: {e}"))?;
        Ok(Self::parse(&line))
    }

    fn parse(line: &str) -> Booted {
        let first = |key: &str| {
            line.split_whitespace()
                .find_map(|word| word.strip_prefix(key))
                .map(str::to_owned)
        };
        Booted {
            slot: first("ferrix.slot=").and_then(|s| Slot::from_letter(&s)),
            disk: first("ferrix.slot.disk=").and_then(|s| parse_guid(&s)),
            how: first("ferrix.slot.boot="),
            sequence: first("ferrix.slot.seq=").and_then(|s| s.parse().ok()),
        }
    }

    fn is_trial(&self) -> bool {
        self.how.as_deref() == Some("trial")
    }
}

/// A GUID as GPT tools print it, `8-4-4-4-12` hex, either case.
fn parse_guid(text: &str) -> Option<Guid> {
    let hex: String = text.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || text.len() != 36 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok();
    let mut printed = [0_u8; 16];
    for (i, out) in printed.iter_mut().enumerate() {
        *out = byte(i)?;
    }
    // The first three fields are stored little-endian.
    let order = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut stored = [0_u8; 16];
    for (to, &from) in stored.iter_mut().zip(&order) {
        *to = printed[from];
    }
    Some(Guid(stored))
}

/// The control record, on the whole disk, in its logical blocks.
struct Control {
    file: File,
    part: ferrix_slots::gpt::Part,
    current: Current,
}

impl Control {
    fn open(ab: &AbDisk) -> Result<Control, String> {
        let mut node =
            disk::open_rw(&ab.control.node).map_err(|e| format!("{}: {e}", ab.control.node))?;
        layout::check_writable(&ab.control, &mut node)?;
        let mut file = disk::open_rw(&ab.disk).map_err(|e| format!("{}: {e}", ab.disk))?;
        let part = ab.slots_layout.control;
        let current = record::read(&mut blocks(&mut file), &part)?;
        Ok(Control {
            file,
            part,
            current,
        })
    }

    fn record(&self) -> Option<Record> {
        record::current(&self.current)
    }

    fn write(&mut self, next: Option<Record>) -> Result<Record, String> {
        let written = record::write(&mut blocks(&mut self.file), &self.part, &self.current, next)?;
        self.current = record::read(&mut blocks(&mut self.file), &self.part)?;
        Ok(written)
    }

    fn copies_valid(&mut self) -> [bool; 2] {
        record::copies_valid(&mut blocks(&mut self.file), &self.part)
    }
}

fn blocks(file: &mut File) -> Blocks<'_> {
    Blocks {
        file,
        block: SECTOR as u32,
    }
}

fn verify(ab: &AbDisk, slot: Slot) -> Result<ferrix_slots::slot::SlotHeader, String> {
    let part = ab.slot(slot);
    let mut file = disk::open(&part.node).map_err(|e| format!("{}: {e}", part.node))?;
    slotio::verify(&mut file, 0, part.bytes()).map_err(|why| {
        format!(
            "slot {} ({}) does not verify: {why}",
            slot.letter(),
            part.node
        )
    })
}

fn status(ab: &AbDisk, booted: &Booted) -> Result<(), String> {
    let mut control = Control::open(ab)?;
    println!(
        "disk:    {} {} (control {}, slot a {}, slot b {})",
        ab.disk, ab.guid, ab.control.node, ab.slots[0].node, ab.slots[1].node
    );
    match booted.slot {
        None => println!("booted:  not from a slot (no ferrix.slot= on the command line)"),
        Some(slot) => println!(
            "booted:  {} as {}, under record {}",
            slot.letter(),
            booted.how.as_deref().unwrap_or("?"),
            booted
                .sequence
                .map_or_else(|| "?".to_owned(), |s| s.to_string())
        ),
    }
    let valid = control.copies_valid();
    match control.record() {
        None => println!(
            "record:  none valid (uninitialised): the loader tries a, then b; `bootctl init`"
        ),
        Some(r) => {
            println!(
                "record:  sequence {}; copies valid: {}, {}",
                r.sequence, valid[0], valid[1]
            );
            println!("good:    {}", r.good.letter());
            match r.trial {
                Some(t) if r.tries == 0 => println!(
                    "trial:   {}, exhausted: it failed, and the loader boots {}",
                    t.letter(),
                    r.good.letter()
                ),
                Some(t) => println!("trial:   {}, {} tries left", t.letter(), r.tries),
                None => println!("trial:   none"),
            }
        }
    }
    for slot in Slot::BOTH {
        let part = ab.slot(slot);
        let state = match verify(ab, slot) {
            Ok(header) => format!(
                "verifies: {} ({} MiB)",
                header.label_text(),
                header.needed() >> 20
            ),
            Err(why) => why,
        };
        println!(
            "slot {}:  {} ({} MiB), {state}",
            slot.letter(),
            part.node,
            part.bytes() >> 20
        );
    }
    Ok(())
}

/// The slot `try` writes: the one that is not good, and never the running
/// one.
fn idle(record: &Record, booted: &Booted) -> Result<Slot, String> {
    if let Some(slot) = booted.slot {
        if booted.is_trial() && record.trial == Some(slot) {
            return Err(format!(
                "this system runs slot {} on trial; run `bootctl good` or `bootctl rollback` \
                 and restart first",
                slot.letter()
            ));
        }
        if slot != record.good {
            return Err(format!(
                "this system booted slot {} ({}) but the record says {} is good; the idle slot \
                 would be the running one, so nothing is written",
                slot.letter(),
                booted.how.as_deref().unwrap_or("?"),
                record.good.letter()
            ));
        }
    }
    let target = record.good.other();
    if booted.slot == Some(target) {
        return Err(format!("slot {} is the one running", target.letter()));
    }
    Ok(target)
}

fn try_image(
    ab: &AbDisk,
    booted: &Booted,
    image: &str,
    tries: u8,
    cmdline: Option<&str>,
) -> Result<(), String> {
    let mut control = Control::open(ab)?;
    let current = control
        .record()
        .ok_or("the control record is uninitialised; run `bootctl init` first")?;
    let target = idle(&current, booted)?;
    let part = ab.slot(target);

    let mut loaded = slotio::load(image)?;
    if let Some(words) = cmdline {
        loaded.cmdline = words.as_bytes().to_vec();
    }
    let words = String::from_utf8_lossy(&loaded.cmdline).into_owned();
    if words.split_whitespace().any(|w| w == "ferrix.root=tmpfs") {
        println!("bootctl: note: the image's command line keeps / in memory (ferrix.root=tmpfs)");
    }
    let mut slot = disk::open_rw(&part.node).map_err(|e| format!("{}: {e}", part.node))?;
    layout::check_writable(part, &mut slot)?;

    // A trial armed on the slot about to be overwritten is taken back first:
    // a slot half written must never be tried.
    let mut current = current;
    if current.trial == Some(target) {
        current = control.write(current.disarmed())?;
        println!(
            "bootctl: took back the trial armed on slot {} before writing it (record {})",
            target.letter(),
            current.sequence
        );
    }

    let generation = current.sequence + 1;
    let label = label_of(&loaded);
    println!(
        "bootctl: writing {} to slot {} ({}): kernel {} KiB, initramfs {} KiB, command line `{words}`",
        loaded.from,
        target.letter(),
        part.node,
        loaded.kernel.len() >> 10,
        loaded.initrd.len() >> 10
    );
    let written = slotio::write(&mut slot, 0, part.bytes(), &loaded, generation, &label)
        .map_err(|why| format!("slot {} ({}): {why}", target.letter(), part.node))?;
    drop(slot);
    let back = verify(ab, target)?;
    if back != written {
        return Err(format!(
            "slot {} verifies but is not what was written; nothing armed",
            target.letter()
        ));
    }
    println!(
        "bootctl: slot {} read back and verified ({} MiB)",
        target.letter(),
        back.needed() >> 20
    );

    let armed = control.write(current.armed(target, tries))?;
    println!(
        "bootctl: slot {} armed for {tries} trial boot{} (record {}); slot {} stays good. \
         Restart to try it, then `bootctl good`.",
        target.letter(),
        if tries == 1 { "" } else { "s" },
        armed.sequence,
        armed.good.letter()
    );
    Ok(())
}

/// The slot's label: the image's source, cut to fit.
fn label_of(image: &slotio::Image) -> String {
    let base = image
        .from
        .rsplit(['/', ' '])
        .next()
        .unwrap_or("image")
        .trim_end_matches(')');
    let mut label: String = format!("ferrix {base}").chars().take(31).collect();
    label.retain(|c| c.is_ascii_graphic() || c == ' ');
    label
}

fn good(ab: &AbDisk, booted: &Booted) -> Result<(), String> {
    let mut control = Control::open(ab)?;
    let r = control
        .record()
        .ok_or("the control record is uninitialised")?;
    let Some(slot) = booted.slot else {
        return Err(
            "no ferrix.slot= on the command line: this boot did not come from a slot".into(),
        );
    };
    if !booted.is_trial() {
        if slot == r.good {
            println!("bootctl: slot {} is already good", slot.letter());
            return Ok(());
        }
        return Err(format!(
            "slot {} was not booted as a trial (ferrix.slot.boot={}); nothing changed",
            slot.letter(),
            booted.how.as_deref().unwrap_or("?")
        ));
    }
    if r.trial != Some(slot) {
        return Err(format!(
            "the record's trial is {}, not slot {}; nothing changed",
            r.trial.map_or("none", Slot::letter),
            slot.letter()
        ));
    }
    if booted.sequence != Some(r.sequence) {
        return Err(format!(
            "this boot acted on record {}, but the record is now {}: the slot was armed again \
             since; nothing changed",
            booted
                .sequence
                .map_or_else(|| "?".to_owned(), |s| s.to_string()),
            r.sequence
        ));
    }
    let next = control.write(r.marked_good())?;
    println!(
        "bootctl: slot {} is good (record {}); slot {} is idle",
        slot.letter(),
        next.sequence,
        slot.other().letter()
    );
    Ok(())
}

fn rollback(ab: &AbDisk, booted: &Booted) -> Result<(), String> {
    let mut control = Control::open(ab)?;
    let r = control
        .record()
        .ok_or("the control record is uninitialised")?;
    let Some(trial) = r.trial else {
        println!(
            "bootctl: nothing on trial; slot {} is good",
            r.good.letter()
        );
        return Ok(());
    };
    // Rollback takes a trial back and writes nothing else: it never makes
    // a slot good (docs/AB-BOOT.md §5; the consultant's S4).
    let next = control.write(r.disarmed())?;
    let running = booted.slot == Some(trial);
    println!(
        "bootctl: the trial of slot {} is taken back (record {}); {}",
        trial.letter(),
        next.sequence,
        if running {
            format!("the next boot is slot {}", r.good.letter())
        } else {
            format!("slot {} stays good", r.good.letter())
        }
    );
    Ok(())
}

fn init(ab: &AbDisk) -> Result<(), String> {
    let mut control = Control::open(ab)?;
    if let Some(r) = control.record() {
        return Err(format!(
            "the control record is already written (sequence {}, good {}); init only writes a \
             first one",
            r.sequence,
            r.good.letter()
        ));
    }
    let good = match (verify(ab, Slot::A), verify(ab, Slot::B)) {
        (Ok(_), _) => Slot::A,
        (Err(_), Ok(_)) => Slot::B,
        (Err(a), Err(b)) => return Err(format!("neither slot verifies: {a}; {b}")),
    };
    let first = control.write(Some(Record::initial(good)))?;
    println!(
        "bootctl: the first control record is written: slot {} is good (record {})",
        good.letter(),
        first.sequence
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(good: Slot, trial: Option<Slot>) -> Record {
        Record {
            sequence: 5,
            good,
            trial,
            tries: u8::from(trial.is_some()),
        }
    }

    #[test]
    fn the_idle_slot_is_never_the_running_one() {
        let good_a = Booted::parse("ferrix.slot=a ferrix.slot.boot=good ferrix.slot.seq=5");
        assert_eq!(idle(&record(Slot::A, None), &good_a), Ok(Slot::B));
        // Armed but not booted yet: B is rewritten (after being disarmed).
        assert_eq!(idle(&record(Slot::A, Some(Slot::B)), &good_a), Ok(Slot::B));
        // Running B on trial: the idle slot would be the good one, A.
        let trial_b = Booted::parse("ferrix.slot=b ferrix.slot.boot=trial ferrix.slot.seq=5");
        assert!(idle(&record(Slot::A, Some(Slot::B)), &trial_b).is_err());
        // Booted A as a fallback: the good slot B is the idle one.
        let fallback = Booted::parse("ferrix.slot=a ferrix.slot.boot=fallback");
        assert!(idle(&record(Slot::B, None), &fallback).is_err());
        // Not booted from a slot: the one not good.
        let none = Booted::parse("ferrix.root=btrfs");
        assert_eq!(idle(&record(Slot::B, None), &none), Ok(Slot::A));
    }

    #[test]
    fn a_rescue_boot_changes_nothing() {
        let rescue = Booted::parse("ferrix.root=btrfs");
        assert!(rescue.slot.is_none());
        for command in [Command::Good, Command::Rollback] {
            assert!(command.changes_what_boots());
        }
        assert!(!Command::Status.changes_what_boots());
        assert!(!Command::Init.changes_what_boots());
        assert!(!Command::Verify(Slot::A).changes_what_boots());
    }

    #[test]
    fn the_boot_disk_is_named_by_its_guid() {
        let printed = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
        let guid = parse_guid(printed).unwrap();
        assert_eq!(guid, ferrix_partition::ESP);
        assert_eq!(guid.to_string(), printed);
        assert_eq!(parse_guid(&printed.to_lowercase()), Some(guid));
        assert_eq!(parse_guid("C12A7328F81F11D2BA4B00A0C93EC93B"), None);
        let booted = Booted::parse(&format!("ferrix.slot=a ferrix.slot.disk={printed}"));
        assert_eq!(booted.disk, Some(guid));
    }

    #[test]
    fn the_loaders_words_come_first() {
        let booted =
            Booted::parse("ferrix.slot=a ferrix.slot.boot=good ferrix.slot.seq=3 ferrix.slot=b");
        assert_eq!(booted.slot, Some(Slot::A));
        assert_eq!(booted.sequence, Some(3));
    }
}
