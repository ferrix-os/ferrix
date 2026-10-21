//! Stage 10's self-check of the chardev core, with a fake driver played by
//! the kernel (the certification consultant's N12, ledger 294 and 297; the
//! fix of F-63).
//!
//! A process given a PCI function asks for a chardev control and says HELLO,
//! as `nvrm` does, every call through the native dispatcher from stage 9's
//! [`Side`]. The programs on the other side are kernel tasks acting for a
//! process of their own, through the core's own admission and wait, which is
//! what an open or ioctl on a node runs. No NVIDIA code is involved, and the
//! nodes are gone again when the check ends.
//!
//! What is required, on a machine with a PCI function:
//!
//! * a HELLO listing a minor no name is given for, one listing a minor
//!   twice, and one naming another device are each refused as the session
//!   says, and the kernel's end closes after each;
//! * the driver's end cannot be duplicated (N2);
//! * an accepted HELLO publishes `nvidia0`, mode 0666, and a second driver
//!   listing the same minor is refused as `Taken`;
//! * an ioctl goes to the driver with its command and argument, the driver
//!   copies the program's bytes in and its own out, and the program's call
//!   returns the driver's value; a copy after the reply is refused;
//! * a copy naming a request of another driver's control is refused;
//! * an abandoned request's copies are refused, and abandoning a request
//!   waits out a copy in flight (N4);
//! * an answered request's call waits out a copy in flight too (L1);
//! * the 257th outstanding request is `EBUSY`, and requests abandoned
//!   while the driver reads nothing never let the queue to it grow past
//!   its room (F-63);
//! * a dmabuf (N3b; the consultant's B10, ledger 316, and B1-B5, ledger
//!   726): unknown flags, a handle that is not a VMO, a VMO handle without
//!   `TRANSFER`, a writable dmabuf of a handle without `WRITE`, and a
//!   memfd's VMO with every right are each refused; an install while the
//!   cookie's last object is still leaving the table is `SHOULD_WAIT`;
//!   install and resolve for an abandoned request are refused and make
//!   nothing; a second install for the cookie gives the same object and
//!   says it made nothing, the cookie over another VMO is refused; resolve
//!   gives the cookie back and refuses no descriptor and a file that is no
//!   dmabuf; install and resolve for an answered request are refused;
//!   `PROT_EXEC` is refused by `mmap` (`EPERM`) and by `mprotect` of a
//!   mapping (`EACCES`); the driver hears one release, only after the last
//!   of two descriptors and a mapping went; `MAX_DMABUFS` objects live
//!   through the product's `find_or_make` and the next is `LIMIT_REACHED`;
//!   once the driver is gone nothing is made for its control, the next
//!   driver's control resolves the old descriptor `BAD_HANDLE`, and the
//!   dmabuf outlives its driver with its bytes, mapped again, its going
//!   after unheard (B7);
//! * a name-only dmabuf (ledger 632, N6): a VMO register that is not 0 --
//!   even one naming a VMO with `TRANSFER` --, a size of 0, not whole
//!   pages or over the bound, a VMO install with a size, and a writable
//!   name-only one (ledger 724, V3) are each refused; a live cookie asked in the other kind or with another size is
//!   `ALREADY_BOUND`; two installs give one object; `mmap`, shared or
//!   private, is `ENODEV`, exec through it is refused with no mapping made
//!   (V2), and `fstat` gives the size; resolve gives the
//!   cookie to its maker and `BAD_HANDLE` to another driver; one release
//!   after the last of two descriptors, and one after a `dup`'s close and
//!   its holder's exit (V1); and one kept past the driver's
//!   death still says its size, still maps nothing and closes unheard;
//! * a fence (N3b sync; ledger 647's S9 and its code review, ledger 725):
//!   unknown flags, a stray register, a positive or out-of-range status and
//!   a signal of no fence are refused with nothing held; a live cookie is
//!   `ALREADY_BOUND`; install for an answered, an abandoned and an unknown
//!   request is refused; `MAX_SYNC_FILES` fences through the product's
//!   `make` and the next `LIMIT_REACHED`; resolve gives the cookie and
//!   whether it is signalled, gives the kernel's own signalled fence (an
//!   empty dmabuf slot's export) as signalled with cookie 0 (a fence of a
//!   control that is gone, its last reference dropped, is never taken for
//!   it), and refuses no
//!   descriptor, a dmabuf, another driver's resolve and the next driver's; another driver's signal is
//!   refused; one fence is signalled once with 0 and wakes its queue, one
//!   past a short deadline reads `ETIMEDOUT` and refuses the late signal;
//!   `SYNC_IOC_FILE_INFO` answers as Linux and `MERGE` is `ENOTTY`; `read`
//!   `EINVAL`, `write` `EBADF`, `mmap` `ENODEV` and exec refused through
//!   the program's own calls; and a program blocked in `poll` on the kept
//!   fence wakes readable within a second of the driver's death, which
//!   reads `ENODEV`;
//! * when the driver goes, a waiting program wakes with `ENODEV` and the
//!   node is unpublished (N9);
//! * a render-only HELLO (`docs/INTEL-GPU.md` §4.2; ledger 2352, K3 (1)),
//!   the first function taken as the Intel integrated GPU
//!   (`device::FORCED_INTEL_GPU`): where its domain translates it publishes
//!   `renderD<N>` with N at least 128, served by its control and named for
//!   its device, no minor of major 195 and no card, all gone after; where it
//!   does not, it is refused `Untranslated`. With every domain taken as
//!   untranslated (`FORCED_UNTRANSLATED`), the same function's HELLO of the
//!   render node, and of `nvidia0` and the render node, are each refused
//!   `Untranslated` with nothing published, while the same HELLO of a
//!   function that is not the Intel GPU is taken as before (§4.4 (a),
//!   `L.object.192`);
//! * a dmabuf's fence slot (K5 of ledger 2352): an export with the slot
//!   empty gives a fence signalled already, and `EINVAL` for flags 0 or
//!   past read and write; `chardev_dmabuf_fence` with flags, for a cookie
//!   no live dmabuf of the control has, and for a live fence cookie is
//!   refused; an export gives the slot's own fence, unsignalled, and still
//!   that one after the slot is set again (a snapshot), the next export the
//!   new one, which the driver's signal signals; an import of the driver's
//!   own unsignalled fence and of a signalled one each reach the driver as
//!   an `Op::DmabufImport` naming the dmabuf, the flags and the fence, the
//!   slot unchanged, and the program's call answers what the driver
//!   answered; an import of a descriptor that is no fence is `EINVAL`, and
//!   nothing reaches the driver; another driver's fence call for the
//!   dmabuf is `BAD_STATE`, and its signal of the slot's fence too, the slot
//!   unchanged; after the driver's death the exported and the slot's fences
//!   read `ENODEV`, and an import is `ENODEV`;
//! * a VMO answered write-combining (K6 of ledger 2352): on x86-64 with
//!   `CLFLUSH` and every PAT programmed the driver's own anonymous VMO is
//!   made write-combining and a program's mapping of it reads back
//!   write-combining and not device, and `vmo_read` of it is `BAD_STATE`;
//!   everywhere else the answer is refused and the program's mmap is `EIO`,
//!   the VMO unchanged; a memfd's VMO and one an address space already
//!   maps are refused on every machine;
//! * replies out of order (C1 of ledger 2352): of two requests outstanding,
//!   the later answered first and then the earlier, each program gets its
//!   own answer, and a second reply to either is refused `BAD_STATE`.
//!
//! A machine with one PCI function checks everything but the two cases that
//! need a second driver, and says so.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use ferrix_chardevctl::message::{Hello, MAX_NODES, Message, Op, VERSION};
use ferrix_chardevctl::node::{MODE, RENDER_MINOR};
use ferrix_chardevctl::session::Refusal;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;

use super::{
    Answer, Ask, CONTROLS, Control, DRIVER_RIGHTS, MAX_OUTSTANDING, MapReply, Request, STARTING,
    abandon, admit, await_answer, copy_done, published,
};
use crate::device::{self, DeviceNode};
use crate::object::check::{SCRATCH, Side, device_handle, reg};
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::process::Process;
use crate::timer;

/// A message being sent.
const OUTBOX: u64 = SCRATCH;
/// A message received.
const INBOX: u64 = SCRATCH + 0x100;
/// A `ReadActual`.
const ACTUAL: u64 = SCRATCH + 0x200;
/// A wait's deadline.
const DEADLINE: u64 = SCRATCH + 0x210;
/// The signals a wait observed.
const OBSERVED: u64 = SCRATCH + 0x218;
/// Handles a read may carry.
const IN_HANDLES: u64 = SCRATCH + 0x220;
/// The driver's copy buffer, and the program's argument buffer.
const BUFFER: u64 = SCRATCH + 0x800;

/// What the program puts in its buffer before the ioctl.
const PING: [u8; 8] = *b"chardev>";
/// What the driver copies back.
const PONG: [u8; 8] = *b"<chardev";
/// The ioctl's command and the driver's answer.
const COMMAND: u32 = 0xC0DE_0001;
const VALUE: u64 = 7;
/// The program's ioctl, on file 1 of minor 0, its argument the buffer.
const IOCTL: Ask = Ask {
    op: Op::Ioctl,
    file: 1,
    minor: 0,
    cmd: COMMAND,
    arg: BUFFER,
    pages: 0,
};

/// How long the check waits for the control's task.
const PATIENCE_NANOS: u64 = 10_000_000_000;
/// How long a call that must still be waiting is given to return wrongly.
const STILL_NANOS: u64 = 50_000_000;
/// Rounds of taking requests in until `EBUSY` and abandoning them (F-63).
const ROUNDS: usize = 3;

/// What the check measured, for the boot log.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls and HELLOs refused with exactly the answer they had to get.
    pub(crate) refusals: u32,
    /// Requests answered, by the driver or with `ENODEV`.
    pub(crate) answered: u32,
    /// Requests taken in and abandoned across the F-63 rounds.
    pub(crate) abandoned: u32,
    /// The requests the queue held once answered ones filled it (F-63).
    pub(crate) most_queued: usize,
    /// dmabuf installs and resolves refused as specified (B10).
    pub(crate) dmabuf_refusals: u32,
    /// name-only dmabuf calls and mappings refused as specified (N6).
    pub(crate) name_refusals: u32,
    /// `sync_file` installs, signals and resolves refused as specified (S9).
    pub(crate) sync_refusals: u32,
    /// How long after its driver's death a program blocked in `poll` on a
    /// fence woke, in milliseconds (S9; at most [`DEATH_WAKE_MS`]).
    pub(crate) fence_death_wake_ms: u64,
    /// The render node a render-only HELLO published, if the function's
    /// domain translates; `None` where it was refused `Untranslated`.
    pub(crate) render_only: Option<u32>,
    /// HELLOs refused `Untranslated` (`L.object.192`).
    pub(crate) untranslated: u32,
    /// Requests answered out of the order they were made (C1).
    pub(crate) out_of_order: u32,
    /// dmabuf fence-slot calls, exports and imports checked (K5).
    pub(crate) slot_checks: u32,
    /// Write-combining VMO answers checked (K6); whether this machine makes
    /// one at all.
    pub(crate) combining_checks: u32,
    /// Whether a VMO was made write-combining here (x86-64 only).
    pub(crate) combining: bool,
    /// Why the cases needing a second driver were not checked.
    pub(crate) one_device: bool,
    /// Why nothing was checked.
    pub(crate) skipped: Option<&'static str>,
}

/// What a helper task is to do, and what it found.
enum Job {
    /// Wait for the request's answer, as a program's call does.
    Await(Arc<Control>, Arc<Request>, Arc<Process>),
    /// Abandon the request, as a signal makes a program's call do.
    Abandon(Arc<Control>, Arc<Request>),
    /// A program's `IMPORT_SYNC_FILE` on a dmabuf, its argument at the
    /// address given (K5).
    Import(Arc<Process>, Arc<super::dmabuf::Dmabuf>, u64),
}

/// The helper's job.
static JOB: SpinLock<Option<Job>> = SpinLock::new(None);
/// The helper's outcome, once it has one.
static OUTCOME: SpinLock<Option<Result<usize, Errno>>> = SpinLock::new(None);

/// Run the check. `Err` names the first thing that was not true.
///
/// # Errors
///
/// What was not true.
pub(crate) fn run() -> Result<Report, &'static str> {
    let nodes: Vec<Arc<DeviceNode>> = device::devices()
        .iter()
        // Not the function holding a lent firmware framebuffer, which
        // nobody drives (`ferrix.efifb`).
        .filter(|node| matches!(node.location(), device::Location::Pci(_)) && !node.yielded())
        .take(2)
        .cloned()
        .collect();
    let Some(first) = nodes.first() else {
        return Ok(Report {
            skipped: Some("no PCI function to serve nodes for"),
            ..Report::default()
        });
    };
    let mut report = Report {
        one_device: nodes.len() < 2,
        ..Report::default()
    };
    let driver = Side::new()?;
    let program = Side::new()?;
    refusals(&driver, first, &mut report)?;
    render_only(&driver, first, &mut report)?;
    let control = accepted(&driver, first, &[0])?;
    let core = control_of(first)?;
    duplicate_refused(&driver, control, &mut report)?;
    if super::file::metadata(0).permissions != MODE || published(0).is_none() {
        return Err("an accepted HELLO did not publish nvidia0, mode 0666");
    }
    let other = nodes.get(1).map(|second| -> Result<_, &'static str> {
        let side = Side::new()?;
        let device = device_handle(&side, second)?;
        let taken = create(&side, device)?;
        hello(&side, taken, location(second), &[0])?;
        expect_refused(&side, taken, Refusal::Taken, &mut report)?;
        let other = accepted(&side, second, &[1])?;
        Ok((side, other))
    });
    let other = other.transpose()?;
    let other_core = nodes.get(1).map(control_of).transpose()?;

    round_trip(
        &driver,
        control,
        &core,
        &program,
        other.as_ref(),
        &mut report,
    )?;
    abandoned(&driver, control, &core, &program, &mut report)?;
    answered_drains(&driver, control, &core, &program, &mut report)?;
    out_of_order(&driver, control, &core, &program, &mut report)?;
    combining_maps(&driver, control, &core, &program, &mut report)?;
    bounded(&driver, control, &core, &program, &mut report)?;
    let kept = dmabufs(&driver, control, &core, &program, &mut report)?;
    let foreign = other
        .as_ref()
        .zip(other_core.as_ref())
        .map(|((side, handle), core)| (side, *handle, core));
    let kept_name = names(&driver, control, &core, &program, foreign, &mut report)?;
    let fence = fences(&driver, control, &core, &program, &mut report)?;
    fence_refusals(
        &driver,
        control,
        &core,
        &program,
        foreign,
        [fence, kept.fd],
        &mut report,
    )?;
    let fenced = dmabuf_fences(&driver, control, &core, &program, foreign, &mut report)?;
    let since = start_poll(&program, fence)?;
    death(&driver, control, &core, &program, &mut report)?;
    report.fence_death_wake_ms = poll_woke(since)?;
    fenced_after_death(&program, &fenced, &mut report)?;
    after_death(&driver, first, &core, &program, &kept, &mut report)?;
    stale_fence_refused(&driver, first, &core, &program, fence, &mut report)?;
    // The dead control's last reference goes; its fence keeps its maker.
    drop(core);
    gone_control_fence_has_a_maker(&program, fence)?;
    fence_outlived(&program, fence)?;
    outlived(&program, &kept)?;
    name_outlived(&program, kept_name, &mut report)?;
    if let Some((side, other)) = other {
        close(&side, other)?;
        side.close_everything();
    }
    driver.close_everything();
    program.close_everything();
    settle()?;
    Ok(report)
}

/// Each HELLO the session refuses, refused, each on a control of its own
/// whose kernel end closes after the refusal.
fn refusals(side: &Side, node: &Arc<DeviceNode>, report: &mut Report) -> Result<(), &'static str> {
    let device = device_handle(side, node)?;
    let at = location(node);
    for (minors, wanted, wrong) in [
        (&[256_u16][..], Refusal::Minor, at),
        (&[3, 3][..], Refusal::Duplicate, at),
        (&[0][..], Refusal::Location, at ^ 1),
    ] {
        let control = create(side, device)?;
        hello(side, control, wrong, minors)?;
        expect_refused(side, control, wanted, report)?;
        settle()?;
        unclaimed(node)?;
    }
    Ok(())
}

/// K6: a driver's mmap answer of a VMO with `MAP_WRITE_COMBINING`.
///
/// Verifies: L.user.132, L.user.133
fn combining_maps(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    use ferrix_chardevctl::message::{MAP_VMO, MAP_WRITE_COMBINING};
    let wc = MAP_VMO | MAP_WRITE_COMBINING;
    // By the architecture first, not by what the product's own tests say,
    // so that a product test answering true off x86-64 is caught.
    let made_here = matches!(crate::arch::ARCH, ferrix_bootinfo::Arch::X86_64)
        && crate::arch::user_combining()
        && crate::object::io_mapping::combining_ready();
    report.combining = made_here;
    // The driver's own anonymous VMO, a page of it written through the
    // kernel's view first, so that `make_combining` has a frame to flush.
    let own = side.handle(nr::VMO_CREATE, &[4096], "vmo_create failed")?;
    side.put(BUFFER, &STAMP)?;
    side.put(DEADLINE, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(own), BUFFER, 8, DEADLINE])
        .map_err(|_| "vmo_write failed")?;
    let vmo = vmo_object(side, own)?;
    let answer = map_answer(side, control, core, program, own, wc)?;
    if made_here {
        let Some(Ok(Answer::Map(MapReply::Vmo { vmo: mapped, .. }))) = answer else {
            return Err("a write-combining answer of the driver's own VMO was refused");
        };
        if !Arc::ptr_eq(&mapped, &vmo) || !vmo.is_combining() {
            return Err("a write-combining answer did not make its VMO write-combining");
        }
        let at = combining_leaf(program, &vmo)?;
        coherent_syscalls(side, program, at)?;
        if side.call(nr::VMO_READ, &[reg(own), BUFFER, 8, DEADLINE]) != Err(status::BAD_STATE) {
            return Err("vmo_read of a write-combining VMO was not BAD_STATE");
        }
        report.combining_checks += 3;
    } else {
        if !matches!(answer, Some(Err(Errno::EIO))) || vmo.is_combining() {
            return Err("a write-combining VMO answer was taken where none may be made");
        }
        report.combining_checks += 1;
    }
    // Refused on every machine: a memfd's VMO, and one already mapped.
    let (memfd, file_vmo) = memfd_vmo(side, program)?;
    let mapped = side.handle(nr::VMO_CREATE, &[4096], "vmo_create failed")?;
    let held = vmo_object(side, mapped)?;
    let flags = ferrix_vma::VmaFlags {
        read: true,
        write: true,
        shared: true,
        ..ferrix_vma::VmaFlags::NONE
    };
    let _at = side
        .process
        .space()
        .map_object(None, 4096, Arc::clone(&held), 0, flags)
        .map_err(|_| "the driver could not map its own VMO")?;
    for (handle, what) in [
        (file_vmo, "a memfd's VMO was made write-combining"),
        (
            mapped,
            "a VMO an address space maps was made write-combining",
        ),
    ] {
        let answer = map_answer(side, control, core, program, handle, wc)?;
        if !matches!(answer, Some(Err(Errno::EIO))) || vmo_object(side, handle)?.is_combining() {
            return Err(what);
        }
        report.combining_checks += 1;
    }
    close_fd(&program.process, memfd)?;
    Ok(())
}

/// A program's mmap of one page, answered by the driver with `handle` and
/// the kind `a5`: what the program's call got.
fn map_answer(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    handle: Handle,
    a5: u64,
) -> Result<Option<Result<Answer, Errno>>, &'static str> {
    const PROT_RW: u32 = 3;
    const MAP_SHARED: u32 = 1;
    let request = admit(
        core,
        &program.process,
        Ask {
            op: Op::Mmap,
            file: 1,
            minor: 0,
            cmd: PROT_RW | (MAP_SHARED << 8),
            arg: 0,
            pages: 1,
        },
    )
    .map_err(|_| "an mmap was not taken in")?;
    let _ = side.call(
        nr::CHARDEV_REPLY,
        &[reg(control), request.wire.id, 0, reg(handle), 0, a5],
    );
    drain_channel(side, control);
    let answer = request.inner.lock().answer.clone();
    Ok(answer)
}

/// A write(2) from and a read(2) into the program's write-combining page at
/// `at`, through a memfd, each go through the coherent touch
/// (`AddressSpace::coherent_touch`, ledger 2368 (1)): the bytes come back,
/// and the touches counted grew by one for each of the four copies.
///
/// Verifies: L.user.133
fn coherent_syscalls(side: &Side, program: &Side, at: u64) -> Result<(), &'static str> {
    let before = crate::user::space::coherent_touches();
    program.put(at, &STAMP)?;
    let (memfd, file_vmo) = memfd_vmo(side, program)?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(file_vmo)]);
    let wrote = crate::syscall::file::sys_write(&program.process, memfd, at, 8);
    let read = crate::syscall::file::sys_pread64(&program.process, memfd, at + 8, 8, 0);
    let back = program.get(at + 8, 8)?;
    close_fd(&program.process, memfd)?;
    if wrote != Ok(8) || read != Ok(8) || back != STAMP {
        return Err("a write(2) from and a read(2) into a write-combining page did not round-trip");
    }
    if crate::user::space::coherent_touches() < before + 4 {
        return Err("a copy to or from a write-combining page was not made coherent");
    }
    Ok(())
}

/// The program's mapping of `vmo`, faulted in by a write, reads back
/// write-combining and not device; answers where.
fn combining_leaf(program: &Side, vmo: &Arc<crate::user::vmo::Vmo>) -> Result<u64, &'static str> {
    let space = program.process.space();
    let flags = ferrix_vma::VmaFlags {
        read: true,
        write: true,
        shared: true,
        ..ferrix_vma::VmaFlags::NONE
    };
    let at = space
        .map_object(None, 4096, Arc::clone(vmo), 0, flags)
        .map_err(|_| "a program could not map a write-combining VMO")?;
    space
        .fault(
            at,
            crate::user::space::Access {
                write: true,
                execute: false,
            },
        )
        .map_err(|_| "a write-combining VMO's page could not be faulted in")?;
    let leaf =
        crate::mm::flags_in(space.root_table(), at).ok_or("a write-combining page has no entry")?;
    if !leaf.write_combining || leaf.device {
        return Err("a program's mapping of a write-combining VMO was not write-combining");
    }
    Ok(at)
}

/// The dmabuf the fence-slot cases use, its cookie.
const FENCED: u64 = 0x00D3_AB5F;
/// The fences put in its slot, in order.
const SLOT: [u64; 3] = [0x5107_0001, 0x5107_0002, 0x5107_0003];
/// `DMA_BUF_SYNC_WRITE`, `DMA_BUF_SYNC_RW`.
const SYNC_WRITE: u32 = 2;
const SYNC_RW: u32 = 3;

/// What the fence-slot cases keep for after the driver's death.
struct Fenced {
    dmabuf: Arc<super::dmabuf::Dmabuf>,
    /// The program's descriptor of it.
    fd: i32,
    /// An exported descriptor of the slot's first fence, unsignalled.
    exported: i32,
}

/// An export on `dmabuf` by `program` with `flags`: the descriptor, or the
/// errno.
fn export(
    program: &Side,
    dmabuf: &super::dmabuf::Dmabuf,
    flags: u32,
) -> Result<Result<i32, Errno>, &'static str> {
    let mut argument = [0u8; 8];
    argument[..4].copy_from_slice(&flags.to_ne_bytes());
    argument[4..].copy_from_slice(&(-1_i32).to_ne_bytes());
    program.put(BUFFER, &argument)?;
    Ok(
        match super::dmabuf::ioctl(
            &program.process,
            dmabuf,
            super::dmabuf::DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
            BUFFER,
        ) {
            Ok(_) => Ok(program.get_u32(BUFFER + 4)? as i32),
            Err(why) => Err(why),
        },
    )
}

/// The fence a program's descriptor `fd` is.
fn fence_of(program: &Side, fd: i32) -> Option<Arc<super::sync::SyncFile>> {
    crate::syscall::fd::file(&program.process, fd)
        .ok()
        .and_then(|file| super::sync::of(file.io()))
}

/// K5: a dmabuf's fence slot, its export, its import, another driver's
/// attempt on it.
///
/// No item requirement names these: the chardev core is the load ring's,
/// and K5's rules are held by this check and its controls (ledger 2352 K5
/// (5)).
fn dmabuf_fences(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    foreign: Option<(&Side, Handle, &Arc<Control>)>,
    report: &mut Report,
) -> Result<Fenced, &'static str> {
    drain_channel(side, control);
    let (fd, dmabuf) = fenced_dmabuf(side, control, core, program)?;
    let set = |cookie: u64, fence: u64, flags: u64| {
        side.call(
            nr::CHARDEV_DMABUF_FENCE,
            &[reg(control), cookie, fence, 0, flags],
        )
    };
    // The slot empty: a fence signalled already; flags as Linux checks them.
    let stub =
        export(program, &dmabuf, SYNC_RW)?.map_err(|_| "an export of a dmabuf was refused")?;
    if !fence_of(program, stub).is_some_and(|fence| {
        fence
            .status(timer::now_nanos())
            .is_some_and(|(code, _)| code == 0)
    }) {
        return Err(
            "an export of a dmabuf with nothing in its slot did not give a signalled fence",
        );
    }
    for flags in [0, 4] {
        if export(program, &dmabuf, flags)? != Err(Errno::EINVAL) {
            return Err("an export with flags Linux refuses was not EINVAL");
        }
        report.slot_checks += 1;
    }
    for (refused, wanted, what) in [
        (
            set(FENCED, SLOT[0], 1),
            status::INVALID_ARGS,
            "a slot fence with flags was made",
        ),
        (
            set(FENCED + 1, SLOT[0], 0),
            status::BAD_STATE,
            "a slot fence was made for a dmabuf the control has not",
        ),
    ] {
        if refused != Err(wanted) {
            return Err(what);
        }
        report.slot_checks += 1;
    }
    let _ = set(FENCED, SLOT[0], 0).map_err(|_| "a dmabuf's slot fence was refused")?;
    if set(FENCED, SLOT[0], 0) != Err(status::ALREADY_BOUND) {
        return Err("a slot fence was made with a live fence's cookie");
    }
    report.slot_checks += 1;
    let first =
        export(program, &dmabuf, SYNC_WRITE)?.map_err(|_| "an export of a dmabuf was refused")?;
    let in_slot =
        super::dmabuf::slot(&dmabuf).ok_or("a slot fence was not in its dmabuf's slot")?;
    let exported = fence_of(program, first).ok_or("an export did not give a fence")?;
    if !Arc::ptr_eq(&exported, &in_slot) || exported.status(timer::now_nanos()).is_some() {
        return Err("an export did not give the slot's own unsignalled fence");
    }
    let _ = set(FENCED, SLOT[1], 0).map_err(|_| "a dmabuf's second slot fence was refused")?;
    let second =
        export(program, &dmabuf, SYNC_RW)?.map_err(|_| "an export of a dmabuf was refused")?;
    let snapshot = fence_of(program, first).is_some_and(|fence| fence.cookie() == SLOT[0]);
    let next = fence_of(program, second).is_some_and(|fence| fence.cookie() == SLOT[1]);
    if !snapshot || !next {
        return Err("an export was not a snapshot of the slot at its call");
    }
    let _ = side
        .call(nr::CHARDEV_SYNC_SIGNAL, &[reg(control), SLOT[1], 0])
        .map_err(|_| "the driver could not signal its slot fence")?;
    if !fence_of(program, second).is_some_and(|fence| {
        fence
            .status(timer::now_nanos())
            .is_some_and(|(code, _)| code == 0)
    }) {
        return Err("an exported slot fence did not read the driver's signal");
    }
    report.slot_checks += 3;
    imports(
        side,
        control,
        core,
        program,
        &dmabuf,
        (first, second),
        report,
    )?;
    foreign_set_refused(&dmabuf, foreign, report)?;
    // Unsignalled in the slot when the driver dies.
    let _ = set(FENCED, SLOT[2], 0).map_err(|_| "a dmabuf's third slot fence was refused")?;
    for fd in [stub, second] {
        close_fd(&program.process, fd)?;
    }
    Ok(Fenced {
        dmabuf,
        fd,
        exported: first,
    })
}

/// Another driver's fence call for the dmabuf, and its signal of the slot's
/// fence, are each `BAD_STATE`, the slot unchanged (K5 (2), (5)).
fn foreign_set_refused(
    dmabuf: &Arc<super::dmabuf::Dmabuf>,
    foreign: Option<(&Side, Handle, &Arc<Control>)>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let dmabuf = &**dmabuf;
    if let Some((other_side, other, _)) = foreign {
        let before = super::dmabuf::slot(dmabuf);
        let refused = other_side.call(
            nr::CHARDEV_DMABUF_FENCE,
            &[reg(other), FENCED, SLOT[2], 0, 0],
        );
        let signal = other_side.call(nr::CHARDEV_SYNC_SIGNAL, &[reg(other), SLOT[0], 0]);
        let same = match (before, super::dmabuf::slot(dmabuf)) {
            (Some(a), Some(b)) => Arc::ptr_eq(&a, &b),
            _ => false,
        };
        if refused != Err(status::BAD_STATE) || signal != Err(status::BAD_STATE) || !same {
            return Err("another driver set a dmabuf's slot or signalled its fence");
        }
        report.slot_checks += 2;
    }
    Ok(())
}

/// A dmabuf of a fresh VMO, cookie [`FENCED`], installed in `program`
/// through a request answered after.
fn fenced_dmabuf(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
) -> Result<(i32, Arc<super::dmabuf::Dmabuf>), &'static str> {
    let vmo = side.handle(nr::VMO_CREATE, &[DMABUF_BYTES], "vmo_create failed")?;
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let fd = side
        .call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, reg(vmo), FENCED, 0],
        )
        .map_err(|_| "a dmabuf for the slot cases was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    let _ = finish()?;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(vmo)]);
    crate::syscall::fd::file(&program.process, fd)
        .ok()
        .and_then(|file| super::dmabuf::of(file.io()))
        .map(|dmabuf| (fd, dmabuf))
        .ok_or("an installed dmabuf is not a dmabuf")
}

/// The imports: of the driver's own unsignalled fence and of a signalled
/// one, each reaching the driver as data with the slot unchanged and
/// answered with the driver's answer; of a descriptor that is no fence,
/// `EINVAL` with nothing sent.
fn imports(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    dmabuf: &Arc<super::dmabuf::Dmabuf>,
    (own, signalled): (i32, i32),
    report: &mut Report,
) -> Result<(), &'static str> {
    use ferrix_chardevctl::message::{IMPORT_OWN, IMPORT_SIGNALLED};
    let slot = super::dmabuf::slot(dmabuf).ok_or("the slot was empty")?;
    for (fd, kind, file, answer) in [
        (own, IMPORT_OWN, SLOT[0], 0_i64),
        (signalled, IMPORT_SIGNALLED, 0, -i64::from(Errno::EINVAL.0)),
    ] {
        let mut argument = [0u8; 8];
        argument[..4].copy_from_slice(&SYNC_WRITE.to_ne_bytes());
        argument[4..].copy_from_slice(&fd.to_ne_bytes());
        program.put(BUFFER + 0x100, &argument)?;
        start(Job::Import(
            Arc::clone(&program.process),
            Arc::clone(dmabuf),
            BUFFER + 0x100,
        ))?;
        let Some(Message::Request(wire)) = receive(side, control)? else {
            return Err("an import did not reach its driver as a REQUEST");
        };
        if wire.op != Op::DmabufImport
            || wire.arg != FENCED
            || wire.cmd != SYNC_WRITE
            || wire.pages != kind
            || wire.file != file
        {
            return Err("an import reached its driver naming another dmabuf, flag or fence");
        }
        if !super::dmabuf::slot(dmabuf).is_some_and(|now| Arc::ptr_eq(&now, &slot)) {
            return Err("an import changed the dmabuf's fence slot");
        }
        let _ = side
            .call(
                nr::CHARDEV_REPLY,
                &[reg(control), wire.id, answer as u64, 0],
            )
            .map_err(|_| "the driver's answer to an import was refused")?;
        let wanted = if answer == 0 {
            Ok(0)
        } else {
            Err(Errno::EINVAL)
        };
        if finish()? != wanted {
            return Err("an import did not answer what its driver answered");
        }
        report.slot_checks += 1;
    }
    let mut argument = [0u8; 8];
    argument[..4].copy_from_slice(&SYNC_WRITE.to_ne_bytes());
    argument[4..].copy_from_slice(&i32::MAX.to_ne_bytes());
    program.put(BUFFER + 0x100, &argument)?;
    let queued_before = queued(core);
    if super::dmabuf::import_as(&program.process, dmabuf, BUFFER + 0x100) != Err(Errno::EINVAL)
        || queued(core) != queued_before
    {
        return Err(
            "an import of a descriptor that is no fence was not EINVAL, or reached the driver",
        );
    }
    report.slot_checks += 1;
    Ok(())
}

/// After the driver's death: the exported fence and the slot's read
/// `ENODEV`, and an import is `ENODEV`.
fn fenced_after_death(
    program: &Side,
    fenced: &Fenced,
    report: &mut Report,
) -> Result<(), &'static str> {
    let enodev = -(Errno::ENODEV.0 as i32);
    let dead = |fd: i32| {
        fence_of(program, fd).is_some_and(|fence| {
            fence
                .status(timer::now_nanos())
                .is_some_and(|(code, _)| code == enodev)
        })
    };
    let now = export(program, &fenced.dmabuf, SYNC_RW)?
        .map_err(|_| "an export after its driver's death was refused")?;
    if !dead(fenced.exported) || !dead(now) {
        return Err("a dead driver's slot fences did not read ENODEV");
    }
    let mut argument = [0u8; 8];
    argument[..4].copy_from_slice(&SYNC_WRITE.to_ne_bytes());
    argument[4..].copy_from_slice(&now.to_ne_bytes());
    program.put(BUFFER + 0x100, &argument)?;
    if super::dmabuf::import_as(&program.process, &fenced.dmabuf, BUFFER + 0x100)
        != Err(Errno::ENODEV)
    {
        return Err("an import into a dead driver's dmabuf was not ENODEV");
    }
    for fd in [now, fenced.exported, fenced.fd] {
        close_fd(&program.process, fd)?;
    }
    report.slot_checks += 2;
    Ok(())
}

/// What the render-only cases force, put back however they end.
struct Forced;

impl Forced {
    /// Take `node` as the Intel integrated GPU, and every domain as
    /// translating or not by `untranslated`.
    fn set(node: &DeviceNode, untranslated: bool) -> Forced {
        device::FORCED_INTEL_GPU.store(node.index().saturating_add(1), Ordering::Release);
        device::FORCED_UNTRANSLATED.store(untranslated, Ordering::Release);
        Forced
    }
}

impl Drop for Forced {
    fn drop(&mut self) {
        device::FORCED_INTEL_GPU.store(0, Ordering::Release);
        device::FORCED_UNTRANSLATED.store(false, Ordering::Release);
    }
}

/// The render-only HELLO (K3 (1)) and the refusal of every node for an
/// Intel GPU whose DMA nobody translates (`L.object.192`).
///
/// The window in which a node is taken as the Intel GPU ([`Forced`]) opens
/// and closes here, at stage 10, before devmgr or any driver can start, so
/// no driver's HELLO for the node can fall in it. The function's domain is
/// made before the window opens, so that `DeviceNode::domain` never gives a
/// product domain the Intel GPU's budget: after the window the node's budget
/// is the one it had, and the kernel bounds it no more than before (ledger
/// 2356 (2)).
///
/// Verifies: L.object.192, H.DMA.10
fn render_only(
    side: &Side,
    node: &Arc<DeviceNode>,
    report: &mut Report,
) -> Result<(), &'static str> {
    let domain = node
        .domain()
        .map_err(|_| "no memory for a device's domain")?;
    let translated = domain.translated();
    let budget = crate::object::pin::counts(&domain).budget;
    let outcome = render_only_cases(side, node, translated, report);
    if crate::object::pin::bounded(&domain) || crate::object::pin::counts(&domain).budget != budget
    {
        return Err("taking a function as the Intel GPU gave its domain the Intel GPU's budget");
    }
    outcome
}

/// [`render_only`]'s cases, on `node`, whose domain translates or not by
/// `translated`.
fn render_only_cases(
    side: &Side,
    node: &Arc<DeviceNode>,
    translated: bool,
    report: &mut Report,
) -> Result<(), &'static str> {
    let device = device_handle(side, node)?;
    let at = location(node);
    let before = (
        super::published_minors(),
        crate::interfaces::render::node_indices(),
    );
    // Every domain taken as untranslated: the Intel GPU's HELLO is refused
    // whatever it lists, and nothing is published.
    {
        let _forced = Forced::set(node, true);
        for minors in [&[RENDER_MINOR][..], &[0, RENDER_MINOR][..]] {
            let control = create(side, device)?;
            hello(side, control, at, minors)?;
            expect_refused(side, control, Refusal::Untranslated, report)?;
            settle()?;
            unclaimed(node)?;
            report.untranslated += 1;
        }
    }
    if (
        super::published_minors(),
        crate::interfaces::render::node_indices(),
    ) != before
    {
        return Err("a HELLO refused Untranslated left a node published");
    }
    // A function that is not the Intel GPU is not held to it.
    {
        device::FORCED_UNTRANSLATED.store(true, Ordering::Release);
        let taken = accepted(side, node, &[RENDER_MINOR]);
        device::FORCED_UNTRANSLATED.store(false, Ordering::Release);
        close(
            side,
            taken.map_err(|_| "a render-only HELLO of a function not the Intel GPU was refused")?,
        )?;
        settle()?;
        unclaimed(node)?;
    }
    // The Intel GPU on the function's own domain.
    let _forced = Forced::set(node, false);
    if !translated {
        let control = create(side, device)?;
        hello(side, control, at, &[RENDER_MINOR])?;
        expect_refused(side, control, Refusal::Untranslated, report)?;
        settle()?;
        unclaimed(node)?;
        report.untranslated += 1;
        return Ok(());
    }
    let control = accepted(side, node, &[RENDER_MINOR])?;
    let published = render_published(node, &before.1);
    close(side, control)?;
    settle()?;
    unclaimed(node)?;
    let index = published?;
    if (
        super::published_minors(),
        crate::interfaces::render::node_indices(),
    ) != before
    {
        return Err("a render-only driver's render node was not gone after it");
    }
    report.render_only = Some(index);
    Ok(())
}

/// The one render node a render-only HELLO for `node` published, after
/// `before`: numbered from 128, served by `node`'s control and named for
/// `node`; no minor of major 195 and no card for it.
fn render_published(node: &Arc<DeviceNode>, before: &[u32]) -> Result<u32, &'static str> {
    let core = control_of(node)?;
    let new: Vec<u32> = crate::interfaces::render::node_indices()
        .into_iter()
        .filter(|index| !before.contains(index))
        .collect();
    let [index] = new.as_slice() else {
        return Err("a render-only HELLO did not publish exactly one render node");
    };
    if *index < 128 {
        return Err("a render-only driver's render node is numbered below 128");
    }
    if !super::render_control(*index).is_some_and(|serving| Arc::ptr_eq(&serving, &core))
        || crate::interfaces::render::node_device(*index) != Some(node.index())
    {
        return Err(
            "a render-only driver's render node is not served by its control for its device",
        );
    }
    if super::PUBLISHED
        .lock()
        .iter()
        .any(|(_, held)| Arc::ptr_eq(held, &core))
    {
        return Err("a render-only HELLO published a minor of major 195");
    }
    let cards = crate::interfaces::display::card_indices();
    if cards
        .iter()
        .filter_map(|card| crate::interfaces::display::card(*card))
        .any(|card| card.node == node.index())
    {
        return Err("a render-only HELLO made a card for its device");
    }
    Ok(*index)
}

/// Two requests outstanding, the later answered first: each program's call
/// gets its own answer, and a second reply to either is refused (C1 of
/// ledger 2352, which the Intel GPU's driver relies on to answer a wait
/// when its fence signals).
fn out_of_order(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let earlier = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    let later = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    reply(side, control, later.wire.id, VALUE + 1)?;
    reply(side, control, earlier.wire.id, VALUE)?;
    let answer = |request: &Request| match request.inner.lock().answer.clone() {
        Some(Ok(Answer::Value(value))) => Some(value),
        _ => None,
    };
    if answer(&later) != Some(VALUE as usize + 1) || answer(&earlier) != Some(VALUE as usize) {
        return Err("of two requests answered out of order, one did not get its own answer");
    }
    for request in [&earlier, &later] {
        if side.call(
            nr::CHARDEV_REPLY,
            &[reg(control), request.wire.id, 0, VALUE],
        ) != Err(status::BAD_STATE)
        {
            return Err("a second reply to an answered request was not refused BAD_STATE");
        }
        report.refusals += 1;
    }
    report.out_of_order += 2;
    report.answered += 2;
    drain_channel(side, control);
    Ok(())
}

/// A control on `node` whose HELLO for `minors` was accepted.
fn accepted(side: &Side, node: &Arc<DeviceNode>, minors: &[u16]) -> Result<Handle, &'static str> {
    let device = device_handle(side, node)?;
    let control = create(side, device)?;
    hello(side, control, location(node), minors)?;
    match receive(side, control)? {
        Some(Message::Ready(count)) if usize::from(count) == minors.len() => Ok(control),
        Some(Message::Refused(_)) => Err("a HELLO as specified was refused"),
        _ => Err("a HELLO as specified was not answered READY"),
    }
}

/// The driver's end of the control cannot be duplicated, and carries no
/// right beyond [`DRIVER_RIGHTS`] (N2).
fn duplicate_refused(
    side: &Side,
    control: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let rights = side
        .process
        .with_handles(|table| table.get(control).map(|(_, rights)| rights))
        .map_err(|_| "the driver's end is not in its table")?;
    if rights != DRIVER_RIGHTS || side.call(nr::HANDLE_DUPLICATE, &[reg(control), 0]).is_ok() {
        return Err("a driver's end of its chardev control could be duplicated");
    }
    report.refusals += 1;
    Ok(())
}

/// An ioctl there and back with a copy each way; then a copy after the
/// reply, and one through another driver's control, refused.
fn round_trip(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    other: Option<&(Side, Handle)>,
    report: &mut Report,
) -> Result<(), &'static str> {
    program.put(BUFFER, &PING)?;
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    if wire.op != Op::Ioctl || wire.cmd != COMMAND || wire.arg != BUFFER || wire.minor != 0 {
        return Err("an ioctl reached its driver with another command or argument");
    }
    if let Some((other_side, other_control)) = other {
        let staged = copy(other_side, *other_control, wire.id, nr::CHARDEV_COPY_IN, 8);
        if staged != Err(status::BAD_STATE) {
            return Err("a copy naming another driver's request was not refused");
        }
        report.refusals += 1;
    }
    let _ = copy(side, control, wire.id, nr::CHARDEV_COPY_IN, 8)
        .map_err(|_| "the driver could not copy the program's bytes in")?;
    if side.get(BUFFER, 8)? != PING {
        return Err("the driver's copy-in did not bring the program's bytes");
    }
    side.put(BUFFER, &PONG)?;
    let _ = copy(side, control, wire.id, nr::CHARDEV_COPY_OUT, 8)
        .map_err(|_| "the driver could not copy its bytes out")?;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return the driver's value");
    }
    if program.get(BUFFER, 8)? != PONG {
        return Err("the driver's copy-out did not reach the program's buffer");
    }
    report.answered += 1;
    if copy(side, control, wire.id, nr::CHARDEV_COPY_OUT, 8) != Err(status::BAD_STATE) {
        return Err("a copy after the reply was not refused");
    }
    report.refusals += 1;
    Ok(())
}

/// An abandoned request waits out a copy in flight, and its copies are
/// refused from then on (N4).
fn abandoned(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    in_flight(&request);
    start(Job::Abandon(Arc::clone(core), Arc::clone(&request)))?;
    still_waiting("an abandoned request's call returned while a copy for it was in flight")?;
    copy_done(&request);
    let _ = finish()?;
    if copy(side, control, request.wire.id, nr::CHARDEV_COPY_IN, 8) != Err(status::BAD_STATE) {
        return Err("a copy for an abandoned request was not refused");
    }
    report.refusals += 1;
    drain_channel(side, control);
    Ok(())
}

/// An answered request's call waits out a copy in flight (the consultant's
/// L1, ledger 297).
fn answered_drains(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    in_flight(&request);
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    reply(side, control, request.wire.id, VALUE)?;
    still_waiting("an answered request's call returned while a copy for it was in flight")?;
    copy_done(&request);
    if finish()? != Ok(VALUE as usize) {
        return Err("an answered request's call did not return its answer after the copy");
    }
    report.answered += 1;
    drain_channel(side, control);
    Ok(())
}

/// The 257th request is refused, and with the driver reading nothing the
/// queue to it never holds more than [`MAX_OUTSTANDING`] requests (F-63):
/// requests abandoned before their turn leave the queue at once, and
/// requests the driver answers before their turn -- it may guess their ids
/// -- still count against the queue's room until they leave it.
///
/// The steps before this one answer requests the control's task has not
/// taken off the queue yet: it skips them only when it next runs, woken by
/// an admit or `RECHECK_NANOS` (`super`) later. So the queue is read empty
/// first, or round 0 finds less room than [`MAX_OUTSTANDING`] (fl-071).
fn bounded(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    emptied(
        side,
        control,
        core,
        "answered requests stayed in the queue before its room was measured",
    )?;
    for round in 0..ROUNDS {
        let taken = fill(core, program)?;
        if taken.len() != MAX_OUTSTANDING {
            return Err(if round == 0 {
                "an empty queue to a driver that reads nothing refused a request before its room was full"
            } else {
                "after every request was abandoned a new one was refused"
            });
        }
        report.refusals += 1;
        for request in &taken {
            abandon(core, request);
        }
        report.abandoned += u32::try_from(taken.len()).unwrap_or(u32::MAX);
        if queued(core) != 0 {
            return Err("abandoned requests stayed in the queue to a driver that reads nothing");
        }
    }
    let answered = fill(core, program)?;
    for request in &answered {
        reply(side, control, request.wire.id, VALUE)?;
    }
    let more = fill(core, program)?;
    let most = queued(core);
    report.most_queued = most;
    if most > MAX_OUTSTANDING {
        return Err("the queue to a driver that reads nothing held more requests than its room");
    }
    for request in &more {
        abandon(core, request);
    }
    report.abandoned += u32::try_from(more.len()).unwrap_or(u32::MAX);
    emptied(
        side,
        control,
        core,
        "answered requests stayed in the queue after its driver read again",
    )
}

/// The cookie the check names its dmabuf by.
const COOKIE: u64 = 0x00D3_AB0F;
/// The cookie the fences are named by.
const FENCE: u64 = 0x000F_E0C3;
/// Its buffer: two pages.
const DMABUF_BYTES: u64 = 2 * 4096;
/// What the driver writes in it, and the program reads through a mapping.
const STAMP: [u8; 8] = *b"dmabuf<>";

/// A descriptor the check keeps open past the driver's death, with its
/// mapping's address.
struct Kept {
    fd: i32,
    at: u64,
    /// Its VMO, for a make after the driver's death.
    vmo: Arc<crate::user::vmo::Vmo>,
}

/// The dmabuf calls (B10): the refusals, one object a cookie, resolve, and
/// one release after the last descriptor and mapping. Answers with a
/// descriptor and a mapping kept for [`outlived`].
///
/// Verifies: L.user.126
fn dmabufs(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<Kept, &'static str> {
    use ferrix_native_abi::rights::Rights;
    use ferrix_native_abi::types::{DMABUF_MADE, DMABUF_TELL_MADE, DMABUF_WRITABLE};
    drain_channel(side, control);
    let vmo = side.handle(nr::VMO_CREATE, &[DMABUF_BYTES], "vmo_create failed")?;
    let other = side.handle(nr::VMO_CREATE, &[DMABUF_BYTES], "vmo_create failed")?;
    let held = |rights: u32| -> Result<Handle, &'static str> {
        side.handle(
            nr::HANDLE_DUPLICATE,
            &[reg(vmo), u64::from(rights)],
            "handle_duplicate of a VMO failed",
        )
    };
    let no_transfer = held(Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0)?;
    let read_only = held(Rights::READ.0 | Rights::TRANSFER.0 | Rights::MAP.0)?;
    side.put(BUFFER, &STAMP)?;
    // `vmo_write` reads its offset from memory.
    side.put(DEADLINE, &0_u64.to_ne_bytes())?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), BUFFER, 8, DEADLINE])
        .map_err(|_| "vmo_write failed")?;

    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let install = |handle: Handle, cookie: u64, flags: u64| {
        side.call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, reg(handle), cookie, flags],
        )
    };
    handle_refusals(&install, [vmo, control, no_transfer, read_only], report)?;
    let (memfd, file_vmo) = memory_refusals(side, core, program, &install, vmo, report)?;
    abandoned_refused(side, control, core, program, vmo, report)?;
    let first = install(vmo, COOKIE, DMABUF_TELL_MADE | DMABUF_WRITABLE)
        .map_err(|_| "a dmabuf of a whole anonymous VMO was refused")?;
    let second = install(vmo, COOKIE, DMABUF_TELL_MADE)
        .map_err(|_| "a second dmabuf install for a live cookie was refused")?;
    if first & DMABUF_MADE == 0 || second & DMABUF_MADE != 0 {
        return Err("a dmabuf install did not say rightly whether it made the object");
    }
    let first = (first & !DMABUF_MADE) as i32;
    let second = (second & !DMABUF_MADE) as i32;
    if !one_dmabuf(&program.process, first, second) {
        return Err("two installs for one cookie did not give one dmabuf");
    }
    if install(other, COOKIE, 0) != Err(status::ALREADY_BOUND) {
        return Err("a live cookie was installed over another VMO");
    }
    report.dmabuf_refusals += 1;
    let resolve = |fd: i32| {
        side.call(
            nr::CHARDEV_DMABUF_RESOLVE,
            &[reg(control), wire.id, fd as u64, BUFFER],
        )
    };
    let _ = resolve(second).map_err(|_| "a dmabuf this control made did not resolve")?;
    if side.get(BUFFER, 8)? != COOKIE.to_ne_bytes() {
        return Err("a dmabuf resolved to another cookie");
    }
    // No descriptor, and a file that is no dmabuf of this core's -- as
    // virtio-gpu's exported buffers are another kind of file, which `of`
    // does not take either.
    if resolve(i32::MAX - 1) != Err(status::BAD_HANDLE) || resolve(memfd) != Err(status::BAD_HANDLE)
    {
        return Err("a descriptor that is no dmabuf resolved");
    }
    report.dmabuf_refusals += 2;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return after its dmabufs");
    }
    report.answered += 1;
    if install(vmo, COOKIE + 1, 0) != Err(status::BAD_STATE)
        || resolve(second) != Err(status::BAD_STATE)
    {
        return Err("a dmabuf call for an answered request was not refused");
    }
    report.dmabuf_refusals += 1;

    one_release(side, control, program, first, second, report)?;
    capped(core, &vmo_object(side, vmo)?)?;
    report.dmabuf_refusals += 1;

    let (fd, at) = keep_one(side, control, core, program, vmo, report)?;
    let kept_vmo = vmo_object(side, vmo)?;
    close_fd(&program.process, memfd)?;
    for handle in [vmo, other, no_transfer, read_only, file_vmo] {
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
    }
    if super::dmabuf::alive(core) != 1 {
        return Err("a control counted other than one live dmabuf");
    }
    Ok(Kept {
        fd,
        at,
        vmo: kept_vmo,
    })
}

/// Whether descriptors `first` and `second` of `process` are one dmabuf.
fn one_dmabuf(process: &Process, first: i32, second: i32) -> bool {
    let objects: Vec<_> = [first, second]
        .iter()
        .filter_map(|fd| crate::syscall::fd::file(process, *fd).ok())
        .filter_map(|file| super::dmabuf::of(file.io()))
        .collect();
    matches!(objects.as_slice(), [one, two] if Arc::ptr_eq(one, two))
}

/// A dmabuf install's arguments: VMO handle, cookie, flags.
type VmoInstall<'a> = &'a dyn Fn(Handle, u64, u64) -> Result<usize, Errno>;

/// The handle refusals: an unknown flag, no VMO, no `TRANSFER`, and a
/// writable one without `WRITE` (B10).
fn handle_refusals(
    install: VmoInstall<'_>,
    handles: [Handle; 4],
    report: &mut Report,
) -> Result<(), &'static str> {
    use ferrix_native_abi::types::DMABUF_WRITABLE;
    let [vmo, control, no_transfer, read_only] = handles;
    for (handle, flags, wanted, what) in [
        (
            vmo,
            1 << 9,
            status::INVALID_ARGS,
            "a dmabuf with an unknown flag was installed",
        ),
        (
            control,
            0,
            status::WRONG_TYPE,
            "a dmabuf of a handle that is no VMO was installed",
        ),
        (
            no_transfer,
            0,
            status::ACCESS_DENIED,
            "a dmabuf of a VMO without TRANSFER was installed",
        ),
        (
            read_only,
            DMABUF_WRITABLE,
            status::ACCESS_DENIED,
            "a writable dmabuf of a VMO without WRITE was installed",
        ),
    ] {
        if install(handle, COOKIE, flags) != Err(wanted) {
            return Err(what);
        }
        report.dmabuf_refusals += 1;
    }
    Ok(())
}

/// The memory refusals: a memfd's VMO, a VMO of no pages, and a cookie whose
/// last object is still going. Answers the memfd and its VMO's handle.
fn memory_refusals(
    side: &Side,
    core: &Arc<Control>,
    program: &Side,
    install: VmoInstall<'_>,
    vmo: Handle,
    report: &mut Report,
) -> Result<(i32, Handle), &'static str> {
    use ferrix_native_abi::types::DMABUF_TELL_MADE;
    // A file's VMO -- a memfd's, in tmpfs -- is not plain anonymous memory,
    // even with every right (L.user.126; the consultant's B5, ledger 726).
    let (memfd, file_vmo) = memfd_vmo(side, program)?;
    // The whole VMO, and never one of no pages (the consultant's (1)).
    let empty = side.handle(nr::VMO_CREATE, &[0], "vmo_create of no bytes failed")?;
    let refused = install(empty, COOKIE, 0);
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(empty)]);
    if refused != Err(status::INVALID_ARGS) {
        return Err("a dmabuf of a VMO of no pages was installed");
    }
    report.dmabuf_refusals += 1;
    if install(file_vmo, COOKIE, 0) != Err(status::INVALID_ARGS) {
        return Err("a dmabuf of a memfd's VMO was installed");
    }
    report.dmabuf_refusals += 1;
    // The cookie's last object is going: its entry is in the table, dead,
    // its drop not yet in. One staged so: SHOULD_WAIT, never a second
    // object (B2, ledger 726).
    core.dmabufs
        .lock()
        .try_reserve(1)
        .map_err(|_| "no memory for the check's dying dmabuf")?;
    // NOALLOC: reserved above.
    core.dmabufs.lock().push((COOKIE, alloc::sync::Weak::new()));
    let dying = install(vmo, COOKIE, DMABUF_TELL_MADE);
    core.dmabufs.lock().retain(|(held, _)| *held != COOKIE);
    if dying != Err(status::SHOULD_WAIT) {
        return Err("a dmabuf was made for a cookie whose last one was still going");
    }
    report.dmabuf_refusals += 1;
    Ok((memfd, file_vmo))
}

/// One release, after the last of two descriptors and a mapping; a mapping
/// past the VMO's end refused on the way.
fn one_release(
    side: &Side,
    control: Handle,
    program: &Side,
    first: i32,
    second: i32,
    report: &mut Report,
) -> Result<(), &'static str> {
    let at = map_dmabuf(&program.process, first)?;
    if read_mapped(&program.process, at)? != STAMP {
        return Err("a dmabuf's mapping did not show its VMO's bytes");
    }
    past_end_refused(&program.process, first)?;
    report.dmabuf_refusals += 1;
    close_fd(&program.process, first)?;
    close_fd(&program.process, second)?;
    if released(side, control)?.is_some() {
        return Err("a dmabuf was released while a mapping of it lived");
    }
    let _ = crate::syscall::memory::sys_munmap(&program.process, at, DMABUF_BYTES)
        .map_err(|_| "unmapping a dmabuf failed")?;
    if released(side, control)? != Some(COOKIE) {
        return Err("a dmabuf's driver did not hear its release after its last mapping went");
    }
    if released(side, control)?.is_some() {
        return Err("a dmabuf's release was heard twice");
    }
    Ok(())
}

/// One dmabuf kept past the driver's death (B7), made for a request of its
/// own, mapped, and refused `PROT_EXEC`: its descriptor and mapping.
fn keep_one(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    vmo: Handle,
    report: &mut Report,
) -> Result<(i32, u64), &'static str> {
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let fd = side
        .call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, reg(vmo), COOKIE + 2, 0],
        )
        .map_err(|_| "a dmabuf for a second cookie was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    let _ = finish()?;
    let at = map_dmabuf(&program.process, fd)?;
    exec_refused(&program.process, fd, at)?;
    report.dmabuf_refusals += 2;
    Ok((fd, at))
}

/// A request the program gave up on: install and resolve for it refused,
/// and nothing made (B5, ledger 726).
fn abandoned_refused(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    vmo: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    let before = super::dmabuf::alive(core);
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    abandon(core, &request);
    let id = request.wire.id;
    let installed = side.call(
        nr::CHARDEV_DMABUF_INSTALL,
        &[reg(control), id, reg(vmo), COOKIE + 3, 0],
    );
    let resolved = side.call(nr::CHARDEV_DMABUF_RESOLVE, &[reg(control), id, 0, BUFFER]);
    if installed != Err(status::BAD_STATE)
        || resolved != Err(status::BAD_STATE)
        || super::dmabuf::alive(core) != before
    {
        return Err("a dmabuf call for an abandoned request was not refused");
    }
    report.dmabuf_refusals += 2;
    drain_channel(side, control);
    Ok(())
}

/// A memfd of [`DMABUF_BYTES`] in `program`, and a handle in `side` to its
/// VMO with every right a dmabuf needs: the descriptor and the handle.
fn memfd_vmo(side: &Side, program: &Side) -> Result<(i32, Handle), &'static str> {
    use ferrix_native_abi::rights::Rights;
    const NAME_AT: u64 = BUFFER + 0x100;
    program.put(NAME_AT, b"dmabuf-check\0")?;
    let fd = crate::syscall::memfd::sys_memfd_create(&program.process, NAME_AT, 0)
        .map_err(|_| "memfd_create failed")? as i32;
    let _ = crate::syscall::fd::sys_ftruncate(&program.process, fd, DMABUF_BYTES as i64)
        .map_err(|_| "a memfd could not be sized")?;
    let file = crate::syscall::fd::file(&program.process, fd).map_err(|_| "a memfd went")?;
    let (object, _) = file
        .io()
        .mapping_at(0)
        .or_else(|| file.inode().mapping_at(0))
        .ok_or("a memfd has no VMO")?;
    let vmo = object
        .downcast::<crate::user::vmo::Vmo>()
        .map_err(|_| "a memfd's object is no VMO")?;
    let rights = Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::TRANSFER.0);
    let handle = side
        .process
        .with_handles(|table| table.insert(crate::object::Object::Vmo(vmo), rights))
        .map_err(|_| "no room for a memfd's VMO handle")?;
    Ok((fd, handle))
}

/// The VMO `handle` names in `side`.
fn vmo_object(side: &Side, handle: Handle) -> Result<Arc<crate::user::vmo::Vmo>, &'static str> {
    side.process.with_handles(|table| match table.get(handle) {
        Ok((crate::object::Object::Vmo(vmo), _)) => Ok(Arc::clone(vmo)),
        _ => Err("the check's VMO handle went"),
    })
}

/// A mapping of the dmabuf `fd` that reaches past its VMO refused, as one
/// starting inside it and one longer than it (the consultant's (1)).
fn past_end_refused(process: &Process, fd: i32) -> Result<(), &'static str> {
    use crate::syscall::memory::{MmapRequest, OffsetUnit};
    use ferrix_linux_abi::types::{MAP_SHARED, PROT_READ};
    for (offset, len) in [(4096, DMABUF_BYTES), (0, DMABUF_BYTES + 4096)] {
        let mapped = crate::syscall::memory::sys_mmap(
            process,
            &MmapRequest {
                addr: 0,
                len,
                prot: PROT_READ,
                flags: MAP_SHARED,
                fd: i64::from(fd),
                offset,
                unit: OffsetUnit::Bytes,
            },
        );
        if let Ok(at) = mapped {
            let _ = crate::syscall::memory::sys_munmap(process, at as u64, len);
            return Err("a dmabuf was mapped past its VMO's end");
        }
    }
    Ok(())
}

/// `PROT_EXEC` on a dmabuf refused both ways: `mmap` with it `EPERM`, and
/// `mprotect` of the mapping at `at` to it `EACCES` (B1, ledger 726).
fn exec_refused(process: &Process, fd: i32, at: u64) -> Result<(), &'static str> {
    use crate::syscall::memory::{MmapRequest, OffsetUnit};
    use ferrix_linux_abi::types::{MAP_SHARED, PROT_EXEC, PROT_READ};
    let mapped = crate::syscall::memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: DMABUF_BYTES,
            prot: PROT_READ | PROT_EXEC,
            flags: MAP_SHARED,
            fd: i64::from(fd),
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    );
    if mapped != Err(Errno::EPERM) {
        return Err("a dmabuf was mapped executable");
    }
    let made =
        crate::syscall::memory::sys_mprotect(process, at, DMABUF_BYTES, PROT_READ | PROT_EXEC);
    if made != Err(Errno::EACCES) {
        return Err("a dmabuf's mapping was made executable by mprotect");
    }
    Ok(())
}

/// [`super::dmabuf::MAX_DMABUFS`] objects alive on one control, through the
/// product's own `find_or_make`, and the next refused `LIMIT_REACHED`;
/// every slot given back after (B5).
fn capped(core: &Arc<Control>, vmo: &Arc<crate::user::vmo::Vmo>) -> Result<(), &'static str> {
    use super::dmabuf::{MAX_DMABUFS, find_or_make};
    let base = super::dmabuf::alive(core);
    let mut held = Vec::new();
    held.try_reserve_exact(MAX_DMABUFS)
        .map_err(|_| "no memory for the check's dmabufs")?;
    let mut cookie = COOKIE << 16;
    while base + held.len() < MAX_DMABUFS {
        let made = find_or_make(core, super::dmabuf::Backing::Vmo(Arc::clone(vmo)), cookie)
            .map_err(|_| "a dmabuf under the cap was refused")?;
        held.push(made);
        cookie += 1;
    }
    let over = find_or_make(core, super::dmabuf::Backing::Vmo(Arc::clone(vmo)), cookie).map(|_| ());
    drop(held);
    if over != Err(status::LIMIT_REACHED) {
        return Err("a dmabuf over the cap was made");
    }
    if super::dmabuf::alive(core) != base {
        return Err("dmabufs under the cap stayed alive after their last holder went");
    }
    Ok(())
}

/// Once its driver is gone, no dmabuf is made for the old control, and the
/// next driver's control does not resolve the old one's descriptor (B3,
/// B7; ledger 726).
fn after_death(
    side: &Side,
    node: &Arc<DeviceNode>,
    old: &Arc<Control>,
    program: &Side,
    kept: &Kept,
    report: &mut Report,
) -> Result<(), &'static str> {
    let before = super::dmabuf::alive(old);
    let made = super::dmabuf::find_or_make(
        old,
        super::dmabuf::Backing::Vmo(Arc::clone(&kept.vmo)),
        COOKIE + 4,
    )
    .map(|_| ());
    if made != Err(status::BAD_STATE) || super::dmabuf::alive(old) != before {
        return Err("a dmabuf was made for a control whose driver was gone");
    }
    report.dmabuf_refusals += 1;
    unclaimed(node)?;
    let control = accepted(side, node, &[0])?;
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let core = loop {
        let found = CONTROLS
            .lock()
            .iter()
            .find(|held| Arc::ptr_eq(&held.device, node) && !Arc::ptr_eq(held, old))
            .cloned();
        if let Some(found) = found {
            break found;
        }
        if timer::now_nanos() > deadline {
            return Err("a second driver's accepted control is not among the live ones");
        }
        sched::sleep_for(1_000_000);
    };
    let request = admit(&core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(&core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let resolved = side.call(
        nr::CHARDEV_DMABUF_RESOLVE,
        &[reg(control), wire.id, kept.fd as u64, BUFFER],
    );
    reply(side, control, wire.id, VALUE)?;
    let _ = finish()?;
    if resolved != Err(status::BAD_HANDLE) {
        return Err("a new driver's control resolved its dead predecessor's dmabuf");
    }
    report.dmabuf_refusals += 1;
    close(side, control)?;
    drop(core);
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while published(0).is_some() {
        if timer::now_nanos() > deadline {
            return Err("a node stayed published after its driver went");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// The status of the fence `fd` of `process` names now.
fn fence_status(process: &Process, fd: i32) -> Result<Option<i32>, &'static str> {
    let file = crate::syscall::fd::file(process, fd).map_err(|_| "a fence's descriptor went")?;
    let fence = super::sync::of(file.io()).ok_or("a fence's descriptor is no sync_file")?;
    Ok(fence.status(timer::now_nanos()).map(|(code, _)| code))
}

/// Fences (N3b sync, the consultant's S9): each refusal refused with
/// nothing held, one signalled once, one past a short deadline read as
/// `ETIMEDOUT` and signalled by the thread, the deadlines clamped, and one
/// kept unsignalled for the driver's death.
fn fences(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<i32, &'static str> {
    deadlines_clamped()?;
    drain_channel(side, control);
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        Arc::clone(&request),
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let install = |cookie: u64, ms: u64, flags: u64, unused: u64| {
        side.call(
            nr::CHARDEV_SYNC_INSTALL,
            &[reg(control), wire.id, cookie, ms, flags, unused],
        )
    };
    let signal = |cookie: u64, code: i64| {
        side.call(
            nr::CHARDEV_SYNC_SIGNAL,
            &[reg(control), cookie, code as u64],
        )
    };
    let before = super::sync::alive(core);
    for (refused, wanted, what) in [
        (
            install(FENCE, 0, 1 << 5, 0),
            status::INVALID_ARGS,
            "a fence with an unknown flag was installed",
        ),
        (
            install(FENCE, 0, 0, 1),
            status::INVALID_ARGS,
            "a fence with a stray register was installed",
        ),
        (
            signal(FENCE, 1),
            status::INVALID_ARGS,
            "a fence was signalled with a positive status",
        ),
        (
            signal(FENCE, -4096),
            status::INVALID_ARGS,
            "a fence was signalled past the errno range",
        ),
        (
            signal(FENCE, 0),
            status::BAD_STATE,
            "a cookie with no fence was signalled",
        ),
    ] {
        if refused != Err(wanted) {
            return Err(what);
        }
        report.sync_refusals += 1;
    }
    if super::sync::alive(core) != before {
        return Err("a refused fence call left a fence behind");
    }
    let once = install(FENCE, 0, 0, 0).map_err(|_| "a fence was refused")? as i32;
    if install(FENCE, 0, 0, 0) != Err(status::ALREADY_BOUND) {
        return Err("a live fence's cookie was installed again");
    }
    report.sync_refusals += 1;
    let resolve = |fd: i32| {
        side.call(
            nr::CHARDEV_SYNC_RESOLVE,
            &[reg(control), wire.id, fd as u64, BUFFER],
        )
    };
    if resolve(once) != Ok(0) || side.get(BUFFER, 8)? != FENCE.to_ne_bytes() {
        return Err("an unsignalled fence did not resolve to its cookie");
    }
    if resolve(i32::MAX - 1) != Err(status::BAD_HANDLE) {
        return Err("a descriptor that is no fence resolved");
    }
    report.sync_refusals += 1;
    kernel_fence_resolves(side, program, &resolve)?;
    signalled_once(program, once, &signal, &resolve, report)?;
    let late = timed_out(program, &install, &signal)?;
    report.sync_refusals += 1;
    let kept = install(FENCE + 2, u64::MAX, 0, 0).map_err(|_| "a third fence was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return after its fences");
    }
    report.answered += 1;
    if install(FENCE + 3, 0, 0, 0) != Err(status::BAD_STATE) {
        return Err("a fence for an answered request was installed");
    }
    report.sync_refusals += 1;
    close_fd(&program.process, once)?;
    close_fd(&program.process, late)?;
    if super::sync::alive(core) != 1 {
        return Err("a control counted other than one unsignalled fence");
    }
    Ok(kept)
}

/// The kernel's own signalled fence, what an empty dmabuf slot exports,
/// resolves as signalled with cookie 0, as Linux imports its stub (S9).
fn kernel_fence_resolves(
    side: &Side,
    program: &Side,
    resolve: &dyn Fn(i32) -> Result<usize, Errno>,
) -> Result<(), &'static str> {
    let stub = super::sync::signalled().map_err(|_| "the kernel's signalled fence was not made")?;
    let stub = super::sync::descriptor(&program.process, stub, true)
        .map_err(|_| "the kernel's signalled fence got no descriptor")?;
    side.put(BUFFER, &u64::MAX.to_ne_bytes())?;
    let resolved = resolve(stub);
    close_fd(&program.process, stub)?;
    if resolved != Ok(1) || side.get(BUFFER, 8)? != 0u64.to_ne_bytes() {
        return Err("the kernel's signalled fence did not resolve as signalled with cookie 0");
    }
    Ok(())
}

/// A fence whose control is gone, the control's last reference dropped,
/// still has a maker: it is never taken for the kernel's own, which resolve
/// answers for any control (S9).
fn gone_control_fence_has_a_maker(program: &Side, fd: i32) -> Result<(), &'static str> {
    let file = crate::syscall::fd::file(&program.process, crate::syscall::fd::arg(fd as u64))
        .map_err(|_| "the dead control's fence has no descriptor")?;
    let fence = super::sync::of(file.io()).ok_or("the dead control's descriptor is no fence")?;
    if fence.made_by_none() {
        return Err("a fence of a control that is gone read as the kernel's own");
    }
    Ok(())
}

/// A driver's deadline of 0 is the default, one past the maximum the
/// maximum, any other kept (S2).
fn deadlines_clamped() -> Result<(), &'static str> {
    use ferrix_native_abi::types::{SYNC_DEADLINE_DEFAULT_MS, SYNC_DEADLINE_MAX_MS};
    if super::sync::deadline_ms(0) != SYNC_DEADLINE_DEFAULT_MS
        || super::sync::deadline_ms(u64::MAX) != SYNC_DEADLINE_MAX_MS
        || super::sync::deadline_ms(3) != 3
    {
        return Err("a fence's deadline was not clamped as specified");
    }
    Ok(())
}

/// A fence with a short deadline reads `ETIMEDOUT` once it passes, and the
/// driver's late signal is refused: its descriptor.
fn timed_out(
    program: &Side,
    install: &dyn Fn(u64, u64, u64, u64) -> Result<usize, Errno>,
    signal: &dyn Fn(u64, i64) -> Result<usize, Errno>,
) -> Result<i32, &'static str> {
    // A short deadline: ETIMEDOUT, and the thread's signal takes the cookie
    // out of the table.
    let late =
        install(FENCE + 1, 1, 0, 0).map_err(|_| "a fence with a deadline was refused")? as i32;
    sched::sleep_for(STILL_NANOS);
    if fence_status(&program.process, late)? != Some(-(Errno::ETIMEDOUT.0 as i32)) {
        return Err("a fence past its deadline did not read as ETIMEDOUT");
    }
    if signal(FENCE + 1, 0) != Err(status::BAD_STATE) {
        return Err("a fence past its deadline took the driver's signal");
    }
    Ok(late)
}

/// One fence signalled once with 0: unsignalled before, its queue woken,
/// `SYNC_IOC_FILE_INFO` as Linux answers it, a second signal refused by the
/// call and by the object, and resolve saying it is signalled (S2, S6).
fn signalled_once(
    program: &Side,
    once: i32,
    signal: &dyn Fn(u64, i64) -> Result<usize, Errno>,
    resolve: &dyn Fn(i32) -> Result<usize, Errno>,
    report: &mut Report,
) -> Result<(), &'static str> {
    if fence_status(&program.process, once)?.is_some() {
        return Err("a fence read as signalled before it was");
    }
    let woken = fence_wakes(&program.process, once)?;
    if signal(FENCE, 0).is_err() || fence_status(&program.process, once)? != Some(0) {
        return Err("a fence was not signalled once with 0");
    }
    if fence_wakes(&program.process, once)? == woken {
        return Err("a fence's signal did not wake its queue");
    }
    file_info(program, once, report)?;
    // Twice through the driver's call, whose cookie is out of the table
    // now, and twice on the object itself, as a racing deadline or death
    // would: refused both ways, the status unchanged (S2).
    let file = crate::syscall::fd::file(&program.process, once)
        .map_err(|_| "a fence's descriptor went")?;
    let object = super::sync::of(file.io()).ok_or("a fence's descriptor is no sync_file")?;
    if signal(FENCE, -5) != Err(status::BAD_STATE)
        || object.signal(-5)
        || fence_status(&program.process, once)? != Some(0)
    {
        return Err("a fence was signalled twice");
    }
    report.sync_refusals += 1;
    if resolve(once) != Ok(1) {
        return Err("a signalled fence did not resolve as signalled");
    }
    Ok(())
}

/// How often the fence `fd` of `process` names has woken its queue.
fn fence_wakes(process: &Process, fd: i32) -> Result<Option<u64>, &'static str> {
    use ferrix_vfs::Inode;
    let file = crate::syscall::fd::file(process, fd).map_err(|_| "a fence's descriptor went")?;
    let fence = super::sync::of(file.io()).ok_or("a fence's descriptor is no sync_file")?;
    Ok(fence.poll_changes())
}

/// `SYNC_IOC_FILE_INFO` through the program's memory, as Linux answers it
/// for one signalled fence, and its refusals (S6).
fn file_info(program: &Side, fd: i32, report: &mut Report) -> Result<(), &'static str> {
    const FILE_INFO: u32 = 0xC038_3E04;
    const MERGE: u32 = 0xC030_3E03;
    let file =
        crate::syscall::fd::file(&program.process, fd).map_err(|_| "a fence's descriptor went")?;
    let fence = super::sync::of(file.io()).ok_or("a fence's descriptor is no sync_file")?;
    let ask = |info: &[u8; 56]| -> Result<Result<usize, Errno>, &'static str> {
        program.put(BUFFER, info)?;
        Ok(super::sync::ioctl(
            &program.process,
            &fence,
            FILE_INFO,
            BUFFER,
        ))
    };
    let word = |bytes: &[u8], at: usize| -> u32 {
        let mut word = [0u8; 4];
        word.copy_from_slice(bytes.get(at..at + 4).unwrap_or(&[0; 4]));
        u32::from_ne_bytes(word)
    };
    // num_fences 0: the count, no array.
    if ask(&[0u8; 56])? != Ok(0) {
        return Err("SYNC_IOC_FILE_INFO with no array was refused");
    }
    let got = program.get(BUFFER, 56)?;
    if word(&got, 32) != 1 || word(&got, 40) != 1 || !got.starts_with(b"nvidia-drm") {
        return Err("SYNC_IOC_FILE_INFO did not say one signalled fence");
    }
    // num_fences 1: one entry, at an array in the program's memory.
    let mut info = [0u8; 56];
    info[40..44].copy_from_slice(&1u32.to_ne_bytes());
    info[48..56].copy_from_slice(&(BUFFER + 0x100).to_ne_bytes());
    if ask(&info)? != Ok(0) || word(&program.get(BUFFER + 0x100, 80)?, 64) != 1 {
        return Err("SYNC_IOC_FILE_INFO did not write its fence's entry");
    }
    let mut flagged = [0u8; 56];
    flagged[36] = 1;
    let mut stray = info;
    stray[48..56].copy_from_slice(&u64::MAX.to_ne_bytes());
    for (refused, wanted, what) in [
        (
            ask(&flagged)?,
            Errno::EINVAL,
            "SYNC_IOC_FILE_INFO with flags set was answered",
        ),
        (
            ask(&stray)?,
            Errno::EFAULT,
            "SYNC_IOC_FILE_INFO wrote through a bad pointer",
        ),
        (
            super::sync::ioctl(&program.process, &fence, MERGE, BUFFER),
            Errno::ENOTTY,
            "SYNC_IOC_MERGE was answered",
        ),
    ] {
        if refused != Err(wanted) {
            return Err(what);
        }
        report.sync_refusals += 1;
    }
    let mut byte = [0u8; 1];
    if ferrix_vfs::Inode::read_at(&*fence, 0, &mut byte) != Err(Errno::EINVAL)
        || ferrix_vfs::Inode::mapping_at(&*fence, 0).is_some()
    {
        return Err("a fence was read or mapped");
    }
    report.sync_refusals += 1;
    Ok(())
}

/// The longest a program blocked in `poll` on a fence may take to wake
/// after the fence's driver dies (S9).
pub(crate) const DEATH_WAKE_MS: u64 = 1000;

/// The rest of S9 (647's code review, B4): install for an abandoned
/// request and for a request id this control does not have, refused;
/// [`super::sync::MAX_SYNC_FILES`] fences through the product's `make` and
/// the next `LIMIT_REACHED`; resolve of a dmabuf descriptor `BAD_HANDLE`;
/// with a second driver, its signal and its resolve of this control's
/// fence refused and the fence left unsignalled; and through the
/// program's own calls, `read` and `write` `EINVAL`, `mmap` `ENODEV` and
/// exec refused. `fds` are the kept fence and a dmabuf.
fn fence_refusals(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    foreign: Option<(&Side, Handle, &Arc<Control>)>,
    [fence, dmabuf]: [i32; 2],
    report: &mut Report,
) -> Result<(), &'static str> {
    drain_channel(side, control);
    // An abandoned request, and one this control never had.
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    abandon(core, &request);
    for id in [request.wire.id, request.wire.id.wrapping_add(1 << 40)] {
        let installed = side.call(
            nr::CHARDEV_SYNC_INSTALL,
            &[reg(control), id, FENCE + 7, 0, 0, 0],
        );
        if installed != Err(status::BAD_STATE) {
            return Err("a fence was installed for a request that was not outstanding");
        }
        report.sync_refusals += 1;
    }
    drain_channel(side, control);
    fence_cap(core)?;
    report.sync_refusals += 1;
    // The dmabuf, resolved as a fence, for a request of its own.
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let resolved = side.call(
        nr::CHARDEV_SYNC_RESOLVE,
        &[reg(control), wire.id, dmabuf as u64, BUFFER],
    );
    reply(side, control, wire.id, VALUE)?;
    let _ = finish()?;
    if resolved != Err(status::BAD_HANDLE) {
        return Err("a dmabuf's descriptor resolved as a fence");
    }
    report.sync_refusals += 1;
    if let Some(foreign) = foreign {
        foreign_fence_refused(program, foreign, fence, report)?;
    }
    fence_syscalls(&program.process, fence)?;
    report.sync_refusals += 3;
    Ok(())
}

/// [`super::sync::MAX_SYNC_FILES`] fences alive on one control through the
/// product's `make`, the next `LIMIT_REACHED`, all gone again after.
fn fence_cap(core: &Arc<Control>) -> Result<(), &'static str> {
    use super::sync::{MAX_SYNC_FILES, alive, make};
    let base = alive(core);
    let mut held = Vec::new();
    held.try_reserve_exact(MAX_SYNC_FILES)
        .map_err(|_| "no memory for the check's fences")?;
    let mut cookie = FENCE << 16;
    while base + held.len() < MAX_SYNC_FILES {
        held.push(make(core, cookie, 1000).map_err(|_| "a fence under the cap was refused")?);
        cookie += 1;
    }
    let over = make(core, cookie, 1000).map(|_| ());
    drop(held);
    if over != Err(status::LIMIT_REACHED) {
        return Err("a fence over the cap was made");
    }
    if alive(core) != base {
        return Err("fences under the cap stayed in the table after their last holder went");
    }
    Ok(())
}

/// A second driver's signal of this control's fence `fd` names nothing,
/// and its resolve is `BAD_HANDLE`; the fence stays unsignalled.
fn foreign_fence_refused(
    program: &Side,
    (other_side, other_control, other_core): (&Side, Handle, &Arc<Control>),
    fd: i32,
    report: &mut Report,
) -> Result<(), &'static str> {
    drain_channel(other_side, other_control);
    let signalled = other_side.call(nr::CHARDEV_SYNC_SIGNAL, &[reg(other_control), FENCE + 2, 0]);
    if signalled != Err(status::BAD_STATE) || fence_status(&program.process, fd)?.is_some() {
        return Err("another driver signalled a fence it did not make");
    }
    report.sync_refusals += 1;
    let request = admit(other_core, &program.process, IOCTL)
        .map_err(|_| "an ioctl was not taken in by the second driver")?;
    start(Job::Await(
        Arc::clone(other_core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(other_side, other_control)? else {
        return Err("an ioctl did not reach the second driver as a REQUEST");
    };
    let resolved = other_side.call(
        nr::CHARDEV_SYNC_RESOLVE,
        &[reg(other_control), wire.id, fd as u64, BUFFER],
    );
    reply(other_side, other_control, wire.id, VALUE)?;
    let _ = finish()?;
    if resolved != Err(status::BAD_HANDLE) {
        return Err("another driver resolved a fence it did not make");
    }
    report.sync_refusals += 1;
    Ok(())
}

/// A fence through the program's own calls: `read` `EINVAL`, `write` `EBADF`,
/// `mmap` `ENODEV`, exec refused with nothing mapped (632's N5).
fn fence_syscalls(process: &Process, fd: i32) -> Result<(), &'static str> {
    if crate::syscall::file::sys_read(process, fd, BUFFER, 8) != Err(Errno::EINVAL) {
        return Err("a fence was read");
    }
    // Opened for reading only, as Linux opens one: `EBADF`.
    if crate::syscall::file::sys_write(process, fd, BUFFER, 8) != Err(Errno::EBADF) {
        return Err("a fence was written");
    }
    if mmap_errno(process, fd, ferrix_linux_abi::types::MAP_SHARED) != Err(Errno::ENODEV) {
        return Err("a fence was mapped");
    }
    not_executable(process, fd).map_err(|_| "a fence was executed")
}

/// The poll helper's outcome and when it came.
static POLLED: SpinLock<Option<(Result<usize, Errno>, u64)>> = SpinLock::new(None);
/// What the poll helper polls: the program and its fence.
static POLL_ON: SpinLock<Option<(Arc<Process>, i32)>> = SpinLock::new(None);
/// Where the poll helper's `struct pollfd` sits in the program's memory.
const POLLFD: u64 = BUFFER + 0x300;

/// Start a task blocked in the program's `poll` on its fence `fd`, and
/// require it still blocked a while later: the time to measure its wake
/// from.
fn start_poll(program: &Side, fd: i32) -> Result<u64, &'static str> {
    let mut pollfd = [0u8; 8];
    pollfd[..4].copy_from_slice(&fd.to_ne_bytes());
    pollfd[4..6].copy_from_slice(&1_i16.to_ne_bytes()); // POLLIN
    program.put(POLLFD, &pollfd)?;
    *POLLED.lock() = None;
    *POLL_ON.lock() = Some((Arc::clone(&program.process), fd));
    let _ = sched::spawn("chardev-poll", poll_helper, 0, ferrix_sched::NICE_0_WEIGHT)?;
    sched::sleep_for(STILL_NANOS);
    if POLLED.lock().is_some() {
        return Err("a poll on an unsignalled fence returned before its driver died");
    }
    Ok(timer::now_nanos())
}

/// The poll helper: the program's `poll` on its fence, with a limit well
/// past the bound, and when it returned.
fn poll_helper(_: usize) {
    let on = POLL_ON.lock().take();
    let outcome = match on {
        Some((process, _)) => crate::syscall::poll::sys_poll(&process, POLLFD, 1, 10_000),
        None => Err(Errno::EINVAL),
    };
    *POLLED.lock() = Some((outcome, timer::now_nanos()));
}

/// The poll helper woke with its fence readable within [`DEATH_WAKE_MS`] of
/// `since`: how many milliseconds it took.
fn poll_woke(since: u64) -> Result<u64, &'static str> {
    let deadline = since.saturating_add(PATIENCE_NANOS);
    loop {
        let polled = POLLED.lock().take();
        if let Some((outcome, at)) = polled {
            let took = at.saturating_sub(since) / 1_000_000;
            if outcome != Ok(1) || took > DEATH_WAKE_MS {
                return Err(
                    "a poll on a fence did not wake readable within a second of its driver's death",
                );
            }
            return Ok(took);
        }
        if timer::now_nanos() > deadline {
            return Err("a poll on a fence never woke after its driver's death");
        }
        sched::sleep_for(1_000_000);
    }
}

/// Nothing is made for the dead control `old`, and the next driver's
/// control resolves its dead predecessor's fence `fd`
/// `BAD_HANDLE`, for a request of its own.
fn stale_fence_refused(
    side: &Side,
    node: &Arc<DeviceNode>,
    old: &Arc<Control>,
    program: &Side,
    fd: i32,
    report: &mut Report,
) -> Result<(), &'static str> {
    // No fence is made for the dead control (725's B1).
    let before = super::sync::alive(old);
    if super::sync::make(old, FENCE + 8, 0).map(|_| ()) != Err(status::BAD_STATE)
        || super::sync::alive(old) != before
    {
        return Err("a fence was made for a control whose driver was gone");
    }
    report.sync_refusals += 1;
    unclaimed(node)?;
    let control = accepted(side, node, &[0])?;
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let core = loop {
        let found = CONTROLS
            .lock()
            .iter()
            .find(|held| Arc::ptr_eq(&held.device, node) && !Arc::ptr_eq(held, old))
            .cloned();
        if let Some(found) = found {
            break found;
        }
        if timer::now_nanos() > deadline {
            return Err("a second driver's accepted control is not among the live ones");
        }
        sched::sleep_for(1_000_000);
    };
    let request = admit(&core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(&core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let resolved = side.call(
        nr::CHARDEV_SYNC_RESOLVE,
        &[reg(control), wire.id, fd as u64, BUFFER],
    );
    reply(side, control, wire.id, VALUE)?;
    let _ = finish()?;
    if resolved != Err(status::BAD_HANDLE) {
        return Err("a new driver's control resolved its dead predecessor's fence");
    }
    report.sync_refusals += 1;
    close(side, control)?;
    drop(core);
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while published(0).is_some() {
        if timer::now_nanos() > deadline {
            return Err("a node stayed published after its driver went");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// After its driver went, the fence it never signalled reads `ENODEV`.
fn fence_outlived(program: &Side, fd: i32) -> Result<(), &'static str> {
    if fence_status(&program.process, fd)? != Some(-(Errno::ENODEV.0 as i32)) {
        return Err("a fence its driver never signalled did not read ENODEV after it went");
    }
    close_fd(&program.process, fd)
}

/// After its driver went, a dmabuf's mapping still shows its bytes, a new
/// mapping still works, and closing it all is heard by nobody (B7).
fn outlived(program: &Side, kept: &Kept) -> Result<(), &'static str> {
    if read_mapped(&program.process, kept.at)? != STAMP {
        return Err("a dmabuf's mapping lost its bytes when its driver went");
    }
    let again = map_dmabuf(&program.process, kept.fd)?;
    if read_mapped(&program.process, again)? != STAMP {
        return Err("a dmabuf could not be mapped again after its driver went");
    }
    for at in [kept.at, again] {
        let _ = crate::syscall::memory::sys_munmap(&program.process, at, DMABUF_BYTES)
            .map_err(|_| "unmapping a dmabuf failed")?;
    }
    close_fd(&program.process, kept.fd)
}

/// The cookie the check names its name-only dmabuf by, and the one it keeps
/// past the driver's death.
const NAME_COOKIE: u64 = 0x000E_D3AB;
/// The size the driver says its name-only dmabuf has: more than the check's
/// VMO, so nothing of that size could be mapped by mistake.
const NAME_BYTES: u64 = 16 * 4096;

/// The name-only dmabuf (ledger 632, N1-N6): its refusals, one object a
/// cookie, no mapping, resolve by its maker only, one release after the
/// last descriptor. Answers with a descriptor kept for [`name_outlived`].
fn names(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    foreign: Option<(&Side, Handle, &Arc<Control>)>,
    report: &mut Report,
) -> Result<i32, &'static str> {
    use ferrix_native_abi::types::{DMABUF_MADE, DMABUF_NAME_ONLY, DMABUF_TELL_MADE};
    drain_channel(side, control);
    let vmo = side.handle(nr::VMO_CREATE, &[DMABUF_BYTES], "vmo_create failed")?;
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let install = |vmo_register: u64, cookie: u64, flags: u64, size: u64| {
        side.call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, vmo_register, cookie, flags, size],
        )
    };
    let name = DMABUF_NAME_ONLY;
    register_refusals(&install, vmo, report)?;
    if super::dmabuf::alive(core) != 1 {
        return Err("a refused name-only install left a dmabuf behind");
    }
    let first = install(0, NAME_COOKIE, name | DMABUF_TELL_MADE, NAME_BYTES)
        .map_err(|_| "a name-only dmabuf was refused")?;
    let second = install(0, NAME_COOKIE, name | DMABUF_TELL_MADE, NAME_BYTES)
        .map_err(|_| "a second name-only install for a live cookie was refused")?;
    if first & DMABUF_MADE == 0 || second & DMABUF_MADE != 0 {
        return Err("a name-only install did not say rightly whether it made the object");
    }
    let first = (first & !DMABUF_MADE) as i32;
    let second = (second & !DMABUF_MADE) as i32;
    one_object(&program.process, first, second)?;
    kind_refusals(&install, vmo, report)?;
    let _ = side
        .call(
            nr::CHARDEV_DMABUF_RESOLVE,
            &[reg(control), wire.id, second as u64, BUFFER],
        )
        .map_err(|_| "a name-only dmabuf its control made did not resolve")?;
    if side.get(BUFFER, 8)? != NAME_COOKIE.to_ne_bytes() {
        return Err("a name-only dmabuf resolved to another cookie");
    }
    let kept = install(0, NAME_COOKIE + 1, name, NAME_BYTES)
        .map_err(|_| "a name-only dmabuf for a second cookie was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    if finish()? != Ok(VALUE as usize) {
        return Err("the program's ioctl did not return after its name-only dmabufs");
    }
    report.answered += 1;
    let _ = side.call(nr::HANDLE_CLOSE, &[reg(vmo)]);

    unmappable(&program.process, first, report)?;
    not_executable(&program.process, first)?;
    report.name_refusals += 1;

    if let Some(foreign) = foreign {
        foreign_refused(program, foreign, second, report)?;
    }

    // One release, after the last of two descriptors.
    close_fd(&program.process, first)?;
    if released(side, control)?.is_some() {
        return Err("a name-only dmabuf was released while a descriptor of it lived");
    }
    close_fd(&program.process, second)?;
    if released(side, control)? != Some(NAME_COOKIE) {
        return Err(
            "a name-only dmabuf's driver did not hear its release after its last descriptor",
        );
    }
    if released(side, control)?.is_some() {
        return Err("a name-only dmabuf's release was heard twice");
    }
    dup_and_exit(side, control, core)?;
    if super::dmabuf::alive(core) != 2 {
        return Err("a control counted other than two live dmabufs");
    }
    Ok(kept)
}

/// A name-only install's arguments: VMO register, cookie, flags, size.
type Install<'a> = &'a dyn Fn(u64, u64, u64, u64) -> Result<usize, Errno>;

/// The register refusals (N1): each `INVALID_ARGS`.
fn register_refusals(
    install: Install<'_>,
    vmo: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    use ferrix_native_abi::types::{DMABUF_NAME_MAX, DMABUF_NAME_ONLY, DMABUF_WRITABLE};
    let name = DMABUF_NAME_ONLY;
    for (vmo_register, flags, size, what) in [
        (0, name, 0, "a name-only dmabuf of no bytes was installed"),
        (
            0,
            name,
            4096 + 1,
            "a name-only dmabuf of no whole pages was installed",
        ),
        (
            0,
            name,
            DMABUF_NAME_MAX + 4096,
            "a name-only dmabuf over the bound was installed",
        ),
        (
            reg(vmo),
            name,
            NAME_BYTES,
            "a name-only dmabuf naming a VMO was installed",
        ),
        (
            reg(vmo),
            0,
            NAME_BYTES,
            "a dmabuf of a VMO with a size was installed",
        ),
        // No handle whose WRITE could decide O_RDWR (ledger 724, V3).
        (
            0,
            name | DMABUF_WRITABLE,
            NAME_BYTES,
            "a writable name-only dmabuf was installed",
        ),
    ] {
        if install(vmo_register, NAME_COOKIE, flags, size) != Err(status::INVALID_ARGS) {
            return Err(what);
        }
        report.name_refusals += 1;
    }
    Ok(())
}

/// A live cookie asked in the other kind or with another size:
/// `ALREADY_BOUND` (N2).
fn kind_refusals(
    install: Install<'_>,
    vmo: Handle,
    report: &mut Report,
) -> Result<(), &'static str> {
    use ferrix_native_abi::types::DMABUF_NAME_ONLY;
    let name = DMABUF_NAME_ONLY;
    for (vmo_register, cookie, flags, size, what) in [
        (
            0,
            NAME_COOKIE,
            name,
            NAME_BYTES * 2,
            "a live name was installed with another size",
        ),
        (
            reg(vmo),
            NAME_COOKIE,
            0,
            0,
            "a live name was installed over a VMO",
        ),
        (
            0,
            COOKIE + 2,
            name,
            DMABUF_BYTES,
            "a live VMO cookie was installed as a name",
        ),
    ] {
        if install(vmo_register, cookie, flags, size) != Err(status::ALREADY_BOUND) {
            return Err(what);
        }
        report.name_refusals += 1;
    }
    Ok(())
}

/// Two installs for one name gave one object.
fn one_object(process: &Process, first: i32, second: i32) -> Result<(), &'static str> {
    let objects: Vec<_> = [first, second]
        .iter()
        .filter_map(|fd| crate::syscall::fd::file(process, *fd).ok())
        .filter_map(|file| super::dmabuf::of(file.io()))
        .collect();
    match objects.as_slice() {
        [one, two] if Arc::ptr_eq(one, two) => {}
        _ => return Err("two name-only installs for one cookie did not give one dmabuf"),
    }
    drop(objects);
    Ok(())
}

/// Another driver's resolve of a name-only dmabuf it did not make is
/// `BAD_HANDLE` (B3), for a request of its own.
fn foreign_refused(
    program: &Side,
    (other_side, other_control, other_core): (&Side, Handle, &Arc<Control>),
    second: i32,
    report: &mut Report,
) -> Result<(), &'static str> {
    drain_channel(other_side, other_control);
    let request = admit(other_core, &program.process, IOCTL)
        .map_err(|_| "an ioctl was not taken in by the second driver")?;
    start(Job::Await(
        Arc::clone(other_core),
        request,
        Arc::clone(&program.process),
    ))?;
    let Some(Message::Request(wire)) = receive(other_side, other_control)? else {
        return Err("an ioctl did not reach the second driver as a REQUEST");
    };
    let resolved = other_side.call(
        nr::CHARDEV_DMABUF_RESOLVE,
        &[reg(other_control), wire.id, second as u64, BUFFER],
    );
    reply(other_side, other_control, wire.id, VALUE)?;
    let _ = finish()?;
    if resolved != Err(status::BAD_HANDLE) {
        return Err("another driver resolved a name-only dmabuf it did not make");
    }
    report.name_refusals += 1;
    Ok(())
}

/// A name-only dmabuf says its size and maps nothing, shared or private
/// (N5), through the program's own `mmap`.
fn unmappable(process: &Process, fd: i32, report: &mut Report) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::{MAP_PRIVATE, MAP_SHARED};
    use ferrix_vfs::Inode;
    let size = crate::syscall::fd::file(process, fd)
        .ok()
        .and_then(|file| super::dmabuf::of(file.io()))
        .map(|dmabuf| dmabuf.metadata().size);
    if size != Some(NAME_BYTES) {
        return Err("a name-only dmabuf did not say the size its driver gave");
    }
    for flags in [MAP_SHARED, MAP_PRIVATE] {
        match mmap_errno(process, fd, flags) {
            Err(Errno::ENODEV) => report.name_refusals += 1,
            Ok(at) => {
                let _ = crate::syscall::memory::sys_munmap(process, at, DMABUF_BYTES);
                return Err("a name-only dmabuf was mapped");
            }
            Err(_) => return Err("a name-only dmabuf's mapping was refused other than ENODEV"),
        }
    }
    Ok(())
}

/// After its driver went, a name-only dmabuf still says its size, still
/// maps nothing, and closes with nobody to hear it.
fn name_outlived(program: &Side, fd: i32, report: &mut Report) -> Result<(), &'static str> {
    unmappable(&program.process, fd, report)?;
    close_fd(&program.process, fd)
}

/// `mmap` of the dmabuf `fd` of `process`, read-only, with `flags`.
fn mmap_errno(process: &Process, fd: i32, flags: u32) -> Result<u64, Errno> {
    use crate::syscall::memory::{MmapRequest, OffsetUnit};
    use ferrix_linux_abi::types::PROT_READ;
    crate::syscall::memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: DMABUF_BYTES,
            prot: PROT_READ,
            flags,
            fd: i64::from(fd),
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map(|at| at as u64)
}

/// An exec of the dmabuf `fd` -- `execveat(fd, "", AT_EMPTY_PATH)`, as
/// `fexecve` makes it -- is refused before anything is mapped, and the
/// program keeps the mappings it had (ledger 724, V2).
fn not_executable(process: &Process, fd: i32) -> Result<(), &'static str> {
    use ferrix_linux_abi::types::AT_EMPTY_PATH;
    const EMPTY_AT: u64 = BUFFER + 0x180;
    uaccess_put(process, EMPTY_AT, &[0])?;
    let regions = |process: &Process| process.space().with_regions(|regions| regions.count());
    let before = regions(process);
    let ran = crate::syscall::exec::sys_execveat(
        process,
        fd,
        EMPTY_AT,
        0,
        0,
        AT_EMPTY_PATH,
        size_of::<usize>(),
    );
    if ran.is_ok() || regions(process) != before {
        return Err("a name-only dmabuf was executed");
    }
    Ok(())
}

/// Put `bytes` at `at` in `process`.
fn uaccess_put(process: &Process, at: u64, bytes: &[u8]) -> Result<(), &'static str> {
    crate::syscall::uaccess::copy_to_user(process.space(), at, bytes)
        .map_err(|_| "could not stage a program's memory")
}

/// RELEASE exactly once after the last of two descriptors of a name-only
/// dmabuf, one made by `dup` and closed by `close`, the other closed by its
/// holder's exit (ledger 724, V1).
fn dup_and_exit(side: &Side, control: Handle, core: &Arc<Control>) -> Result<(), &'static str> {
    use ferrix_native_abi::types::DMABUF_NAME_ONLY;
    const HELD: u64 = NAME_COOKIE + 5;
    let holder = Side::new()?;
    let request = admit(core, &holder.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&holder.process),
    ))?;
    let Some(Message::Request(wire)) = receive(side, control)? else {
        return Err("an ioctl did not reach its driver as a REQUEST");
    };
    let fd = side
        .call(
            nr::CHARDEV_DMABUF_INSTALL,
            &[reg(control), wire.id, 0, HELD, DMABUF_NAME_ONLY, NAME_BYTES],
        )
        .map_err(|_| "a name-only dmabuf for a holder was refused")? as i32;
    reply(side, control, wire.id, VALUE)?;
    let _ = finish()?;
    let copy = crate::syscall::fd::sys_dup(&holder.process, fd)
        .map_err(|_| "a name-only dmabuf's descriptor could not be duplicated")?
        as i32;
    close_fd(&holder.process, copy)?;
    if released(side, control)?.is_some() {
        return Err("a name-only dmabuf was released while its holder still had a descriptor");
    }
    crate::syscall::process::kill(&holder.process, 0);
    holder.close_everything();
    drop(holder);
    if released(side, control)? != Some(HELD) {
        return Err("a name-only dmabuf's release was not heard after its holder's exit");
    }
    if released(side, control)?.is_some() {
        return Err("a name-only dmabuf's release was heard twice after its holder's exit");
    }
    Ok(())
}

/// Map the whole dmabuf `fd` of `process`, shared and read-only.
fn map_dmabuf(process: &Process, fd: i32) -> Result<u64, &'static str> {
    use crate::syscall::memory::{MmapRequest, OffsetUnit};
    use ferrix_linux_abi::types::{MAP_SHARED, PROT_READ};
    crate::syscall::memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: DMABUF_BYTES,
            prot: PROT_READ,
            flags: MAP_SHARED,
            fd: i64::from(fd),
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map(|at| at as u64)
    .map_err(|_| "a dmabuf could not be mapped")
}

/// The first eight bytes at `at` in `process`.
fn read_mapped(process: &Process, at: u64) -> Result<[u8; 8], &'static str> {
    let mut bytes = [0u8; 8];
    crate::syscall::uaccess::copy_from_user(process.space(), at, &mut bytes)
        .map_err(|_| "a dmabuf's mapping could not be read")?;
    Ok(bytes)
}

/// Close descriptor `fd` of `process`.
fn close_fd(process: &Process, fd: i32) -> Result<(), &'static str> {
    crate::syscall::fd::sys_close(process, fd)
        .map(|_| ())
        .map_err(|_| "closing a dmabuf's descriptor failed")
}

/// The cookie of the next `DmabufRelease` on `control`, if one comes within
/// a short while; any other message is an error.
fn released(side: &Side, control: Handle) -> Result<Option<u64>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(STILL_NANOS * 4);
    loop {
        if let Some(bytes) = read(side, control) {
            return match Message::decode(&bytes) {
                Ok(Message::Request(wire)) if wire.op == Op::DmabufRelease => Ok(Some(wire.arg)),
                _ => Err("the chardev core wrote something other than a dmabuf's release"),
            };
        }
        if timer::now_nanos() > deadline {
            return Ok(None);
        }
        sched::sleep_for(1_000_000);
    }
}

/// Take requests in until `EBUSY`; a 257th taken is an error.
fn fill(core: &Arc<Control>, program: &Side) -> Result<Vec<Arc<Request>>, &'static str> {
    let mut taken = Vec::new();
    taken
        .try_reserve_exact(MAX_OUTSTANDING + 1)
        .map_err(|_| "no memory for the check's requests")?;
    loop {
        match admit(core, &program.process, IOCTL) {
            Ok(request) => taken.push(request),
            Err(Errno::EBUSY) => return Ok(taken),
            Err(_) => return Err("a request was refused other than EBUSY"),
        }
        if taken.len() > MAX_OUTSTANDING {
            return Err("a 257th request was admitted");
        }
    }
}

/// The driver goes: a waiting program wakes with `ENODEV`, the node is
/// unpublished (N9).
fn death(
    side: &Side,
    control: Handle,
    core: &Arc<Control>,
    program: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let request = admit(core, &program.process, IOCTL).map_err(|_| "an ioctl was not taken in")?;
    start(Job::Await(
        Arc::clone(core),
        request,
        Arc::clone(&program.process),
    ))?;
    close(side, control)?;
    if finish()? != Err(Errno::ENODEV) {
        return Err("a program waiting on a driver that went did not wake with ENODEV");
    }
    report.answered += 1;
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while published(0).is_some() {
        if timer::now_nanos() > deadline {
            return Err("a node stayed published after its driver went");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The driver's side
// ---------------------------------------------------------------------------

/// Wait until `node`'s claim has gone back. A control's task gives it back
/// after unpublishing and tearing down (`run`), so a node no longer
/// published can still be claimed for a while; devmgr's quiesce waits this
/// out (`wait_until_unserved`), and a driver the check plays next must too.
fn unclaimed(node: &Arc<DeviceNode>) -> Result<(), &'static str> {
    super::CLAIMS
        .wait_within(node, &|| false, PATIENCE_NANOS)
        .map_err(|_| "a dead driver's claim on its device did not go back")
}

/// `chardev_control_create` on `device`.
fn create(side: &Side, device: Handle) -> Result<Handle, &'static str> {
    side.handle(
        nr::CHARDEV_CONTROL_CREATE,
        &[reg(device)],
        "chardev_control_create on a device with MANAGE failed",
    )
}

/// The device's location word, which HELLO must name.
fn location(node: &DeviceNode) -> u32 {
    node.describe().location
}

/// Say HELLO for `minors` at `at`.
fn hello(side: &Side, control: Handle, at: u32, minors: &[u16]) -> Result<(), &'static str> {
    let mut list = [0_u16; MAX_NODES];
    for (slot, minor) in list.iter_mut().zip(minors) {
        *slot = *minor;
    }
    let encoded = Message::Hello(Hello {
        version: VERSION,
        location: at,
        count: minors.len(),
        minors: list,
    })
    .encode();
    side.put(OUTBOX, encoded.as_bytes())?;
    let _ = side
        .call(
            nr::CHANNEL_WRITE,
            &[reg(control), OUTBOX, encoded.as_bytes().len() as u64, 0, 0],
        )
        .map_err(|_| "sending a HELLO failed")?;
    Ok(())
}

/// The next message on `control`, or `None` once the kernel's end closed.
fn receive(side: &Side, control: Handle) -> Result<Option<Message>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    side.put(DEADLINE, &deadline.to_ne_bytes())?;
    let _ = side
        .call(
            nr::OBJECT_WAIT_ONE,
            &[
                reg(control),
                u64::from((Signals::READABLE | Signals::PEER_CLOSED).0),
                DEADLINE,
                OBSERVED,
            ],
        )
        .map_err(|_| "the chardev core did not answer in time")?;
    match read(side, control) {
        Some(bytes) => Message::decode(&bytes)
            .map(Some)
            .map_err(|_| "the chardev core wrote something that is not a message"),
        None => Ok(None),
    }
}

/// One message's bytes off `control`, if one is there.
fn read(side: &Side, control: Handle) -> Option<Vec<u8>> {
    let _ = side
        .call(
            nr::CHANNEL_READ,
            &[
                reg(control),
                INBOX,
                ferrix_chardevctl::message::MAX_BYTES as u64,
                IN_HANDLES,
                0,
                ACTUAL,
            ],
        )
        .ok()?;
    let bytes = side.get_u32(ACTUAL).ok()? as usize;
    side.get(INBOX, bytes).ok()
}

/// Read and drop whatever REQUESTs are waiting on `control`.
fn drain_channel(side: &Side, control: Handle) {
    while read(side, control).is_some() {}
}

/// Require REFUSED with `wanted`, then the kernel's end to close.
fn expect_refused(
    side: &Side,
    control: Handle,
    wanted: Refusal,
    report: &mut Report,
) -> Result<(), &'static str> {
    match receive(side, control)? {
        Some(Message::Refused(reason)) if reason == wanted => {}
        Some(Message::Refused(_)) => return Err("a HELLO was refused for the wrong reason"),
        _ => return Err("a HELLO that had to be refused was not"),
    }
    if receive(side, control)?.is_some() {
        return Err("the chardev core wrote after refusing a HELLO");
    }
    if !Signals(side.get_u32(OBSERVED)?).intersects(Signals::PEER_CLOSED) {
        return Err("the chardev core kept its end of a refused control open");
    }
    close(side, control)?;
    report.refusals += 1;
    Ok(())
}

/// `chardev_copy_in` or `_out` of `len` bytes between the program's
/// [`BUFFER`] and the driver's.
fn copy(side: &Side, control: Handle, id: u64, number: usize, len: u64) -> Result<usize, Errno> {
    side.call(number, &[reg(control), id, BUFFER, BUFFER, len])
}

/// `chardev_reply` with success and `value`.
fn reply(side: &Side, control: Handle, id: u64, value: u64) -> Result<(), &'static str> {
    side.call(nr::CHARDEV_REPLY, &[reg(control), id, 0, value])
        .map(|_| ())
        .map_err(|_| "the driver's reply was refused")
}

/// Close the driver's end of `control`.
fn close(side: &Side, control: Handle) -> Result<(), &'static str> {
    side.call(nr::HANDLE_CLOSE, &[reg(control)])
        .map(|_| ())
        .map_err(|_| "closing a driver's end failed")
}

// ---------------------------------------------------------------------------
// The core's side, read and driven from here
// ---------------------------------------------------------------------------

/// The live control serving `node`.
fn control_of(node: &Arc<DeviceNode>) -> Result<Arc<Control>, &'static str> {
    CONTROLS
        .lock()
        .iter()
        .find(|control| Arc::ptr_eq(&control.device, node))
        .cloned()
        .ok_or("an accepted control is not among the live ones")
}

/// How many requests the queue to `control`'s driver holds.
fn queued(control: &Control) -> usize {
    let state = control.state.lock();
    state
        .outgoing
        .iter()
        .filter(|held| matches!(held, super::Outgoing::Request(_)))
        .count()
}

/// Read `control` dry until the queue to `core`'s driver holds no request,
/// or fail with `late` after [`PATIENCE_NANOS`].
fn emptied(
    side: &Side,
    control: Handle,
    core: &Control,
    late: &'static str,
) -> Result<(), &'static str> {
    drain_channel(side, control);
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while queued(core) != 0 {
        if timer::now_nanos() > deadline {
            return Err(late);
        }
        drain_channel(side, control);
        sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// Mark a copy for `request` in flight, as `copy` does before its chunks,
/// so a wait for its end can be seen to wait.
fn in_flight(request: &Request) {
    let mut inner = request.inner.lock();
    inner.copying = inner.copying.saturating_add(1);
}

/// Start the helper on `job`.
fn start(job: Job) -> Result<(), &'static str> {
    *OUTCOME.lock() = None;
    *JOB.lock() = Some(job);
    let _ = sched::spawn("chardev-check", helper, 0, ferrix_sched::NICE_0_WEIGHT)?;
    Ok(())
}

/// The helper: run the job, record the outcome.
fn helper(_: usize) {
    let job = JOB.lock().take();
    let outcome = match job {
        // An ioctl is answered with a value; a mapping would be wrong, and
        // fails the check's comparison as EIO.
        Some(Job::Await(control, request, client)) => await_answer(&control, &request, &client)
            .and_then(|answer| match answer {
                Answer::Value(value) => Ok(value),
                Answer::Map(_) => Err(Errno::EIO),
            }),
        Some(Job::Abandon(control, request)) => {
            abandon(&control, &request);
            Ok(0)
        }
        Some(Job::Import(client, dmabuf, arg)) => super::dmabuf::import_as(&client, &dmabuf, arg),
        None => Err(Errno::EINVAL),
    };
    *OUTCOME.lock() = Some(outcome);
}

/// Require the helper to have no outcome yet, a while after it started.
fn still_waiting(what: &'static str) -> Result<(), &'static str> {
    sched::sleep_for(STILL_NANOS);
    if OUTCOME.lock().is_some() {
        return Err(what);
    }
    Ok(())
}

/// The helper's outcome, within the patience.
fn finish() -> Result<Result<usize, Errno>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let outcome = OUTCOME.lock().take();
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
        if timer::now_nanos() > deadline {
            return Err("a program's call into the chardev core never returned");
        }
        sched::sleep_for(1_000_000);
    }
}

/// Wait until every control's task of the check has ended and let its
/// device go.
fn settle() -> Result<(), &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while !CONTROLS.lock().is_empty() || !STARTING.lock().is_empty() {
        if timer::now_nanos() > deadline {
            return Err("a chardev control's task did not end after its driver went");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(())
}
