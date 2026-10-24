//! The reader's side of tombstones and blobs (`docs/LOGGING.md` §21):
//! `lode tombstones`, `lode tombstone SEQ|CURSOR`, `lode blob ID`, and the
//! blob check `lode verify` runs. The references are resolved here, so a
//! person never handles a path.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::blob::{self, BlobId};
use ferrix_lode_core::cursor;
use ferrix_lode_core::format::{self, Output};
use ferrix_lode_core::record::Record;
use ferrix_lode_core::tombstone;

use crate::cli::{self, Options, complain, out, usage};
use crate::{blobs, sys};

/// The store directories as descriptors, and every record of the boot
/// `options` names (this one unless `-b` says).
fn records(words: &[String]) -> Result<(Vec<usize>, Vec<Record>), i32> {
    let mut options: Options = cli::parse(words).map_err(|why| usage(&why))?;
    let _ = options.boot.get_or_insert_with(String::new);
    let source = cli::directories(&options).map_err(|why| {
        complain(&alloc::format!("lode: {why}"));
        1
    })?;
    let segments = cli::known(&source);
    let boot = options
        .boot
        .as_ref()
        .and_then(|word| cli::chosen_boot(word, &cli::boots(&segments)).ok());
    let mut records = cli::load(&segments, boot);
    records.sort_by_key(|record| record.seqnum);
    cli::release(&segments);
    Ok((source.dirs.iter().map(|dir| dir.fd).collect(), records))
}

/// `lode tombstones [-b ...]`: a line a tombstone, newest last.
pub(crate) fn list(words: &[String]) -> i32 {
    let (_, records) = match records(words) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let zone = crate::zone::reader();
    let mut text = Vec::new();
    for record in records
        .iter()
        .filter(|record| tombstone::is_tombstone(record))
    {
        text.extend_from_slice(alloc::format!("{:>10} ", record.seqnum).as_bytes());
        format::write(record, Output::Short, &zone, &mut text);
    }
    out(&text);
    0
}

/// `lode tombstone SEQ|CURSOR [-b ...]`: the tombstone whole -- its fields,
/// the crash record, the process's last lines, the screenshot's and the
/// heavy parts' references with whether each is there.
pub(crate) fn show(words: &[String]) -> i32 {
    let Some(which) = words.first() else {
        return usage("tombstone SEQ|CURSOR");
    };
    let Some(seqnum) = which.parse::<u64>().ok().or_else(|| cursor::seqnum(which)) else {
        return usage("tombstone SEQ|CURSOR");
    };
    let (stores, records) = match records(words.get(1..).unwrap_or_default()) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let Some(stone) = records
        .iter()
        .find(|record| record.seqnum == seqnum && tombstone::is_tombstone(record))
    else {
        complain(&alloc::format!(
            "lode: no tombstone at {seqnum} in this boot"
        ));
        return 1;
    };
    let zone = crate::zone::reader();
    let mut text = Vec::new();
    format::write(stone, Output::Verbose, &zone, &mut text);
    let at = |field: &str| stone.get_str(field).and_then(cursor::seqnum);
    let pid = stone.get_str("TOMBSTONE_PID").map(String::from);
    if let Some(crash) = at("TOMBSTONE_OF").and_then(|seq| records.iter().find(|r| r.seqnum == seq))
    {
        text.extend_from_slice(b"\n-- the crash\n");
        format::write(crash, Output::Short, &zone, &mut text);
        for capture in records.iter().filter(|record| {
            record.get_str("CAPTURE_OF") == Some(&*alloc::format!("{}", crash.seqnum))
        }) {
            text.extend_from_slice(b"-- its screenshot\n");
            format::write(capture, Output::Short, &zone, &mut text);
            describe_blobs(capture, &stores, &mut text);
        }
    }
    if let (Some(first), Some(last)) = (at("TOMBSTONE_TAIL_FIRST"), at("TOMBSTONE_TAIL_LAST")) {
        text.extend_from_slice(b"\n-- its last lines\n");
        for record in records.iter().filter(|record| {
            record.seqnum >= first
                && record.seqnum <= last
                && record.get_str("_PID") == pid.as_deref()
        }) {
            format::write(record, Output::Short, &zone, &mut text);
        }
    }
    describe_blobs(stone, &stores, &mut text);
    out(&text);
    0
}

/// A line for each blob `record` names: its part, size, type, and whether
/// the container has it.
fn describe_blobs(record: &Record, stores: &[usize], text: &mut Vec<u8>) {
    for (name, reference) in blob::references(record) {
        let there = blobs::find(stores, &reference.id).is_some();
        text.extend_from_slice(
            alloc::format!(
                "   {name}: {} bytes of {}, blob {}{}\n",
                reference.size,
                reference.kind,
                blob::hex(&reference.id),
                if there {
                    ""
                } else {
                    " (expired, or never written)"
                }
            )
            .as_bytes(),
        );
    }
    if let Some(dropped) = record.get_str("BLOB_DROPPED") {
        text.extend_from_slice(alloc::format!("   blob dropped: {dropped}\n").as_bytes());
    }
}

/// `lode blob ID [-o FILE]`: a blob's bytes, to standard output or FILE.
pub(crate) fn blob(words: &[String]) -> i32 {
    let Some(id) = words.first().and_then(|word| blob::parse_id(word)) else {
        return usage("blob ID [-o FILE]: ID is 64 hex digits");
    };
    let target = match (words.get(1).map(String::as_str), words.get(2)) {
        (Some("-o"), Some(path)) => Some(path.clone()),
        (None, _) => None,
        _ => return usage("blob ID [-o FILE]"),
    };
    let options = Options::default();
    let source = match cli::directories(&options) {
        Ok(source) => source,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let stores: Vec<usize> = source.dirs.iter().map(|dir| dir.fd).collect();
    let Some((_, bytes)) = blobs::find(&stores, &id) else {
        complain(&alloc::format!(
            "lode: blob {}: not in the container",
            blob::hex(&id)
        ));
        return 1;
    };
    match target {
        None => {
            let _ = sys::write_all(1, &bytes);
        }
        Some(path) => {
            let written = sys::open(
                path.as_bytes(),
                sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
            )
            .and_then(|fd| {
                let result = sys::write_all(fd, &bytes);
                sys::close(fd);
                result
            });
            if let Err(error) = written {
                complain(&alloc::format!("lode: {path}: errno {}", error.0));
                return 1;
            }
        }
    }
    0
}

/// `lode verify`'s blob check: every reference against the container,
/// and blobs no record names. Lines to print and whether all is well: a
/// pack is damaged when a blob in it does not read back to its id. A
/// named blob that is gone is said, not a failure: the quota evicts.
pub(crate) fn verify(stores: &[usize], records: &[Record]) -> (Vec<u8>, bool) {
    let present: Vec<(BlobId, String, blob::Entry)> = blobs::all(stores);
    let have: BTreeSet<BlobId> = present.iter().map(|(id, _, _)| *id).collect();
    let mut named = BTreeSet::new();
    let mut missing = 0_usize;
    let mut text = Vec::new();
    for record in records {
        for (name, reference) in blob::references(record) {
            let _ = named.insert(reference.id);
            if !have.contains(&reference.id) {
                missing += 1;
                text.extend_from_slice(
                    alloc::format!(
                        "record {}: {name} names blob {}, which is not in the container \
                         (expired by the quota, or never written)\n",
                        record.seqnum,
                        blob::hex(&reference.id)
                    )
                    .as_bytes(),
                );
            }
        }
    }
    let orphans = have.difference(&named).count();
    let damaged = present
        .iter()
        .filter(|(id, _, _)| blobs::find(stores, id).is_none())
        .count();
    if damaged > 0 {
        text.extend_from_slice(
            alloc::format!("blobs: {damaged} do not read back to their ids\n").as_bytes(),
        );
    }
    text.extend_from_slice(
        alloc::format!(
            "blobs: {} in the container, {} named, {missing} missing, {orphans} named by no record \
             (swept after an hour)\n",
            have.len(),
            named.len()
        )
        .as_bytes(),
    );
    (text, damaged == 0)
}
