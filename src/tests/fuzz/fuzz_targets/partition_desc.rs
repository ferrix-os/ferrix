//! Fuzz the partitions description, which ring 0 reads under `personality=user`
//! (`docs/microkernel/BOOT-AUTHORITY.md`, BA.9, ledger 5896).
//!
//! Beyond not panicking: a description that parses reads back inside its bounds, what it
//! says of itself stays true under its accessors, and no first message overflows, so that
//! the kernel's refusal of what it cannot hand over is never the first to see it.

#![no_main]

use ferrix_partition_desc::{Description, MAX_BYTES, MAX_DEVICES, MAX_PARTITIONS};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(description) = Description::parse(bytes) else {
        return;
    };
    assert!(bytes.len() <= MAX_BYTES);
    assert!(description.count() <= MAX_PARTITIONS);
    let mut devices = 0;
    let mut servers = 0;
    for (index, partition) in description.partitions().enumerate() {
        assert!(description.partition(index).is_some());
        assert!(!partition.name().is_empty());
        assert!(partition.devices().count() <= MAX_DEVICES);
        devices += partition.devices().count();
        servers += usize::from(partition.role() == ferrix_partition_desc::ROLE_FILES);
    }
    assert!(description.partition(description.count()).is_none());
    assert!(servers <= 1);
    assert!(servers == 0 || devices == 0, "a file server beside a safety device");
    assert!(3 + 2 * servers + description.count() + devices <= 64, "past the first message");
});
