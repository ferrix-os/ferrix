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
//! # A second object for time namespaces
//!
//! The data page is one page for every process, so a process in a time
//! namespace with offsets must not map it: it would read the clock unshifted
//! (`docs/NAMESPACES.md` §12.1). Such a process is given a second object
//! instead, the same image over a data page whose mode is
//! [`ferrix_vdso::MODE_SYSCALL`], where every function makes the system call
//! and the system call applies the offsets. Built the first time a process in
//! such a namespace is started or forked, and kept; nothing in the image
//! differs.
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

/// The variant for a process in a time namespace that shifts a clock: built
/// on first need, like [`VDSO`].
static SHIFTED: Once<Option<Vdso>> = Once::new();

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
/// page filled in. `None` on an architecture with no code for one. With
/// `shifted` every function makes its system call.
fn build(shifted: bool) -> Option<Vdso> {
    let spec = crate::arch::vdso_spec()?;
    let vmo = Vmo::new_anonymous(2).ok()?;
    let held = vmo.hold(0, 2).ok()?;
    let mut image = alloc::vec![0_u8; ferrix_vdso::IMAGE_BYTES];
    ferrix_vdso::build(&spec, &mut image).ok()?;
    vmo.write_page(1, 0, &image).ok()?;
    let vdso = Vdso { vmo, held };
    let mode = if crate::arch::vdso_can_read_counter() && !shifted {
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
    let vdso = VDSO.call_once(|| build(false)).as_ref()?;
    if !built {
        // Whatever the offset became while the page was being built.
        publish_realtime_offset(|| {});
    }
    Some(vdso)
}

/// The variant, built the first time it is asked for.
fn shifted_vdso() -> Option<&'static Vdso> {
    SHIFTED.call_once(|| build(true)).as_ref()
}

/// Map the vDSO into `space` and answer where its image is, for
/// `AT_SYSINFO_EHDR`: `None` with no vDSO, or no room for one, and the
/// program is started without, as it would be on an architecture without.
/// With `shifted`, for a process in a time namespace with offsets, the
/// variant whose functions all make their system calls.
pub(crate) fn map_into(space: &AddressSpace, shifted: bool) -> Option<u64> {
    let vdso = if shifted { shifted_vdso()? } else { vdso()? };
    space.map_shared_code(Arc::clone(&vdso.vmo)).ok()
}

/// Whether a process in `namespace` is given the variant: it shifts a clock.
pub(crate) fn is_shifted(namespace: &super::timens::TimeNamespace) -> bool {
    !namespace.is_first()
}

/// Change the view of the vDSO `space` has to the one a process in a
/// namespace that `shifted` says needs: what a fork, a native child and
/// `setns` do for a process whose time namespace is not the one its space was
/// mapped for. Nothing if the space maps no view that reads wrongly: a program
/// that unmapped it, an architecture without one, or a space already right.
///
/// It fails closed. A space that maps the view reading the first namespace's
/// clocks, for a namespace that shifts one, is never left so: if the variant
/// cannot be had, the answer is an error and the caller must not run the
/// space in that namespace. The other way, a space keeping the variant in the
/// first namespace reads the right time, through the system call, and is left
/// so when the main object cannot be had.
///
/// # Errors
///
/// The variant could not be built, or the space could not be given the other
/// view (it then has none).
pub(crate) fn retarget(space: &AddressSpace, shifted: bool) -> Result<(), ()> {
    let to = if shifted { shifted_vdso() } else { vdso() };
    retarget_to(space, shifted, to)
}

/// [`retarget`] with `to` as the view wanted, `None` when it could not be
/// had.
fn retarget_to(space: &AddressSpace, shifted: bool, to: Option<&Vdso>) -> Result<(), ()> {
    let from = if shifted { &VDSO } else { &SHIFTED };
    let Some(at) = from
        .get()
        .and_then(Option::as_ref)
        .and_then(|from| space.shared_code_at(&from.vmo))
    else {
        return Ok(());
    };
    match to {
        Some(to) => space
            .replace_shared_code(at, Arc::clone(&to.vmo))
            .map_err(drop),
        // The view that reads the first namespace's clocks, in one that
        // shifts them: refused, never kept.
        None if shifted => Err(()),
        None => Ok(()),
    }
}

/// [`retarget`] to a shifted namespace as it is when the variant cannot be
/// built, for the checks: it must fail, leaving the space unchanged.
pub(crate) fn retarget_without_variant(space: &AddressSpace) -> Result<(), ()> {
    retarget_to(space, true, None)
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
    let data = [VDSO.get(), SHIFTED.get()]
        .into_iter()
        .flatten()
        .filter_map(Option::as_ref)
        .find_map(|vdso| space.shared_code_at(&vdso.vmo))?;
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

/// The variant's data page mode word, for the checks: builds it if need be.
pub(crate) fn shifted_mode() -> Option<u64> {
    let vdso = shifted_vdso()?;
    Some(
        vdso.word(ferrix_vdso::VVAR_MODE)?
            .load(core::sync::atomic::Ordering::Acquire),
    )
}

/// Where `space` maps the main object's data page and the variant's, for the
/// checks: `None` for a view it does not have.
pub(crate) fn mapped_views(space: &AddressSpace) -> [Option<u64>; 2] {
    let at = |once: &Once<Option<Vdso>>| {
        let vdso = once.get()?.as_ref()?;
        space.shared_code_at(&vdso.vmo)
    };
    [at(&VDSO), at(&SHIFTED)]
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
