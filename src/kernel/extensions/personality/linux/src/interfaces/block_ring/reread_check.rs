//! The kernel acts on the completion it read, not on what a driver writes
//! over it afterwards (`docs/certification/SAFETY-MANUAL.md` AoU-3: the
//! uncertified load is untrusted).
//!
//! A ring-3 driver can write the ring VMO at any moment, including between
//! the kernel's read of a field and its use. `docs/BLOCK-RING.md` §4 answers
//! that by having the kernel read every field of the peer's once, into its
//! own memory, check the copy and act on the copy. This check plays a driver
//! that cheats in exactly that window: over one page of a VMO, read and
//! written through the same [`Pages`] a served ring uses, it runs a real
//! [`DriverSide`] against the kernel's [`KernelSide`], and once the driver
//! has posted a completion, rewrites each field of it -- the completion tail,
//! the id, `bytes_done` and the status -- the instant after the kernel's
//! first read of that field. Every rewrite is to a value that would pass the
//! kernel's checks: the id of the other request outstanding, no bytes done,
//! an I/O error, a second completion posted. A kernel that read a field
//! again after checking it would act on one of them; what is required is
//! the completion as first posted, the other request still outstanding, and
//! every rewrite having fired.
//!
//! It also counts every access narrower than the field to the shared
//! indices and want-bell flags, which must be none: they are `u32`s both
//! sides change while the other reads them, and must be read and written
//! whole (F-45's rule, in the block ring).

use core::cell::Cell;

use ferrix_blkring::layout::{completion, header};
use ferrix_blkring::{
    Completed, Consumed, Device, DeviceFlags, DriverSide, KernelSide, RingLayout, RingMemory, Slot,
    Status, Submission,
};
use ferrix_bootinfo::PAGE_SIZE;

use super::Pages;
use crate::user::vmo::Vmo;

/// The ring's entries: room for the two requests and no more.
const ENTRIES: u32 = 2;
/// The request the driver completes, and the other one outstanding.
const POSTED: u64 = 7;
const OTHER: u64 = 8;
/// One sector each.
const BLOCK: u32 = 512;

/// What the check measured.
#[derive(Debug)]
pub(crate) struct Report {
    /// Fields of a posted completion the driver rewrote after the kernel had
    /// read them, each of which the kernel ignored.
    pub(crate) rewrites: u32,
}

/// A field to rewrite the instant after the kernel first reads it.
#[derive(Clone, Copy, Debug)]
struct Rewrite {
    /// Its byte offset in the ring VMO.
    at: usize,
    /// What the driver writes there: the value's little-endian bytes, and
    /// how many of them the field has.
    bytes: [u8; 8],
    len: usize,
}

/// The kernel's view of the ring: the served ring's own [`Pages`], with a
/// driver behind it that rewrites fields once they have been read.
struct Cheating<'a> {
    pages: Pages,
    /// Armed rewrites; each fires once and is then `None`.
    rewrites: &'a [Cell<Option<Rewrite>>],
    /// How many have fired.
    fired: &'a Cell<u32>,
    /// Accesses to a shared index or want-bell flag narrower than the field.
    narrow: &'a Cell<u32>,
}

impl Cheating<'_> {
    /// The kernel has just read `len` bytes at `at`: fire what waits there.
    fn after_read(&self, at: usize, len: usize) {
        if (header::SUB_TAIL..header::RESERVED).contains(&at) && len < 4 {
            self.narrow.set(self.narrow.get().saturating_add(1));
        }
        for armed in self.rewrites {
            let Some(rewrite) = armed.get() else {
                continue;
            };
            if rewrite.at != at {
                continue;
            }
            armed.set(None);
            let bytes = rewrite.bytes.get(..rewrite.len).unwrap_or_default();
            if self.pages.copy_in(at as u64, bytes) {
                self.fired.set(self.fired.get().saturating_add(1));
            }
        }
    }

    /// A write narrower than a shared index, counted as a read is.
    fn before_write(&self, at: usize, len: usize) {
        if (header::SUB_TAIL..header::RESERVED).contains(&at) && len < 4 {
            self.narrow.set(self.narrow.get().saturating_add(1));
        }
    }
}

impl RingMemory for Cheating<'_> {
    fn read_u8(&self, offset: usize) -> u8 {
        let value = self.pages.read_u8(offset);
        self.after_read(offset, 1);
        value
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.before_write(offset, 1);
        self.pages.write_u8(offset, value);
    }

    fn barrier(&self) {
        self.pages.barrier();
    }

    fn read_u16(&self, offset: usize) -> u16 {
        let value = self.pages.read_u16(offset);
        self.after_read(offset, 2);
        value
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let value = self.pages.read_u32(offset);
        self.after_read(offset, 4);
        value
    }

    fn read_u64(&self, offset: usize) -> u64 {
        let value = self.pages.read_u64(offset);
        self.after_read(offset, 8);
        value
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.before_write(offset, 2);
        self.pages.write_u16(offset, value);
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.pages.write_u32(offset, value);
    }

    fn write_u64(&mut self, offset: usize, value: u64) {
        self.pages.write_u64(offset, value);
    }
}

/// A `len`-byte field holding `value`, to be written at `at`.
fn rewrite(at: usize, value: u64, len: usize) -> Cell<Option<Rewrite>> {
    Cell::new(Some(Rewrite {
        at,
        bytes: value.to_le_bytes(),
        len,
    }))
}

/// Run the check. `Err` names the first thing that was not true.
///
/// # Errors
///
/// What was not true.
pub(crate) fn run() -> Result<Report, &'static str> {
    let vmo = Vmo::new_anonymous(1).map_err(|_| "no memory for the rewrite check's ring")?;
    let held = vmo
        .hold(0, 1)
        .map_err(|_| "the rewrite check's ring could not be held")?;
    let device = Device::new(BLOCK, 1024, 1, DeviceFlags::default(), u64::from(BLOCK) * 4)
        .map_err(|_| "the rewrite check's device is not one HELLO could describe")?;
    let layout = RingLayout::standard(ENTRIES).map_err(|_| "the rewrite check's layout")?;
    if layout.ring_bytes() > PAGE_SIZE {
        return Err("the rewrite check's ring does not fit its page");
    }
    let mut driver = DriverSide::new(
        Pages::over(&held, PAGE_SIZE),
        layout.ring_bytes(),
        layout,
        device,
    )
    .map_err(|_| "the rewrite check's driver refused its own layout")?;

    let rewrites: [Cell<Option<Rewrite>>; 4] = Default::default();
    let fired = Cell::new(0);
    let narrow = Cell::new(0);
    let memory = Cheating {
        pages: Pages::over(&held, PAGE_SIZE),
        rewrites: &rewrites,
        fired: &fired,
        narrow: &narrow,
    };
    let mut storage = [Slot::EMPTY; ENTRIES as usize];
    let mut kernel = KernelSide::attach(memory, layout.ring_bytes(), device, &mut storage)
        .map_err(|_| "the kernel refused the rewrite check's ring")?;

    post(&mut kernel, &mut driver)?;

    // Posted. Now the driver cheats: each field, once the kernel has read
    // it, becomes something the kernel's checks would also have passed.
    let entry = layout.completion_at(0);
    let armed = [
        rewrite(header::COMP_TAIL, 2, 4),
        rewrite(entry + completion::ID, OTHER, 8),
        rewrite(entry + completion::BYTES_DONE, 0, 8),
        rewrite(
            entry + completion::STATUS,
            u64::from(Status::IoError.raw()),
            4,
        ),
    ];
    for (slot, armed) in rewrites.iter().zip(armed) {
        slot.set(armed.get());
    }
    let completed = kernel
        .poll()
        .map_err(|_| "the kernel called a cheated completion corrupt")?
        .ok_or("the kernel saw no completion where one was posted")?;
    first_read_acted_on(&completed, fired.get(), kernel.outstanding())?;

    // The driver is honest again: the tail as posted, then the
    // other read completed properly, which the kernel must still know.
    let honest = Pages::over(&held, PAGE_SIZE);
    let _ = honest.copy_in(header::COMP_TAIL as u64, &1_u32.to_le_bytes());
    if kernel
        .poll()
        .map_err(|_| "the kernel called an honest ring corrupt")?
        .is_some()
    {
        return Err("the kernel took the completion tail the driver rewrote after it was read");
    }
    driver
        .complete(OTHER, Status::Ok, u64::from(BLOCK))
        .map_err(|_| "the rewrite check's driver could not complete its second read")?;
    let _ = driver.publish();
    let second = kernel
        .poll()
        .map_err(|_| "the kernel called the honest second completion corrupt")?
        .ok_or("the kernel saw no second completion")?;
    if second.submission.id != OTHER || kernel.outstanding() != 0 {
        return Err("the kernel lost track of a request after a cheated completion");
    }
    if narrow.get() != 0 {
        return Err("the kernel read or wrote a shared ring index by halves (F-45)");
    }
    Ok(Report {
        rewrites: fired.get(),
    })
}

/// Two reads on the ring; the driver takes both and completes the first.
fn post(
    kernel: &mut KernelSide<'_, Cheating<'_>>,
    driver: &mut DriverSide<Pages>,
) -> Result<(), &'static str> {
    for (id, sector) in [(POSTED, 3), (OTHER, 5)] {
        kernel
            .submit(Submission::read(id, sector, 1, 0))
            .map_err(|_| "the kernel would not submit the rewrite check's read")?;
    }
    let _ = kernel.publish();
    for _ in 0..2 {
        match driver.consume() {
            Ok(Some(Consumed::Request(_))) => {}
            _ => return Err("the rewrite check's driver did not take both reads"),
        }
    }
    driver
        .complete(POSTED, Status::Ok, u64::from(BLOCK))
        .map_err(|_| "the rewrite check's driver could not complete its read")?;
    let _ = driver.publish();
    Ok(())
}

/// Require that the kernel took the completion as first posted, with
/// `outstanding` requests left, after every one of the rewrites `fired`.
fn first_read_acted_on(
    completed: &Completed,
    fired: u32,
    outstanding: usize,
) -> Result<(), &'static str> {
    if fired != 4 {
        return Err("the rewrite check's driver did not get to rewrite every field once read");
    }
    if completed.submission.id != POSTED {
        return Err("the kernel acted on a completion id the driver wrote after it was read");
    }
    if completed.bytes_done != u64::from(BLOCK) {
        return Err("the kernel acted on a length the driver wrote after it was read");
    }
    if completed.status != Status::Ok {
        return Err("the kernel acted on a status the driver wrote after it was read");
    }
    if outstanding != 1 {
        return Err(
            "the kernel's outstanding requests moved by a completion id rewritten after it was read",
        );
    }
    Ok(())
}
