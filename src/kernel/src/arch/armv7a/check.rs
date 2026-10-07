//! The boot check for what this architecture decides on its own from values
//! the machine hands it.
//!
//! Stage 3's trap check proves the entry path with two breakpoints and three
//! page faults, which is every vector entry and fault status an ordinary boot
//! raises. The decoding behind it has more to say: an alignment fault, an
//! external abort, the vector entries a running kernel should never see. Each
//! is required here to become the trap and the signal Linux raises for it
//! (`arch/arm/mm/fault.c`, `arch/arm/kernel/traps.c`), from a frame built
//! with that vector entry and status. The rest is the same kind of thing:
//!
//! * which serial port a device tree makes the console, from trees built
//!   here: a `console=` override in `/chosen/bootargs`, one naming no port,
//!   and a tree with no `stdout-path` whose first port is turned off --
//!   what a board being brought up has and QEMU's `virt` does not;
//! * cleaning a buffer from the data cache for a device that does not snoop,
//!   which leaves what it holds;
//! * masking a line at the controller and letting it through again, which a
//!   device whose driver holds its interrupt relies on, read back from the
//!   distributor, a shared line and a private one;
//! * the refusals: an identifier the controller does not have, and a
//!   `GICv2m` frame at address zero or naming a range that is not shared
//!   peripheral interrupts.
//!
//! Every frame, tree, line and request is made for the check and given back:
//! the lines were ones nothing had enabled, and are left that way.

use alloc::format;
use alloc::vec::Vec;
use core::hint::black_box;

use ferrix_fdt::{FDT_BEGIN_NODE, FDT_END, FDT_END_NODE, FDT_MAGIC, FDT_PROP, Fdt, HEADER_SIZE};
use ferrix_linux_abi::types::{SIGBUS, SIGILL, SIGSEGV, SIGTRAP};

use super::trap::{TrapFrame, UserRegs, classify, fault_signal};
use crate::arch::arm_common::gicv2;
use crate::console::println;
use crate::trap::{PageFault, Trap};

/// Vector entries, as the stubs number them.
const RESET: u32 = 0;
const UNDEFINED: u32 = 1;
const SVC: u32 = 2;
const PREFETCH_ABORT: u32 = 3;
const DATA_ABORT: u32 = 4;
const HYP_TRAP: u32 = 5;
const IRQ: u32 = 6;
const FIQ: u32 = 7;

/// `CPSR.M` for user mode, and for supervisor mode.
const USR: u32 = 0x10;
const SVC_MODE: u32 = 0x13;

/// `si_code` values the signal carries.
const SEGV_MAPERR: i32 = 1;
const SEGV_ACCERR: i32 = 2;
const BUS_ADRALN: i32 = 1;
const BUS_OBJERR: i32 = 3;
const ILL_ILLOPC: i32 = 1;
const TRAP_BRKPT: i32 = 1;

/// Where the frames say the trap was taken and what it touched.
const PC: u32 = 0x0040_1002;
const FAR: u32 = 0x0dea_d000;

/// Run the check, and say what it proved.
///
/// # Errors
///
/// What did not hold.
pub(crate) fn check() -> Result<(), &'static str> {
    let decoded = check_trap_decoding()?;
    check_frames_render()?;
    let trees = check_chosen(&console_trees())?;
    check_cache_maintenance()?;
    let lines = check_masking()?;
    let refused = check_refusals()?;
    println!(
        "  machine  {decoded} vector entries and fault statuses decoded to the trap and signal \
         Linux gives them, {trees} device trees given their console port, {lines} idle lines \
         masked and let through at the controller, {refused} requests refused as specified"
    );
    Ok(())
}

/// A frame for an exception taken at vector entry `kind` with fault status
/// `fsr`, from the mode `cpsr` names.
const fn frame(kind: u32, fsr: u32, cpsr: u32) -> TrapFrame {
    TrapFrame {
        kind,
        fsr,
        far: FAR,
        reserved: 0,
        r: [0; 13],
        lr: 0,
        pc: PC,
        cpsr,
    }
}

/// One case: the frame, the trap it must classify as, and the signal a
/// program that took it must get -- `None` where the trap is not a fault.
type Case = (TrapFrame, Trap, Option<(u32, i32, u64)>);

/// The vector entries and fault statuses, and what each becomes. Returns
/// how many.
fn check_trap_decoding() -> Result<usize, &'static str> {
    /// Data fault status: the access was a write.
    const WRITE: u32 = 1 << 11;
    /// Long-descriptor fault statuses.
    const TRANSLATION: u32 = 0b00_0101;
    const ACCESS_FLAG: u32 = 0b00_1001;
    const PERMISSION: u32 = 0b00_1111;
    const DEBUG: u32 = 0b10_0010;
    const ALIGNMENT: u32 = 0b10_0001;
    const EXTERNAL: u32 = 0b01_0000;

    let pc = u64::from(PC);
    let far = u64::from(FAR);
    let fault = |write, execute, user, present| {
        Trap::PageFault(PageFault {
            address: far,
            write,
            execute,
            user,
            present,
        })
    };
    let named = |name, code| Trap::Fault { name, code };
    let cases: [Case; 15] = [
        (
            frame(UNDEFINED, 0, USR),
            Trap::IllegalInstruction,
            Some((SIGILL, ILL_ILLOPC, pc)),
        ),
        (frame(SVC, 0, USR), Trap::SystemCall, None),
        // A `bkpt` arrives as a prefetch abort with the debug status.
        (
            frame(PREFETCH_ABORT, DEBUG, USR),
            Trap::Breakpoint,
            Some((SIGTRAP, TRAP_BRKPT, pc)),
        ),
        (
            frame(DATA_ABORT, WRITE | TRANSLATION, USR),
            fault(true, false, true, false),
            Some((SIGSEGV, SEGV_MAPERR, far)),
        ),
        (
            frame(DATA_ABORT, PERMISSION, SVC_MODE),
            fault(false, false, false, true),
            Some((SIGSEGV, SEGV_ACCERR, far)),
        ),
        // The write bit means something only for a data abort.
        (
            frame(PREFETCH_ABORT, WRITE | ACCESS_FLAG, USR),
            fault(false, true, true, false),
            Some((SIGSEGV, SEGV_MAPERR, far)),
        ),
        (
            frame(DATA_ABORT, ALIGNMENT, USR),
            named("alignment fault", u64::from(ALIGNMENT)),
            Some((SIGBUS, BUS_ADRALN, far)),
        ),
        (
            frame(DATA_ABORT, EXTERNAL, USR),
            named("data abort", u64::from(EXTERNAL)),
            Some((SIGBUS, BUS_OBJERR, far)),
        ),
        (
            frame(PREFETCH_ABORT, EXTERNAL, USR),
            named("prefetch abort", u64::from(EXTERNAL)),
            Some((SIGBUS, BUS_OBJERR, far)),
        ),
        (
            frame(RESET, 0, USR),
            named("reset", u64::from(RESET)),
            Some((SIGILL, ILL_ILLOPC, pc)),
        ),
        (
            frame(HYP_TRAP, 0, USR),
            named("hypervisor trap", u64::from(HYP_TRAP)),
            Some((SIGILL, ILL_ILLOPC, pc)),
        ),
        (
            frame(FIQ, 0, USR),
            named("FIQ", u64::from(FIQ)),
            Some((SIGILL, ILL_ILLOPC, pc)),
        ),
        // No stub numbers an entry past the eighth.
        (
            frame(9, 0, USR),
            named("exception", 9),
            Some((SIGILL, ILL_ILLOPC, pc)),
        ),
        (frame(IRQ, 0, USR), Trap::Interrupt(IRQ), None),
        (frame(IRQ, 0, SVC_MODE), Trap::Interrupt(IRQ), None),
    ];
    // Through `black_box`, so that the decoder runs on each frame as it would
    // on a trap's, rather than being folded away against constant input.
    for (frame, trap, signal) in &cases {
        let frame = &black_box(*frame);
        if classify(frame) != *trap {
            return Err("a vector entry and fault status were classified as the wrong trap");
        }
        if let Some(signal) = signal
            && fault_signal(frame, trap) != *signal
        {
            return Err("a program's fault was given a signal Linux would not give it");
        }
    }
    if !frame(SVC, 0, USR).came_from_user() || frame(SVC, 0, SVC_MODE).came_from_user() {
        return Err("a trap frame misreported whether user mode took the exception");
    }
    Ok(cases.len())
}

/// A frame, and a program's registers made of one, render for a report:
/// each type carries `Debug` so that it can, and a rendering without the
/// registers would be no use to one.
fn check_frames_render() -> Result<(), &'static str> {
    let frame = frame(DATA_ABORT, 0, USR);
    let text = format!("{frame:?} {:?}", UserRegs(frame));
    if !text.contains("fsr") || !text.contains("UserRegs") {
        return Err("a trap frame's rendering left out its registers");
    }
    Ok(())
}

/// A device tree under construction, just enough of the format for the
/// console trees below: nodes, string properties and `reg`, one cell each.
struct Tree {
    structs: Vec<u8>,
    strings: Vec<u8>,
}

impl Tree {
    /// A root with one-cell addresses and sizes.
    fn new() -> Tree {
        let mut tree = Tree {
            structs: Vec::new(),
            strings: Vec::new(),
        };
        tree.begin("");
        tree.cells("#address-cells", &[1]);
        tree.cells("#size-cells", &[1]);
        tree
    }

    fn word(&mut self, word: u32) {
        self.structs.extend_from_slice(&word.to_be_bytes());
    }

    fn padded(&mut self, bytes: &[u8]) {
        self.structs.extend_from_slice(bytes);
        self.structs.push(0);
        while !self.structs.len().is_multiple_of(4) {
            self.structs.push(0);
        }
    }

    fn begin(&mut self, name: &str) {
        self.word(FDT_BEGIN_NODE);
        self.padded(name.as_bytes());
    }

    fn end(&mut self) {
        self.word(FDT_END_NODE);
    }

    fn property(&mut self, name: &str, value: &[u8]) {
        let offset = u32::try_from(self.strings.len()).unwrap_or(u32::MAX);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.word(FDT_PROP);
        self.word(u32::try_from(value.len()).unwrap_or(u32::MAX));
        self.word(offset);
        self.structs.extend_from_slice(value);
        while !self.structs.len().is_multiple_of(4) {
            self.structs.push(0);
        }
    }

    fn text(&mut self, name: &str, value: &str) {
        let mut bytes = Vec::from(value.as_bytes());
        bytes.push(0);
        self.property(name, &bytes);
    }

    fn cells(&mut self, name: &str, cells: &[u32]) {
        let bytes: Vec<u8> = cells.iter().flat_map(|cell| cell.to_be_bytes()).collect();
        self.property(name, &bytes);
    }

    /// A serial port node at `address`, compatible with `compatible`, and
    /// turned off when `status` says so.
    fn port(&mut self, name: &str, compatible: &str, address: u32, status: Option<&str>) {
        self.begin(name);
        self.text("compatible", compatible);
        self.cells("reg", &[address, 0x1000]);
        if let Some(status) = status {
            self.text("status", status);
        }
        self.end();
    }

    /// The blob: header, an empty reservation map, the structure block and
    /// the strings.
    fn finish(mut self) -> Vec<u8> {
        self.end();
        self.word(FDT_END);
        let size = |bytes: usize| u32::try_from(bytes).unwrap_or(u32::MAX);
        let rsvmap = HEADER_SIZE;
        let structs = rsvmap + 16;
        let strings = structs + self.structs.len();
        let total = strings + self.strings.len();
        let header = [
            FDT_MAGIC,
            size(total),
            size(structs),
            size(strings),
            size(rsvmap),
            17,
            16,
            0,
            size(self.strings.len()),
            size(self.structs.len()),
        ];
        let mut blob: Vec<u8> = header.iter().flat_map(|word| word.to_be_bytes()).collect();
        blob.extend_from_slice(&[0; 16]);
        blob.extend_from_slice(&self.structs);
        blob.extend_from_slice(&self.strings);
        blob
    }
}

/// The PL011's and the USART's addresses in the trees below.
const PL011_AT: u32 = 0x0900_0000;
const USART_AT: u32 = 0x4001_0000;

/// Trees for [`check_chosen`], each with the address of the
/// port it must choose.
fn console_trees() -> Vec<(Vec<u8>, u64)> {
    let mut trees = Vec::new();

    // `stdout-path` names the USART, and `console=pl011` overrides it.
    let mut forced = Tree::new();
    forced.begin("chosen");
    forced.text("stdout-path", "/serial@40010000");
    forced.text("bootargs", "earlycon console=pl011 quiet");
    forced.end();
    forced.port("serial@40010000", "st,stm32h7-uart", USART_AT, None);
    forced.port("pl011@9000000", "arm,pl011", PL011_AT, None);
    trees.push((forced.finish(), u64::from(PL011_AT)));

    // An override naming no port this kernel drives falls through to
    // `stdout-path`.
    let mut unknown = Tree::new();
    unknown.begin("chosen");
    unknown.text("stdout-path", "/pl011@9000000");
    unknown.text("bootargs", "console=ttyS0");
    unknown.end();
    unknown.port("serial@40010000", "st,stm32h7-uart", USART_AT, None);
    unknown.port("pl011@9000000", "arm,pl011", PL011_AT, None);
    trees.push((unknown.finish(), u64::from(PL011_AT)));

    // No `stdout-path`: the first port the tree has not turned off.
    let mut first = Tree::new();
    first.port(
        "serial@40010000",
        "st,stm32h7-uart",
        USART_AT,
        Some("disabled"),
    );
    first.port("pl011@9000000", "arm,pl011", PL011_AT, Some("okay"));
    trees.push((first.finish(), u64::from(PL011_AT)));

    // And one that is on, an STM32MP15's USART.
    let mut board = Tree::new();
    board.port("serial@40010000", "st,stm32h7-uart", USART_AT, None);
    trees.push((board.finish(), u64::from(USART_AT)));

    trees
}

/// An idle shared line and an idle private one are masked and let through,
/// each read back. Returns how many lines.
fn check_masking() -> Result<usize, &'static str> {
    let mut lines = 0;
    for private in [false, true] {
        let Some(line) = gicv2::idle_line_for_check(private) else {
            continue;
        };
        super::unmask_interrupt(line)?;
        let through = gicv2::enabled_for_check(line);
        super::mask_interrupt(line)?;
        if !through || gicv2::enabled_for_check(line) {
            return Err("a line the controller was told to mask or let through did not change");
        }
        lines += 1;
    }
    if lines == 0 {
        return Err("the controller has no idle line to mask");
    }
    Ok(lines)
}

/// What the interrupt controller must refuse. Returns how many requests.
fn check_refusals() -> Result<usize, &'static str> {
    // 1020 to 1023 are the controller's answers, not lines: 1023 is
    // "spurious".
    let special = gicv2::FIRST_SPECIAL_ID;
    if super::mask_interrupt(special).is_ok() || super::unmask_interrupt(special + 3).is_ok() {
        return Err("a special interrupt identifier was masked or let through as a line");
    }
    // Refused before anything is recorded, so the frame the machine has is
    // untouched.
    if gicv2::init_msi_frame(0, None).is_ok()
        || gicv2::init_msi_frame(0x0802_0000, Some((1024, 64))).is_ok()
    {
        return Err("a GICv2m frame at address zero or past the SPIs was taken");
    }
    // A device's MSI writes land in the frame's page, which an IOMMU domain
    // maps whole.
    if gicv2::msi_doorbell().is_some_and(|page| !page.is_multiple_of(0x1000)) {
        return Err("the GICv2m doorbell is not a page");
    }
    Ok(4)
}

/// The port [`super::console::chosen`] picks from each of `trees`, held to the address of
/// the one it must: for the boot check, whose trees have
/// what QEMU's `virt` does not -- an override in `/chosen/bootargs`, no
/// `stdout-path`, a port turned off. Returns how many trees.
///
/// # Errors
///
/// A tree that does not parse, or one whose console is another port.
///
/// Verifies: L.console.41
fn check_chosen(trees: &[(Vec<u8>, u64)]) -> Result<usize, &'static str> {
    for (blob, address) in trees {
        let tree =
            Fdt::parse(blob).map_err(|_| "a device tree built for the check did not parse")?;
        let chosen = super::console::chosen(&tree)
            .and_then(|(node, _)| node.reg().next())
            .map(|registers| registers.address);
        if chosen != Some(*address) {
            return Err("a device tree's console was not the port its description chooses");
        }
    }
    Ok(trees.len())
}

/// Cleaning and invalidating a range of the data cache to the point of
/// coherency leaves what the range holds: the maintenance a buffer gets
/// before a device that does not snoop the caches is given it -- the DK1's
/// display and GPU, which QEMU's `virt` does not have. Over an odd start and
/// length, so the first and last lines are partial.
fn check_cache_maintenance() -> Result<(), &'static str> {
    let buffer: Vec<u8> = (0..=255_u8).cycle().take(1000).collect();
    let start = buffer.as_ptr() as u64 + 3;
    super::flush_for_device(black_box(start), black_box(990));
    if buffer
        .iter()
        .zip((0..=255_u8).cycle())
        .any(|(held, was)| *held != was)
    {
        return Err("cleaning a buffer from the data cache changed what it held");
    }
    Ok(())
}

/// Processors that answered [`check_user_counter`]'s look, and those whose
/// `CNTKCTL` left user mode without the virtual counter or gave it more.
static COUNTER_LOOKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static COUNTER_SHUT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static COUNTER_OPEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// One processor's own `CNTKCTL`, read on that processor.
fn look_at_counter_access(_me: &'static crate::smp::PerCpu) {
    use core::sync::atomic::Ordering::Relaxed;
    let control = super::cpu::read_cntkctl();
    if control & super::cpu::CNTKCTL_PL0VCTEN == 0 {
        let _ = COUNTER_SHUT.fetch_add(1, Relaxed);
    }
    if control & super::cpu::CNTKCTL_CLOSED != 0 {
        let _ = COUNTER_OPEN.fetch_add(1, Relaxed);
    }
    let _ = COUNTER_LOOKED.fetch_add(1, Relaxed);
}

/// Every processor lets user mode read the virtual counter and nothing else
/// of the generic timer: each reads its own `CNTKCTL`, after every processor
/// is online, and finds `PL0VCTEN` set and `PL0PCTEN`, `PL0VTEN` and
/// `PL0PTEN` clear. And the counter a program reads advances. That a program
/// really reads it is the block driver's to show: it times its requests with
/// `CNTVCT` from user mode, and the seam check refuses a run in which it
/// could not.
///
/// # Errors
///
/// A processor that did not look, or whose register is wrong; a counter
/// that stands still.
///
/// Verifies: L.armv7a.4
pub(crate) fn check_user_counter() -> Result<(), &'static str> {
    use core::sync::atomic::Ordering::Relaxed;
    for count in [&COUNTER_LOOKED, &COUNTER_SHUT, &COUNTER_OPEN] {
        count.store(0, Relaxed);
    }
    crate::smp::run_everywhere(look_at_counter_access)?;
    let processors = crate::smp::count();
    let looked = COUNTER_LOOKED.load(Relaxed);
    if looked != processors {
        return Err("a processor did not report what user mode may read of its timer");
    }
    if COUNTER_SHUT.load(Relaxed) != 0 {
        return Err("a processor does not let user mode read the virtual counter");
    }
    if COUNTER_OPEN.load(Relaxed) != 0 {
        return Err("a processor lets user mode reach the physical counter or a timer");
    }
    let before = super::cpu::read_cntvct();
    if !(0..1_000_000).any(|_| super::cpu::read_cntvct() != before) {
        return Err("the virtual counter does not advance");
    }
    println!(
        "  usercnt  user mode reads the virtual counter on {looked} of {processors} processors, \
         the physical counter and both timers closed to it"
    );
    Ok(())
}
