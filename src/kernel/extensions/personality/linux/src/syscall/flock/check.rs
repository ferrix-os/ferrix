//! Record locks charged to the job that sets them (certification finding
//! F-37), for `fs::kmem_check`: the lock table is `flock`'s own, so the
//! paths into it that the check drives are here.

use alloc::sync::Arc;

use ferrix_vfs::{Errno, OpenFile};

use super::{OFFSET_MAX, Owner, RECORDS, carve, key_of, place, record_charge};

/// Take the one-byte write lock at `2 * at` of `file`, as an open file
/// description lock: every other byte, so no two merge and each is a record
/// of its own.
///
/// # Errors
///
/// `ENOLCK` past the running task's job's memory limit, as `F_OFD_SETLK`
/// answers it, and `EAGAIN` for a range someone else holds.
pub(crate) fn lock_byte(file: &Arc<OpenFile>, at: usize) -> Result<(), Errno> {
    let owner = Owner::Description(Arc::downgrade(file));
    let byte = u64::try_from(at)
        .map_err(|_| Errno::EINVAL)?
        .saturating_mul(2);
    let mut charge = Some(record_charge()?);
    let mut spare = Some(record_charge()?);
    if place(
        key_of(file),
        &owner,
        true,
        byte,
        byte,
        &mut charge,
        &mut spare,
    ) {
        Ok(())
    } else {
        Err(Errno::EAGAIN)
    }
}

/// Give up every lock `file` holds, as an unlock of the whole file does.
pub(crate) fn unlock_all(file: &Arc<OpenFile>) {
    let owner = Owner::Description(Arc::downgrade(file));
    let key = key_of(file);
    let mut spare = None;
    carve(&mut RECORDS.lock(), key, &owner, 0, OFFSET_MAX, &mut spare);
}
