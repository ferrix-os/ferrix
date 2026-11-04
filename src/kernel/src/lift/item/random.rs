use crate::arch;
use crate::sync::SpinLock;
use ferrix_bootinfo::BootInfo;
use ferrix_bootinfo::FIRMWARE_SEED;
use ferrix_crng::Crng;
use ferrix_crng::KEY_BYTES;
pub fn fill(bytes: &mut [u8]) {
    let _ = ferrix_native::lift_calls::random_fill(ferrix_rt::Kernel, bytes);
}
