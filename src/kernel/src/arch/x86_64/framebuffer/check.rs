//! The framebuffer window's type from its first map, read back on every
//! x86-64 boot that has one (`L.x86_64.176`, `L.x86_64.127`).

#![cfg(target_arch = "x86_64")]

use ferrix_kernel_core::arch::x86_64 as core_x86_64;

use ferrix_bootinfo::PAGE_SIZE;

/// The kernel's framebuffer window is write-combining from its first map,
/// and every processor translates it by the kernel's table alone: every
/// page of the window reads back write-combining, not device, writable, not
/// user's and global; the boot processor's `IA32_PAT` read when the window
/// was mapped was [`ferrix_paging::X86_PAT`]; every secondary arrived in
/// `secondary_start` with its trampoline's write of it already in place,
/// before it programmed it again; and every processor counted as
/// programmed; and every cache line of the window was flushed before its
/// first store, counted (that the lines left the caches is build evidence:
/// no emulator keeps one to find). The lend's read-back (`discovery::firmware`) is held both
/// ways: true on the window, false on a page the kernel maps as device
/// memory. A boot with no window says so.
///
/// Verifies: `L.x86_64.176`
/// Verifies: `L.x86_64.127`
/// Verifies: L.device.43
pub fn check_framebuffer_window() -> Result<(), &'static str> {
    let processors = crate::smp::count();
    if core_x86_64::write_combining_processors() != Some(processors) {
        return Err("a processor's PAT was not programmed with its write-combining entry");
    }
    if core_x86_64::pat_at_entry() != processors.saturating_sub(1) {
        return Err("a secondary arrived without the write-combining PAT");
    }
    let Some((window, mapped)) = crate::panic::screen::window() else {
        crate::println!(
            "  fbwindow no framebuffer window: nothing mapped to read back; PAT on {} \
             secondaries before paging",
            processors.saturating_sub(1)
        );
        return Ok(());
    };
    if core_x86_64::framebuffer::window_pat() != Some(ferrix_paging::X86_PAT) {
        return Err("the PAT in force when the framebuffer window was mapped was not X86_PAT");
    }
    let root = crate::mm::root_table();
    let pages = mapped.div_ceil(PAGE_SIZE);
    for page in 0..pages {
        let flags = crate::mm::flags_in(root, window + page * PAGE_SIZE)
            .ok_or("the kernel's framebuffer window has a hole")?;
        if !flags.write_combining || flags.device || !flags.write || flags.user || !flags.global {
            return Err("the kernel's framebuffer window is not write-combining");
        }
    }
    if !crate::discovery::firmware::window_combining(window, mapped) {
        return Err("the lend's read-back refused the write-combining window");
    }
    // A page the kernel maps as device memory: one of its own device
    // windows, mapped once more for the check.
    let (phys, _) = crate::vmap::device_windows()
        .first()
        .copied()
        .ok_or("the kernel maps no device window to read back")?;
    let device = crate::vmap::map_device(phys - phys % PAGE_SIZE, PAGE_SIZE)
        .map_err(|_| "a device page could not be mapped for the read-back")?;
    let passed = crate::discovery::firmware::window_combining(device, PAGE_SIZE);
    crate::vmap::unmap_device(device).map_err(|_| "the read-back's device page stayed mapped")?;
    if passed {
        return Err("a device-mapped page passed the lend's read-back");
    }
    let (lines, ticks) = core_x86_64::framebuffer::window_flush();
    if lines == 0 || lines != core_x86_64::framebuffer::window_lines(window, mapped) {
        return Err("the framebuffer window was not flushed before its first store");
    }
    let hz = core_x86_64::clock::counter_hz();
    let (took, unit) = if core_x86_64::clock::counter_is_tsc() && hz != 0 {
        (
            u128::from(ticks).saturating_mul(1_000_000) / u128::from(hz),
            "us",
        )
    } else {
        (u128::from(ticks), "TSC ticks")
    };
    crate::println!(
        "  fbwindow framebuffer window write-combining from its first map: {pages} pages, \
         PAT X86_PAT at the map and on {} secondaries before paging, a device page refused; \
         {lines} cache lines flushed first in {took} {unit}",
        processors.saturating_sub(1)
    );
    Ok(())
}
