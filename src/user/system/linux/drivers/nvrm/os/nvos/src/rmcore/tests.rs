//! Every refusal of [`super::check`] and [`super::maps_check`], each on a
//! core built here that differs from a good one in that one way.

use super::{BASE, END, MAGIC, Refusal, VERSION, build_id_in, check, maps_check};
use crate::sha256::digest;

const ID: [u8; 20] = [7; 20];
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_GNU_STACK: u32 = 0x6474_e551;
const R: u32 = 4;
const RX: u32 = 5;
const RW: u32 = 6;
const RWX: u32 = 7;

/// A program header and the bytes it loads.
#[derive(Clone)]
struct Seg {
    kind: u32,
    flags: u32,
    address: u64,
    bytes: Vec<u8>,
    memory: u64,
}

/// A core: the ELF header, the program headers at 64, each segment's bytes
/// at its own page of the file.
fn elf(segments: &[Seg]) -> Vec<u8> {
    let mut file = vec![0_u8; 64];
    file[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
    file[16..18].copy_from_slice(&2_u16.to_le_bytes());
    file[18..20].copy_from_slice(&62_u16.to_le_bytes());
    file[32..40].copy_from_slice(&64_u64.to_le_bytes());
    file[54..56].copy_from_slice(&56_u16.to_le_bytes());
    file[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());
    for (index, segment) in segments.iter().enumerate() {
        let offset = 0x1000 * (index as u64 + 1);
        let mut header = Vec::new();
        header.extend_from_slice(&segment.kind.to_le_bytes());
        header.extend_from_slice(&segment.flags.to_le_bytes());
        header.extend_from_slice(&offset.to_le_bytes());
        header.extend_from_slice(&segment.address.to_le_bytes());
        header.extend_from_slice(&segment.address.to_le_bytes());
        header.extend_from_slice(&(segment.bytes.len() as u64).to_le_bytes());
        header.extend_from_slice(&segment.memory.to_le_bytes());
        header.extend_from_slice(&0x1000_u64.to_le_bytes());
        file.extend_from_slice(&header);
    }
    for (index, segment) in segments.iter().enumerate() {
        file.resize(0x1000 * (index + 1), 0);
        file.extend_from_slice(&segment.bytes);
    }
    file
}

/// The export header: two exports, data in read-only data and a function
/// in text, in that order.
fn header(magic: u64, version: u32, count: u32, id: &[u8; 20], entries: &[u64]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&magic.to_le_bytes());
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(id);
    bytes.extend_from_slice(&20_u32.to_le_bytes());
    for entry in entries {
        bytes.extend_from_slice(&entry.to_le_bytes());
    }
    bytes
}

const TEXT: u64 = BASE + 0x1000;
const DATA: u64 = BASE + 0x2000;

/// A good core's segments: the header and read-only data, text, data.
fn good() -> Vec<Seg> {
    let mut rodata = header(MAGIC, VERSION, 2, &ID, &[BASE + 0x100, TEXT]);
    rodata.resize(0x200, 0);
    vec![
        Seg {
            kind: PT_LOAD,
            flags: R,
            address: BASE,
            memory: 0x200,
            bytes: rodata,
        },
        Seg {
            kind: PT_LOAD,
            flags: RX,
            address: TEXT,
            memory: 0x10,
            bytes: vec![0xc3; 0x10],
        },
        Seg {
            kind: PT_LOAD,
            flags: RW,
            address: DATA,
            memory: 0x100,
            bytes: vec![1; 8],
        },
        Seg {
            kind: PT_GNU_STACK,
            flags: RW,
            address: 0,
            memory: 0,
            bytes: Vec::new(),
        },
    ]
}

/// Check `segments` built, pinned to themselves.
fn run(segments: &[Seg]) -> Result<super::Plan, Refusal> {
    let file = elf(segments);
    check(&file, &digest(&file), &ID, 2, 1)
}

#[test]
fn a_good_core_is_planned() {
    let plan = run(&good()).expect("the good core");
    assert_eq!(plan.segments().len(), 3);
    assert_eq!(plan.exports, 2);
    assert_eq!(plan.entries, 0x1000 + 40);
    assert_eq!(plan.segments()[1].address, TEXT);
}

#[test]
fn an_unset_pin_is_refused_first() {
    let file = elf(&good());
    assert_eq!(check(&file, &[0; 32], &ID, 2, 1), Err(Refusal::Unpinned));
}

#[test]
fn one_flipped_byte_fails_the_pin() {
    let mut file = elf(&good());
    let pin = digest(&file);
    file[0x2000] ^= 1;
    assert_eq!(check(&file, &pin, &ID, 2, 1), Err(Refusal::Hash));
}

#[test]
fn an_empty_file_is_refused() {
    assert_eq!(check(&[], &[1; 32], &ID, 2, 1), Err(Refusal::Size(0)));
}

#[test]
fn not_an_x86_64_executable() {
    let mut file = elf(&good());
    file[18] = 183; // EM_AARCH64
    assert_eq!(
        check(&file, &digest(&file), &ID, 2, 1),
        Err(Refusal::NotElf)
    );
    let mut file = elf(&good());
    file[16] = 3; // ET_DYN
    assert_eq!(
        check(&file, &digest(&file), &ID, 2, 1),
        Err(Refusal::NotElf)
    );
}

#[test]
fn another_program_header_or_an_executable_stack() {
    let mut segments = good();
    segments[3].kind = PT_DYNAMIC;
    assert_eq!(run(&segments), Err(Refusal::Shape));
    let mut segments = good();
    segments[3].flags = RWX;
    assert_eq!(run(&segments), Err(Refusal::Shape));
}

#[test]
fn a_writable_and_executable_segment() {
    let mut segments = good();
    segments[1].flags = RWX;
    assert_eq!(run(&segments), Err(Refusal::WriteExecute));
}

#[test]
fn a_malformed_segment() {
    let mut segments = good();
    segments[2].address = DATA + 8;
    assert_eq!(run(&segments), Err(Refusal::Segment));
    let mut segments = good();
    segments[2].memory = 4;
    assert_eq!(run(&segments), Err(Refusal::Segment));
}

#[test]
fn a_segment_past_two_gib_or_below_the_base() {
    let mut segments = good();
    segments[2].address = END;
    assert_eq!(run(&segments), Err(Refusal::Range));
    let mut segments = good();
    segments[2].address = END - 0x1000;
    segments[2].memory = 0x1001;
    assert_eq!(run(&segments), Err(Refusal::Range));
    let mut segments = good();
    segments[0].address = BASE - 0x1000;
    assert_eq!(run(&segments), Err(Refusal::Range));
}

#[test]
fn overlapping_or_unordered_segments() {
    let mut segments = good();
    segments[1].address = BASE;
    assert_eq!(run(&segments), Err(Refusal::Overlap));
    let mut segments = good();
    segments.swap(1, 2);
    assert_eq!(run(&segments), Err(Refusal::Overlap));
}

#[test]
fn the_export_header() {
    let mut segments = good();
    segments[0].bytes[0] ^= 1;
    assert_eq!(run(&segments), Err(Refusal::Magic));
    let mut segments = good();
    segments[0].bytes[8] = 2;
    assert_eq!(run(&segments), Err(Refusal::Version));
    let mut segments = good();
    segments[0].bytes[12] = 3;
    assert_eq!(run(&segments), Err(Refusal::Count));
    let mut segments = good();
    segments[0].bytes[16] ^= 1;
    assert_eq!(run(&segments), Err(Refusal::BuildId));
    let mut segments = good();
    segments[0].flags = RX;
    segments[1].flags = R;
    assert_eq!(run(&segments), Err(Refusal::Magic));
}

#[test]
fn a_core_linked_against_another_nvrm() {
    let file = elf(&good());
    assert_eq!(
        check(&file, &digest(&file), &[8; 20], 2, 1),
        Err(Refusal::BuildId)
    );
}

#[test]
fn an_export_outside_its_segment() {
    // The function in data, the data in text, and an address in no segment.
    for entries in [[BASE + 0x100, DATA], [TEXT + 4, TEXT], [END + 0x100, TEXT]] {
        let mut segments = good();
        let replaced = header(MAGIC, VERSION, 2, &ID, &entries);
        segments[0].bytes[..replaced.len()].copy_from_slice(&replaced);
        assert_eq!(run(&segments), Err(Refusal::Export), "{entries:x?}");
    }
}

fn plan() -> super::Plan {
    run(&good()).expect("the good core")
}

#[test]
fn maps_of_a_loaded_core_pass() {
    let maps = format!(
        "00400000-00500000 r-xp 00000000 00:00 0 /lib/drivers/nvrm\n\
         {BASE:08x}-{:08x} r--p 00000000 00:00 0\n\
         {TEXT:08x}-{:08x} r-xp 00000000 00:00 0\n\
         {DATA:08x}-{:08x} rw-p 00000000 00:00 0\n\
         7ffff000-7ffff800 rw-p 00000000 00:00 0 [stack]\n",
        BASE + 0x1000,
        TEXT + 0x1000,
        DATA + 0x1000,
    );
    // The stack line straddles nothing: it ends below END, but it is in the
    // core's range, so it is a stranger.
    assert_eq!(
        maps_check(maps.as_bytes(), &plan()),
        Err(Refusal::MapsProtection)
    );
    let without_stranger: String = maps
        .lines()
        .take(4)
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(maps_check(without_stranger.as_bytes(), &plan()), Ok(()));
}

#[test]
fn maps_with_a_writable_and_executable_page_are_refused() {
    let maps = format!(
        "{BASE:08x}-{:08x} r--p 0 00:00 0\n{TEXT:08x}-{:08x} rwxp 0 00:00 0\n",
        BASE + 0x1000,
        TEXT + 0x1000,
    );
    assert_eq!(
        maps_check(maps.as_bytes(), &plan()),
        Err(Refusal::MapsWriteExecute)
    );
    // The control's page, with no core mapped at all.
    let control = format!("{:08x}-{END:08x} rwxp 0 00:00 0\n", END - 0x1000);
    let empty = super::Plan { count: 0, ..plan() };
    assert_eq!(
        maps_check(control.as_bytes(), &empty),
        Err(Refusal::MapsWriteExecute)
    );
}

#[test]
fn maps_with_text_left_writable_or_a_segment_missing_are_refused() {
    let writable = format!(
        "{BASE:08x}-{:08x} r--p 0 00:00 0\n{TEXT:08x}-{:08x} rw-p 0 00:00 0\n{DATA:08x}-{:08x} rw-p 0 00:00 0\n",
        BASE + 0x1000,
        TEXT + 0x1000,
        DATA + 0x1000,
    );
    assert_eq!(
        maps_check(writable.as_bytes(), &plan()),
        Err(Refusal::MapsProtection)
    );
    let missing = format!(
        "{BASE:08x}-{:08x} r--p 0 00:00 0\n{TEXT:08x}-{:08x} r-xp 0 00:00 0\n",
        BASE + 0x1000,
        TEXT + 0x1000,
    );
    assert_eq!(
        maps_check(missing.as_bytes(), &plan()),
        Err(Refusal::MapsProtection)
    );
}

#[test]
fn a_build_id_note_is_found() {
    let mut notes = Vec::new();
    // A note of another kind first, then the build-id.
    for (kind, name, desc) in [
        (1_u32, &b"GNU\0"[..], &[0_u8; 16][..]),
        (3, b"GNU\0", &ID[..]),
    ] {
        notes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        notes.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        notes.extend_from_slice(&kind.to_le_bytes());
        notes.extend_from_slice(name);
        notes.extend_from_slice(desc);
    }
    assert_eq!(build_id_in(&notes), Some(&ID[..]));
    assert_eq!(build_id_in(&notes[..20]), None);
}
