//! Jobs: what a set of processes is killed as.

use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::types;

use crate::call::{Call, Syscall};
use crate::error::{Error, decode_handle, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register, rights_register};

object_handle!(
    /// A job.
    Job
);

/// `job_for_cgroup`: the job behind the cgroupfs directory the descriptor
/// `dirfd` is open on, with `rights`, which the caller's access to that
/// directory's `cgroup.procs` bounds (`docs/CGROUPS.md` §5). What a native
/// service manager waits on for [`crate::Signals::EMPTY`] in place of
/// `cgroup.events`.
///
/// # Errors
///
/// [`Error::BadHandle`] for a descriptor not open; [`Error::WrongType`] for
/// one that is not a cgroup directory; [`Error::AccessDenied`] for rights
/// the caller's access does not allow; [`Error::BadState`] for a removed
/// cgroup; [`Error::NoHandles`].
pub fn for_cgroup<S: Syscall>(sys: S, dirfd: i32, rights: Requested) -> Result<Job<S>, Error> {
    let value = Call::new(nr::JOB_FOR_CGROUP)
        .value(dirfd as u32 as usize)
        .value(rights_register(rights))
        .make(sys);
    let handle = decode_handle(value)?;
    Ok(Job::from_owned(OwnedHandle::from_raw(sys, handle)))
}

/// A resource a job is limited in: `job_set_limit` and `job_get_quota`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// Physical memory, in bytes.
    Memory,
    /// Kernel objects its programs made: VMOs, channel ends, ports, jobs.
    Objects,
    /// Tasks: a process, and each thread beside its first.
    Tasks,
    /// Its processor weight, 1 to 10,000; 100 is one task's.
    CpuWeight,
}

impl Resource {
    /// The number the calls take.
    const fn number(self) -> u64 {
        match self {
            Resource::Memory => types::JOB_MEMORY,
            Resource::Objects => types::JOB_OBJECTS,
            Resource::Tasks => types::JOB_TASKS,
            Resource::CpuWeight => types::JOB_CPU_WEIGHT,
        }
    }
}

/// What a job holds of a resource: `job_get_quota`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// What it and every job inside it hold now.
    pub used: u64,
    /// Its limit, or [`types::UNLIMITED`]; the weight, for
    /// [`Resource::CpuWeight`].
    pub limit: u64,
    /// How many charges that limit has refused.
    pub refused: u64,
}

impl<S: Syscall> Job<S> {
    /// `job_set_limit`: limit what this job and every job inside it may hold
    /// of `resource`, or set its processor weight. [`types::UNLIMITED`] lifts
    /// a limit.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MANAGE`; [`Error::InvalidArgs`] for
    /// a weight out of range; [`Error::BadState`] for the root job.
    pub fn set_limit(&self, resource: Resource, limit: u64) -> Result<(), Error> {
        let bytes = limit.to_ne_bytes();
        decode_unit(
            Call::new(nr::JOB_SET_LIMIT)
                .value(register(self.handle()))
                .value(resource.number() as usize)
                .input(&bytes)
                .make(self.syscall()),
        )
    }

    /// `job_get_quota`: what this job holds of `resource`, its limit, and how
    /// many charges the limit has refused.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `WAIT`.
    pub fn quota(&self, resource: Resource) -> Result<Quota, Error> {
        let mut out = [0_u8; 24];
        decode_unit(
            Call::new(nr::JOB_GET_QUOTA)
                .value(register(self.handle()))
                .value(resource.number() as usize)
                .output(&mut out)
                .make(self.syscall()),
        )?;
        let word = |at: usize| {
            out.get(at..at + 8)
                .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                .map_or(0, u64::from_ne_bytes)
        };
        Ok(Quota {
            used: word(0),
            limit: word(8),
            refused: word(16),
        })
    }

    /// `job_create`: a job inside this one.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MANAGE`; [`Error::BadState`] once
    /// this job has been killed; [`Error::NoMemory`], [`Error::NoHandles`].
    pub fn create_child(&self) -> Result<Job<S>, Error> {
        self.create_with(0)
    }

    /// `job_create` with [`types::JOB_SPECULATION_DOMAIN`]: a job inside this
    /// one that is one speculation domain, so that a switch between two of
    /// its processes born in it skips the predictor barrier. For programs
    /// that may read each other's memory, and nothing else.
    ///
    /// # Errors
    ///
    /// As [`Job::create_child`].
    pub fn create_speculation_domain(&self) -> Result<Job<S>, Error> {
        self.create_with(types::JOB_SPECULATION_DOMAIN)
    }

    /// `job_create` with [`types::JOB_SEALED`]: a job inside this one that no
    /// cgroupfs write moves a process into or out of, whoever writes, and
    /// whose own children are sealed too. What devmgr makes each driver's
    /// job, so that its kill reaches everything the driver started.
    ///
    /// # Errors
    ///
    /// As [`Job::create_child`].
    pub fn create_sealed_child(&self) -> Result<Job<S>, Error> {
        self.create_with(types::JOB_SEALED)
    }

    /// `job_create` with `options`.
    fn create_with(&self, options: u64) -> Result<Job<S>, Error> {
        let value = Call::new(nr::JOB_CREATE)
            .value(register(self.handle()))
            .value(usize::try_from(options).unwrap_or(usize::MAX))
            .make(self.syscall());
        let handle = decode_handle(value)?;
        Ok(Job::from_owned(OwnedHandle::from_raw(
            self.syscall(),
            handle,
        )))
    }

    /// `job_kill`: end every process in this job and every job inside it.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `MANAGE`.
    pub fn kill(&self) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::JOB_KILL)
                .value(register(self.handle()))
                .make(self.syscall()),
        )
    }
}
