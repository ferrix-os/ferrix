use crate::object::process::Process;
use crate::object::quota::Resource;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
pub use ferrix_audit::BOOTED;
pub use ferrix_audit::CGROUP_KILLED;
pub use ferrix_audit::CGROUP_LIMIT;
pub use ferrix_audit::CONTROL;
pub use ferrix_audit::Config;
pub use ferrix_audit::DELEGATED;
pub use ferrix_audit::DEVICE_LIMIT;
pub use ferrix_audit::DEVICE_LIMIT_SET;
pub use ferrix_audit::DEVMGR_STARTED;
pub use ferrix_audit::DOMAIN;
pub use ferrix_audit::EXCEPTION_ATTACHED;
pub use ferrix_audit::EXCEPTION_CONTEXT;
pub use ferrix_audit::EXCEPTION_KILLED;
pub use ferrix_audit::Event;
pub use ferrix_audit::JOB_KILLED;
pub use ferrix_audit::LIMIT_SET;
pub use ferrix_audit::NO_UID;
pub use ferrix_audit::Outcome;
pub use ferrix_audit::PAGER;
pub use ferrix_audit::PROCESS_MADE;
pub use ferrix_audit::QUIESCED;
pub use ferrix_audit::READER_GIVEN;
pub use ferrix_audit::RIGHTS;
pub use ferrix_audit::ROOT_SWITCHED;
pub use ferrix_audit::Record;
pub use ferrix_audit::STARTER_GIVEN;
pub use ferrix_audit::WIDEN;
pub use ferrix_audit::saturated;
pub use ferrix_audit::target;
use ferrix_sync::IrqSpinLock;
pub const HIGH_RECORDS: usize = 512;
pub const REFUSAL_RECORDS: usize = 4096;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subject {
    pub pid: u32,
    pub uid: u32,
    pub job: u64,
    pub budget: u64,
}
impl Subject {
    pub const KERNEL: Subject = Subject {
        pid: 0,
        uid: NO_UID,
        job: 0,
        budget: 0,
    };
    pub fn of(process: &Process) -> Subject {
        Subject::KERNEL
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub kind: u32,
    pub id: u64,
}
impl Target {
    pub const NONE: Target = Target { kind: 0, id: 0 };
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    High,
    Refusals,
    Boot,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read {
    pub copied: usize,
    pub next: u64,
    pub lost: u64,
    pub id: u128,
}
pub fn resource_number(resource: Resource) -> u64 {
    0
}
pub fn limit_set(event: Event, subject: Subject, job: u64, resource: u64, limit: u64) {}
pub fn ended(event: Event, subject: Subject, target: Target, detail: [u32; 3]) {}
pub fn power(subject: Subject, action: u32) {}
pub fn id() -> u128 {
    0
}
pub fn read(which: Which, from: u64, out: &mut [Record]) -> Read {
    panic!("shim: audit::read")
}

pub fn limit_refused(
    job: u64,
    budget: u64,
    resource: crate::object::quota::Resource,
    amount: u64,
    limit: u64,
    at: u32,
) {
}
pub fn oom_killed(pid: u32, limited: u64, resident: u64) {}
pub fn record(
    event: Event,
    outcome: Outcome,
    status: i16,
    subject: Subject,
    target: Target,
    detail: [u32; 3],
) {
}
