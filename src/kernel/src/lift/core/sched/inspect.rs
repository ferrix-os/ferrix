//! /proc/<pid>/stack and /proc/<pid>/syscall: nothing to show yet (P in the map).
use super::Task;
use crate::trap::Abi;
use alloc::sync::Arc;
pub const MAX_FRAMES: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Blocked {
        number: u64,
        args: [u64; 6],
        sp: u64,
        pc: u64,
    },
    Trapped {
        sp: u64,
        pc: u64,
    },
}
/// The call a blocked thread is in, as the restricted loop noted it, and where the
/// program stood, read the first time it parks. Linuxd holds no frames of the program's.
pub fn call_of(task: &Arc<Task>, _abi: Abi) -> Option<Call> {
    let (number, args, sp, pc) = task.call_seen()?;
    if !task.is_blocked() {
        return None;
    }
    Some(Call::Blocked {
        number,
        args,
        sp,
        pc,
    })
}
pub fn calls_are_visible() -> bool {
    true
}
/// Two frames: where linuxd parks it, and where the program called from. Linuxd's
/// own frames between them are not walked.
pub fn chain_of(task: &Arc<Task>, out: &mut [u64; MAX_FRAMES]) -> Option<usize> {
    let (.., pc) = task.call_seen()?;
    if !task.is_blocked() {
        return None;
    }
    *out.first_mut()? = crate::sys::park as *const () as usize as u64;
    *out.get_mut(1)? = pc;
    Some(2)
}
