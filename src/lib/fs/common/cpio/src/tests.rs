//! Tests for the newc reader.
//!
//! Archives are built here rather than checked in as blobs, so that a failure
//! names the header field that is wrong instead of pointing at a hexdump, and
//! so the malformed cases can be produced by changing one field of an archive
//! that is otherwise known good. The builder deliberately computes padding the
//! long way — by looking at how many bytes it has already emitted — because
//! that is the property the reader is being tested against.

extern crate std;

use std::format;
use std::vec;
use std::vec::Vec;

use super::*;

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// One entry to serialise.
///
/// The `namesize` and `filesize` overrides exist so a test can write a header
/// that disagrees with the bytes that follow it, which is the shape every
/// interesting rejection takes.
#[derive(Clone, Debug)]
struct Spec {
    name: Vec<u8>,
    mode: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u32,
    dev_major: u32,
    dev_minor: u32,
    rdev_major: u32,
    rdev_minor: u32,
    check: u32,
    data: Vec<u8>,
    magic: Option<&'static [u8]>,
    namesize: Option<u32>,
    filesize: Option<u32>,
    terminate: bool,
}

impl Spec {
    fn raw(name: &[u8], mode: u32, data: &[u8]) -> Spec {
        Spec {
            name: name.to_vec(),
            mode,
            ino: 1,
            uid: 0,
            gid: 0,
            nlink: 1,
            mtime: 0x6000_0000,
            dev_major: 3,
            dev_minor: 1,
            rdev_major: 0,
            rdev_minor: 0,
            check: 0,
            data: data.to_vec(),
            magic: None,
            namesize: None,
            filesize: None,
            terminate: true,
        }
    }

    fn ino(mut self, ino: u32) -> Spec {
        self.ino = ino;
        self
    }

    fn nlink(mut self, nlink: u32) -> Spec {
        self.nlink = nlink;
        self
    }

    fn magic(mut self, magic: &'static [u8]) -> Spec {
        self.magic = Some(magic);
        self
    }

    fn namesize(mut self, namesize: u32) -> Spec {
        self.namesize = Some(namesize);
        self
    }

    fn filesize(mut self, filesize: u32) -> Spec {
        self.filesize = Some(filesize);
        self
    }

    fn without_nul(mut self) -> Spec {
        self.terminate = false;
        self
    }
}

/// A regular file.
fn file(name: &str, mode: u32, data: &[u8]) -> Spec {
    Spec::raw(name.as_bytes(), S_IFREG | mode, data)
}

/// A directory.
fn dir(name: &str, mode: u32) -> Spec {
    Spec::raw(name.as_bytes(), S_IFDIR | mode, &[])
}

/// A symbolic link, whose target is carried as the file data.
fn symlink(name: &str, target: &str) -> Spec {
    Spec::raw(name.as_bytes(), S_IFLNK | 0o777, target.as_bytes())
}

/// A character device node.
fn chardev(name: &str, mode: u32, major: u32, minor: u32) -> Spec {
    let mut spec = Spec::raw(name.as_bytes(), S_IFCHR | mode, &[]);
    spec.rdev_major = major;
    spec.rdev_minor = minor;
    spec
}

/// An archive under construction.
#[derive(Debug)]
struct Builder {
    bytes: Vec<u8>,
    magic: &'static [u8],
    upper: bool,
}

impl Builder {
    fn new() -> Builder {
        Builder {
            bytes: Vec::new(),
            magic: MAGIC_NEWC,
            upper: false,
        }
    }

    fn with_magic(magic: &'static [u8]) -> Builder {
        let mut builder = Builder::new();
        builder.magic = magic;
        builder
    }

    fn uppercase(mut self) -> Builder {
        self.upper = true;
        self
    }

    /// Offset the next entry will be written at, which is also the offset of
    /// the trailer once every entry has been pushed.
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn number(&mut self, value: u32) {
        let text = if self.upper {
            format!("{value:08X}")
        } else {
            format!("{value:08x}")
        };
        self.bytes.extend_from_slice(text.as_bytes());
    }

    /// Zero-fill up to the next 4-byte boundary of the archive.
    fn pad(&mut self) {
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
    }

    fn push(&mut self, spec: &Spec) {
        let stated_name = u32::try_from(spec.name.len()).unwrap() + u32::from(spec.terminate);
        let namesize = spec.namesize.unwrap_or(stated_name);
        let filesize = spec
            .filesize
            .unwrap_or(u32::try_from(spec.data.len()).unwrap());

        self.bytes
            .extend_from_slice(spec.magic.unwrap_or(self.magic));
        for value in [
            spec.ino,
            spec.mode,
            spec.uid,
            spec.gid,
            spec.nlink,
            spec.mtime,
            filesize,
            spec.dev_major,
            spec.dev_minor,
            spec.rdev_major,
            spec.rdev_minor,
            namesize,
            spec.check,
        ] {
            self.number(value);
        }

        self.bytes.extend_from_slice(&spec.name);
        if spec.terminate {
            self.bytes.push(0);
        }
        self.pad();
        self.bytes.extend_from_slice(&spec.data);
        self.pad();
    }

    fn finish(mut self) -> Vec<u8> {
        let trailer = Spec::raw(TRAILER_NAME.as_bytes(), 0, &[]);
        self.push(&trailer);
        self.bytes
    }

    fn finish_without_trailer(self) -> Vec<u8> {
        self.bytes
    }
}

/// The archive most tests work from: an initramfs that could actually boot.
fn realistic_archive() -> Vec<u8> {
    let mut builder = Builder::new();
    builder.push(&dir(".", 0o755));
    builder.push(&file("init", 0o755, b"#!/bin/sh\nexec /bin/sh\n"));
    builder.push(&dir("dev", 0o755));
    builder.push(&chardev("dev/console", 0o600, 5, 1));
    builder.push(&dir("bin", 0o755));
    builder.push(&file(
        "bin/sh",
        0o755,
        &[0x7F, b'E', b'L', b'F', 2, 1, 1, 0],
    ));
    builder.push(&symlink("bin/init", "../init"));
    builder.finish()
}

/// Collect every entry, failing the test on the first error.
fn entries(archive: &[u8]) -> Vec<Entry<'_>> {
    Archive::new(archive)
        .entries()
        .map(|entry| entry.unwrap())
        .collect()
}

/// The error an archive is rejected with, failing the test if it parses.
fn error(archive: &[u8]) -> CpioError {
    for entry in Archive::new(archive).entries() {
        if let Err(error) = entry {
            return error;
        }
    }
    panic!("archive parsed cleanly but was expected to be rejected");
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[test]
fn header_layout_places_every_field_where_the_format_says() {
    let mut spec = Spec::raw(b"f", S_IFREG | 0o640, b"data").magic(MAGIC_CRC);
    spec.ino = 0x0000_1234;
    spec.uid = 0x1000;
    spec.gid = 0x1001;
    spec.nlink = 7;
    spec.mtime = 0x5F5E_0FF0;
    spec.dev_major = 0xDE;
    spec.dev_minor = 0xAD;
    spec.rdev_major = 0xBE;
    spec.rdev_minor = 0xEF;
    spec.check = 0x00C0_FFEE;

    let mut builder = Builder::with_magic(MAGIC_CRC);
    builder.push(&spec);
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(parsed.len(), 1, "the trailer is not an entry");
    let entry = parsed[0];
    assert_eq!(
        entry.ino, 0x0000_1234,
        "c_ino decoded from the wrong offset"
    );
    assert_eq!(entry.mode, S_IFREG | 0o640, "c_mode decoded wrongly");
    assert_eq!(entry.uid, 0x1000, "c_uid decoded wrongly");
    assert_eq!(entry.gid, 0x1001, "c_gid decoded wrongly");
    assert_eq!(entry.nlink, 7, "c_nlink decoded wrongly");
    assert_eq!(entry.mtime, 0x5F5E_0FF0, "c_mtime decoded wrongly");
    assert_eq!(entry.dev_major, 0xDE, "c_devmajor decoded wrongly");
    assert_eq!(entry.dev_minor, 0xAD, "c_devminor decoded wrongly");
    assert_eq!(entry.rdev_major, 0xBE, "c_rdevmajor decoded wrongly");
    assert_eq!(entry.rdev_minor, 0xEF, "c_rdevminor decoded wrongly");
    assert_eq!(entry.check, 0x00C0_FFEE, "c_check decoded wrongly");
    assert_eq!(
        entry.name, "f",
        "c_namesize or the name bytes decoded wrongly"
    );
    assert_eq!(entry.data, b"data", "c_filesize decoded wrongly");
    assert_eq!(entry.header_offset, 0, "the first header is at offset zero");
}

#[test]
fn realistic_initramfs_parses() {
    let archive = realistic_archive();
    let parsed = entries(&archive);

    let names: Vec<&str> = parsed.iter().map(|entry| entry.name).collect();
    assert_eq!(
        names,
        vec![
            ".",
            "init",
            "dev",
            "dev/console",
            "bin",
            "bin/sh",
            "bin/init"
        ],
        "entries come back in archive order with the trailer removed"
    );

    assert!(parsed[0].is_dir(), "the root entry is a directory");
    assert_eq!(
        parsed[1].data, b"#!/bin/sh\nexec /bin/sh\n",
        "/init keeps its contents"
    );
    assert_eq!(
        parsed[1].permissions(),
        0o755,
        "/init keeps its executable bits"
    );
    assert_eq!(
        parsed[3].file_type(),
        FileType::CharDevice,
        "/dev/console is a character device"
    );
    assert_eq!(
        parsed[3].rdev(),
        Some((5, 1)),
        "/dev/console carries its device numbers"
    );
    assert_eq!(
        parsed[5].data.len(),
        8,
        "/bin/sh keeps every byte of its contents"
    );
    assert_eq!(
        parsed[6].symlink_target(),
        Some("../init"),
        "a symlink's target is its file data"
    );
}

#[test]
fn trailer_is_not_reported() {
    let archive = realistic_archive();
    assert!(
        entries(&archive).iter().all(|e| e.name != TRAILER_NAME),
        "the trailer entry must never reach the caller"
    );
}

#[test]
fn bytes_after_the_trailer_are_ignored() {
    let mut archive = realistic_archive();
    let entries_before = entries(&archive).len();
    // Real archives are padded out to a block boundary after the trailer.
    archive.extend_from_slice(&[0u8; 512]);
    assert_eq!(
        entries(&archive).len(),
        entries_before,
        "block padding after the trailer is not an entry"
    );
}

#[test]
fn padding_covers_all_four_name_alignments() {
    // A header is 110 bytes, so name lengths of 1 to 4 place the file data at
    // each of the four possible distances from the 4-byte boundary.
    let mut builder = Builder::new();
    for length in 1..=4usize {
        let name: Vec<u8> = vec![b'a'; length];
        let data: Vec<u8> = (0..16u8).map(|byte| byte + length as u8).collect();
        builder.push(&Spec::raw(&name, S_IFREG | 0o644, &data));
    }
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(parsed.len(), 4, "one entry per name alignment");
    for (index, entry) in parsed.iter().enumerate() {
        let length = index + 1;
        let expected: Vec<u8> = (0..16u8).map(|byte| byte + length as u8).collect();
        assert_eq!(entry.name.len(), length, "name length round-trips");
        assert_eq!(
            entry.data,
            &expected[..],
            "data of a {length}-byte name is misaligned by the name padding"
        );
    }
}

#[test]
fn padding_covers_all_four_data_alignments() {
    let mut builder = Builder::new();
    for length in 4..=7usize {
        let name = format!("f{length}");
        let data: Vec<u8> = (0..length as u8).map(|byte| byte | 0x40).collect();
        builder.push(&file(&name, 0o644, &data));
    }
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(parsed.len(), 4, "one entry per data alignment");
    for (index, entry) in parsed.iter().enumerate() {
        let length = index + 4;
        let expected: Vec<u8> = (0..length as u8).map(|byte| byte | 0x40).collect();
        assert_eq!(
            entry.data,
            &expected[..],
            "a {length}-byte file leaves the following header misaligned"
        );
        assert_eq!(
            entry.header_offset % 4,
            0,
            "every header after the first starts on a 4-byte boundary"
        );
    }
}

#[test]
fn zero_length_file_has_empty_data() {
    let mut builder = Builder::new();
    builder.push(&file("empty", 0o644, &[]));
    builder.push(&file("after", 0o644, b"still here"));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert!(parsed[0].data.is_empty(), "a zero-length file has no data");
    assert_eq!(
        parsed[1].data, b"still here",
        "a zero-length file does not shift the entry after it"
    );
}

#[test]
fn file_size_multiple_of_four_needs_no_padding() {
    let mut builder = Builder::new();
    builder.push(&file("aligned", 0o644, b"12345678"));
    builder.push(&file("next", 0o644, b"ok"));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(
        parsed[0].data, b"12345678",
        "an aligned file keeps its data"
    );
    assert_eq!(
        parsed[1].data, b"ok",
        "no padding is inserted after a file whose size is a multiple of four"
    );
}

#[test]
fn hard_links_share_an_inode() {
    let mut builder = Builder::new();
    builder.push(&file("bin/busybox", 0o755, &[]).ino(42).nlink(2));
    builder.push(&file("bin/sh", 0o755, b"ELF").ino(42).nlink(2));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(
        parsed[0].ino, parsed[1].ino,
        "hard links are expressed as entries sharing c_ino"
    );
    assert_eq!(parsed[0].nlink, 2, "c_nlink counts the names of the file");
    assert!(
        parsed[0].data.is_empty(),
        "every hard link but the last carries no data"
    );
    assert_eq!(
        parsed[1].data, b"ELF",
        "the last link to a file carries the contents"
    );
}

// ---------------------------------------------------------------------------
// Magics and hex
// ---------------------------------------------------------------------------

#[test]
fn plain_magic_accepted() {
    let mut builder = Builder::with_magic(MAGIC_NEWC);
    builder.push(&file("a", 0o644, b"x"));
    let archive = builder.finish();
    assert_eq!(entries(&archive).len(), 1, "070701 is a newc archive");
}

#[test]
fn crc_magic_accepted_and_check_reported() {
    let mut spec = file("a", 0o644, b"x").magic(MAGIC_CRC);
    spec.check = 0x0000_0078;
    let mut builder = Builder::with_magic(MAGIC_CRC);
    builder.push(&spec);
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(parsed.len(), 1, "070702 is also a newc archive");
    assert_eq!(
        parsed[0].check, 0x78,
        "c_check is reported for the CRC format"
    );
}

#[test]
fn uppercase_hex_accepted() {
    let mut builder = Builder::new().uppercase();
    builder.push(&file("upper", 0o644, b"abcd"));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(parsed.len(), 1, "uppercase hex headers are still headers");
    assert_eq!(
        parsed[0].data, b"abcd",
        "uppercase digits decode to the same value"
    );
}

#[test]
fn non_hex_field_rejected_naming_the_field() {
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x"));
    let mut archive = builder.finish();
    archive[OFF_FILESIZE + 3] = b'g';

    assert_eq!(
        error(&archive),
        CpioError::BadHexField {
            offset: 0,
            field: Field::FileSize
        },
        "a non-hex character must be rejected, not read as zero"
    );
}

#[test]
fn non_hex_in_any_field_is_rejected() {
    let cases = [
        (OFF_INO, Field::Ino),
        (OFF_MODE, Field::Mode),
        (OFF_UID, Field::Uid),
        (OFF_GID, Field::Gid),
        (OFF_NLINK, Field::Nlink),
        (OFF_MTIME, Field::Mtime),
        (OFF_FILESIZE, Field::FileSize),
        (OFF_DEVMAJOR, Field::DevMajor),
        (OFF_DEVMINOR, Field::DevMinor),
        (OFF_RDEVMAJOR, Field::RdevMajor),
        (OFF_RDEVMINOR, Field::RdevMinor),
        (OFF_NAMESIZE, Field::NameSize),
        (OFF_CHECK, Field::Check),
    ];
    for (offset, field) in cases {
        let mut builder = Builder::new();
        builder.push(&file("a", 0o644, b"x"));
        let mut archive = builder.finish();
        archive[offset] = b' ';
        assert_eq!(
            error(&archive),
            CpioError::BadHexField { offset: 0, field },
            "a space in {field} must be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// Rejection
// ---------------------------------------------------------------------------

#[test]
fn bad_magic_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x"));
    let mut archive = builder.finish();
    archive[3] = b'9';

    assert_eq!(
        error(&archive),
        CpioError::BadMagic { offset: 0 },
        "070901 is not a format this reader knows"
    );
}

#[test]
fn old_binary_magic_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x"));
    let mut archive = builder.finish();
    archive[0..6].copy_from_slice(b"070707");

    assert_eq!(
        error(&archive),
        CpioError::BadMagic { offset: 0 },
        "the old portable format has a different header and must not be parsed as newc"
    );
}

#[test]
fn truncated_header_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("init", 0o755, b"hello"));
    let trailer_offset = builder.len();
    let archive = builder.finish();
    let cut = &archive[..trailer_offset + 50];

    assert_eq!(
        error(cut),
        CpioError::TruncatedHeader {
            offset: trailer_offset
        },
        "half a header is not a header"
    );
}

#[test]
fn truncated_name_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("init", 0o755, b"hello"));
    let trailer_offset = builder.len();
    let archive = builder.finish();
    // The header is whole but only part of the 11-byte trailer name is there.
    let cut = &archive[..trailer_offset + HEADER_SIZE + 5];

    assert_eq!(
        error(cut),
        CpioError::TruncatedName {
            offset: trailer_offset
        },
        "a name that runs past the archive must be rejected"
    );
}

#[test]
fn name_without_nul_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("noterm", 0o644, b"x").without_nul());
    let archive = builder.finish();

    assert_eq!(
        error(&archive),
        CpioError::UnterminatedName { offset: 0 },
        "c_namesize counts the NUL, so the last byte has to be one"
    );
}

#[test]
fn interior_nul_in_name_rejected() {
    let mut builder = Builder::new();
    builder.push(&Spec::raw(b"ab\0cd", S_IFREG | 0o644, b"x"));
    let archive = builder.finish();

    assert_eq!(
        error(&archive),
        CpioError::InteriorNul { offset: 0 },
        "a name that ends early at a C interface is not the name that was checked"
    );
}

#[test]
fn non_utf8_name_rejected() {
    let mut builder = Builder::new();
    builder.push(&Spec::raw(&[b'b', b'a', 0xFF, b'd'], S_IFREG | 0o644, b"x"));
    let archive = builder.finish();

    assert_eq!(
        error(&archive),
        CpioError::NameNotUtf8 { offset: 0 },
        "a name that is not UTF-8 is an error, never a panic"
    );
}

#[test]
fn file_size_past_end_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("big", 0o644, b"tiny").filesize(0x0010_0000));
    let archive = builder.finish();

    assert_eq!(
        error(&archive),
        CpioError::TruncatedData { offset: 0 },
        "a file cannot be longer than the archive holding it"
    );
}

#[test]
fn file_size_that_overflows_an_offset_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("big", 0o644, b"tiny").filesize(0xFFFF_FFFF));
    let archive = builder.finish();

    let found = error(&archive);
    assert!(
        matches!(
            found,
            CpioError::TruncatedData { .. } | CpioError::OffsetOverflow { .. }
        ),
        "a 4 GiB file size must be refused rather than wrapped, got {found:?}"
    );
}

#[test]
fn missing_trailer_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("init", 0o755, b"hello"));
    builder.push(&dir("bin", 0o755));
    let archive = builder.finish_without_trailer();

    assert_eq!(
        error(&archive),
        CpioError::MissingTrailer,
        "an archive that just stops is a prefix, not an archive"
    );
}

#[test]
fn empty_archive_rejected() {
    assert_eq!(
        error(&[]),
        CpioError::MissingTrailer,
        "zero bytes cannot be a complete archive"
    );
}

#[test]
fn zero_name_size_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x").namesize(0));
    let archive = builder.finish();

    assert_eq!(
        error(&archive),
        CpioError::EmptyName { offset: 0 },
        "a name of zero bytes has no room even for its terminator"
    );
}

#[test]
fn absurd_name_size_rejected() {
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x").namesize(0xFFFF_FFFF));
    let archive = builder.finish();

    assert_eq!(
        error(&archive),
        CpioError::NameTooLong {
            offset: 0,
            size: 0xFFFF_FFFF
        },
        "a 4 GiB name is a corrupt header, and must not be reported as truncation"
    );
}

#[test]
fn name_size_just_over_the_limit_rejected() {
    let size = MAX_NAME_SIZE + 1;
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x").namesize(size));
    let archive = builder.finish();

    assert_eq!(
        error(&archive),
        CpioError::NameTooLong { offset: 0, size },
        "the name limit is enforced at its boundary"
    );
}

#[test]
fn iteration_stops_after_an_error() {
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x"));
    builder.push(&file("b", 0o644, b"y"));
    let mut archive = builder.finish();
    archive[0] = b'X';

    let mut iterator = Archive::new(&archive).entries();
    assert!(
        matches!(
            iterator.next(),
            Some(Err(CpioError::BadMagic { offset: 0 }))
        ),
        "the first entry is rejected"
    );
    assert!(
        iterator.next().is_none(),
        "an archive cannot be resynchronised after a bad header"
    );
}

#[test]
fn error_display_names_the_field_and_the_offset() {
    let text = format!(
        "{}",
        CpioError::BadHexField {
            offset: 220,
            field: Field::FileSize,
        }
    );
    assert!(
        text.contains("c_filesize") && text.contains("220"),
        "the message must identify the entry and the field, got {text}"
    );
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

#[test]
fn file_type_decodes_every_mode() {
    let cases = [
        (S_IFREG, FileType::Regular),
        (S_IFDIR, FileType::Directory),
        (S_IFLNK, FileType::Symlink),
        (S_IFCHR, FileType::CharDevice),
        (S_IFBLK, FileType::BlockDevice),
        (S_IFIFO, FileType::Fifo),
        (S_IFSOCK, FileType::Socket),
    ];
    for (bits, expected) in cases {
        assert_eq!(
            FileType::from_mode(bits | 0o644),
            expected,
            "mode {bits:o} decodes to the wrong type"
        );
    }
    assert_eq!(
        FileType::from_mode(0o030000),
        FileType::Unknown(0o030000),
        "an unknown type is reported by number rather than guessed at"
    );
}

#[test]
fn permissions_mask_off_the_type() {
    let mut builder = Builder::new();
    builder.push(&file("setuid", 0o4755, &[]));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(
        parsed[0].permissions(),
        0o4755,
        "the set-id bits are part of the permissions"
    );
    assert!(parsed[0].is_file(), "the type bits still say regular file");
}

#[test]
fn rdev_is_reported_only_for_device_nodes() {
    let mut builder = Builder::new();
    builder.push(&chardev("dev/null", 0o666, 1, 3));
    builder.push(&file("regular", 0o644, b"x"));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(
        parsed[0].rdev(),
        Some((1, 3)),
        "a device node reports its numbers"
    );
    assert_eq!(
        parsed[1].rdev(),
        None,
        "the zeros a regular file carries must not read as device 0:0"
    );
}

#[test]
fn symlink_target_is_only_for_symlinks() {
    let mut builder = Builder::new();
    builder.push(&symlink("link", "target/path"));
    builder.push(&file("plain", 0o644, b"target/path"));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert_eq!(
        parsed[0].symlink_target(),
        Some("target/path"),
        "a symlink's target is its data"
    );
    assert_eq!(
        parsed[1].symlink_target(),
        None,
        "a regular file has no link target however its data reads"
    );
}

#[test]
fn find_locates_an_entry_by_name() {
    let archive = realistic_archive();
    let found = Archive::new(&archive).find("bin/sh").unwrap();
    assert!(found.is_some(), "bin/sh is in the archive");
    assert_eq!(
        Archive::new(&archive).find("bin/bash").unwrap(),
        None,
        "a name that is not there is absence, not an error"
    );
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

#[test]
fn summary_totals_the_archive() {
    let archive = realistic_archive();
    let summary = Archive::new(&archive).summary().unwrap();

    assert_eq!(summary.entries, 7, "the trailer is not counted");
    assert_eq!(summary.directories, 3, "., dev and bin are directories");
    assert_eq!(summary.regular_files, 2, "init and bin/sh are files");
    assert_eq!(summary.symlinks, 1, "bin/init is a symlink");
    assert_eq!(
        summary.data_bytes,
        23 + 8 + 7,
        "data_bytes totals the file, program and link-target bytes"
    );
    assert_eq!(
        summary.largest_file, 23,
        "the largest member is /init's script"
    );
    let names: u64 = [
        ".",
        "init",
        "dev",
        "dev/console",
        "bin",
        "bin/sh",
        "bin/init",
    ]
    .iter()
    .map(|name| name.len() as u64 + 1)
    .sum();
    assert_eq!(
        summary.name_bytes, names,
        "name_bytes counts every name and its terminator"
    );
}

#[test]
fn summary_counts_hard_linked_data_once() {
    let mut builder = Builder::new();
    builder.push(&file("busybox", 0o755, &[]).ino(9).nlink(2));
    builder.push(&file("sh", 0o755, b"1234567890").ino(9).nlink(2));
    let archive = builder.finish();
    let summary = Archive::new(&archive).summary().unwrap();

    assert_eq!(summary.entries, 2, "both names are entries");
    assert_eq!(
        summary.data_bytes, 10,
        "a hard-linked file's contents are stored once"
    );
}

#[test]
fn summary_rejects_what_iteration_rejects() {
    let mut builder = Builder::new();
    builder.push(&file("a", 0o644, b"x"));
    let archive = builder.finish_without_trailer();

    assert_eq!(
        Archive::new(&archive).summary(),
        Err(CpioError::MissingTrailer),
        "sizing the ram filesystem must fail on the archive that unpacking would fail on"
    );
}

#[test]
fn summary_of_an_archive_with_only_a_trailer_is_empty() {
    let archive = Builder::new().finish();
    let summary = Archive::new(&archive).summary().unwrap();

    assert_eq!(
        summary,
        Summary::default(),
        "an empty archive totals to zero"
    );
}

#[test]
fn many_entries_parse() {
    let count = 5000;
    let mut builder = Builder::new();
    for index in 0..count {
        builder.push(&file(&format!("f{index:05}"), 0o644, b"ab"));
    }
    let archive = builder.finish();
    let summary = Archive::new(&archive).summary().unwrap();

    assert!(
        count < MAX_ENTRIES,
        "the fixture stays under the entry bound"
    );
    assert_eq!(summary.entries, count, "every entry is walked");
    assert_eq!(
        summary.data_bytes,
        2 * count as u64,
        "every file is counted"
    );
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

#[test]
fn is_safe_path_accepts_relative_names() {
    for name in ["bin/sh", "init", ".", "./init", "a.b/c-d_e", "dev/console"] {
        assert!(
            is_safe_path(name),
            "{name} stays inside the unpack root and must be accepted"
        );
    }
}

#[test]
fn is_safe_path_rejects_escapes() {
    for name in [
        "/bin/sh",
        "../etc",
        "a/../../b",
        "..",
        "../",
        "a/..",
        "/",
        "",
        "a\0b",
    ] {
        assert!(
            !is_safe_path(name),
            "{name:?} can leave the unpack root and must be rejected"
        );
    }
}

#[test]
fn entry_path_safety_follows_the_name() {
    let mut builder = Builder::new();
    builder.push(&file("etc/passwd", 0o644, b"root"));
    builder.push(&file("../../etc/passwd", 0o644, b"root"));
    let archive = builder.finish();
    let parsed = entries(&archive);

    assert!(parsed[0].is_safe_path(), "a relative name unpacks in place");
    assert!(
        !parsed[1].is_safe_path(),
        "an escaping name parses fine and must be refused by the caller"
    );
}

// ---------------------------------------------------------------------------
// Totality
// ---------------------------------------------------------------------------

#[test]
fn corrupting_any_byte_never_panics() {
    let good = realistic_archive();
    let limit = core::cmp::min(good.len(), 1024);
    let patches = [0x00u8, 0x01, 0x2F, 0x41, 0x7A, 0xFF];
    let mut cases = 0usize;

    for position in 0..limit {
        for patch in patches {
            let mut bytes = good.clone();
            bytes[position] = patch;

            let archive = Archive::new(&bytes);
            let mut sink = 0u64;
            for entry in archive.entries() {
                match entry {
                    Ok(entry) => {
                        sink = sink
                            .wrapping_add(entry.ino.into())
                            .wrapping_add(entry.mode.into())
                            .wrapping_add(entry.uid.into())
                            .wrapping_add(entry.gid.into())
                            .wrapping_add(entry.nlink.into())
                            .wrapping_add(entry.mtime.into())
                            .wrapping_add(entry.dev_major.into())
                            .wrapping_add(entry.dev_minor.into())
                            .wrapping_add(entry.rdev_major.into())
                            .wrapping_add(entry.rdev_minor.into())
                            .wrapping_add(entry.check.into())
                            .wrapping_add(entry.name.len() as u64)
                            .wrapping_add(entry.data.len() as u64)
                            .wrapping_add(entry.header_offset as u64)
                            .wrapping_add(u64::from(entry.is_safe_path()))
                            .wrapping_add(u64::from(entry.rdev().is_some()))
                            .wrapping_add(u64::from(entry.symlink_target().is_some()))
                            .wrapping_add(match entry.file_type() {
                                FileType::Unknown(bits) => u64::from(bits),
                                _ => 1,
                            });
                    }
                    Err(error) => {
                        sink = sink.wrapping_add(format!("{error}").len() as u64);
                    }
                }
            }
            let _ = archive.summary();
            let _ = sink;
            cases += 1;
        }
    }

    assert_eq!(
        cases,
        limit * patches.len(),
        "every single-byte corruption was walked to completion without a panic"
    );
}

#[test]
fn truncating_at_any_length_never_panics() {
    let good = realistic_archive();
    let mut cases = 0usize;

    for length in 0..good.len() {
        let archive = Archive::new(&good[..length]);
        for entry in archive.entries() {
            let _ = entry.map(|entry| entry.data.len());
        }
        let _ = archive.summary();
        cases += 1;
    }

    assert_eq!(
        cases,
        good.len(),
        "every prefix of the archive was walked to completion without a panic"
    );
}
