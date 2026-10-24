//! A copy through a device's pages is refused, and the boot goes on (F-55).
//!
//! A program that maps device memory -- a driver's aperture through an I/O
//! mapping, a render node's window of host memory -- can hand a pointer into
//! it to any system call that copies. The kernel copies through the direct
//! map, which translates RAM alone, so for a device's page the alias it would
//! form is either nothing, and the copy faults in the kernel, or a cacheable
//! alias of memory the program maps uncached. Until copies go through a
//! mapping with the device's own attributes, both directions are `EFAULT`,
//! decided from the region before its page is faulted in.
//!
//! The windows are an aperture a device node really has, through the native
//! objects a driver uses; a cached window past all RAM, where Venus's host
//! window lay when the panic was found; and, where the memory map has one, a
//! window over a hole between two runs of RAM, inside the direct map's span.
//! Only the aperture's page is ever faulted in, and nothing reads or writes
//! any of them.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::O_NONBLOCK;
use ferrix_native_abi::nr;
use ferrix_vma::VmaFlags;

use crate::device;
use crate::mm;
use crate::object::check::{SCRATCH, Side, device_handle, reg};
use crate::syscall::{fd, file, pipe};
use crate::user::space::{Access, FilePlace, SpaceError, WindowPages};

/// Where `pipe2` puts the two descriptors: the scratch region's second page,
/// which the object checks' staging leaves alone.
const FDS: u64 = SCRATCH + PAGE_SIZE;
/// The one-segment `iovec` the copies are asked through.
const IOV: u64 = SCRATCH + PAGE_SIZE + 0x10;
/// The aperture's `io_mapping_create` spec: its physical address and length.
const SPEC: u64 = SCRATCH + PAGE_SIZE + 0x20;
/// Bytes of ordinary memory put into the pipe, for a read to have something
/// to copy.
const DATA: u64 = SCRATCH + PAGE_SIZE + 0x40;
/// How many bytes each copy asks for.
const LEN: u64 = 64;

/// What the check did, for the boot log.
#[derive(Debug)]
pub struct Report {
    /// Device windows a copy was asked through.
    pub windows: u32,
    /// Copies refused with `EFAULT`, the page never faulted in by one.
    pub refused: u32,
    /// Whether one of the windows was a device's own aperture.
    pub aperture: bool,
    /// Whether one lay between two runs of RAM, inside the direct map's span.
    pub inside: bool,
}

/// Run it.
///
/// Verifies: L.user.107, L.mm.62
pub fn run() -> Result<Report, &'static str> {
    let side = Side::new()?;
    let checked = copy_through_windows(&side);
    side.close_everything();
    checked
}

/// Make the pipe and the windows, and ask each for a copy both ways.
fn copy_through_windows(side: &Side) -> Result<Report, &'static str> {
    let process = &side.process;
    if pipe::sys_pipe2(process, FDS, O_NONBLOCK) != Ok(0) {
        return Err("pipe2 was refused");
    }
    let reader = descriptor(side, FDS)?;
    let writer = descriptor(side, FDS + 4)?;
    side.put(DATA, &[0x5a; LEN as usize])?;

    let mut report = Report {
        windows: 0,
        refused: 0,
        aperture: false,
        inside: false,
    };
    if let Some(at) = map_an_aperture(side)? {
        refuse_both_ways(side, at, reader, writer, &mut report)?;
        // And once its page is there: the refusal is the region's, not the
        // translation's absence.
        side.process
            .space()
            .fault(at, Access::READ)
            .map_err(|_| "an aperture's page could not be faulted in")?;
        copy_refused(side, at, reader, writer, &mut report)?;
        report.aperture = true;
    }

    let past = mm::check::past_ram();
    let venus = map_window(side, past)?;
    refuse_both_ways(side, venus, reader, writer, &mut report)?;

    if let Some(hole) = mm::check::hole_in_ram() {
        let inside = map_window(side, hole)?;
        refuse_both_ways(side, inside, reader, writer, &mut report)?;
        report.inside = true;
    }

    for end in [reader, writer] {
        if fd::sys_close(process, end) != Ok(0) {
            return Err("a pipe end would not close");
        }
    }
    Ok(report)
}

/// A descriptor `pipe2` wrote at `at`.
fn descriptor(side: &Side, at: u64) -> Result<i32, &'static str> {
    i32::try_from(side.get_u32(at)?).map_err(|_| "pipe2 wrote no descriptor")
}

/// Map a device's whole-page aperture through `io_mapping_create` and
/// `io_mapping_map`, as a driver would, and say where; `None` on a machine
/// whose firmware described none. The first such aperture, as the stage's
/// other device checks take.
fn map_an_aperture(side: &Side) -> Result<Option<u64>, &'static str> {
    let Some((node, aperture)) = device::devices()
        .iter()
        .filter(|node| !node.yielded())
        .find_map(|node| {
            node.apertures()
                .iter()
                .find(|aperture| aperture.whole_pages())
                .map(|aperture| (Arc::clone(node), *aperture))
        })
    else {
        return Ok(None);
    };
    if mm::direct_map_ram(aperture.phys()).is_some() {
        return Err("a device's aperture has a checked direct-map alias");
    }
    let handle = device_handle(side, &node)?;
    let spec: Vec<u8> = aperture
        .phys()
        .to_ne_bytes()
        .into_iter()
        .chain(aperture.len().to_ne_bytes())
        .collect();
    side.put(SPEC, &spec)?;
    let mapping = side.handle(
        nr::IO_MAPPING_CREATE,
        &[reg(handle), SPEC],
        "io_mapping_create of a device's own aperture failed",
    )?;
    let at = side
        .call(nr::IO_MAPPING_MAP, &[reg(mapping), 0])
        .map_err(|_| "io_mapping_map failed")?;
    Ok(Some(at as u64))
}

/// Map one page at `physical` as a cached window, as `mmap` of a render
/// node's blob does, and say where.
fn map_window(side: &Side, physical: u64) -> Result<u64, &'static str> {
    if mm::direct_map_ram(physical).is_some() {
        return Err("a page that is not RAM has a checked direct-map alias");
    }
    let keeper: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
    side.process
        .space()
        .map_window(
            FilePlace::Anywhere(None),
            PAGE_SIZE,
            WindowPages::all(physical, PAGE_SIZE),
            VmaFlags::READ_WRITE,
            true,
            keeper,
        )
        .map_err(|_| "a window of device memory could not be mapped")
}

/// Ask for a copy from and into the window at `at`, and require both to be
/// refused without its page having been faulted in.
fn refuse_both_ways(
    side: &Side,
    at: u64,
    reader: i32,
    writer: i32,
    report: &mut Report,
) -> Result<(), &'static str> {
    copy_refused(side, at, reader, writer, report)?;
    if mm::translate_in(side.process.space().root_table(), at).is_some() {
        return Err("a copy through a device's page faulted the page in before refusing it");
    }
    report.windows += 1;
    Ok(())
}

/// A `writev` from the window at `at` into the pipe and a `readv` out of the
/// pipe into it are each `EFAULT`, and `with_present_page` refuses the page
/// for either access: with the page absent it would otherwise have answered
/// that it must be faulted first.
fn copy_refused(
    side: &Side,
    at: u64,
    reader: i32,
    writer: i32,
    report: &mut Report,
) -> Result<(), &'static str> {
    let process = &side.process;
    stage_iovec(side, at)?;
    if file::sys_writev(process, writer, IOV, 1) != Err(Errno::EFAULT) {
        return Err("a writev from a device's page was not EFAULT");
    }
    report.refused += 1;
    if file::sys_write(process, writer, DATA, LEN) != Ok(LEN as usize) {
        return Err("a write of ordinary memory into the pipe came back short");
    }
    if file::sys_readv(process, reader, IOV, 1) != Err(Errno::EFAULT) {
        return Err("a readv into a device's page was not EFAULT");
    }
    report.refused += 1;
    for access in [Access::READ, Access::WRITE] {
        match process.space().with_present_page(at, access, |_| ()) {
            Err(SpaceError::Refused(_)) => {}
            _ => return Err("with_present_page did not refuse a device's page"),
        }
    }
    Ok(())
}

/// Stage a one-segment `iovec` of [`LEN`] bytes at `at`, in the kernel's
/// pointer width, which is the check process's.
fn stage_iovec(side: &Side, at: u64) -> Result<(), &'static str> {
    let width = size_of::<usize>();
    let base = at.to_le_bytes();
    let len = LEN.to_le_bytes();
    let (Some(base), Some(len)) = (base.get(..width), len.get(..width)) else {
        return Err("impossible pointer width");
    };
    side.put(IOV, base)?;
    side.put(IOV + width as u64, len)
}
