//! Fuzz the newc reader an initramfs is unpacked from.
//!
//! The kernel walks this archive in ring 0 before any disk driver exists,
//! with `overflow-checks` on, over bytes a build on some other machine
//! produced. The reader promises to borrow the archive, copy nothing, and
//! answer every input with an entry or an error.
//!
//! # The properties
//!
//! Not panicking is the floor. Beyond it:
//!
//! 1. **Everything an entry borrows lies inside the archive, where the format
//!    puts it**: the name right after its 110-byte header and followed by its
//!    NUL, the data at the next four-byte boundary, and the next header at the
//!    boundary after the data.
//! 2. **The walk ends**, after at most one entry per header's worth of bytes,
//!    yields an error at most once and only last, and never yields the trailer.
//! 3. **The summary is the walk**: it succeeds exactly when the walk meets no
//!    error, fails with the same error when it does, and its totals are the
//!    walk's.
//! 4. **`find` returns the first entry of a name**, as the walk saw it.
//! 5. **A prefix never disagrees**: the archive cut short yields a prefix of
//!    the same entries and then stops, and never a different entry.
//! 6. **A round trip**: every archive that walks to its trailer, written out
//!    again with the fields the reader reported, reads back as exactly the
//!    same entries at the same offsets.
//! 7. **`file_named` is the walk's first regular file of a safe name** (L.object.335),
//!    ring 0's one read of the archive under `personality=user`.

#![no_main]

use ferrix_cpio::{
    Archive, CpioError, Entry, FileType, HEADER_SIZE, MAGIC_CRC, MAX_NAME_SIZE, Summary,
    TRAILER_NAME,
};
use libfuzzer_sys::fuzz_target;

/// Where `inner` starts in `outer`, if it lies wholly inside it.
///
/// An empty slice is inside wherever it points, since reading none of it reads
/// nothing out of bounds.
fn offset_in(outer: &[u8], inner: &[u8]) -> Option<usize> {
    let outer_range = outer.as_ptr_range();
    let inner_range = inner.as_ptr_range();
    if inner.is_empty() {
        return Some(0);
    }
    if inner_range.start < outer_range.start || inner_range.end > outer_range.end {
        return None;
    }
    Some(inner_range.start as usize - outer_range.start as usize)
}

/// Round up to the format's four-byte alignment.
fn align4(value: usize) -> usize {
    (value + 3) & !3
}

/// Property 1 for one entry; returns where the next header must start.
fn check_layout(bytes: &[u8], entry: &Entry<'_>) -> usize {
    let header = entry.header_offset;
    assert!(
        header.is_multiple_of(4),
        "a header at {header} is not aligned"
    );
    let name_at = header + HEADER_SIZE;
    let name_end = name_at + entry.name.len();
    if !entry.name.is_empty() {
        assert_eq!(
            offset_in(bytes, entry.name.as_bytes()),
            Some(name_at),
            "the name of the entry at {header} is not where its header ends"
        );
    }
    assert_eq!(bytes.get(name_end), Some(&0), "the name is not terminated");
    assert!(!entry.name.as_bytes().contains(&0), "a name kept a NUL");
    assert!(
        entry.name.len() < MAX_NAME_SIZE as usize,
        "a name past the limit"
    );
    assert_ne!(entry.name, TRAILER_NAME, "the trailer was yielded");

    let data_at = align4(name_end + 1);
    let data_end = data_at + entry.data.len();
    assert!(data_end <= bytes.len(), "data runs past the archive");
    if !entry.data.is_empty() {
        assert_eq!(
            offset_in(bytes, entry.data),
            Some(data_at),
            "the data of the entry at {header} is not after its name"
        );
    }
    assert_eq!(
        entry.symlink_target().is_some(),
        entry.file_type() == FileType::Symlink && core::str::from_utf8(entry.data).is_ok(),
    );
    align4(data_end)
}

/// Write `entries` out as a newc archive, with a trailer.
fn encode(bytes: &[u8], entries: &[Entry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        // Keep each entry's own magic, so a `070702` archive stays one.
        let magic = bytes
            .get(entry.header_offset..entry.header_offset + 6)
            .expect("a yielded entry has its header");
        push_entry(&mut out, magic, entry);
    }
    let trailer = Entry {
        name: TRAILER_NAME,
        ino: 0,
        mode: 0,
        uid: 0,
        gid: 0,
        nlink: 1,
        mtime: 0,
        dev_major: 0,
        dev_minor: 0,
        rdev_major: 0,
        rdev_minor: 0,
        check: 0,
        data: &[],
        header_offset: out.len(),
    };
    push_entry(&mut out, b"070701", &trailer);
    out
}

/// Append one entry, header, name and data, each padded as the format says.
fn push_entry(out: &mut Vec<u8>, magic: &[u8], entry: &Entry<'_>) {
    let fields = [
        entry.ino,
        entry.mode,
        entry.uid,
        entry.gid,
        entry.nlink,
        entry.mtime,
        u32::try_from(entry.data.len()).expect("the data length came from a u32"),
        entry.dev_major,
        entry.dev_minor,
        entry.rdev_major,
        entry.rdev_minor,
        u32::try_from(entry.name.len() + 1).expect("the name is below the limit"),
        entry.check,
    ];
    out.extend_from_slice(magic);
    for field in fields {
        out.extend_from_slice(format!("{field:08X}").as_bytes());
    }
    out.extend_from_slice(entry.name.as_bytes());
    out.push(0);
    out.resize(align4(out.len()), 0);
    out.extend_from_slice(entry.data);
    out.resize(align4(out.len()), 0);
}

/// Property 3: the totals a summary must report for `entries`.
fn expected_summary(entries: &[Entry<'_>]) -> Summary {
    let mut summary = Summary::default();
    for entry in entries {
        let size = entry.data.len() as u64;
        summary.entries += 1;
        summary.data_bytes += size;
        summary.name_bytes += entry.name.len() as u64 + 1;
        summary.largest_file = summary.largest_file.max(size);
        match entry.file_type() {
            FileType::Directory => summary.directories += 1,
            FileType::Regular => summary.regular_files += 1,
            FileType::Symlink => summary.symlinks += 1,
            _ => {}
        }
    }
    summary
}

/// What property 7 holds `file_named` to: a name without any run of leading `./` and `/`,
/// and `.` itself as the empty name.
fn normalised(name: &str) -> &str {
    let mut name = name;
    while let Some(rest) = name.strip_prefix("./").or_else(|| name.strip_prefix('/')) {
        name = rest;
    }
    if name == "." { "" } else { name }
}

/// The entries of `bytes`, and the error the walk ended on, if any.
fn walk(bytes: &[u8]) -> (Vec<Entry<'_>>, Option<CpioError>) {
    let archive = Archive::new(bytes);
    let mut iterator = archive.entries();
    let bound = iterator.size_hint().1.expect("the hint has an upper bound");
    let mut entries = Vec::new();
    let mut error = None;
    for item in iterator.by_ref() {
        assert!(error.is_none(), "an entry followed an error");
        match item {
            Ok(entry) => entries.push(entry),
            Err(failure) => error = Some(failure),
        }
    }
    assert!(iterator.next().is_none(), "the walk resumed after it ended");
    assert!(
        entries.len() <= bound && entries.len() <= bytes.len() / HEADER_SIZE,
        "{} entries out of {} bytes",
        entries.len(),
        bytes.len()
    );
    (entries, error)
}

fuzz_target!(|bytes: &[u8]| {
    let archive = Archive::new(bytes);
    let (entries, error) = walk(bytes);

    // 7. For each name the walk saw, and for one it did not.
    for entry in &entries {
        let name = normalised(entry.name);
        let wanted = entries
            .iter()
            .find(|other| other.is_safe_path() && other.is_file() && normalised(other.name) == name)
            .map(|found| found.data);
        assert_eq!(ferrix_cpio::file_named(bytes, name.as_bytes()), wanted);
    }
    assert_eq!(ferrix_cpio::file_named(bytes, b"\0not a name"), None);

    // 1. Layout, entry by entry, and each next header where the last one said.
    let mut expected_header = 0;
    for entry in &entries {
        assert_eq!(
            entry.header_offset, expected_header,
            "an entry started somewhere its predecessor did not end"
        );
        expected_header = check_layout(bytes, entry);
    }

    // 3. The summary is the walk.
    match (archive.summary(), error) {
        (Ok(summary), None) => assert_eq!(summary, expected_summary(&entries)),
        (Err(summarised), Some(walked)) => assert_eq!(summarised, walked),
        (summary, walked) => panic!("summary {summary:?} but the walk ended in {walked:?}"),
    }

    // 4. `find` answers with the first entry of each name.
    for (index, entry) in entries.iter().enumerate().take(64) {
        if entries[..index]
            .iter()
            .any(|earlier| earlier.name == entry.name)
        {
            continue;
        }
        assert_eq!(archive.find(entry.name), Ok(Some(*entry)));
    }

    // 5. A prefix never disagrees, cut at a point the input chooses.
    if let Some(&last) = bytes.last() {
        let cut = bytes.len() * usize::from(last) / 256;
        let (prefix, prefix_error) = walk(&bytes[..cut]);
        assert!(prefix.len() <= entries.len());
        assert_eq!(
            prefix[..],
            entries[..prefix.len()],
            "a prefix read differently"
        );
        // A prefix that reached a trailer reached the whole archive's: the
        // full walk passes the same bytes on the way.
        if prefix_error.is_none() {
            assert_eq!(error, None, "a prefix read to a trailer the whole did not");
            assert_eq!(prefix.len(), entries.len());
        }
    }

    // 6. Written out again, the archive reads back the same, magic and all.
    if error.is_none() {
        let written = encode(bytes, &entries);
        let (reread, reread_error) = walk(&written);
        assert_eq!(reread_error, None, "a rewritten archive failed to read");
        assert_eq!(reread, entries, "a rewritten archive read back differently");
        for entry in &entries {
            let magic = |archive: &[u8]| {
                archive.get(entry.header_offset..entry.header_offset + 6) == Some(MAGIC_CRC)
            };
            assert_eq!(magic(bytes), magic(&written), "an entry changed its magic");
        }
    }
});
