//! K14's sealing, measured spaces and the digest record
//! (`docs/microkernel/K14.md` §2-§4). `present` and `desktop` make no measured
//! job of their own, so the check makes one, and drives the native calls.

#![allow(clippy::disallowed_methods, reason = "a raised check")]

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::status;
use ferrix_native_abi::types::{self, JOB_LEVEL_TOP, MAP_EXECUTE, MAP_READ, MAP_WRITE};

use crate::object::Object;
use crate::object::check::{SCRATCH, Side, Spawner, reg};
use crate::object::space::SpaceRef;
use crate::user::vmo::{SealRefusal, VmoError};

/// Where a check's small inputs and outputs go in the scratch region.
const OFFSET_AT: u64 = SCRATCH + PAGE_SIZE + 0x80;
const BYTES_AT: u64 = SCRATCH + PAGE_SIZE + 0x90;
const DIGEST_AT: u64 = SCRATCH + PAGE_SIZE + 0x100;

/// SHA-256 of the 8 bytes of a one-page object's length (4096, little-endian)
/// followed by that page of zeros, computed independently by the host:
/// `(4096).to_bytes(8,'little') + bytes(4096)`, as K14.12 frames it.
const ONE_ZERO_PAGE: &str = "34085a3cad6a1a45a68869e5a5eb2bcb79b0b6d84c0af33568f4f062aa43fc69";

/// What the check did, for the boot log.
#[derive(Debug, Default)]
pub struct Report {
    /// Objects sealed.
    pub sealed: u32,
    /// Calls refused with the answer the rule names.
    pub refusals: u32,
    /// Digests a process's record held after an unmap.
    pub recorded: usize,
    /// Bytes of the one object timed.
    pub timed_bytes: u64,
    /// Nanoseconds its seal took.
    pub timed_nanos: u64,
}

fn refused(
    answer: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    if answer == Err(wanted) {
        report.refusals += 1;
        Ok(())
    } else {
        Err(what)
    }
}

/// A one-page VMO through the native call.
fn one_page(side: &Side) -> Result<Handle, &'static str> {
    side.handle(nr::VMO_CREATE, &[PAGE_SIZE], "vmo_create failed")
}

/// The object a handle names.
fn object(side: &Side, vmo: Handle) -> Result<Arc<crate::user::vmo::Vmo>, &'static str> {
    side.process.with_handles(|table| {
        let (object, _) = table.get(vmo).map_err(|_| "a VMO handle was gone")?;
        let Object::Vmo(vmo) = object else {
            return Err("a handle was not a VMO");
        };
        Ok(Arc::clone(vmo))
    })
}

/// Seal a VMO through the native call, answering the new handle.
fn seal(side: &Side, vmo: Handle, report: &mut Report) -> Result<Handle, &'static str> {
    let sealed = side.handle(
        nr::VMO_EXECUTABLE,
        &[reg(vmo), types::VMO_SEAL],
        "a plain anonymous VMO could not be sealed",
    )?;
    report.sealed += 1;
    Ok(sealed)
}

/// A measured space in the check's table, as a handle with every right.
fn measured_space(side: &Side) -> Result<(Arc<SpaceRef>, Handle), &'static str> {
    let space = SpaceRef::create(JOB_LEVEL_TOP).map_err(|_| "no memory for a space")?;
    space.space().measure();
    let handle = side
        .process
        .with_handles(|table| table.insert(Object::Space(Arc::clone(&space)), Rights::SPACE))
        .map_err(|_| "no room for a space's handle")?;
    Ok((space, handle))
}

/// `space_map` of one page.
fn map(side: &Side, space: Handle, vmo: Handle, protection: u32) -> Result<usize, Errno> {
    side.call(
        nr::SPACE_MAP,
        &[
            reg(space),
            reg(vmo),
            0,
            PAGE_SIZE,
            u64::from(protection),
            OFFSET_AT,
        ],
    )
}

/// Run it.
///
/// Verifies: L.mm.111, L.mm.112, L.mm.113, L.mm.114, L.mm.115, L.mm.116
/// Verifies: L.mm.117, L.mm.118, L.mm.119, L.mm.120, L.mm.121, L.mm.122
/// Verifies: L.mm.124, L.mm.125, L.mm.126
/// Verifies: H.MEM.29, H.MEM.30
pub fn run() -> Result<Report, &'static str> {
    let side = Side::new()?;
    let mut report = Report::default();
    let checked = run_on(&side, &mut report);
    side.close_everything();
    checked.map(|()| report)
}

fn run_on(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
    kinds_and_busy(side, report)?;
    every_write_path(side, report)?;
    digest_is_the_frames(side, report)?;
    no_map_before_the_digest(side, report)?;
    measured_rules(side, report)?;
    timed(side, report)?;
    jobs(side, report)
}

/// A shared process of a measured job (ledger 6499 X1, X2): the region is measured, so
/// an unsealed executable map into it is refused; plain `VMO_EXECUTABLE` from normal mode
/// is refused, whatever its restricted space is; and that space stays unmeasured, as it
/// runs other people's code by design (`docs/microkernel/K14.md`).
///
/// Verifies: L.user.189
fn region_of_a_measured_job(
    side: &Side,
    spawner: &Spawner,
    report: &mut Report,
) -> Result<(), &'static str> {
    if !crate::arch::HAS_RESTRICTED {
        return Ok(());
    }
    let job = side.handle(
        nr::JOB_CREATE,
        &[reg(spawner.root), types::JOB_MEASURED],
        "job_create measured failed",
    )?;
    let region = side.handle(
        nr::REGION_CREATE,
        &[reg(job), DIGEST_AT],
        "region_create in a measured job failed",
    )?;
    let region_space = Handle(side.get_u32(DIGEST_AT)?);
    let plain = one_page(side)?;
    let runnable = side.handle(
        nr::VMO_EXECUTABLE,
        &[reg(plain), 0],
        "vmo_executable failed",
    )?;
    refused(
        map(side, region_space, runnable, MAP_READ | MAP_EXECUTE),
        status::BAD_STATE,
        "an unsealed VMO was mapped executable in a measured job's region",
        report,
    )?;
    side.put(BYTES_AT, b"m")?;
    let shared = side.handle(
        nr::PROCESS_CREATE_SHARED,
        &[reg(region), reg(job), 0, BYTES_AT, 1, DIGEST_AT],
        "process_create_shared in a measured job failed",
    )?;
    let host = side.process.with_handles(|table| match table.get(shared) {
        Ok((Object::Process(process), _)) => process.control().and_then(|control| control.host()),
        _ => None,
    });
    let host = host.ok_or("a shared process's handle named no process")?;
    if host.core().own_space().is_measured() {
        return Err("a measured job's restricted space was measured");
    }
    let args = crate::trap::SyscallArgs {
        abi: crate::trap::Abi::Native,
        number: nr::VMO_EXECUTABLE,
        args: [0, 0, 0, 0, 0, 0],
        ip: 0,
    };
    refused(
        crate::syscall::native::dispatch(&args, Some(&*host)),
        status::BAD_STATE,
        "plain vmo_executable was answered in normal mode of a measured job's process",
        report,
    )?;
    // A new space in normal mode is measured, and an unmeasured one is not cloned
    // (`L.mm.116`'s rule, by the space the call means).
    let args = crate::trap::SyscallArgs {
        abi: crate::trap::Abi::Native,
        number: nr::SPACE_CREATE,
        args: [0, 0, 0, 0, 0, 0],
        ip: 0,
    };
    let made = crate::syscall::native::dispatch(&args, Some(&*host))
        .map_err(|_| "space_create in normal mode of a measured job's process failed")?;
    let made = u32::try_from(made)
        .map(Handle)
        .map_err(|_| "a space's handle was not one")?;
    let measured = host.core().with_handles(|table| match table.get(made) {
        Ok((Object::Space(space), _)) => space.space().is_measured(),
        _ => false,
    });
    if !measured {
        return Err("a new space in normal mode of a measured job's process was unmeasured");
    }
    let plain = SpaceRef::create(JOB_LEVEL_TOP).map_err(|_| "no memory for a space")?;
    let plain = host
        .core()
        .with_handles(|table| table.insert(Object::Space(plain), Rights::SPACE))
        .map_err(|_| "no room for a space's handle")?;
    let args = crate::trap::SyscallArgs {
        abi: crate::trap::Abi::Native,
        number: nr::SPACE_CLONE,
        args: [reg(plain), 0, 0, 0, 0, 0],
        ip: 0,
    };
    refused(
        crate::syscall::native::dispatch(&args, Some(&*host)),
        status::BAD_STATE,
        "an unmeasured space was cloned in normal mode of a measured job's process",
        report,
    )?;
    Ok(())
}

/// The time a seal takes, for MEMORY-AND-TIMING 2.2ah (K14.10); and that a sealed
/// object's length does not move (H.MEM.29, `grow_to`).
fn timed(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    /// A megabyte: long enough to read on a clock, short enough for every boot.
    const TIMED_BYTES: u64 = 1024 * 1024;
    let vmo = side.handle(nr::VMO_CREATE, &[TIMED_BYTES], "a VMO could not be made")?;
    let before = crate::timer::now_nanos();
    let sealed = seal(side, vmo, report)?;
    report.timed_nanos = crate::timer::now_nanos().saturating_sub(before);
    report.timed_bytes = TIMED_BYTES;
    let made = object(side, sealed)?;
    let pages = made.len_pages();
    made.grow_to(pages + 1);
    if made.len_pages() != pages {
        return Err("a sealed VMO's length grew");
    }
    Ok(())
}

/// `L.mm.111`, `L.mm.112`.
fn kinds_and_busy(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    // A read-only handle is no authority to freeze.
    let vmo = one_page(side)?;
    let read_only = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(vmo), u64::from(Rights::READ.0 | Rights::MAP.0)],
        "a read-only duplicate failed",
    )?;
    refused(
        side.call(nr::VMO_EXECUTABLE, &[reg(read_only), types::VMO_SEAL]),
        status::ACCESS_DENIED,
        "a read-only handle sealed a VMO",
        report,
    )?;
    // Each kind whose bytes can change under the kernel.
    let coherent = object(side, one_page(side)?)?;
    if !coherent.make_coherent() || coherent.seal() != Err(SealRefusal::Kind) {
        return Err("a coherent VMO was sealed");
    }
    let view = object(side, one_page(side)?)?;
    if !view.take_kernel_view() || view.seal() != Err(SealRefusal::Kind) {
        return Err("a VMO with a kernel view was sealed");
    }
    report.refusals += 2;
    // A writable mapping by handle, the case the file count would miss.
    let mapped = one_page(side)?;
    let at = side
        .call(
            nr::VMO_MAP,
            &[
                reg(mapped),
                0,
                PAGE_SIZE,
                u64::from(MAP_READ | MAP_WRITE),
                OFFSET_AT,
            ],
        )
        .map_err(|_| "vmo_map failed")?;
    let busy = object(side, mapped)?;
    if busy.seal() != Err(SealRefusal::Busy) {
        return Err("a writably mapped VMO was sealed");
    }
    report.refusals += 1;
    // Unmapped, the count has fallen.
    side.process
        .space()
        .unmap(at as u64, PAGE_SIZE)
        .map_err(|_| "an unmap failed")?;
    if busy.seal().is_err() {
        return Err("a VMO with no writable mapping left could not be sealed");
    }
    report.sealed += 1;
    // A live hold, whatever its direction.
    let holder = object(side, one_page(side)?)?;
    let held = holder.hold(0, 1).map_err(|_| "a hold failed")?;
    if holder.seal() != Err(SealRefusal::Busy) {
        return Err("a VMO with a live hold was sealed");
    }
    drop(held);
    report.refusals += 1;
    // Frames shared with a fork.
    let forked = object(side, one_page(side)?)?;
    let _ = forked.commit(0).map_err(|_| "a commit failed")?;
    let child = forked.fork().map_err(|_| "a fork failed")?;
    if forked.seal() != Err(SealRefusal::Shared) {
        return Err("a VMO sharing frames with a fork was sealed");
    }
    drop(child);
    report.refusals += 1;
    // Twice is refused.
    if busy.seal() != Err(SealRefusal::Already) {
        return Err("a sealed VMO was sealed again");
    }
    report.refusals += 1;
    Ok(())
}

/// `L.mm.113`: every path that could change a sealed object is refused, and
/// its bytes read back as they were.
fn every_write_path(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let vmo = one_page(side)?;
    side.put(BYTES_AT, b"kept")?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(vmo), BYTES_AT, 4, OFFSET_AT])
        .map_err(|_| "vmo_write before a seal failed")?;
    let other_writer = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(vmo), u64::from(SAME_RIGHTS)],
        "a duplicate failed",
    );
    let sealed = seal(side, vmo, report)?;
    let held = object(side, sealed)?;
    if let Ok(writer) = other_writer {
        refused(
            side.call(nr::VMO_WRITE, &[reg(writer), BYTES_AT, 4, OFFSET_AT]),
            status::BAD_STATE,
            "a sealed VMO was written through a handle kept from before",
            report,
        )?;
        refused(
            side.call(
                nr::VMO_MAP,
                &[
                    reg(writer),
                    0,
                    PAGE_SIZE,
                    u64::from(MAP_READ | MAP_WRITE),
                    OFFSET_AT,
                ],
            ),
            status::BAD_STATE,
            "a sealed VMO was mapped writable",
            report,
        )?;
    }
    let mut paths: u32 = 0;
    let mut refuse = |ok: bool, what: &'static str| -> Result<(), &'static str> {
        if ok {
            paths += 1;
            Ok(())
        } else {
            Err(what)
        }
    };
    refuse(
        held.write_page(0, 0, b"x") == Err(VmoError::Sealed),
        "a sealed VMO was written",
    )?;
    refuse(
        held.commit_and_write(0, 0, b"x") == Err(VmoError::Sealed),
        "a sealed VMO was written",
    )?;
    refuse(
        held.zero_from(0, 0) == Err(VmoError::Sealed),
        "a sealed VMO was zeroed",
    )?;
    refuse(
        held.decommit_range(0, 1) == 0,
        "a sealed VMO gave up a page",
    )?;
    refuse(
        held.decommit_from(0) == 0,
        "a sealed VMO gave up a page from an index",
    )?;
    refuse(held.hold(0, 1).is_err(), "a sealed VMO was held for write")?;
    refuse(!held.make_coherent(), "a sealed VMO was made coherent")?;
    refuse(!held.make_combining(), "a sealed VMO was made combining")?;
    refuse(!held.take_kernel_view(), "a sealed VMO gave a kernel view")?;
    refuse(
        held.raise_writable_for_check().is_err(),
        "a sealed VMO took a writable region",
    )?;
    let read = held
        .hold_read(0, 1)
        .map_err(|_| "a read hold of a sealed VMO failed")?;
    drop(read);
    report.refusals += paths;
    let mut out = [0_u8; 4];
    held.read_page(0, 0, &mut out)
        .map_err(|_| "a sealed VMO could not be read")?;
    if &out != b"kept" {
        return Err("a sealed VMO's bytes changed");
    }
    Ok(())
}

/// `L.mm.114`: the digest is SHA-256 of the length and the frames, against a
/// value computed apart from the kernel; a byte's difference moves it.
fn digest_is_the_frames(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let vmo = one_page(side)?;
    let sealed = seal(side, vmo, report)?;
    let _ = side
        .call(nr::VMO_DIGEST, &[reg(sealed), DIGEST_AT])
        .map_err(|_| "vmo_digest refused a sealed VMO")?;
    let got = side.get(DIGEST_AT, 32)?;
    let mut expected = [0_u8; 32];
    for (index, slot) in expected.iter_mut().enumerate() {
        let digit = |at: usize| {
            ONE_ZERO_PAGE
                .as_bytes()
                .get(at)
                .and_then(|byte| char::from(*byte).to_digit(16))
                .unwrap_or(0) as u8
        };
        *slot = digit(index * 2) << 4 | digit(index * 2 + 1);
    }
    if got.as_slice() != expected {
        return Err("a seal's digest differs from the known vector");
    }
    let other = one_page(side)?;
    side.put(BYTES_AT, b"!")?;
    let _ = side
        .call(nr::VMO_WRITE, &[reg(other), BYTES_AT, 1, OFFSET_AT])
        .map_err(|_| "vmo_write failed")?;
    let other = seal(side, other, report)?;
    if object(side, other)?.digest() == Some(expected) {
        return Err("a different byte left a seal's digest the same");
    }
    refused(
        side.call(nr::VMO_DIGEST, &[reg(one_page(side)?), DIGEST_AT]),
        status::BAD_STATE,
        "vmo_digest answered for a VMO not sealed",
        report,
    )
}

/// `L.mm.115`: frozen with no digest yet, nothing can run from it.
fn no_map_before_the_digest(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let vmo = one_page(side)?;
    let executable = side.handle(
        nr::HANDLE_DUPLICATE,
        &[reg(vmo), u64::from(SAME_RIGHTS)],
        "a duplicate failed",
    )?;
    object(side, vmo)?.freeze_for_check()?;
    refused(
        side.call(nr::VMO_DIGEST, &[reg(vmo), DIGEST_AT]),
        status::SHOULD_WAIT,
        "vmo_digest answered during a hash",
        report,
    )?;
    let (_space, space) = measured_space(side)?;
    let runnable = side.handle(
        nr::VMO_EXECUTABLE,
        &[reg(executable), 0],
        "vmo_executable failed",
    )?;
    refused(
        map(side, space, runnable, MAP_READ | MAP_EXECUTE),
        status::BAD_STATE,
        "an image mapped executable before its digest existed",
        report,
    )
}

/// `L.mm.117`-`L.mm.119`, `L.mm.122`, `L.mm.124`: what a measured space takes.
fn measured_rules(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let (space_ref, space) = measured_space(side)?;
    // An unsealed VMO with EXECUTE on both handles: refused.
    let plain = one_page(side)?;
    let runnable = side.handle(
        nr::VMO_EXECUTABLE,
        &[reg(plain), 0],
        "vmo_executable failed",
    )?;
    refused(
        map(side, space, runnable, MAP_READ | MAP_EXECUTE),
        status::BAD_STATE,
        "an unsealed VMO was mapped executable in a measured job",
        report,
    )?;
    if !refusal_recorded(nr::SPACE_MAP) {
        return Err("a SPACE_MAP refused for want of a measurement left no audit record");
    }
    // Nor readable, for a later protect to raise: its ceiling holds EXECUTE.
    refused(
        map(side, space, runnable, MAP_READ),
        status::BAD_STATE,
        "an unsealed executable handle was mapped in a measured space",
        report,
    )?;
    // Anonymous memory.
    refused(
        side.call(
            nr::SPACE_ALLOCATE,
            &[reg(space), 0, PAGE_SIZE, u64::from(MAP_READ | MAP_EXECUTE)],
        ),
        status::BAD_STATE,
        "anonymous memory was made executable in a measured space",
        report,
    )?;
    let data = side
        .call(
            nr::SPACE_ALLOCATE,
            &[reg(space), 0, PAGE_SIZE, u64::from(MAP_READ | MAP_WRITE)],
        )
        .map_err(|_| "anonymous memory was refused in a measured space")?;
    refused(
        side.call(
            nr::SPACE_PROTECT,
            &[
                reg(space),
                data as u64,
                PAGE_SIZE,
                u64::from(MAP_READ | MAP_EXECUTE),
            ],
        ),
        status::BAD_STATE,
        "anonymous memory was protected executable in a measured space",
        report,
    )?;
    // A sealed one is mapped, and recorded, and stays recorded.
    let image = one_page(side)?;
    let image = seal(side, image, report)?;
    let at = map(side, space, image, MAP_READ | MAP_EXECUTE)
        .map_err(|_| "a sealed VMO could not be mapped executable")?;
    let digest = object(side, image)?.digest();
    // The hash ran with no preempt count held (K14.10).
    if crate::user::vmo::SEAL_PREEMPT_COUNT.load(core::sync::atomic::Ordering::Relaxed) != 0 {
        return Err("a seal's hash ran with preemption held");
    }
    let _ = side
        .call(nr::SPACE_UNMAP, &[reg(space), at as u64, PAGE_SIZE])
        .map_err(|_| "space_unmap failed")?;
    let mut record = [[0_u8; 32]; 2];
    let count = space_ref.space().images_into(&mut record);
    if count != 1 || Some(record[0]) != digest {
        return Err("an unmapped image's digest left the record");
    }
    report.recorded = count;
    // Mapped twice, it is one digest.
    let _ = map(side, space, image, MAP_READ | MAP_EXECUTE)
        .map_err(|_| "a sealed VMO could not be mapped again")?;
    if space_ref.space().images_into(&mut record) != 1 {
        return Err("one image was recorded twice");
    }
    // A clone keeps the state and the record.
    let clone = side.handle(nr::SPACE_CLONE, &[reg(space)], "space_clone failed")?;
    let copied = side.process.with_handles(|table| {
        let (object, _) = table.get(clone).map_err(|_| "a clone's handle was gone")?;
        let Object::Space(copied) = object else {
            return Err("a clone was not a space");
        };
        Ok(Arc::clone(copied))
    })?;
    if !copied.space().is_measured() || copied.space().images_into(&mut record) != 1 {
        return Err("a measured space's clone lost its state or its record");
    }
    Ok(())
}

/// `L.mm.116`, `L.mm.120`, `L.mm.121`, `L.mm.125`, `L.mm.126`.
fn jobs(side: &Side, report: &mut Report) -> Result<(), &'static str> {
    let spawner = Spawner::new(side)?;
    region_of_a_measured_job(side, &spawner, report)?;
    let job = side.handle(
        nr::JOB_CREATE,
        &[reg(spawner.root), types::JOB_MEASURED],
        "job_create measured failed",
    )?;
    let inner = side.handle(nr::JOB_CREATE, &[reg(job), 0], "job_create inside failed")?;
    let of = |handle: Handle| {
        side.process.with_handles(|table| {
            let (object, _) = table.get(handle).map_err(|_| "a job's handle was gone")?;
            let Object::Job(job) = object else {
                return Err("a handle was not a job");
            };
            Ok(Arc::clone(job))
        })
    };
    if !of(job)?.measured() || !of(inner)?.measured() {
        return Err("a measured job made an unmeasured child");
    }
    // An unsealed image in a measured job, by process_create.
    if spawner.created(job).is_ok() {
        return Err("an unsealed image was started in a measured job");
    }
    report.refusals += 1;
    // A process does not cross the line by a move, either way.
    let plain = of(side.handle(nr::JOB_CREATE, &[reg(spawner.root), 0], "job_create failed")?)?;
    let held = of(job)?;
    if !crate::object::job::Job::measure_refuses(&held, &plain, false)
        || !crate::object::job::Job::measure_refuses(&plain, &held, false)
        || crate::object::job::Job::measure_refuses(&plain, &held, true)
    {
        return Err("a move crossed the measured line");
    }
    report.refusals += 2;
    // A process outside a measured job answers that its list is not whole.
    let child = spawner.made(spawner.root, "process_create failed")?;
    let answer = side
        .call(nr::PROCESS_IMAGES, &[reg(child), DIGEST_AT, 1])
        .map_err(|_| "process_images failed")?;
    if answer & types::IMAGES_UNMEASURED == 0 {
        return Err("a partial digest list read as whole");
    }
    // A measured caller's plain vmo_executable and clone of an unmeasured
    // space: the second process is measured for this.
    let other = Side::new()?;
    other.process.space().measure();
    let vmo = one_page(&other)?;
    refused(
        other.call(nr::VMO_EXECUTABLE, &[reg(vmo), 0]),
        status::BAD_STATE,
        "plain vmo_executable passed in a measured job",
        report,
    )?;
    let unmeasured = SpaceRef::create(JOB_LEVEL_TOP).map_err(|_| "no memory for a space")?;
    let handle = other
        .process
        .with_handles(|table| table.insert(Object::Space(unmeasured), Rights::SPACE))
        .map_err(|_| "no room for a space's handle")?;
    let answered = other.call(nr::SPACE_CLONE, &[reg(handle)]);
    other.close_everything();
    refused(
        answered,
        status::BAD_STATE,
        "a measured caller cloned an unmeasured space",
        report,
    )
}

/// Whether the refusals' ring holds K14's event with `call`'s number.
fn refusal_recorded(call: usize) -> bool {
    let mut records = [crate::audit::Record::EMPTY; 16];
    let mut from = 0;
    let mut found = false;
    loop {
        let read = crate::audit::read(crate::audit::Which::Refusals, from, &mut records);
        if read.copied == 0 {
            return found;
        }
        found |= records
            .iter()
            .take(read.copied)
            .any(|record| record.is(crate::audit::UNMEASURED) && record.detail[0] as usize == call);
        from = read.next;
    }
}
