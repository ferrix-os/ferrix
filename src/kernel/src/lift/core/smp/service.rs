use crate::audit;
use crate::sched;
use crate::sched::ServiceRefusal;
use crate::sync::SchedParker;
use ferrix_sync::Once;
use ferrix_sync::SleepLock;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Door {
    Root,
    Lode,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    NoSuchProcessor,
    Unsupported,
    Interrupts,
    Scheduler(ServiceRefusal),
}
pub fn set_in_service(
    cpu: usize,
    wanted: bool,
    door: Door,
    subject: audit::Subject,
) -> Result<(), Refusal> {
    panic!("shim: smp::service::set_in_service")
}
