//! What `ferrix-install` and `bootctl` share: reading a disk's GUID
//! partition table, and the A/B layout of `docs/DEV-ON-FERRIX.md` §4.4 (D8)
//! -- which partitions are the two system slots and the control record, and
//! the checks that keep every other partition unwritten.

pub mod disk;
pub mod fat;
pub mod layout;
pub mod record;
pub mod slotio;

#[cfg(test)]
mod tests;
