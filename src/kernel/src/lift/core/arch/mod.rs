pub mod percpu;
pub mod speculation;
pub mod x86_64;
#[cfg(target_arch = "aarch64")]
use aarch64::speculation as machine_speculation;
#[cfg(target_arch = "arm")]
use armv7a::speculation as machine_speculation;
pub use speculation::HARDENED;
pub use speculation::switch_barriers;
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatLayout {
    Legacy,
    Generic,
    Stat64,
}
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
pub use x86_64::cpuid;
pub use x86_64::user_extended_state;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;
pub const CAN_MOVE_INTERRUPTS: bool = cfg!(target_arch = "x86_64");
#[cfg(not(target_arch = "x86_64"))]
pub const fn thread_core(_hardware_id: u64) -> Option<(u64, u32)> {
    None
}
#[cfg(not(target_arch = "x86_64"))]
pub const fn cpuid(_leaf: u32, _subleaf: u32) -> Option<[u32; 4]> {
    None
}
#[cfg(not(target_arch = "x86_64"))]
pub const fn user_extended_state() -> u64 {
    0
}
#[cfg(target_arch = "x86_64")]
pub const fn midr_of(_logical: usize) -> Option<u32> {
    None
}
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(not(target_arch = "arm"))]
pub const fn sigpage_code() -> Option<&'static [u8]> {
    None
}
#[cfg(target_arch = "x86_64")]
pub use percpu::this_cpu_add;
#[cfg(target_arch = "x86_64")]
pub use x86_64::ARCH;
pub use x86_64::COMPAT_USER_END;
pub use x86_64::EPOLL_EVENT_BYTES;
#[cfg(target_arch = "x86_64")]
pub use x86_64::FAST_WRITE_READ;
pub use x86_64::Irq;
pub use x86_64::NAME;
pub use x86_64::OPEN_FLAGS;
#[cfg(target_arch = "x86_64")]
pub use x86_64::SIGNAL_RED_ZONE;
pub use x86_64::STAT_LAYOUT;
pub use x86_64::TrapFrame;
pub use x86_64::USER_ARGUMENT_PROGRAM;
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
pub use x86_64::USER_GETUID_PROGRAM;
#[cfg(target_arch = "x86_64")]
pub use x86_64::USER_INTO_CGROUP_PROGRAM;
pub use x86_64::USER_OOM_PROGRAM;
#[cfg(target_arch = "x86_64")]
pub use x86_64::USER_RMAP_PROGRAM;
pub use x86_64::USER_STOPPED_PROGRAM;
pub use x86_64::USER_TEST_PROGRAM;
pub use x86_64::USER_TEST_STATUS;
pub use x86_64::UserContext;
pub use x86_64::UserRegs;
pub use x86_64::UserState;
pub use x86_64::audit_arch;
pub use x86_64::counter_hz;
pub use x86_64::counter_now;
pub use x86_64::decode_compat_syscall;
pub use x86_64::decode_syscall;
pub use x86_64::disable_interrupts;
pub use x86_64::enable_interrupts;
pub use x86_64::enter_user;
#[cfg(target_arch = "x86_64")]
pub use x86_64::halt;
pub use x86_64::image_abi;
pub use x86_64::interrupts_enabled;
pub use x86_64::power_off;
pub use x86_64::read_console_byte;
pub use x86_64::reset;
pub use x86_64::reset_user_state;
pub use x86_64::restore_signal_frame;
pub use x86_64::resume_user;
#[cfg(target_arch = "x86_64")]
pub use x86_64::ring3_reads_counter;
pub use x86_64::set_thread_area;
pub use x86_64::setup_signal_frame;
pub use x86_64::shutdown;
pub use x86_64::thread_area;
pub use x86_64::user_base;
pub use x86_64::user_hwcaps;
pub use x86_64::user_platform;
#[cfg(target_arch = "x86_64")]
#[cfg(not(target_arch = "x86_64"))]
pub fn render_machine_check(_out: &mut impl core::fmt::Write) -> core::fmt::Result {
    Ok(())
}
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "aarch64")]
use aarch64::FRAMEBUFFER_FLAGS;
#[cfg(target_arch = "arm")]
use armv7a::FRAMEBUFFER_FLAGS;
#[cfg(target_arch = "x86_64")]
pub use x86_64::clean_for_device;
#[cfg(target_arch = "x86_64")]
pub use x86_64::render_machine_check;
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(not(target_arch = "x86_64"))]
pub const fn user_combining() -> bool {
    false
}
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
pub use x86_64::sync_instructions;
