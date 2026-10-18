//! Starting the other processors on x86-64.
//!
//! The protocol is from the multiprocessor specification and has not changed
//! since 1997: send the target an `INIT`, wait, send it a `STARTUP` carrying a
//! page number, wait, send the `STARTUP` again. The processor begins executing
//! in sixteen-bit real mode at the start of that page.
//!
//! The second `STARTUP` is not a typo and not superstition. The specification
//! requires two, because on some parts the first is dropped if it arrives while
//! the `INIT` is still being processed. A processor that received the first one
//! is already running and ignores the second, so sending both costs nothing and
//! sending one costs a processor that sometimes does not start.
//!
//! # The page below 1 MiB
//!
//! `STARTUP` carries the address as a single byte, shifted left by twelve. That
//! is the only reason the trampoline lives in low memory: 8 bits of page number
//! is 1 MiB of address space, and there is nowhere else to put it. The page is
//! taken from the frame allocator like any other — see
//! [`crate::mm::allocate_frames_below`] — rather than hard-coded, so nothing
//! else in the system has to know it is spoken for.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_acpi::MadtEntry;
use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_paging::MapFlags;

use super::{apic, gdt, trap, trampoline};
use crate::{mm, percpu, smp, vmap};

/// The first frame at or above 1 MiB, which is where `STARTUP`'s reach ends.
const LOW_MEMORY_LIMIT: u64 = 0x10_0000 / PAGE_SIZE;

/// The physical address of the trampoline page, once one has been taken.
static TRAMPOLINE: AtomicU64 = AtomicU64::new(0);

/// Number every processor the MADT describes, this one first.
///
/// The boot processor takes number zero explicitly rather than by trusting the
/// table's order. Firmware conventionally lists it first and is not required
/// to, and a kernel that renumbers its own boot CPU midway through bring-up
/// would lose every per-CPU counter written before the renumbering.
///
/// # Safety
///
/// The local APIC must already be mapped, since this reads this CPU's own
/// identifier out of it.
pub(crate) unsafe fn enumerate_cpus(view: &BootView<'_>) -> Result<usize, &'static str> {
    let firmware =
        crate::acpi::Firmware::open(view).map_err(|_| "the machine has no readable ACPI tables")?;
    let madt = firmware
        .acpi()
        .madt()
        .map_err(|_| "the machine has no MADT")?;

    let boot = apic::local_id();
    if percpu::assign(boot as usize).is_none() {
        return Err("the boot processor could not be given a number");
    }

    for entry in madt.entries() {
        let MadtEntry::LocalApic(local) = entry else {
            continue;
        };
        // `online_capable` without `enabled` means a socket that is empty now
        // and could be filled while the machine runs. Ferrix does not hot plug
        // CPUs, and starting one that is not there hangs on the delivery wait.
        if !local.is_enabled() {
            continue;
        }
        let id = u32::from(local.apic_id);
        if id == boot || percpu::index_of(id as usize).is_some() {
            continue;
        }
        // Out of numbers is not an error: `MAX_CPUS` is a deliberate bound, and
        // a machine with more cores than that should run on the ones it fits
        // rather than refuse to boot.
        if percpu::assign(id as usize).is_none() {
            break;
        }
    }

    Ok(percpu::assigned())
}

/// Start the processor holding CPU number `cpu`.
///
/// Returns once the `STARTUP` has been sent. Whether the processor actually
/// arrived is the caller's business — see [`crate::smp::start_others`], which
/// waits for it to say so.
///
/// # Safety
///
/// `cpu` must be a number [`enumerate_cpus`] assigned to a processor that has
/// not been started. Starting one twice sends `INIT` to a running CPU, which
/// resets it in the middle of whatever it was doing.
pub(crate) unsafe fn start_cpu(cpu: usize) -> Result<(), &'static str> {
    let apic_id = u32::try_from(percpu::identifier_of(cpu).ok_or("that CPU has no identifier")?)
        .map_err(|_| "that CPU's identifier does not fit a local APIC's")?;

    let page = trampoline_page()?;
    let stack = vmap::allocate_stack().map_err(|_| "no kernel stack for the new processor")?;

    // SAFETY: the page is one this module took from the frame allocator and
    // mapped at its own address; `cpu` is the caller's to promise.
    unsafe { write_trampoline(page, stack.top, cpu) }?;

    // SAFETY: `apic_id` names a processor the MADT described as enabled and
    // that nothing has started, which is the caller's contract.
    unsafe { apic::send_init(apic_id) };
    crate::timer::spin_nanos(INIT_SETTLE_NANOS);

    let vector = u8::try_from(page / PAGE_SIZE).map_err(|_| "the trampoline is above 1 MiB")?;
    for _ in 0..2 {
        // SAFETY: as above, and the page holds the trampoline written a moment
        // ago and identity mapped in the `CR3` the trampoline loads.
        unsafe { apic::send_startup(apic_id, vector) };
        crate::timer::spin_nanos(STARTUP_SETTLE_NANOS);
    }

    Ok(())
}

/// How long to leave an `INIT` to take effect. The specification says 10 ms.
const INIT_SETTLE_NANOS: u64 = 10_000_000;

/// How long to leave a `STARTUP`. The specification says 200 µs; this is
/// longer, because under an emulator the processor being started is a host
/// thread that may not have been scheduled yet.
const STARTUP_SETTLE_NANOS: u64 = 1_000_000;

/// Take the low page the trampoline runs on, and map it at its own address.
///
/// Once for the machine: the page is reused for every processor, because they
/// are started one at a time.
fn trampoline_page() -> Result<u64, &'static str> {
    let existing = TRAMPOLINE.load(Ordering::Acquire);
    if existing != 0 {
        return Ok(existing);
    }

    let frame = mm::allocate_frames_below(0, LOW_MEMORY_LIMIT)
        .ok_or("the machine has no free page below 1 MiB to start a processor from")?;
    let page = frame * PAGE_SIZE;

    // Identity, not the direct map: the first instructions after the trampoline
    // turns paging on are fetched from the physical address it is already
    // running at, so that address has to translate to itself. The mapping is
    // dropped again by `finish`, before the W^X sweep would see a page that is
    // both writable and executable — it is not, because this alias is not
    // writable and the direct map alias is not executable, but a live identity
    // mapping of low memory is exactly the thing dropping the loader's map
    // exists to prove is gone.
    mm::map_kernel(page, page, PAGE_SIZE, MapFlags::KERNEL_CODE)
        .map_err(|_| "the trampoline page could not be mapped at its own address")?;

    TRAMPOLINE.store(page, Ordering::Release);
    Ok(page)
}

/// Copy the blob to the trampoline page and fill in its parameters.
///
/// # Safety
///
/// `page` must be the trampoline page, mapped at its own address and covered
/// by the direct map, and no processor may currently be running the blob that
/// is there — which holds because processors are started one at a time and the
/// one before has reached Rust before this is called again.
unsafe fn write_trampoline(page: u64, stack_top: u64, cpu: usize) -> Result<(), &'static str> {
    let blob = trampoline::blob();
    if blob.len() + size_of::<u64>() > PAGE_SIZE as usize {
        return Err("the trampoline does not fit in a page");
    }

    let root = mm::root_table();
    if root >= u64::from(u32::MAX) {
        // `mov %eax, %cr3` in the trampoline is a 32-bit move, because long
        // mode does not exist yet at that point. A root table above 4 GiB would
        // silently load the wrong one.
        return Err("the kernel's page table root is above 4 GiB");
    }

    let at = mm::direct_map_of(page);
    // SAFETY: `at` is this page through the direct map, which is writable and
    // aliases nothing else; the length is the blob's and was just checked to
    // fit. The blob is a `&[u8]` in the kernel image, so the two cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(blob.as_ptr(), at as *mut u8, blob.len()) };

    let params = trampoline::parameters();
    // SAFETY: every offset is inside the blob just copied, and each names a
    // naturally aligned `u64` the assembly reserved for exactly this.
    unsafe {
        write_param(at, params.cr3, root);
        write_param(at, params.stack, stack_top);
        write_param(at, params.entry, ap_entry as usize as u64);
        write_param(at, params.index, cpu as u64);
    }

    Ok(())
}

/// Write one parameter into the copied blob.
///
/// # Safety
///
/// `base + offset` must be an eight-byte-aligned address inside the trampoline
/// page, through the direct map.
unsafe fn write_param(base: u64, offset: usize, value: u64) {
    // SAFETY: the caller's contract.
    unsafe { core::ptr::write_volatile((base + offset as u64) as *mut u64, value) };
}

/// Where an application processor arrives, in Rust, in long mode, on its own
/// stack.
///
/// The trampoline jumps here; nothing calls it. It never returns.
extern "C" fn ap_entry(cpu: u64) -> ! {
    // This CPU's own descriptor tables first, before anything can fault. Until
    // they are loaded it is still using the trampoline's three-entry GDT, which
    // has no task state segment and therefore no stack for a fault to land on.
    //
    // SAFETY: called once, on this processor, before it enables interrupts.
    unsafe { gdt::init(cpu as usize) };
    // SAFETY: after `gdt::init`, whose kernel code selector every gate names.
    unsafe { trap::load_idt() };
    // SAFETY: this processor has an IDT now and its interrupts are masked.
    unsafe { apic::init_secondary() };

    smp::ap_main()
}

/// Drop the identity mapping the trampoline needed.
///
/// Called once every processor is up. The page itself goes back to the frame
/// allocator with it: nothing restarts a processor, and stage 4 is the last
/// stage that could.
pub(crate) fn finish_smp() {
    let page = TRAMPOLINE.swap(0, Ordering::AcqRel);
    if page == 0 {
        return;
    }
    let _ = mm::unmap_kernel(page, PAGE_SIZE, |frame, order| {
        mm::deallocate_frames(frame, order);
    });
}

/// Send the inter-processor interrupt to every other CPU.
pub(crate) fn send_ipi_to_others() {
    apic::send_ipi_to_others(apic::IPI_VECTOR);
}

/// What this CPU is called by the hardware.
pub(crate) fn cpu_identifier() -> usize {
    apic::local_id() as usize
}

/// The interrupt number the inter-processor interrupt arrives on.
pub(crate) fn ipi_irq() -> u32 {
    apic::ipi_irq()
}
