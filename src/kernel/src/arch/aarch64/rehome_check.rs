//! AArch64 has nothing of its own here yet: its token and its rollback are
//! L.aarch64.51's, whose statement is the entry's (`docs/microkernel/B5.md`). Its
//! caller is `trap::rehome_check::run`.

/// Run them, and answer how many cases they are.
///
/// # Errors
///
/// The first thing that was not so, by name.
pub(crate) fn run() -> Result<u64, &'static str> {
    Ok(0)
}
