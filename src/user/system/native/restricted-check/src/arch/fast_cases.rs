//! K1's fast path's cases on the architecture that has one (`../fast.rs`), and none elsewhere.

#[cfg(target_arch = "x86_64")]
#[path = "../fast.rs"]
mod cases;

#[cfg(target_arch = "x86_64")]
pub(super) use cases::run;

/// No fast path on this architecture.
#[cfg(not(target_arch = "x86_64"))]
pub(super) fn run(_given: &super::Given) -> Result<(), i32> {
    Ok(())
}
