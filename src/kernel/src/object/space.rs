//! An address space named by a handle (`docs/microkernel/K2.md`).
//!
//! A handle to an [`AddressSpace`] lets its holder map, unmap and protect in
//! it, and copy out of it, as far as the handle's rights go
//! (`Rights::SPACE`). It holds the space alive, as a process does, and
//! nothing else: no process, no thread, no handle table.
//!
//! # Who pays
//!
//! Every handle object records the space's **owner**: the quota slot of the
//! job that pays for what a by-handle call commits in the space (a page a
//! `space_read` faults in, the page tables that fault builds). For a space
//! `space_create` made, the caller's job; for a process's space that
//! `process_space` gave, the process's job at the grant. The slot is held for
//! as long as the object lives, so the index stays valid after the job goes,
//! as a frame's does. The caller of a by-handle call never pays for the
//! target's memory (the K2 review, ledger 2845 (3)).

use alloc::sync::Arc;

use crate::fallible::{self, AllocError};
use crate::object::quota::{self, Charge, Resource};
use crate::user::space::AddressSpace;

/// What a space handle names.
#[derive(Debug)]
pub(crate) struct SpaceRef {
    /// The space.
    space: Arc<AddressSpace>,
    /// The job that pays for what by-handle calls commit there, held for as
    /// long as this lives: see the module.
    owner: u32,
    /// The object itself, charged to the job that made it, or nothing for a
    /// handle to a space that already existed. Holds `owner` too.
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the job"
    )]
    charge: Charge,
}

impl SpaceRef {
    /// A new, empty space, charged as one object to the running task's job,
    /// which owns it.
    ///
    /// # Errors
    ///
    /// [`AllocError`] for no memory, no root, or a job at its object limit.
    pub(crate) fn create() -> Result<Arc<SpaceRef>, AllocError> {
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| AllocError)?;
        let owner = crate::sched::running_group();
        let space = AddressSpace::new().map_err(|_| AllocError)?;
        fallible::try_arc(SpaceRef {
            space,
            owner,
            charge,
        })
    }

    /// A handle to `space`, which already exists, owned by `owner`, the
    /// slot of the job of the process whose space it is.
    ///
    /// # Errors
    ///
    /// [`AllocError`].
    pub(crate) fn of(space: Arc<AddressSpace>, owner: u32) -> Result<Arc<SpaceRef>, AllocError> {
        // Nothing charged, and the owner's slot held all the same.
        let charge = Charge::to(owner, Resource::Objects, 0).map_err(|_| AllocError)?;
        fallible::try_arc(SpaceRef {
            space,
            owner,
            charge,
        })
    }

    /// A copy-on-write clone of this space (`AddressSpace::fork`), charged as
    /// one object to the running task's job, which owns it.
    ///
    /// # Errors
    ///
    /// [`AllocError`] for no memory, or a job at its object limit.
    pub(crate) fn clone_space(&self) -> Result<Arc<SpaceRef>, AllocError> {
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| AllocError)?;
        let owner = crate::sched::running_group();
        let space = self.space.fork().map_err(|_| AllocError)?;
        fallible::try_arc(SpaceRef {
            space,
            owner,
            charge,
        })
    }

    /// The space.
    pub(crate) fn space(&self) -> &Arc<AddressSpace> {
        &self.space
    }

    /// Run `work`, the target-side half of a by-handle call, with every frame
    /// and table it commits charged to the space's owner rather than to the
    /// caller.
    pub(crate) fn charging_owner<R>(&self, work: impl FnOnce() -> R) -> R {
        quota::charging_to(self.owner, work)
    }
}
