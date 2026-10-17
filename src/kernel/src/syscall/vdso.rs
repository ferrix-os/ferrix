//! The vDSO: the page of code every Linux program is given to read the clock
//! with, and the page of data that code reads.
//!
//! `src/lib/kernel/vdso` lays the image out and holds its code; this is the kernel's
//! half. The two pages are one object, built the first time a program is
//! started and kept for good, held in place so that nothing -- no `madvise`,
//! no reclaim -- ever takes a frame out from under the processes mapping it.
//! Each program's exec maps it, the data page read-only and the image
//! read-and-run above it, and tells the program where with `AT_SYSINFO_EHDR`.
//! glibc and musl find `__vdso_clock_gettime` there and call it instead of
//! making the system call. Chrome made forty thousand of those calls a second.
//!
//! # What the data page holds, and keeping it true
//!
//! Which counter the kernel reads, the counter's frequency, and the real-time
//! clock's offset from it: `src/lib/kernel/vdso` names the words. The first two are
//! set when the page is built and never change. The offset changes whenever
//! the real-time clock is set, and each change is written here under the same
//! lock as the kernel's own copy is stored, so that the page cannot be left
//! behind by a change made while the page was being built. One aligned store
//! of one word is the whole update, so a program reading it concurrently sees
//! the old offset or the new one, never half of each.
//!
//! # x86-64 only
//!
//! The one architecture with code for it (`crate::arch::vdso_spec`), and the
//! one a browser runs on. On the others there is no image, no mapping and no
//! `AT_SYSINFO_EHDR`, and the C library makes the system call, as it did
//! before.

use alloc::sync::Arc;

use ferrix_sync::Once;

use crate::sync::SpinLock;
use crate::user::space::AddressSpace;
use crate::user::vmo::{Held, Vmo};

/// The object every process maps, once built.
#[derive(Debug)]
struct Vdso {
    /// The data page and the image.
    vmo: Arc<Vmo>,
    /// Both pages, held in place for good.
    held: Held,
}

/// Built by the first exec: `None` on an architecture without one, or if
/// building failed, and then no process gets one.
static VDSO: Once<Option<Vdso>> = Once::new();

/// Taken to change the real-time offset and to write it into the data page,
/// so that the two can only be changed together.
static PUBLISH: SpinLock<()> = SpinLock::new(());

impl Vdso {
    /// The data page's word at `offset`, through the direct map.
    fn word(&self, offset: usize) -> Option<&core::sync::atomic::AtomicU64> {
        let frame = *self.held.frames().first()?;
        if !offset.is_multiple_of(8) || offset + 8 > ferrix_vdso::IMAGE_BYTES {
            return None;
        }
        let at = crate::mm::direct_map(frame * ferrix_bootinfo::PAGE_SIZE) + offset as u64;
        // SAFETY: the data page's frame is held for as long as `self` lives,
        // which is the kernel's life; the direct map maps it writable; `at`
        // is inside it and eight-byte aligned; and every access to it, the
        // kernel's and the programs', is a single aligned word, which is
        // what an `AtomicU64` over it may be used with.
        Some(unsafe { core::sync::atomic::AtomicU64::from_ptr(at as *mut u64) })
    }

    fn store(&self, offset: usize, value: u64) {
        if let Some(word) = self.word(offset) {
            word.store(value, core::sync::atomic::Ordering::Release);
        }
    }
}

/// Build the object: two pages, held, the image in the second and the data
/// page filled in. `None` on an architecture with no code for one.
fn build() -> Option<Vdso> {
    let spec = crate::arch::vdso_spec()?;
    let vmo = Vmo::new_anonymous(2).ok()?;
    let held = vmo.hold(0, 2).ok()?;
    let mut image = alloc::vec![0_u8; ferrix_vdso::IMAGE_BYTES];
    ferrix_vdso::build(&spec, &mut image).ok()?;
    vmo.write_page(1, 0, &image).ok()?;
    let vdso = Vdso { vmo, held };
    let mode = if crate::arch::vdso_can_read_counter() {
        ferrix_vdso::MODE_TSC
    } else {
        ferrix_vdso::MODE_SYSCALL
    };
    vdso.store(ferrix_vdso::VVAR_COUNTER_HZ, crate::timer::counter_hz());
    vdso.store(ferrix_vdso::VVAR_MODE, mode);
    Some(vdso)
}

/// The object, built the first time it is asked for.
fn vdso() -> Option<&'static Vdso> {
    let built = VDSO.get().is_some();
    let vdso = VDSO.call_once(build).as_ref()?;
    if !built {
        // Whatever the offset became while the page was being built.
        publish_realtime_offset(|| {});
    }
    Some(vdso)
}

/// Map the vDSO into `space` and answer where its image is, for
/// `AT_SYSINFO_EHDR`: `None` with no vDSO, or no room for one, and the
/// program is started without, as it would be on an architecture without.
///
/// Its two pages end at [`exec::VDSO_END`](super::exec::VDSO_END), 128 MiB
/// below the stack's top, which keeps them out of the range Wine's preloader
/// reserves at the top of the user half.
pub(crate) fn map_into(space: &AddressSpace) -> Option<u64> {
    let vdso = vdso()?;
    space
        .map_shared_code(Arc::clone(&vdso.vmo), super::exec::VDSO_END)
        .ok()
}

/// The name of the return trampoline a signal handler without
/// `SA_RESTORER` goes back through: the crate's, which the architecture's
/// image names too.
pub(crate) use ferrix_vdso::SIGRETURN;

/// Where `space`'s vDSO has [`SIGRETURN`], if the architecture's image has
/// one and `space` maps the vDSO: the address a handler without
/// `SA_RESTORER` returns to, as Linux's AArch64 kernel points it there.
/// glibc on AArch64 never sets `SA_RESTORER` -- it has no trampoline of its
/// own -- so without this every handler returned to address zero.
pub(crate) fn sigreturn(space: &AddressSpace) -> Option<u64> {
    let spec = crate::arch::vdso_spec()?;
    let function = spec
        .functions
        .iter()
        .find(|function| function.name == SIGRETURN)?;
    let vdso = VDSO.get()?.as_ref()?;
    let data = space.shared_code_at(&vdso.vmo)?;
    let offset = u64::try_from(ferrix_vdso::CODE_AT + function.offset).ok()?;
    data.checked_add(ferrix_bootinfo::PAGE_SIZE)?
        .checked_add(offset)
}

/// Run `change`, which stores the kernel's real-time offset, and write the
/// offset it leaves into the data page, both under one lock.
pub(crate) fn publish_realtime_offset(change: impl FnOnce()) {
    let _guard = PUBLISH.lock();
    change();
    if let Some(Some(vdso)) = VDSO.get() {
        let offset = super::time::realtime_offset();
        vdso.store(ferrix_vdso::VVAR_REALTIME_OFFSET, offset.cast_unsigned());
    }
}

/// The image, for the checks: what the second page holds.
pub(crate) fn image() -> Option<alloc::vec::Vec<u8>> {
    let vdso = vdso()?;
    let frame = *vdso.held.frames().get(1)?;
    let at = crate::mm::direct_map(frame * ferrix_bootinfo::PAGE_SIZE) as *const u8;
    // SAFETY: the image's frame is held for the kernel's life and mapped by
    // the direct map; nothing writes it after `build`.
    let bytes = unsafe { core::slice::from_raw_parts(at, ferrix_vdso::IMAGE_BYTES) };
    Some(bytes.to_vec())
}

/// The data page's words, for the checks: the mode, the frequency and the
/// real-time offset.
pub(crate) fn data() -> Option<[u64; 3]> {
    let vdso = vdso()?;
    let read = |offset| {
        vdso.word(offset)
            .map(|word| word.load(core::sync::atomic::Ordering::Acquire))
    };
    Some([
        read(ferrix_vdso::VVAR_MODE)?,
        read(ferrix_vdso::VVAR_COUNTER_HZ)?,
        read(ferrix_vdso::VVAR_REALTIME_OFFSET)?,
    ])
}
