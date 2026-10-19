//! `/proc/partitions`.
//!
//! ```text
//! major minor  #blocks  name
//!
//!    7        0          4 loop0
//! ```
//!
//! A header, a blank line, and a row per block device and per partition, as
//! `show_partition` in `block/genhd.c` writes them: `"%4d  %7d %10llu %pg\n"`,
//! the size in 1 KiB blocks. Readers find the rows with `sscanf(" %u %u %u
//! %s")` and skip every line that does not match, which is why the header can
//! be there at all.

use alloc::vec::Vec;

use crate::text::put;

/// The header, and the blank line after it.
pub const HEADER: &[u8] = b"major minor  #blocks  name\n\n";

/// One block device or partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition<'a> {
    /// The device number's major half.
    pub major: u32,
    /// The device number's minor half.
    pub minor: u32,
    /// The size in 1 KiB blocks: sectors halved.
    pub blocks: u64,
    /// The name under `/dev`.
    pub name: &'a [u8],
}

/// Append the header and a row for each of `partitions`, or nothing if there
/// are none.
///
/// `show_partition_start` prints the header in front of the first device, so
/// a machine with no block device has an empty `/proc/partitions`, and so does
/// this.
pub fn render(out: &mut Vec<u8>, partitions: &[Partition<'_>]) {
    if partitions.is_empty() {
        return;
    }
    out.extend_from_slice(HEADER);
    for partition in partitions {
        put(
            out,
            format_args!(
                "{:4}  {:7} {:10} ",
                partition.major, partition.minor, partition.blocks
            ),
        );
        out.extend_from_slice(partition.name);
        out.push(b'\n');
    }
}
