//! Regions: a partition's shared half (`docs/microkernel/K11.md`).
//!
//! A region holds `linuxd`'s code, data and heap (an address space, K2's
//! object) and the partition's shared handle table. Each Linux process is a
//! kernel process of a region: its own restricted space, its own private
//! handle table, its own job, and the region mapped above the restricted
//! space. Which table and which half a call reaches is decided by the mode the
//! call came from, never by an argument (`Process::with_handles`,
//! `AddressSpace::route`).
//!
//! # The end
//!
//! A shared table may hold a handle to its own region, as `linuxd`'s does, so
//! a count alone would never end one. The rule: a process and a handle each
//! hold the region; when the last process leaves, the shared table is emptied,
//! whatever is in it, which drops any region handle inside it. A handle held
//! outside then keeps an emptied region, and a process cannot be made on one
//! ([`Region::enter`]).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::fallible::{self, AllocError};
use crate::object::job::Job;
use crate::object::quota::{Charge, Resource};
use crate::object::{self, HANDLE_LIMIT, HandleTable};
use crate::sync::SpinLock;
use crate::user::space::AddressSpace;

/// A partition's shared half and shared handle table.
#[derive(Debug)]
pub struct Region {
    /// `linuxd`'s address space: the window every process of the region
    /// shares in normal mode.
    space: Arc<AddressSpace>,
    /// The job every process of the region is in, or beneath.
    job: Arc<Job>,
    /// The shared handle table: what normal mode's calls name.
    handles: SpinLock<HandleTable>,
    /// Processes of the region that have not ended, and one [`Region::enter`]
    /// is making. Under `handles`' lock.
    processes: SpinLock<u64>,
    /// Whether the last process has left: nothing more may enter.
    ended: AtomicBool,
    /// The copy window (`docs/microkernel/K1.md` §2.5): its code range's
    /// start, zero for none.
    window_start: AtomicU64,
    /// One past its end.
    window_end: AtomicU64,
    /// Where a fault in it lands.
    window_landing: AtomicU64,
    /// Itself, charged to the job that made it.
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the job"
    )]
    charge: Charge,
}

impl Region {
    /// A new region under `job`, charged as one object to it.
    ///
    /// # Errors
    ///
    /// [`AllocError`] for no memory, or a job at its object limit.
    pub fn new(job: Arc<Job>) -> Result<Arc<Region>, AllocError> {
        let charge = Charge::to(job.quota_index(), Resource::Objects, 1).map_err(|_| AllocError)?;
        let space = AddressSpace::new_region().map_err(|_| AllocError)?;
        fallible::try_arc(Region {
            space,
            job,
            handles: SpinLock::new(HandleTable::new(HANDLE_LIMIT)),
            processes: SpinLock::new(0),
            ended: AtomicBool::new(false),
            window_start: AtomicU64::new(0),
            window_end: AtomicU64::new(0),
            window_landing: AtomicU64::new(0),
            charge,
        })
    }

    /// Its space.
    pub fn space(&self) -> &Arc<AddressSpace> {
        &self.space
    }

    /// The job its processes are under.
    pub fn job(&self) -> &Arc<Job> {
        &self.job
    }

    /// The shared table.
    pub(crate) const fn table(&self) -> &SpinLock<HandleTable> {
        &self.handles
    }

    /// Do something with the shared table, under its lock. What `change` takes
    /// out it hands back, for the reason `Process::with_handles` gives.
    pub fn with_handles<R>(&self, change: impl FnOnce(&mut HandleTable) -> R) -> R {
        change(&mut self.handles.lock())
    }

    /// Count a process in, or refuse: an ended region takes no more.
    ///
    /// # Errors
    ///
    /// [`Ended`] once the last process has left.
    pub fn enter(&self) -> Result<(), Ended> {
        let mut processes = self.processes.lock();
        if self.ended.load(Ordering::Acquire) {
            return Err(Ended);
        }
        *processes += 1;
        Ok(())
    }

    /// A process it counted has ended. The last one empties the shared table:
    /// the module's rule. Runs with interrupts open and no lock held.
    pub fn leave(&self) {
        let mut processes = self.processes.lock();
        *processes = processes.saturating_sub(1);
        let last = *processes == 0;
        if last {
            self.ended.store(true, Ordering::Release);
        }
        drop(processes);
        if last {
            object::dispose(self.with_handles(HandleTable::close));
        }
    }

    /// Set the copy window: `start..end`, landing at `landing`.
    pub fn set_window(&self, start: u64, end: u64, landing: u64) {
        self.window_end.store(0, Ordering::Release);
        self.window_landing.store(landing, Ordering::Release);
        self.window_start.store(start, Ordering::Release);
        self.window_end.store(end, Ordering::Release);
    }

    /// Where a fault at `pc` lands, if `pc` is in the copy window.
    pub(crate) fn landing_for(&self, pc: u64) -> Option<u64> {
        let end = self.window_end.load(Ordering::Acquire);
        let start = self.window_start.load(Ordering::Acquire);
        (start != 0 && pc >= start && pc < end).then(|| self.window_landing.load(Ordering::Acquire))
    }
}

/// An ended region: [`Region::enter`]'s refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended;
