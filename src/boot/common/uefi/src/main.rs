//! The Ferrix UEFI loader.
//!
//! Firmware calls [`efi_main`] with a stack set up and the MMU on — in 64-bit
//! mode on the 64-bit pair, in SVC mode on ARMv7-A — which is why this project
//! has no bootstrap assembly on any architecture. From there the job is:
//!
//! 1. read the kernel off the volume the loader came from,
//! 2. choose where it, the direct map and the vmap arena go (`kaslr`), copy
//!    it, and patch it to run there,
//! 3. build the address space `docs/ARCHITECTURE.md` §4 describes,
//! 4. take firmware's memory map and leave boot services,
//! 5. install the new tables and jump.
//!
//! Only step 5 is assembly, and only because the return address of a Rust
//! function call would be in the address space we just replaced.

#![no_std]
#![no_main]

mod arch;
mod console;
mod kaslr;
mod load;
mod services;
mod uefi;

use core::convert::Infallible;
use core::panic::PanicInfo;
use core::ptr;

use ferrix_bootinfo::{
    BOOT_STACK_SIZE, BOOTINFO_MAGIC, BOOTINFO_VERSION, BootInfo, FIRMWARE_SEED, FIRMWARE_TIME,
    Framebuffer, MemRegion, PAGE_SIZE,
};

use console::println;
use load::{AddressSpace, DirectMap, KernelImage, LoaderMemory};
use services::{Allocation, BootError, MemoryMap, Result, Services};
use uefi::tables::{ACPI_10_GUID, ACPI_20_GUID, DEVICE_TREE_GUID, MemoryType, SystemTable};
use uefi::{Handle, Status};

/// Where the kernel lives on the EFI system partition.
const KERNEL_PATH: &str = "/FERRIX/KERNEL.ELF";

/// Where the initramfs is, when the image carries one.
const INITRD_PATH: &str = "/FERRIX/INITRD.IMG";

/// Where the loader looks for a kernel command line: a text file beside the
/// kernel, so an option such as `ferrix.onexit=reset` survives a reset that
/// clears what firmware was told.
const CMDLINE_PATH: &str = "/FERRIX/CMDLINE.TXT";

/// Where the image keeps options of its own: `cargo xtask flash --compositor`
/// writes `ferrix.checks=skip` here, and every `flash` rewrites or removes it.
/// A file of its own rather than lines in `CMDLINE.TXT`, which is the card
/// owner's and which `flash` never touches; appended after it, so that an
/// option given in both is the owner's, the kernel taking the first of a key.
const DEFAULTS_PATH: &str = "/FERRIX/DEFAULTS.TXT";

/// Bytes set aside for the boot info structure and the memory map behind it.
/// At 24 bytes a region this holds around 2700 of them; firmware typically
/// reports fewer than a hundred.
const BOOT_INFO_BYTES: u64 = 64 * 1024;

/// Offset of the memory region array within that allocation.
const REGIONS_OFFSET: u64 = PAGE_SIZE;

/// Offset of the command line within that allocation: straight after the
/// `BootInfo`, before the region array, which bounds how long it may be.
const CMDLINE_OFFSET: u64 = size_of::<BootInfo>() as u64;

/// What every flattened device tree begins with, big-endian.
const FDT_MAGIC: u32 = 0xD00D_FEED;

/// The largest device tree the loader will copy. QEMU's is a few kilobytes and
/// a board's a few tens; the bound is there so that a corrupt size field
/// cannot make the loader try to allocate the machine.
const MAX_DEVICE_TREE: u64 = 2 * 1024 * 1024;

/// Firmware's entry point.
///
/// The name is what the UEFI targets look for; it is not called from Rust.
#[unsafe(no_mangle)]
extern "efiapi" fn efi_main(image: Handle, system_table: *mut SystemTable) -> Status {
    if !system_table.is_null() {
        // SAFETY: firmware passed this table and guarantees it is live for the
        // whole of boot services.
        let table = unsafe { &*system_table };
        // SAFETY: `con_out` is firmware's own text output protocol, and
        // `console::shutdown` is called before boot services end.
        unsafe { console::init(table.con_out) };
    }

    println!(
        "Ferrix loader {} ({})",
        env!("CARGO_PKG_VERSION"),
        arch::ARCH.name()
    );

    match boot(image, system_table) {
        // `boot` returns `Infallible` on success, so this arm cannot be reached
        // and the compiler knows it.
        Ok(never) => match never {},
        Err(error) => {
            println!("FERRIX-PANIC loader: {error}");
            Status::LOAD_ERROR
        }
    }
}

/// Where the direct map will end, measured before anything the kernel is
/// handed is allocated, so that all of it can be placed below that.
///
/// Only a 32-bit machine with more RAM than the direct map holds is changed by
/// the ceiling this sets; everywhere else it is the top of RAM.
fn measure_direct_map(services: &Services) -> Result<DirectMap> {
    let probe = services.allocate(
        "allocating a buffer to measure RAM",
        services.memory_map_size()?,
        MemoryType::LOADER_DATA,
    )?;
    let direct = DirectMap::of(&services.memory_map(probe)?)?;
    services.free("freeing the RAM measuring buffer", probe)?;
    Ok(direct)
}

/// Say what the kernel's address space came to.
fn report_address_space(memory: &LoaderMemory, space: &AddressSpace, switch: load::Switch) {
    println!(
        "  {} page tables, roots {:#x}/{:#x}",
        memory.tables_used(),
        space.kernel_root.0,
        space.identity_root.0
    );
    // Only on a machine that needed either, so that the log of a machine that
    // did not is the log it always was.
    if let Some((base, len)) = space.loader_alias {
        println!("  loader mapped at {base:#x} inside the kernel tree, {len} bytes");
    }
    if let Some(page) = switch.trampoline {
        println!("  switch copied to a trampoline at {:#x}", page.address);
    }
}

/// Everything between firmware and the kernel.
fn boot(image: Handle, system_table: *mut SystemTable) -> Result<Infallible> {
    // SAFETY: these are the arguments firmware passed to `efi_main`, and boot
    // services have not been exited.
    let services = unsafe { Services::new(image, system_table)? };
    arch::prepare_cpu().map_err(BootError::plain)?;

    let direct = measure_direct_map(&services)?;
    services.allocate_below(direct.end());

    // Before the kernel is placed, because the command line can say
    // `nokaslr`, and that decides where the kernel goes.
    let (info_area, cmdline_len) = boot_info_area(&services)?;
    let loader = services.image_range()?;
    let (kernel, choice) = stage_kernel(&services, (direct, loader), (info_area, cmdline_len))?;
    let direct = direct.at(choice.physmap_base);
    let mut memory = LoaderMemory::new(&services)?;

    let stack = services.allocate(
        "allocating the boot stack",
        BOOT_STACK_SIZE,
        MemoryType::FERRIX_BOOT_STACK,
    )?;
    let device_tree = copy_device_tree(&services)?;
    let initrd = load_initrd(&services)?;
    let switch = load::place_switch(
        &services,
        direct,
        loader,
        (kernel.image.virt_base, kernel.image.memory.len),
    )?;
    let map_buffer = services.allocate(
        "allocating the memory map buffer",
        services.memory_map_size()?,
        MemoryType::LOADER_DATA,
    )?;

    // A first look at the memory map, only to learn where RAM is. The map
    // fetched here is stale the moment anything else is allocated, which is
    // why the one handed to the kernel is fetched again below.
    let first_look = services.memory_map(map_buffer)?;
    direct.confirm(&first_look)?;
    println!(
        "  direct map of {:#x}..{:#x}, kernel at {:#x}",
        direct.origin,
        direct.end(),
        kernel.image.memory.address
    );

    // Everything the kernel is handed has to be reachable through the direct
    // map. The ceiling set above is meant to guarantee it; this is the check
    // that it did, since firmware is the one choosing the addresses.
    let handed_over = [kernel.image.memory, memory.pool(), stack, info_area];
    let copied = device_tree.map(|(copy, _)| copy);
    if !handed_over
        .into_iter()
        .chain(copied)
        .chain(initrd.map(|(file, _)| file))
        .all(|allocation| direct.covers(allocation))
    {
        return Err(BootError::plain(
            "firmware placed a loader allocation above the direct map",
        ));
    }

    let space = load::build_address_space(
        &mut memory,
        &kernel.elf()?,
        &kernel.image,
        // Still the map `direct` was measured from: nothing has been
        // allocated since, and the buffer is not fetched into again until
        // the final map is taken below.
        (direct, &first_look),
        switch.identity,
        firmware_rsdp(&services),
    )?;
    report_address_space(&memory, &space, switch);

    // After the address space is built from `first_look`: asking firmware
    // for its graphics outputs allocates, which changes the map.
    let framebuffer = report_framebuffer(services.framebuffer(&first_look));

    write_boot_info(
        &services,
        (&kernel.image, choice),
        &space,
        stack,
        (info_area, cmdline_len),
        direct,
        Carried {
            device_tree,
            initrd,
            framebuffer,
        },
    );

    // Past this line firmware is gone: no allocation, no console, no protocols.
    console::shutdown();
    let map = leave_firmware(&services, map_buffer)?;
    let regions = record_memory_map(&map, info_area);
    finish_boot_info(info_area, regions, direct, &map);

    // The Arm architectures turn the MMU off in the middle of the switch, so
    // anything still dirty in a cache would vanish. No-op on x86-64.
    arch::clean_dcache(memory.pool().address, memory.pool().len);
    // The loader's own image, because the switch turns the caches off and then
    // fetches the rest of itself from RAM — and firmware wrote this image, and
    // its relocations, as data.
    arch::clean_dcache(loader.0, loader.1);
    arch::clean_dcache(kernel.image.memory.address, kernel.image.memory.len);
    arch::clean_dcache(info_area.address, info_area.len);
    arch::clean_dcache(stack.address, stack.len);
    switch.clean();
    if let Some((copy, _)) = device_tree {
        arch::clean_dcache(copy.address, copy.len);
    }
    if let Some((file, _)) = initrd {
        arch::clean_dcache(file.address, file.len);
    }

    // SAFETY: boot services are gone, `prepare_cpu` ran above, and the tables
    // in `space` identity map the page the switch runs from — the loader's own
    // code, or the trampoline — which is what makes the instruction after the
    // switch fetchable.
    unsafe {
        arch::enter_kernel(arch::Handoff {
            root_table: space.kernel_root.0,
            identity_table: space.identity_root.0,
            entry: kernel.image.entry,
            // Both the stack and the boot info are in RAM, so the direct map
            // already covers them and neither needs a mapping of its own.
            stack_top: direct.address(stack.address + stack.len),
            boot_info: direct.address(info_area.address),
            switch: switch.trampoline.map(|page| page.address),
        })
    }
}

/// Read the initramfs, if the image carries one.
///
/// Absent is not an error: a board card flashed before stage 8, or an image
/// assembled by hand, boots as it always did and the kernel reports that it
/// was handed nothing. Every other failure to read it is one, because a
/// half-read archive would unpack as a filesystem quietly missing files.
///
/// In memory the map reports as `Initrd`, which nothing reclaims: the kernel
/// reads the archive through the direct map after the loader is gone.
fn load_initrd(services: &Services) -> Result<Option<(Allocation, u64)>> {
    match services.read_file(INITRD_PATH, MemoryType::FERRIX_INITRD) {
        Ok((file, len)) => {
            println!("  initramfs {} KiB", len.div_ceil(1024));
            Ok(Some((file, len)))
        }
        Err(error) if error.status == Some(Status::NOT_FOUND) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Copy firmware's device tree into memory the kernel keeps.
///
/// Firmware's own copy lives wherever firmware put it — U-Boot uses
/// `EfiACPIReclaimMemory`, which the kernel hands to the frame allocator once
/// interrupt bring-up has read it — and stage 10 enumerates devices from the
/// tree long after that. So the loader takes a copy, in memory the map reports
/// as `DeviceTree`, which nothing reclaims.
///
/// `None` on a machine that offers no tree, which is the ordinary case on
/// x86-64 and on AArch64 under ACPI.
fn copy_device_tree(services: &Services) -> Result<Option<(Allocation, u64)>> {
    let Some(tree) = services.configuration_table(&DEVICE_TREE_GUID) else {
        return Ok(None);
    };

    // SAFETY: firmware published a device tree at this address, identity
    // mapped under boot services, and every tree begins with a forty-byte
    // header whose first eight bytes are read here.
    let header = unsafe { core::slice::from_raw_parts(tree as *const u8, 8) };
    let word = |at: usize| {
        header
            .get(at..at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map_or(0, u32::from_be_bytes)
    };
    if word(0) != FDT_MAGIC {
        return Err(BootError::plain(
            "firmware's device tree does not begin with the device tree magic",
        ));
    }
    let len = u64::from(word(4));
    if !(40..=MAX_DEVICE_TREE).contains(&len) {
        return Err(BootError::plain(
            "firmware's device tree claims an impossible size",
        ));
    }

    let copy = services.allocate(
        "copying the device tree",
        len,
        MemoryType::FERRIX_DEVICE_TREE,
    )?;
    // SAFETY: firmware's tree is `len` bytes long by its own header, the copy
    // is a fresh allocation of at least that many, and the two cannot overlap.
    unsafe { ptr::copy_nonoverlapping(tree as *const u8, copy.address as *mut u8, len as usize) };
    println!("  device tree copied, {len} bytes");
    Ok(Some((copy, len)))
}

/// The kernel file and the image copied out of it.
///
/// The file allocation is kept because the parsed [`ferrix_elf::Elf`] borrows
/// it, and the address space is built from the same parse that placed it.
#[derive(Clone, Copy, Debug)]
struct StagedKernel {
    file: Allocation,
    file_len: u64,
    image: KernelImage,
}

impl StagedKernel {
    /// Re-borrow the file bytes as a parsed image.
    fn elf(&self) -> Result<ferrix_elf::Elf<'_>> {
        // SAFETY: `file` is our own allocation, firmware filled `file_len` of
        // its bytes, and it is identity mapped and not freed until we exit.
        let bytes = unsafe {
            core::slice::from_raw_parts(self.file.address as *const u8, self.file_len as usize)
        };
        load::parse_kernel(bytes)
    }
}

/// Allocate the boot info area and read the command line into it, and say
/// how many bytes that came to.
fn boot_info_area(services: &Services) -> Result<(Allocation, u64)> {
    let info_area = services.allocate(
        "allocating the boot info",
        BOOT_INFO_BYTES,
        MemoryType::FERRIX_BOOT_INFO,
    )?;
    Ok((info_area, load_cmdline(services, info_area)))
}

/// Read the kernel, decide where it and the other moving regions go, and copy
/// it there.
///
/// `info_area` holds the `cmdline_len` bytes of command line
/// [`load_cmdline`] put there, which may say `nokaslr`.
fn stage_kernel(
    services: &Services,
    (direct, loader): (DirectMap, (u64, u64)),
    (info_area, cmdline_len): (Allocation, u64),
) -> Result<(StagedKernel, kaslr::Choice)> {
    let (file, file_len) = load::read_kernel_file(services, KERNEL_PATH)?;
    let staged = StagedKernel {
        file,
        file_len,
        image: KernelImage::EMPTY,
    };
    let elf = staged.elf()?;
    // SAFETY: `load_cmdline` wrote `cmdline_len` bytes of UTF-8 there, inside
    // the loader's own allocation, and nothing writes them again.
    let cmdline = unsafe {
        core::slice::from_raw_parts(
            (info_area.address + CMDLINE_OFFSET) as *const u8,
            cmdline_len as usize,
        )
    };
    let declined = core::str::from_utf8(cmdline).is_ok_and(kaslr::declined);
    let mut choice = kaslr::choose(
        services,
        &elf,
        load::kernel_span(&elf)?,
        direct,
        loader,
        declined,
    )?;
    // EXPERIMENT ONLY (po9-user), never lands: force the image slide.
    if let Some(slide) = core::str::from_utf8(cmdline)
        .ok()
        .and_then(|line| ferrix_bootinfo::option_in(line, "po9.slide"))
        .and_then(|hex| u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
    {
        choice.kernel_virt = choice.kaslr.link + slide;
        crate::console::println!("  kaslr    po9 experiment: slide forced to {:#x}", slide);
    }
    let image = load::place_kernel(services, &elf, choice.kernel_virt)?;
    Ok((StagedKernel { image, ..staged }, choice))
}

/// What the loader copied into memory the kernel keeps, besides the kernel.
///
/// Both optional, and both described to the kernel the same way: where the
/// copy is, and how many of its bytes are real.
#[derive(Clone, Copy, Debug)]
struct Carried {
    /// The device tree, on a machine that has one.
    device_tree: Option<(Allocation, u64)>,
    /// The initramfs, on an image that carries one.
    initrd: Option<(Allocation, u64)>,
    /// The framebuffer [`Services::framebuffer`] chose, if any.
    framebuffer: Option<Framebuffer>,
}

/// Where firmware's ACPI root system description pointer is, or zero.
///
/// Asked twice, by the direct map and by the boot info, and answered the same
/// both times: the configuration table does not change under boot services.
fn firmware_rsdp(services: &Services) -> u64 {
    services
        .configuration_table(&ACPI_20_GUID)
        .or_else(|| services.configuration_table(&ACPI_10_GUID))
        .unwrap_or(0)
}

/// Fill in everything about the boot info that firmware can still be asked.
fn write_boot_info(
    services: &Services,
    (kernel, choice): (&KernelImage, kaslr::Choice),
    space: &AddressSpace,
    stack: Allocation,
    (info_area, cmdline_len): (Allocation, u64),
    direct: DirectMap,
    carried: Carried,
) {
    let Carried {
        device_tree,
        initrd,
        framebuffer,
    } = carried;
    let time = services.firmware_time();
    let seed = services.firmware_seed();
    // What the kernel's clock and random generator start from, said here
    // because firmware is the only one who can say which it lacked.
    println!(
        "  firmware clock {}, random number protocol {}",
        if time.is_some() { "read" } else { "absent" },
        if seed.is_some() { "read" } else { "absent" }
    );
    let info = BootInfo {
        magic: BOOTINFO_MAGIC,
        version: BOOTINFO_VERSION,
        arch: arch::ARCH,
        // Filled in by `finish_boot_info` once the final map has been taken.
        regions: 0,
        regions_len: 0,
        physmap_base: direct.base,
        physmap_phys: direct.origin,
        physmap_len: direct.len,
        kernel_phys: kernel.memory.address,
        kernel_virt: kernel.virt_base,
        kernel_len: kernel.memory.len,
        root_table_phys: space.kernel_root.0,
        ttbr0_phys: if arch::SEPARATE_IDENTITY_TABLE {
            space.identity_root.0
        } else {
            0
        },
        loader_alias_phys: space.loader_alias.map_or(0, |(base, _)| base),
        loader_alias_len: space.loader_alias.map_or(0, |(_, len)| len),
        boot_stack_top: direct.address(stack.address + stack.len),
        boot_stack_size: stack.len,
        framebuffer: framebuffer.unwrap_or(Framebuffer::NONE),
        initrd_phys: initrd.map_or(0, |(file, _)| file.address),
        initrd_len: initrd.map_or(0, |(_, len)| len),
        rsdp: firmware_rsdp(services),
        dtb: device_tree.map_or(0, |(copy, _)| copy.address),
        dtb_len: device_tree.map_or(0, |(_, len)| len),
        uefi_system_table: services.system_table() as u64,
        cmdline: if cmdline_len == 0 {
            0
        } else {
            direct.address(info_area.address + CMDLINE_OFFSET)
        },
        cmdline_len,
        firmware_time: time.unwrap_or(0),
        firmware_seed: seed.unwrap_or([0; 32]),
        firmware_flags: time.map_or(0, |_| FIRMWARE_TIME) | seed.map_or(0, |_| FIRMWARE_SEED),
        firmware_seed_len: seed.map_or(0, |bytes| bytes.len() as u64),
        kaslr: choice.kaslr,
    };

    // SAFETY: `info_area` is our own allocation of BOOT_INFO_BYTES, identity
    // mapped, and larger than one BootInfo.
    unsafe { ptr::write_volatile(info_area.address as *mut BootInfo, info) };
}

/// Say which framebuffer the loader chose, of how many graphics outputs, and
/// whether the kernel will draw a panic on it; and pass the choice on.
fn report_framebuffer((framebuffer, outputs): (Option<Framebuffer>, usize)) -> Option<Framebuffer> {
    if let Some(chosen) = framebuffer {
        println!(
            "  framebuffer {}x{} at {:#x}, of {outputs} graphics outputs, {}",
            chosen.width,
            chosen.height,
            chosen.phys,
            if chosen.is_reclaimable() {
                "in memory the kernel reclaims: no panic screen"
            } else {
                "outside the allocator's memory"
            }
        );
    }
    framebuffer
}

/// Take the final memory map and leave boot services.
///
/// The specification allows `exit_boot_services` to fail if the map changed
/// between fetching it and the call, and says to fetch it again and retry once.
fn leave_firmware(services: &Services, buffer: Allocation) -> Result<MemoryMap> {
    let map = services.memory_map(buffer)?;
    if services.exit_boot_services(map.key).is_ok() {
        return Ok(map);
    }

    let map = services.memory_map(buffer)?;
    services.exit_boot_services(map.key)?;
    Ok(map)
}

/// Read `CMDLINE.TXT` and then `DEFAULTS.TXT`, where the volume has them, into
/// the boot info area as one command line, and say how many bytes it came to.
///
/// A missing file is the ordinary case and says nothing. A file that cannot be
/// used is reported and ignored rather than refused: every option the kernel
/// reads has a safe default, and a board that will not boot because of a typo
/// in a text file is worse than one that boots without the option. The two
/// are joined by a space, the owner's first, so the kernel -- which takes the
/// first word with a key -- takes the owner's value over the image's.
fn load_cmdline(services: &Services, info_area: Allocation) -> u64 {
    let capacity = (REGIONS_OFFSET - CMDLINE_OFFSET) as usize;
    // SAFETY: these bytes are inside `info_area`, the loader's own zeroed
    // allocation of BOOT_INFO_BYTES, between the `BootInfo` and the region
    // array, which nothing else refers to until the kernel is entered; the
    // files the text is read into are other allocations.
    let destination = unsafe {
        core::slice::from_raw_parts_mut((info_area.address + CMDLINE_OFFSET) as *mut u8, capacity)
    };
    let mut written = 0usize;
    for (path, whose) in [
        (CMDLINE_PATH, ""),
        (DEFAULTS_PATH, ", the image's defaults"),
    ] {
        let Some(text) = read_command_file(services, path, capacity) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let gap = usize::from(written > 0);
        let Some(slot) = destination.get_mut(written..written + gap + text.len()) else {
            println!("  cmdline  {path} is ignored: the command line has no room left for it");
            continue;
        };
        let (space, rest) = slot.split_at_mut(gap);
        space.fill(b' ');
        rest.copy_from_slice(text.as_bytes());
        written += gap + text.len();
        println!("  cmdline  {text}  (from {path}{whose})");
    }
    written as u64
}

/// One command-line file's text, trimmed, or nothing when it is missing or
/// cannot be used -- which [`load_cmdline`] explains.
fn read_command_file<'a>(services: &Services, path: &str, capacity: usize) -> Option<&'a str> {
    let (file, len) = match services.read_file(path, MemoryType::LOADER_DATA) {
        Ok(read) => read,
        Err(error) if error.status == Some(Status::NOT_FOUND) => return None,
        Err(error) => {
            println!("  cmdline  {path} could not be read, so it is ignored: {error}");
            return None;
        }
    };
    // SAFETY: `read_file` filled `len` bytes of its own allocation, which is
    // identity mapped under boot services, never freed and not written again.
    let bytes = unsafe { core::slice::from_raw_parts(file.address as *const u8, len as usize) };
    match ferrix_bootinfo::command_line_from_file(bytes, capacity) {
        Ok(text) => Some(text),
        Err(why) => {
            println!("  cmdline  {path} is ignored: {why}");
            None
        }
    }
}

/// Copy the firmware memory map into the boot info, sorted by address.
///
/// Runs after `exit_boot_services`, so it touches nothing but memory the loader
/// allocated for itself.
fn record_memory_map(map: &MemoryMap, info_area: Allocation) -> u64 {
    let array = (info_area.address + REGIONS_OFFSET) as *mut MemRegion;
    let capacity = (info_area.len - REGIONS_OFFSET) / size_of::<MemRegion>() as u64;

    // SAFETY: the array sits inside the loader's own allocation, `capacity` was
    // computed from that allocation's length, and `Services::allocate` zeroed
    // it. An all-zero `MemRegion` is a valid one -- base 0, length 0, kind
    // `Usable` -- so the slice is initialised even before anything is written.
    let slots = unsafe { core::slice::from_raw_parts_mut(array, capacity as usize) };

    let mut count = 0usize;
    for (slot, descriptor) in slots.iter_mut().zip(map.entries()) {
        *slot = load::describe(&descriptor);
        count += 1;
    }

    // Firmware usually reports the map in address order but is not required
    // to, and the kernel's frame allocator walks it assuming it is. Sorting a
    // slice needs no allocator, which is just as well: there is none, and
    // firmware is gone.
    let Some(written) = slots.get_mut(..count) else {
        return 0;
    };
    written.sort_unstable_by_key(|region| region.base);

    count as u64
}

/// Point the boot info at the memory map now that it exists.
fn finish_boot_info(info_area: Allocation, regions: u64, direct: DirectMap, map: &MemoryMap) {
    let info = info_area.address as *mut BootInfo;
    // SAFETY: `write_boot_info` put a BootInfo here, and nothing else refers to
    // it.
    let mut value = unsafe { ptr::read_volatile(info) };
    // The kernel reads this through the direct map, so the pointer it is given
    // has to be the virtual one.
    value.regions = direct.address(info_area.address + REGIONS_OFFSET);
    value.regions_len = regions;
    // Again from the final map, which is the one the frame allocator walks,
    // so what the kernel reads is what the allocator will do.
    if value.framebuffer.is_present() {
        let owned = ferrix_bootinfo::allocator_owns(
            map.entries().map(|descriptor| load::describe(&descriptor)),
            value.framebuffer.phys,
            value.framebuffer.size,
        );
        value.framebuffer.reclaimable = u32::from(owned);
    }
    // SAFETY: as above.
    unsafe { ptr::write_volatile(info, value) };
}

/// Where a loader panic ends up.
///
/// There is nothing to unwind to and, after `exit_boot_services`, nothing left
/// to print with — so this says what it can and stops the machine rather than
/// letting it wander.
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    println!("FERRIX-PANIC loader: {info}");
    loop {
        core::hint::spin_loop();
    }
}
