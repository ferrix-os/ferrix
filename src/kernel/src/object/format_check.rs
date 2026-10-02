//! Every object of the core, and every reason the memory layer gives for a
//! refusal, formats for a diagnostic: completely, under its own name, and
//! without waiting for a lock.
//!
//! A diagnostic is written when something has already gone wrong -- a check
//! about to fail prints what it caught, a refused bring-up step prints its
//! error on the way to the panic report -- which is the one moment nothing
//! else has run the formatting code. Derived `Debug` is written by the
//! compiler, but what it calls is not: every lock's `Debug` formats without
//! waiting (`ferrix_sync`), and a report that waited for the lock the failure
//! was holding would never be written. So each object here is formatted whole,
//! through every lock it holds, and each error's text is compared with what
//! it has to say.
//!
//! The device objects -- an interrupt, an I/O mapping, a pin -- are formatted
//! by the checks that make them (`object::check::run_devices`), with
//! [`names`], because only those checks have a device to make them from.

use alloc::sync::Arc;
use core::fmt::{self, Debug, Display, Write};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_native_abi::signals::Signals;
use ferrix_vma::VmaFlags;

use crate::console::println;
use crate::mm::MemoryError;
use crate::mmio::Mmio;
use crate::object::channel::Endpoint;
use crate::object::check::Side;
use crate::object::job::{Job, NodeAttributes};
use crate::object::port::{Observer, Port};
use crate::object::process::ProcessRef;
use crate::object::{self, Object};
use crate::user::space::{Access, SpaceError};
use crate::user::vmo::{Vmo, VmoError};
use crate::vmap::{Stack, VmapError};

/// How much of a formatted value is kept to compare: more than any name or
/// text below.
const HEAD: usize = 96;

/// The first [`HEAD`] bytes of a formatted value, and how many it had in all.
struct Sink {
    /// The bytes kept.
    head: [u8; HEAD],
    /// Every byte written, kept or not.
    len: usize,
}

impl Write for Sink {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if let Some(slot) = self.head.get_mut(self.len) {
                *slot = byte;
            }
            self.len = self.len.saturating_add(1);
        }
        Ok(())
    }
}

impl Sink {
    /// `arguments`, formatted.
    fn of(arguments: fmt::Arguments<'_>) -> Result<Sink, &'static str> {
        let mut sink = Sink {
            head: [0; HEAD],
            len: 0,
        };
        sink.write_fmt(arguments)
            .map_err(|_| "a value's formatting reported an error")?;
        Ok(sink)
    }

    /// Whether what was written begins with `prefix`.
    fn begins(&self, prefix: &str) -> bool {
        prefix.len() <= self.len && self.head.get(..prefix.len()) == Some(prefix.as_bytes())
    }

    /// What was kept, for the console.
    fn kept(&self) -> &str {
        let kept = self.head.get(..self.len.min(HEAD)).unwrap_or(&[]);
        core::str::from_utf8(kept).unwrap_or("<not UTF-8>")
    }
}

/// Format `value` with `Debug` and require it to begin with `name`. Answers
/// how many bytes it came to.
///
/// # Errors
///
/// When it begins with something else, which the console shows.
pub(crate) fn names(value: &dyn Debug, name: &str) -> Result<usize, &'static str> {
    let sink = Sink::of(format_args!("{value:?}"))?;
    if !sink.begins(name) {
        println!("  format   expected {name:?}, formatted {:?}", sink.kept());
        return Err("a diagnostic does not begin with the name of what it formats");
    }
    Ok(sink.len)
}

/// Format `value` with `Display` and require it to begin with `text`.
fn says(value: &dyn Display, text: &str) -> Result<(), &'static str> {
    let sink = Sink::of(format_args!("{value}"))?;
    if !sink.begins(text) {
        println!("  format   expected {text:?}, formatted {:?}", sink.kept());
        return Err("an error's text is not what it has to say");
    }
    Ok(())
}

/// Format every error and every object below. Answers how many values were
/// formatted.
pub(crate) fn run() -> Result<u32, &'static str> {
    let errors = memory_errors()? + space_errors()?;
    Ok(errors + plain_values()? + objects()?)
}

/// What bring-up and the kernel arena say when they refuse, in words and by
/// variant.
fn memory_errors() -> Result<u32, &'static str> {
    says(
        &MemoryError::NoUsableMemory,
        "firmware reported no usable memory",
    )?;
    says(
        &MemoryError::NoRoomForPageArray(8192),
        "no usable region holds the 8192-byte page array",
    )?;
    says(
        &MemoryError::TooManyRamRuns(129),
        "RAM falls into 129 runs, more than can be recorded",
    )?;
    let vmap = [
        (VmapError::NotReady, "the vmap arena is not up yet"),
        (
            VmapError::BadLength(3),
            "3 is not a length a mapping can have",
        ),
        (
            VmapError::NoAddressSpace(8192),
            "no free range of 8192 bytes in the kernel arena",
        ),
        (VmapError::OutOfMemory, "no frame available"),
        (
            VmapError::MapFailed(ferrix_paging::MapError::OutOfMemory),
            "the mapping was refused: ",
        ),
        (
            VmapError::NotAllocated(0x1000),
            "0x1000 was not allocated by vmap",
        ),
        (
            VmapError::KernelImage(0x2000),
            "0x2000 is the kernel's own image, not a device",
        ),
    ];
    for (error, text) in vmap {
        says(&error, text)?;
    }
    let debugged = [
        (VmapError::NotReady, "NotReady"),
        (VmapError::BadLength(3), "BadLength(3)"),
        (VmapError::NoAddressSpace(8), "NoAddressSpace(8)"),
        (VmapError::OutOfMemory, "OutOfMemory"),
        (
            VmapError::MapFailed(ferrix_paging::MapError::OutOfMemory),
            "MapFailed(OutOfMemory)",
        ),
        (VmapError::NotAllocated(4), "NotAllocated(4)"),
        (VmapError::KernelImage(5), "KernelImage(5)"),
    ];
    for (error, name) in debugged {
        let _ = names(&error, name)?;
    }
    Ok(16)
}

/// What an address space and an object say when they refuse, by variant.
fn space_errors() -> Result<u32, &'static str> {
    let space = [
        (SpaceError::OutOfMemory, "OutOfMemory"),
        (SpaceError::NotUserRange(1), "NotUserRange(1)"),
        (SpaceError::BadRange, "BadRange"),
        (SpaceError::NotMapped(2), "NotMapped(2)"),
        (SpaceError::Refused(3), "Refused(3)"),
        (
            SpaceError::Backing(VmoError::OutOfMemory),
            "Backing(OutOfMemory)",
        ),
        (SpaceError::PastEnd(4), "PastEnd(4)"),
        (SpaceError::Unreadable(5), "Unreadable(5)"),
        (SpaceError::WindowFault(6), "WindowFault(6)"),
        (SpaceError::WindowChange, "WindowChange"),
    ];
    for (error, name) in space {
        let _ = names(&error, name)?;
    }
    let _ = names(
        &VmoError::OutOfRange { index: 5, pages: 4 },
        "OutOfRange { index: 5, pages: 4 }",
    )?;
    Ok(9)
}

/// Values with no lock in them: a kernel stack, a register window, a
/// cgroupfs node's owner.
fn plain_values() -> Result<u32, &'static str> {
    let _ = names(
        &Stack {
            base: 0x1000,
            top: 0x5000,
        },
        "Stack { base: 4096, top: 20480 }",
    )?;
    let _ = names(&Mmio::unmapped(), "Mmio { base: 0 }")?;
    let _ = names(
        &NodeAttributes {
            uid: 1,
            gid: 2,
            permissions: 0o755,
        },
        "NodeAttributes { uid: 1, gid: 2, permissions: 493 }",
    )?;
    Ok(3)
}

/// A process with a mapped, touched object; a channel with a registration on
/// a port; a job inside another: each formatted as a handle names it.
fn objects() -> Result<u32, &'static str> {
    let side = Side::new()?;
    let space = side.process.space();
    let vmo = Vmo::new_anonymous(2).map_err(|_| "no memory for the format check's VMO")?;
    let at = space
        .map_object(
            None,
            2 * PAGE_SIZE,
            Arc::clone(&vmo),
            0,
            VmaFlags::READ_WRITE,
        )
        .map_err(|_| "could not map the format check's VMO")?;
    space
        .fault(at, Access::WRITE)
        .map_err(|_| "could not touch the format check's VMO")?;
    let (near, _far) = Endpoint::pair().map_err(|_| "no memory for the format check's channel")?;
    let port = Port::new().map_err(|_| "no memory for the format check's port")?;
    let observer = Observer::new(&port, 1, Signals::PEER_CLOSED)
        .map_err(|_| "no memory for the format check's registration")?;
    near.observe(observer)
        .map_err(|_| "could not register on the format check's channel")?;
    let parent = Job::new_root().map_err(|_| "no memory for the format check's job")?;
    let child = parent
        .new_child()
        .map_err(|_| "no memory for the format check's inner job")?;

    let _ = names(&**space, "AddressSpace {")?;
    let _ = names(&**side.process, "Process {")?;
    let handled = [
        (Object::Vmo(vmo), "Vmo(Vmo {"),
        (Object::Channel(near), "Channel(Endpoint {"),
        (Object::Port(port), "Port(Port {"),
        (Object::Job(child), "Job(Job {"),
        (
            Object::Process(ProcessRef::new(&side.process)),
            "Process(ProcessRef {",
        ),
    ];
    for (object, name) in &handled {
        let _ = names(object, name)?;
    }
    space
        .unmap(at, 2 * PAGE_SIZE)
        .map_err(|_| "could not unmap the format check's VMO")?;
    object::dispose(handled.into_iter().map(|(object, _)| object));
    side.close_everything();
    Ok(7)
}
