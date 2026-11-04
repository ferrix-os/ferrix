//! The kernel's one loader, with kernel objects alone: no process, no call
//! (`docs/microkernel/K15.md` §6). Every image here is built by the check.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::{PAGE_SIZE, USER_VIRT_END};
use ferrix_elf::{PF_R, PF_W, PF_X, PT_INTERP, PT_LOAD};
use ferrix_native_abi::types::{NATIVE_STACK_GUARD, NATIVE_STACK_SIZE};

use super::first_image::{self, Refused};
use crate::mm;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;

/// What the check found, for the boot line.
#[derive(Debug, Default, Clone, Copy)]
struct Report {
    /// Malformed images, each refused without a stop.
    malformed: u64,
    /// User leaves the sweep saw.
    leaves: u64,
    /// Of those, executable.
    executable: u64,
}

/// A program header: type, flags, offset, address, file size, memory size.
type Header = (u32, u32, u64, u64, u64, u64);

/// Where the check's programs are linked.
const AT: u64 = 0x48_0000;
/// The bytes of its text.
const TEXT: u8 = 0xC3;
/// The bytes of its data.
const DATA: u8 = 0x5A;

/// The header sizes this kernel's word width has, and whether it is 64-bit.
const WIDE: bool = cfg!(target_pointer_width = "64");

/// Write `value` in `size` little-endian bytes at `at`.
fn put(file: &mut [u8], at: usize, size: usize, value: u64) {
    let bytes = value.to_le_bytes();
    if let (Some(to), Some(from)) = (file.get_mut(at..at + size), bytes.get(..size)) {
        to.copy_from_slice(from);
    }
}

/// An ELF of this kernel's machine and width: `kind` is `ET_EXEC` or `ET_DYN`.
fn elf(kind: u64, entry: u64, headers: &[Header], size: usize) -> Vec<u8> {
    let (head, phent) = if WIDE { (64, 56) } else { (52, 32) };
    let mut file = vec![0_u8; size.max(head + phent * headers.len())];
    if let Some(start) = file.get_mut(..7) {
        start.copy_from_slice(&[0x7F, b'E', b'L', b'F', if WIDE { 2 } else { 1 }, 1, 1]);
    }
    put(&mut file, 16, 2, kind);
    put(&mut file, 18, 2, u64::from(crate::arch::ARCH.elf_machine()));
    put(&mut file, 20, 4, 1);
    if WIDE {
        put(&mut file, 24, 8, entry);
        put(&mut file, 32, 8, head as u64);
        put(&mut file, 52, 2, head as u64);
        put(&mut file, 54, 2, phent as u64);
        put(&mut file, 56, 2, headers.len() as u64);
    } else {
        put(&mut file, 24, 4, entry);
        put(&mut file, 28, 4, head as u64);
        put(&mut file, 40, 2, head as u64);
        put(&mut file, 42, 2, phent as u64);
        put(&mut file, 44, 2, headers.len() as u64);
    }
    for (index, h) in headers.iter().enumerate() {
        let at = head + phent * index;
        put(&mut file, at, 4, u64::from(h.0));
        if WIDE {
            put(&mut file, at + 4, 4, u64::from(h.1));
            put(&mut file, at + 8, 8, h.2);
            put(&mut file, at + 16, 8, h.3);
            put(&mut file, at + 24, 8, h.3);
            put(&mut file, at + 32, 8, h.4);
            put(&mut file, at + 40, 8, h.5);
            put(&mut file, at + 48, 8, PAGE_SIZE);
        } else {
            put(&mut file, at + 4, 4, h.2);
            put(&mut file, at + 8, 4, h.3);
            put(&mut file, at + 12, 4, h.3);
            put(&mut file, at + 16, 4, h.4);
            put(&mut file, at + 20, 4, h.5);
            put(&mut file, at + 24, 4, u64::from(h.1));
            put(&mut file, at + 28, 4, PAGE_SIZE);
        }
    }
    file
}

/// A text page, then a data page with a `.bss` page after it.
fn program() -> Vec<u8> {
    let page = PAGE_SIZE as usize;
    let mut file = elf(
        2,
        AT,
        &[
            (PT_LOAD, PF_R | PF_X, PAGE_SIZE, AT, PAGE_SIZE, PAGE_SIZE),
            (
                PT_LOAD,
                PF_R | PF_W,
                2 * PAGE_SIZE,
                AT + PAGE_SIZE,
                0x100,
                2 * PAGE_SIZE,
            ),
        ],
        3 * page,
    );
    if let Some(text) = file.get_mut(page..2 * page) {
        text.fill(TEXT);
    }
    if let Some(data) = file.get_mut(2 * page..2 * page + 0x100) {
        data.fill(DATA);
    }
    file
}

/// One byte of `space` at `at`.
fn byte(space: &AddressSpace, at: u64) -> Result<u8, &'static str> {
    let mut one = [0_u8; 1];
    uaccess::copy_from_user(space, at, &mut one)
        .map_err(|_| "first image self-check failed: a loaded byte could not be read")?;
    Ok(one[0])
}

/// A fresh space.
fn space() -> Result<alloc::sync::Arc<AddressSpace>, &'static str> {
    AddressSpace::new().map_err(|_| "first image self-check failed: no memory for a space")
}

/// L.user.200 and L.user.203: what a good image becomes.
///
/// Verifies: L.user.200
/// Verifies: L.user.203
fn a_good_image(report: &mut Report) -> Result<(), &'static str> {
    let made = space()?;
    let at = first_image::load(&made, &program())
        .map_err(|_| "first image self-check failed: a good image was refused")?;
    if at.entry != AT {
        return Err("first image self-check failed: the entry is not the image's");
    }
    if byte(&made, AT)? != TEXT || byte(&made, AT + PAGE_SIZE)? != DATA {
        return Err("first image self-check failed: the file bytes were not copied");
    }
    if byte(&made, AT + PAGE_SIZE + 0x100)? != 0 || byte(&made, AT + 2 * PAGE_SIZE)? != 0 {
        return Err("first image self-check failed: .bss is not zero");
    }
    let sweep = mm::check::sweep_user_root(&made)
        .map_err(|_| "first image self-check failed: a leaf is writable and executable")?;
    if sweep.executable == 0 {
        return Err("first image self-check failed: the text is not executable");
    }
    report.leaves = sweep.leaves;
    report.executable = sweep.executable;
    // The stack and its guards, read back from the space's own regions.
    let top = (USER_VIRT_END - PAGE_SIZE) & !15;
    let low = top - NATIVE_STACK_SIZE;
    let (mut stack, mut above, mut below) = (false, false, false);
    made.with_regions(|regions| {
        for region in regions {
            let write = region.flags.write;
            let none = !region.flags.read && !write;
            stack |= region.start == low && region.end == top && write;
            above |= region.start == top && region.end == USER_VIRT_END && none;
            below |= region.start == low - NATIVE_STACK_GUARD && region.end == low && none;
        }
    });
    if at.stack != top || !stack || !above || !below {
        return Err("first image self-check failed: the stack or a guard is not where it belongs");
    }
    Ok(())
}

/// L.user.201: a page that would be writable and executable refuses the image,
/// with nothing mapped.
///
/// Verifies: L.user.201
fn write_and_execute_are_refused() -> Result<(), &'static str> {
    let cases: [&[Header]; 2] = [
        &[(PT_LOAD, PF_R | PF_W | PF_X, PAGE_SIZE, AT, 0x10, 0x10)],
        &[
            (PT_LOAD, PF_R | PF_X, PAGE_SIZE, AT, 0x10, 0x10),
            (
                PT_LOAD,
                PF_R | PF_W,
                PAGE_SIZE + 0x800,
                AT + 0x800,
                0x10,
                0x10,
            ),
        ],
    ];
    for headers in cases {
        let made = space()?;
        let answer = first_image::load(&made, &elf(2, AT, headers, 2 * PAGE_SIZE as usize));
        if !matches!(answer, Err(Refused::WriteExecute(_))) {
            return Err("first image self-check failed: a W+X page was not refused");
        }
        if made.with_regions(|regions| regions.count()) != 0 {
            return Err("first image self-check failed: a refused image left a region behind");
        }
    }
    Ok(())
}

/// L.user.202: what cannot run here is refused, never a stop.
///
/// Verifies: L.user.202
fn what_cannot_run_is_refused(report: &mut Report) -> Result<(), &'static str> {
    let page = PAGE_SIZE as usize;
    let load = (PT_LOAD, PF_R | PF_X, PAGE_SIZE, AT, 0x10, 0x10);
    let mut machine = elf(2, AT, &[load], 2 * page);
    put(&mut machine, 18, 2, 0x7FFF);
    let mut class = elf(2, AT, &[load], 2 * page);
    put(&mut class, 4, 1, if WIDE { 1 } else { 2 });
    let cases = [
        machine,
        class,
        elf(2, AT, &[load, (PT_INTERP, PF_R, 0x100, 0, 4, 4)], 2 * page),
        elf(2, USER_VIRT_END, &[load], 2 * page),
        elf(
            2,
            AT,
            &[(PT_LOAD, PF_R, PAGE_SIZE, USER_VIRT_END, 0x10, 0x10)],
            2 * page,
        ),
        vec![0x7F, b'E', b'L', b'F'],
        elf(2, AT, &[], page),
    ];
    for bytes in &cases {
        let made = space()?;
        if first_image::load(&made, bytes).is_ok() {
            return Err("first image self-check failed: an image that cannot run was loaded");
        }
        report.malformed += 1;
    }
    Ok(())
}

// CHECKS-ONLY: object/check.rs, object/space_check.rs
/// The one loader, for the checks above this crate that need a child with a
/// program in it (`Side::built`): product code never comes through here.
///
/// # Errors
///
/// As [`first_image::load`].
pub fn load_for_check(
    space: &AddressSpace,
    image: &[u8],
) -> Result<crate::syscall::native::EntryPoint, Refused> {
    first_image::load(space, image)
}

// CHECKS-ONLY: syscall/build_check.rs
/// What the kernel hands `devmgr` an image with.
pub const fn handed_image_rights() -> ferrix_native_abi::rights::Rights {
    super::devmgr::image_rights()
}

// CHECKS-ONLY: stages_check.rs
/// Every case.
///
/// Verifies: H.MEM.27
///
/// # Errors
///
/// The first case that did not hold. Answers the malformed images refused, the
/// user leaves the sweep saw, and how many of them are executable.
pub fn run() -> Result<[u64; 3], &'static str> {
    let mut report = Report::default();
    a_good_image(&mut report)?;
    write_and_execute_are_refused()?;
    what_cannot_run_is_refused(&mut report)?;
    Ok([report.malformed, report.leaves, report.executable])
}
