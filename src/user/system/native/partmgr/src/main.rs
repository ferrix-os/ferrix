//! The partition manager (`docs/MICROKERNEL.md` §5,
//! `docs/microkernel/BOOT-AUTHORITY.md` §8): the first process the kernel
//! starts. It reads a static description, starts each safety partition it names
//! from the image the kernel sealed, holds their authority and supervises them.
//! So a safety partition's authority never passes through init, `linuxd` or
//! `devmgr`. It speaks to nothing uncertified and has no heap.
//!
//! The exit status is the diagnosis. Its death is the machine's safe state: the
//! kernel panics on it (`FX-0954`), a clean exit included.

#![no_std]
#![no_main]

use ferrix_native_abi::first::{FIRST_BYTES, FIRST_FIXED_HANDLES, READY_MAGIC, decode};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;
use ferrix_partition_desc::{Description, MAX_BYTES, MAX_PARTITIONS, ROLE_FILES, ROLE_LINUX};
use ferrix_process_builder::build;
use ferrix_rt::native::channel::{self, Channel};
use ferrix_rt::native::device::Device;
use ferrix_rt::native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_rt::native::job::Job;
use ferrix_rt::native::pending::Process;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::Vmo;
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// Where it gave up, as the exit status.
#[derive(Clone, Copy)]
enum Step {
    NoBootstrap = 1,
    Read = 2,
    Message = 3,
    Description = 4,
    Port = 5,
    Ready = 6,
    Wait = 7,
}

/// What it keeps of a partition.
struct Running {
    name: [u8; 32],
    digest: [u8; 32],
    restarts: u32,
    job: Job<Kernel>,
    image: Vmo<Kernel>,
    process: Option<Process<Kernel>>,
}

/// The description's bytes: the most a file can be, static, since there is no heap.
static mut DESCRIPTION: [u8; MAX_BYTES] = [0; MAX_BYTES];

fn say(what: core::fmt::Arguments<'_>) {
    ferrix_rt::notice!("partmgr  {}", what);
}

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?")
}

/// Start `running`'s partition. Each check is of the kernel's own record, never of
/// anything the image says of itself.
fn start(
    running: &mut Running,
    key: u64,
    port: &Port<Kernel>,
    log: Handle,
    server: Option<[OwnedHandle<Kernel>; 2]>,
) -> Result<(), &'static str> {
    let mut digest = [0_u8; 32];
    running
        .image
        .digest(&mut digest)
        .map_err(|_| "its image has no digest: it is not sealed")?;
    if digest != running.digest {
        return Err("its image's digest is not the description's");
    }
    let name = text(running.name.split(|&byte| byte == 0).next().unwrap_or(&[]));
    let built = build(&running.job, &running.image, name, ferrix_rt::ELF_MACHINE)
        .map_err(|_| "it could not be built")?;
    built
        .process()
        .notify_on_exit(port, key)
        .map_err(|_| "its end could not be watched")?;
    // Its record against the description, exactly: nothing but this image.
    let mut record = [[0_u8; 32]; 2];
    let count = built
        .process()
        .images(&mut record)
        .map_err(|_| "its record could not be read")?;
    if count.count != 1 || count.unmeasured || record[0] != running.digest {
        return Err("its record is not exactly its image");
    }
    let (near, far) = channel::create(Kernel).map_err(|_| "no channel for it")?;
    // The debug log for its lines, on the channel only it holds.
    let line = OwnedHandle::from_raw(Kernel, log);
    let given = core::mem::ManuallyDrop::new(line)
        .duplicate(Requested::Same)
        .map_err(|_| "no debug log for it")?;
    match server {
        // Its channels, and its own space, so that it can let a dead driver's ring
        // go (H.4). It reaches nothing else with it.
        Some([cores, files]) => {
            let space = built
                .process()
                .space(Requested::Exactly(Rights(
                    Rights::MAP.0 | Rights::TRANSFER.0,
                )))
                .map_err(|_| "no space handle for it")?;
            // Its own job's figures, for its lines: read only (H.1's row).
            let job = running
                .job
                .as_owned()
                .duplicate(Requested::Exactly(Rights(
                    Rights::WAIT.0 | Rights::TRANSFER.0,
                )))
                .map_err(|_| "no job handle for it")?;
            near.write_with(b"PARTITON", [given, cores, files, space.into_owned(), job])
                .map_err(|_| "its start could not be written")?;
        }
        None => near
            .write_with(b"PARTITON", [given])
            .map_err(|_| "its start could not be written")?,
    }
    let process = built
        .run(far.into_owned())
        .map_err(|_| "it could not be started")?;
    running.process = Some(process);
    // `near` closes here: what it was told is in the channel for it to read.
    Ok(())
}

fn main(bootstrap: Bootstrap) -> i32 {
    match run(bootstrap) {
        Ok(()) => 0,
        Err(step) => step as i32,
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the manager's one sequence, read in one place: read, start each, supervise"
)]
fn run(bootstrap: Bootstrap) -> Result<(), Step> {
    let channel: Channel<Kernel> = bootstrap.ok_or(Step::NoBootstrap)?;
    let _ = channel
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
        .map_err(|_| Step::Read)?;
    let mut bytes = [0_u8; FIRST_BYTES];
    let mut handles = [Handle::INVALID; CHANNEL_MAX_HANDLES];
    let got = channel
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Read)?;
    let (images, devices) = decode(bytes.get(..got.bytes).unwrap_or(&[])).ok_or(Step::Message)?;
    let (images, devices) = (images as usize, devices as usize);
    let handle = |index: usize| {
        OwnedHandle::from_raw(
            Kernel,
            handles.get(index).copied().unwrap_or(Handle::INVALID),
        )
    };
    let job = Job::from_owned(handle(0));
    let log = handles[1];
    ferrix_rt::log::use_debug_log(log);
    let description_vmo = Vmo::from_owned(handle(2));

    let place = core::ptr::addr_of_mut!(DESCRIPTION);
    // SAFETY: this is the process's one thread, and nothing else names the array.
    let store = unsafe { &mut *place };
    let size = description_vmo.size().map_err(|_| Step::Description)? as usize;
    let file = store.get_mut(..size).ok_or(Step::Description)?;
    description_vmo
        .read(file, 0)
        .map_err(|_| Step::Description)?;
    // The kernel parsed it to filter devices; this reads it again for itself.
    let length = {
        let mut first = [0_u8; 2];
        first.copy_from_slice(file.get(6..8).ok_or(Step::Description)?);
        ferrix_partition_desc::HEADER_BYTES
            + usize::from(u16::from_le_bytes(first)) * ferrix_partition_desc::PARTITION_BYTES
    };
    let description = Description::parse(file.get(..length).ok_or(Step::Description)?)
        .map_err(|_| Step::Description)?;
    if description.count() != images || description.count() > MAX_PARTITIONS {
        return Err(Step::Description);
    }
    // A file server's two channels come last, for it alone (BTRFSD §2).
    let serves = description
        .partitions()
        .any(|partition| partition.role() == ROLE_FILES);
    if got.handles != FIRST_FIXED_HANDLES + images + devices + if serves { 2 } else { 0 } {
        return Err(Step::Message);
    }
    let mut server_ends = serves.then(|| {
        let first = FIRST_FIXED_HANDLES + images + devices;
        [handle(first), handle(first + 1)]
    });
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    let mut running: [Option<Running>; MAX_PARTITIONS] = [const { None }; MAX_PARTITIONS];
    let mut next_device = FIRST_FIXED_HANDLES + images;
    for (index, partition) in description.partitions().enumerate() {
        let image = Vmo::from_owned(handle(FIRST_FIXED_HANDLES + index));
        // A further Linux partition is the kernel's to start; it has no device here.
        if partition.role() == ROLE_LINUX {
            continue;
        }
        let mut name = [0_u8; 32];
        for (slot, byte) in name.iter_mut().zip(partition.name()) {
            *slot = *byte;
        }
        // Each device bound before anyone can drive it (K8), in the manager's hands.
        for _ in partition.devices() {
            let device = Device::from_owned(handle(next_device));
            next_device += 1;
            if device.bind_domain().is_err() {
                say(format_args!(
                    "{}: a device's domain could not be bound; not started",
                    text(partition.name())
                ));
            }
            // Kept for the machine's life, a safety partition's device: no close.
            let _ = device.into_owned().into_raw();
        }
        let Ok(child) = job.create_measured_child() else {
            say(format_args!("{}: no job for it", text(partition.name())));
            continue;
        };
        let mut entry = Running {
            name,
            digest: partition.digest(),
            restarts: partition.restarts(),
            job: child,
            image,
            process: None,
        };
        let gifts = if partition.role() == ROLE_FILES {
            server_ends.take()
        } else {
            None
        };
        match start(&mut entry, index as u64, &port, log, gifts) {
            Ok(()) => say(format_args!(
                "{} started, its image's digest checked",
                text(partition.name())
            )),
            Err(why) => say(format_args!(
                "{} not started: {}",
                text(partition.name()),
                why
            )),
        }
        if let Some(slot) = running.get_mut(index) {
            *slot = Some(entry);
        }
    }
    let mut ready = [0_u8; 8];
    ready[..4].copy_from_slice(&READY_MAGIC.to_le_bytes());
    channel.write(&ready).map_err(|_| Step::Ready)?;
    say(format_args!("{} partitions in hand", description.count()));

    // Supervision: one port, an end or an exception at a time.
    loop {
        let packet = port.wait(Deadline::Never).map_err(|_| Step::Wait)?;
        let Some(Some(entry)) = running.get_mut(packet.key as usize) else {
            continue;
        };
        let name = entry.name;
        let shown = text(name.split(|&byte| byte == 0).next().unwrap_or(&[]));
        let status = entry
            .process
            .take()
            .and_then(|process| process.status().ok());
        say(format_args!("{shown} ended: {status:?}"));
        if entry.restarts == 0 {
            say(format_args!("{shown} stays down: no restart left"));
            continue;
        }
        entry.restarts -= 1;
        let key = packet.key;
        match start(entry, key, &port, log, None) {
            Ok(()) => say(format_args!("{shown} started again")),
            Err(why) => say(format_args!("{shown} could not be started again: {why}")),
        }
    }
}
