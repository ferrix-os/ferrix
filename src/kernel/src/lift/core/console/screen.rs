use core::hint::spin_loop;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::BootView;
use ferrix_fbtext::GLYPH_HEIGHT;
use ferrix_fbtext::GLYPH_WIDTH;
use ferrix_fbtext::Rgb;
use ferrix_fbtext::Surface;
use ferrix_sync::SpinLock;
pub fn stop() {}
