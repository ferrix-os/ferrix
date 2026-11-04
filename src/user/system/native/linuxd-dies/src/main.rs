//! A partition's `linuxd` that dies at its start: `cargo xtask test-partitions`'s,
//! so that the row shows a partition's death rather than an orderly end.

#![no_std]
#![no_main]

use ferrix_rt::Bootstrap;

ferrix_rt::entry!(main);

fn main(_bootstrap: Bootstrap) -> i32 {
    86
}
