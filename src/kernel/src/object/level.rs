//! Criticality levels between jobs (`docs/microkernel/K17.md`): the one door a
//! handle passes as it lands in another job's table.
//!
//! A level is data on a job, fixed as the job is made. Between jobs of one level
//! nothing here refuses anything, and everything on a kernel whose jobs never
//! asked for a level is at one level.

use ferrix_native_abi::rights::Rights;

use crate::object::Object;

/// A handle refused between levels: the levels, for the audit record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    /// The holder's level.
    pub holder: u8,
    /// The object's.
    pub object: u8,
}

/// Whether a job at level `holder` may hold `object` with `rights`: rules (a) and
/// (c), a handle to a higher job's processes, threads, jobs, spaces, region,
/// exception or pager; and rule (b)'s half, a write right to a VMO a higher job maps. O(1).
///
/// # Errors
///
/// [`Refused`]. A VMO's own marks are lowered only by a landing that is allowed.
pub fn may_hold(holder: u8, object: &Object, rights: Rights) -> Result<(), Refused> {
    if let Object::Vmo(vmo) = object {
        if rights.contains(Rights::WRITE) && !vmo.may_write_from(holder) {
            return Err(Refused {
                holder,
                object: vmo.highest_mapper(),
            });
        }
        return Ok(());
    }
    let level = match object {
        Object::Process(_)
        | Object::Thread(_)
        | Object::Job(_)
        | Object::Space(_)
        | Object::Region(_)
        | Object::Pager(_)
        | Object::PagerKeeper(_)
        | Object::Exception(_) => level_of(object),
        _ => return Ok(()),
    };
    if level > holder {
        return Err(Refused {
            holder,
            object: level,
        });
    }
    Ok(())
}

/// `object`'s level, for the record of a refusal: the top for what has none.
pub fn level_of(object: &Object) -> u8 {
    match object {
        Object::Process(process) => process.exit().level(),
        Object::Thread(thread) => thread.level(),
        Object::Job(job) => job.level(),
        Object::Space(space) => space.level(),
        Object::Region(region) => region.job().level(),
        Object::Exception(exception) => exception.level(),
        Object::Pager(pager) => pager.level(),
        Object::PagerKeeper(keeper) => keeper.level(),
        Object::Vmo(vmo) => vmo.highest_mapper(),
        _ => ferrix_native_abi::types::JOB_LEVEL_TOP,
    }
}
