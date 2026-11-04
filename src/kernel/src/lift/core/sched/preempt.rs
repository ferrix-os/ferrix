//! No preemption count in ring 3 (docs/microkernel/M5-LIFT.md §3).
pub fn counting() -> bool {
    false
}
#[derive(Debug, Clone, Copy, Default)]
pub struct Preempt;
// SAFETY: a lock's holder in ring 3 can always be preempted; the shim's locks
// yield behind one, and no code here relies on not being preempted.
unsafe impl ferrix_sync::PreemptControl for Preempt {
    fn disable() {}
    fn enable() {}
}
