//! The signal return page: code the kernel maps into every process for a
//! handler installed without `SA_RESTORER` to return through, on an
//! architecture whose C libraries could not read the vDSO image (F-48).
//!
//! ARMv7-A's programs are 32-bit, and the vDSO image is a 64-bit ELF, which a
//! 32-bit C library given `AT_SYSINFO_EHDR` would misread. So ARMv7-A gets no
//! vDSO, and Linux's answer instead: one page of return sequences, mapped
//! read-and-run wherever the vDSO would go, told to nobody through the
//! auxiliary vector, and named only by the link register a signal frame
//! leaves (`arch/arm/kernel/signal.c`'s `sigpage`). Without it the frame's
//! own copy of the sequence was the return address, on a stack no page of
//! which may run, and a handler that returned was killed.
//!
//! Built once, from `arch::sigpage_code`, and never written again: every
//! process shares the one page, and none may write it.

use alloc::sync::Arc;

use ferrix_sync::Once;

use crate::user::space::AddressSpace;
use crate::user::vmo::{Held, Vmo};

/// The page every process maps, once built.
#[derive(Debug)]
struct Sigpage {
    /// The one page.
    vmo: Arc<Vmo>,
    /// Held in place for good.
    _held: Held,
}

/// Built by the first exec: `None` on an architecture without one, or if
/// building failed, and then no process gets one.
static SIGPAGE: Once<Option<Sigpage>> = Once::new();

/// Build the page, the architecture's sequences at its start.
fn build() -> Option<Sigpage> {
    let code = crate::arch::sigpage_code()?;
    let vmo = Vmo::new_anonymous(1).ok()?;
    let held = vmo.hold(0, 1).ok()?;
    vmo.write_page(0, 0, code).ok()?;
    Some(Sigpage { vmo, _held: held })
}

/// Map the page into `space`, if the architecture has one.
///
/// A space it cannot be mapped into goes on without it, as a space the vDSO
/// cannot be mapped into does: its handlers without `SA_RESTORER` then
/// return where they did before.
pub fn map_into(space: &AddressSpace) -> Option<u64> {
    let page = SIGPAGE.call_once(build).as_ref()?;
    space.map_code_page(Arc::clone(&page.vmo)).ok()
}

/// Where `space` has byte `offset` of the page, if it maps the page.
pub(crate) fn address(space: &AddressSpace, offset: u64) -> Option<u64> {
    let page = SIGPAGE.get()?.as_ref()?;
    space.shared_code_at(&page.vmo)?.checked_add(offset)
}
