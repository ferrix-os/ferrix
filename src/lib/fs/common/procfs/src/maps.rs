//! `/proc/<pid>/maps`.
//!
//! ```text
//! 608f06057000-608f06099000 rw-p 00000000 00:00 0                          [heap]
//! 608ed5aa4000-608ed5aa5000 rw-p 00000000 00:00 0
//! ```
//!
//! What `show_map_vma` in Linux's `fs/proc/task_mmu.c` prints: the range as
//! hexadecimal of at least eight digits, four permission letters, the file
//! offset as at least eight hex digits, the device as two hex numbers of at
//! least two digits, the inode in decimal, and a space. Then, only if the
//! region has a name, padding to a fixed column, one more space, and the name.
//! A region with no name ends in that space after the inode — the second line
//! above has one — which a program splitting on whitespace never notices and
//! a program comparing lines does.
//!
//! # The column
//!
//! `seq_setwidth(m, 25 + sizeof(void *) * 6 - 1)` before the line and
//! `seq_pad(m, ' ')` before the name: padding to byte 72 and a space on a
//! 64-bit kernel, so the name starts at byte 73, and 48 and 49 on a 32-bit one.
//! It is the pointer width of the *kernel*, not of the addresses — a 64-bit
//! kernel pads a program mapped at `0x400000` to the same column — which is
//! why [`Width`] is an argument rather than something inferred from the line.

use alloc::vec::Vec;

use crate::text::{pad_to, put};

/// The kernel's pointer width, which decides the name column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// A 32-bit kernel: names start at byte 49.
    Bits32,
    /// A 64-bit kernel: names start at byte 73.
    Bits64,
}

impl Width {
    /// The pointer width of the code calling this, which in the kernel is the
    /// kernel's.
    #[must_use]
    pub const fn native() -> Width {
        if size_of::<usize>() == 8 {
            Width::Bits64
        } else {
            Width::Bits32
        }
    }

    /// `seq_setwidth`'s argument: where padding stops.
    const fn pad_until(self) -> usize {
        let pointer = match self {
            Width::Bits32 => 4,
            Width::Bits64 => 8,
        };
        25 + pointer * 6 - 1
    }
}

/// One line: a region of an address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the four permission letters are four independent flags, printed one each"
)]
pub struct Mapping<'a> {
    /// First address.
    pub start: u64,
    /// First address past the end.
    pub end: u64,
    /// `r`.
    pub read: bool,
    /// `w`.
    pub write: bool,
    /// `x`.
    pub execute: bool,
    /// `s` rather than `p`: `MAP_SHARED`.
    pub shared: bool,
    /// Byte offset into the file, zero for anonymous memory.
    pub offset: u64,
    /// The file's device major number.
    pub major: u32,
    /// The file's device minor number.
    pub minor: u32,
    /// The file's inode number, zero for anonymous memory.
    pub inode: u64,
    /// A path, or a bracketed name such as `[heap]`; `None` prints nothing.
    pub name: Option<&'a [u8]>,
}

/// Append one line, newline included.
pub fn render(out: &mut Vec<u8>, mapping: &Mapping<'_>, width: Width) {
    let line_start = out.len();
    put(
        out,
        format_args!("{:08x}-{:08x} ", mapping.start, mapping.end),
    );
    out.extend_from_slice(&[
        letter(mapping.read, b'r'),
        letter(mapping.write, b'w'),
        letter(mapping.execute, b'x'),
        if mapping.shared { b's' } else { b'p' },
    ]);
    put(
        out,
        format_args!(
            " {:08x} {:02x}:{:02x} {} ",
            mapping.offset, mapping.major, mapping.minor, mapping.inode
        ),
    );
    if let Some(name) = mapping.name {
        pad_to(out, line_start, width.pad_until());
        out.push(b' ');
        // `seq_path(m, path, "\n")`: a newline in a file name would split the
        // line, so it is the one byte escaped, as a backslash and three octal
        // digits.
        for &byte in name {
            if byte == b'\n' {
                out.extend_from_slice(b"\\012");
            } else {
                out.push(byte);
            }
        }
    }
    out.push(b'\n');
}

/// A permission letter, or `-`.
const fn letter(set: bool, letter: u8) -> u8 {
    if set { letter } else { b'-' }
}

/// Read one line back, with or without its newline.
///
/// Strict: anything [`render`] could not have produced is `None`, because the
/// parser's use is to check the renderer's output rather than to be lenient
/// about somebody else's. The name is returned as printed, escapes and all.
#[must_use]
pub fn parse(line: &[u8]) -> Option<Mapping<'_>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let mut fields = line.splitn(6, |&byte| byte == b' ');
    let (range, perms, offset, device, inode) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    let rest = fields.next()?;

    let mut ends = range.splitn(2, |&byte| byte == b'-');
    let start = hex(ends.next()?, 8)?;
    let end = hex(ends.next()?, 8)?;
    let mut numbers = device.splitn(2, |&byte| byte == b':');
    let major = u32::try_from(hex(numbers.next()?, 2)?).ok()?;
    let minor = u32::try_from(hex(numbers.next()?, 2)?).ok()?;

    let &[read, write, execute, share] = perms else {
        return None;
    };
    let name = rest.iter().position(|&byte| byte != b' ');
    Some(Mapping {
        start,
        end,
        read: flag(read, b'r')?,
        write: flag(write, b'w')?,
        execute: flag(execute, b'x')?,
        shared: match share {
            b's' => true,
            b'p' => false,
            _ => return None,
        },
        offset: hex(offset, 8)?,
        major,
        minor,
        inode: decimal(inode)?,
        name: name.and_then(|at| rest.get(at..)),
    })
}

/// A permission letter read back.
fn flag(byte: u8, letter: u8) -> Option<bool> {
    match byte {
        b'-' => Some(false),
        _ if byte == letter => Some(true),
        _ => None,
    }
}

/// Lower-case hexadecimal of at least `digits` digits, as `seq_put_hex_ll`
/// prints it.
fn hex(field: &[u8], digits: usize) -> Option<u64> {
    if field.len() < digits || field.len() > 16 {
        return None;
    }
    field.iter().try_fold(0_u64, |value, &byte| {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => return None,
        };
        Some((value << 4) | u64::from(digit))
    })
}

/// Decimal digits, and nothing else.
fn decimal(field: &[u8]) -> Option<u64> {
    if field.is_empty() {
        return None;
    }
    field.iter().try_fold(0_u64, |value, &byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))
    })
}
