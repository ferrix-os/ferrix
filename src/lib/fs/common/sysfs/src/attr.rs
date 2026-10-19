//! The files that hold one value, each as Linux's `show` function prints it.
//!
//! Every one ends in a newline, because every Linux attribute does and
//! programs read them with `fscanf("%x")` or `getline`, which a missing
//! newline does not break but a missing value does.

use alloc::vec::Vec;

use crate::text::put;

/// A sixteen-bit identifier: a PCI vendor, device or subsystem, as
/// `vendor_show` prints it, `0x1af4`.
pub fn hex16(out: &mut Vec<u8>, value: u16) {
    put(out, format_args!("0x{value:04x}\n"));
}

/// An eight-bit identifier: a PCI revision, `0x01`.
pub fn hex8(out: &mut Vec<u8>, value: u8) {
    put(out, format_args!("0x{value:02x}\n"));
}

/// A PCI class code, all three bytes: `0x038000`.
pub fn class(out: &mut Vec<u8>, class: u32) {
    put(out, format_args!("0x{:06x}\n", class & 0x00ff_ffff));
}

/// A number with no sign, `%u` or `%llu`.
pub fn decimal(out: &mut Vec<u8>, value: u64) {
    put(out, format_args!("{value}\n"));
}

/// A number that may be negative, `%d`.
pub fn signed(out: &mut Vec<u8>, value: i64) {
    put(out, format_args!("{value}\n"));
}

/// A number in hexadecimal as C's `%#x` prints it: `0x1003`, and plain `0`
/// for zero, which the `#` flag leaves without its prefix.
pub fn alternate_hex(out: &mut Vec<u8>, value: u64) {
    if value == 0 {
        out.extend_from_slice(b"0\n");
    } else {
        put(out, format_args!("{value:#x}\n"));
    }
}

/// A string, as `%s\n` prints it: the bytes, then the newline.
pub fn line(out: &mut Vec<u8>, text: &[u8]) {
    out.extend_from_slice(text);
    out.push(b'\n');
}

/// A device number, as `print_dev_t` writes the `dev` file: `226:0`.
pub fn dev(out: &mut Vec<u8>, major: u32, minor: u32) {
    put(out, format_args!("{major}:{minor}\n"));
}

/// A set of processors as a list of ranges, `%*pbl`: `0-3`, `0,2-3,5`, and
/// an empty line for none. `cpus` must be ascending.
pub fn cpu_list(out: &mut Vec<u8>, cpus: &[u32]) {
    let mut rest = cpus.iter().copied().peekable();
    let mut first = true;
    while let Some(start) = rest.next() {
        let mut end = start;
        while rest.peek() == Some(&end.saturating_add(1)) && end < u32::MAX {
            end += 1;
            let _ = rest.next();
        }
        if !first {
            out.push(b',');
        }
        first = false;
        if start == end {
            put(out, format_args!("{start}"));
        } else {
            put(out, format_args!("{start}-{end}"));
        }
    }
    out.push(b'\n');
}

/// A hardware address as `%pM` prints it, lower case and colon-separated:
/// `52:54:00:12:34:56`.
pub fn hardware_address(out: &mut Vec<u8>, address: &[u8]) {
    for (index, byte) in address.iter().enumerate() {
        if index > 0 {
            out.push(b':');
        }
        put(out, format_args!("{byte:02x}"));
    }
    out.push(b'\n');
}

/// A truth value as `%d`: `1` or `0`.
pub fn flag(out: &mut Vec<u8>, value: bool) {
    out.extend_from_slice(if value { b"1\n" } else { b"0\n" });
}

/// A disk's size in the 512-byte sectors its `size` file counts, whatever
/// its own sector size, as `part_size_show` does. A size past the counter
/// saturates rather than wraps.
#[must_use]
pub const fn size_in_512_byte_sectors(sectors: u64, sector_size: u32) -> u64 {
    let bytes = sectors.saturating_mul(sector_size as u64);
    bytes / 512
}
