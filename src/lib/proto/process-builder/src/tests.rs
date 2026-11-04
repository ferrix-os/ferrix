//! The builder against a kernel played by a recorder: every call it makes, in
//! order, for images built here byte by byte.

extern crate std;

use core::cell::RefCell;
use std::vec::Vec;

use ferrix_elf::{EHDR_SIZE, PF_R, PF_W, PF_X, PHDR_SIZE, PT_INTERP, PT_LOAD};
use ferrix_native::handle::OwnedHandle;
use ferrix_native::job::Job;
use ferrix_native::vmo::Vmo;
use ferrix_native::{Handle, Raw, Syscall};
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::IMAGE_RIGHTS;
use ferrix_native_abi::types::{MAP_EXECUTE, MAP_NO_PAGER, MAP_NONE, MAP_READ, MAP_WRITE};

use super::*;

/// The machine of the images built here.
const NATIVE_ELF_MACHINE: u16 = ferrix_elf::EM_X86_64;

const SPACE: usize = 0x77;
const PROCESS: usize = 0x55;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Made {
    number: usize,
    args: [usize; 6],
    written: Vec<u8>,
}

/// The kernel: answers `vmo_size` and `vmo_read` from the image, hands out
/// handles, and fails the call numbered by `fail`.
#[derive(Debug, Default)]
struct Kernel {
    image: RefCell<Vec<u8>>,
    calls: RefCell<Vec<Made>>,
    fail: RefCell<Option<(usize, isize)>>,
    short_write: RefCell<bool>,
}

impl Syscall for &Kernel {
    fn call(self, mut raw: Raw<'_>) -> usize {
        let number = raw.number();
        let args = raw.args();
        let mut written = Vec::new();
        if number == nr::SPACE_WRITE {
            written = raw.memory(args[2], args[3]).unwrap().to_vec();
        }
        self.calls.borrow_mut().push(Made {
            number,
            args,
            written,
        });
        if let Some((failing, errno)) = *self.fail.borrow()
            && failing == number
        {
            return errno.cast_unsigned();
        }
        let image = self.image.borrow();
        match number {
            nr::VMO_GET_SIZE => {
                let bytes = (image.len() as u64).to_ne_bytes();
                raw.memory_mut(args[1], 8).unwrap().copy_from_slice(&bytes);
                0
            }
            nr::VMO_READ => {
                let (at, len) = (args[1], args[2]);
                let offset = raw.memory(args[3], 8).unwrap();
                let from = u64::from_ne_bytes(offset.try_into().unwrap()) as usize;
                let end = from + len;
                let out = raw.memory_mut(at, len).unwrap();
                out.copy_from_slice(&image[from..end]);
                0
            }
            nr::PROCESS_MAKE => PROCESS,
            nr::PROCESS_SPACE => SPACE,
            nr::SPACE_ALLOCATE => args[1],
            nr::SPACE_MAP => args[2],
            nr::SPACE_WRITE => {
                if *self.short_write.borrow() {
                    0
                } else {
                    args[3]
                }
            }
            nr::VMO_EXECUTABLE => 0x31,
            nr::HANDLE_REPLACE => 0x32,
            _ => 0,
        }
    }
}

/// One program header: type, flags, file offset, address, file and memory size.
type Header = (u32, u32, u64, u64, u64, u64);

/// A little-endian `ET_EXEC` or `ET_DYN` of this machine and word width.
fn elf(kind: u16, entry: u64, headers: &[Header], size: usize) -> Vec<u8> {
    let mut file = std::vec![0_u8; size.max(EHDR_SIZE + PHDR_SIZE * headers.len())];
    file[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
    file[4] = 2;
    file[5] = 1;
    file[6] = 1;
    file[16..18].copy_from_slice(&kind.to_le_bytes());
    file[18..20].copy_from_slice(&NATIVE_ELF_MACHINE.to_le_bytes());
    file[20..24].copy_from_slice(&1_u32.to_le_bytes());
    file[24..32].copy_from_slice(&entry.to_le_bytes());
    file[32..40].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
    file[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    file[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    file[56..58].copy_from_slice(&(headers.len() as u16).to_le_bytes());
    for (index, h) in headers.iter().enumerate() {
        let at = EHDR_SIZE + PHDR_SIZE * index;
        file[at..at + 4].copy_from_slice(&h.0.to_le_bytes());
        file[at + 4..at + 8].copy_from_slice(&h.1.to_le_bytes());
        file[at + 8..at + 16].copy_from_slice(&h.2.to_le_bytes());
        file[at + 16..at + 24].copy_from_slice(&h.3.to_le_bytes());
        file[at + 24..at + 32].copy_from_slice(&h.3.to_le_bytes());
        file[at + 32..at + 40].copy_from_slice(&h.4.to_le_bytes());
        file[at + 40..at + 48].copy_from_slice(&h.5.to_le_bytes());
        file[at + 48..at + 56].copy_from_slice(&0x1000_u64.to_le_bytes());
    }
    file
}

/// Read-only, then text, then data with `.bss`, as lld lays them out.
fn program() -> Vec<u8> {
    let mut file = elf(
        2,
        0x20_1000,
        &[
            (PT_LOAD, PF_R, 0, 0x20_0000, 0x400, 0x400),
            (PT_LOAD, PF_R | PF_X, 0x1000, 0x20_1000, 0x1800, 0x1800),
            (PT_LOAD, PF_R | PF_W, 0x2800, 0x20_3800, 0x100, 0x2000),
        ],
        0x3000,
    );
    file[0x2800..0x2900].fill(0xDA);
    file
}

fn job(kernel: &Kernel) -> Job<&Kernel> {
    Job::from_owned(OwnedHandle::from_raw(kernel, Handle(0x11)))
}

fn image_of(kernel: &Kernel, bytes: Vec<u8>) -> Vmo<&Kernel> {
    *kernel.image.borrow_mut() = bytes;
    Vmo::from_owned(OwnedHandle::from_raw(kernel, Handle(0x22)))
}

fn of(kernel: &Kernel, number: usize) -> Vec<Made> {
    kernel
        .calls
        .borrow()
        .iter()
        .filter(|made| made.number == number)
        .cloned()
        .collect()
}

const RX: usize = (MAP_READ | MAP_EXECUTE) as usize;
const RW: usize = (MAP_READ | MAP_WRITE) as usize;
const R: usize = MAP_READ as usize;
const NO_PAGER: usize = MAP_NO_PAGER as usize;

#[test]
fn text_and_read_only_data_are_mapped_and_data_is_copied() {
    let kernel = Kernel::default();
    let image = image_of(&kernel, program());
    let built = build(&job(&kernel), &image, "program", NATIVE_ELF_MACHINE).expect("it builds");
    assert_eq!(built.entry(), 0x20_1000);
    assert_eq!(built.stack(), STACK_TOP);

    let maps = of(&kernel, nr::SPACE_MAP);
    let shown: Vec<(usize, usize, usize)> = maps
        .iter()
        .map(|made| (made.args[2], made.args[3], made.args[4]))
        .collect();
    assert_eq!(
        shown,
        [
            (0x20_0000, 0x1000, R | NO_PAGER),
            (0x20_1000, 0x2000, RX | NO_PAGER),
        ],
    );

    // The data page holds file bytes and `.bss`: private, and only its file
    // bytes written.
    let writes = of(&kernel, nr::SPACE_WRITE);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].args[1], 0x20_3800);
    assert_eq!(writes[0].written, std::vec![0xDA; 0x100]);
}

#[test]
fn no_executable_private_memory_is_ever_asked_for() {
    let kernel = Kernel::default();
    let image = image_of(&kernel, program());
    let _built = build(&job(&kernel), &image, "program", NATIVE_ELF_MACHINE).expect("it builds");
    for made in of(&kernel, nr::SPACE_ALLOCATE)
        .iter()
        .chain(of(&kernel, nr::SPACE_PROTECT).iter())
    {
        assert_eq!(made.args[3] & MAP_EXECUTE as usize, 0, "{made:?}");
    }
}

#[test]
fn the_stack_and_its_guards_are_where_the_constants_say() {
    let kernel = Kernel::default();
    let image = image_of(&kernel, program());
    let _built = build(&job(&kernel), &image, "program", NATIVE_ELF_MACHINE).expect("it builds");
    let allocated: Vec<(usize, usize, usize)> = of(&kernel, nr::SPACE_ALLOCATE)
        .iter()
        .map(|made| (made.args[1], made.args[2], made.args[3]))
        .collect();
    let top = STACK_TOP as usize;
    let size = NATIVE_STACK_SIZE as usize;
    let guard = NATIVE_STACK_GUARD as usize;
    assert!(allocated.contains(&(top - size, size, RW)));
    assert!(allocated.contains(&(top, USER_VIRT_END as usize - top, MAP_NONE as usize)));
    assert!(allocated.contains(&(top - size - guard, guard, MAP_NONE as usize)));
}

#[test]
fn the_space_handle_asks_no_read_and_is_closed_before_the_run() {
    let kernel = Kernel::default();
    let image = image_of(&kernel, program());
    let built = build(&job(&kernel), &image, "program", NATIVE_ELF_MACHINE).expect("it builds");
    let asked = of(&kernel, nr::PROCESS_SPACE);
    assert_eq!(asked.len(), 1);
    let closed = of(&kernel, nr::HANDLE_CLOSE);
    assert!(
        closed.iter().any(|made| made.args[0] == SPACE),
        "{closed:?}"
    );
    let far = OwnedHandle::from_raw(&kernel, Handle(0x99));
    let _process = built.run(far).expect("it runs");
    let run = of(&kernel, nr::PROCESS_RUN);
    assert_eq!(
        run[0].args[..4],
        [PROCESS, 0x20_1000, STACK_TOP as usize, 0x99]
    );
    let calls = kernel.calls.borrow();
    let close = calls
        .iter()
        .position(|made| made.number == nr::HANDLE_CLOSE && made.args[0] == SPACE);
    let start = calls.iter().position(|made| made.number == nr::PROCESS_RUN);
    assert!(close < start);
}

#[test]
fn a_new_image_is_narrowed_to_the_image_rights() {
    let kernel = Kernel::default();
    let vmo = Vmo::from_owned(OwnedHandle::from_raw(&kernel, Handle(0x22)));
    let _image = image(vmo).expect("it narrows");
    assert_eq!(of(&kernel, nr::VMO_EXECUTABLE)[0].args[0], 0x22);
    let replaced = of(&kernel, nr::HANDLE_REPLACE);
    assert_eq!(replaced[0].args[0], 0x31);
    assert_eq!(replaced[0].args[1], IMAGE_RIGHTS.0 as usize);
    assert!(!IMAGE_RIGHTS.contains(Rights::WRITE));
    assert!(!IMAGE_RIGHTS.contains(Rights::DUPLICATE));
}

#[test]
fn a_position_independent_image_goes_at_the_base() {
    let kernel = Kernel::default();
    let bytes = elf(
        3,
        0x1000,
        &[(PT_LOAD, PF_R | PF_X, 0x1000, 0x1000, 0x1000, 0x1000)],
        0x2000,
    );
    let image = image_of(&kernel, bytes);
    let built = build(&job(&kernel), &image, "pie", NATIVE_ELF_MACHINE).expect("it builds");
    assert_eq!(built.entry(), PIE_BASE + 0x1000 - 0x1000);
}

/// `image` is refused with `want`, with no process made.
fn refused_before_making(bytes: Vec<u8>, want: Refused) {
    let kernel = Kernel::default();
    let image = image_of(&kernel, bytes);
    assert_eq!(
        build(&job(&kernel), &image, "bad", NATIVE_ELF_MACHINE).err(),
        Some(want),
    );
    assert!(of(&kernel, nr::PROCESS_MAKE).is_empty());
}

#[test]
fn a_page_writable_and_executable_refuses_the_image() {
    refused_before_making(
        elf(
            2,
            0x20_0000,
            &[
                (PT_LOAD, PF_R | PF_X, 0x1000, 0x20_0000, 0x800, 0x800),
                (PT_LOAD, PF_R | PF_W, 0x1800, 0x20_0800, 0x100, 0x100),
            ],
            0x2000,
        ),
        Refused::WriteExecute(0x20_0000),
    );
    refused_before_making(
        elf(
            2,
            0x20_0000,
            &[(PT_LOAD, PF_R | PF_W | PF_X, 0x1000, 0x20_0000, 0x800, 0x800)],
            0x2000,
        ),
        Refused::WriteExecute(0x20_0000),
    );
}

#[test]
fn an_executable_page_that_cannot_be_mapped_refuses_the_image() {
    // File offset and address disagree within a page.
    refused_before_making(
        elf(
            2,
            0x20_0100,
            &[(PT_LOAD, PF_R | PF_X, 0x1000, 0x20_0100, 0x800, 0x800)],
            0x2000,
        ),
        Refused::Unmappable(0x20_0000),
    );
    // Two executable segments on one page.
    refused_before_making(
        elf(
            2,
            0x20_0000,
            &[
                (PT_LOAD, PF_R | PF_X, 0x1000, 0x20_0000, 0x100, 0x100),
                (PT_LOAD, PF_R, 0x1100, 0x20_0100, 0x100, 0x100),
            ],
            0x2000,
        ),
        Refused::Unmappable(0x20_0000),
    );
    // Executable memory past the file: a page nothing can supply.
    refused_before_making(
        elf(
            2,
            0x20_0000,
            &[(PT_LOAD, PF_R | PF_X, 0x1000, 0x20_0000, 0x800, 0x2000)],
            0x2000,
        ),
        Refused::Unmappable(0x20_0000),
    );
}

#[test]
fn what_cannot_run_here_is_refused() {
    refused_before_making(
        elf(
            2,
            USER_VIRT_END,
            &[(PT_LOAD, PF_R, 0, 0x20_0000, 0x10, 0x10)],
            0x1000,
        ),
        Refused::NotUser(USER_VIRT_END),
    );
    refused_before_making(
        elf(
            2,
            0x1000,
            &[(PT_LOAD, PF_R, 0, USER_VIRT_END, 0x10, 0x10)],
            0x1000,
        ),
        Refused::NotUser(USER_VIRT_END),
    );
    refused_before_making(
        elf(2, 0x1000, &[(PT_LOAD, 0, 0, 0x20_0000, 0x10, 0x10)], 0x1000),
        Refused::NoAccess(0x20_0000),
    );
    refused_before_making(
        elf(
            2,
            0x20_0000,
            &[
                (PT_LOAD, PF_R, 0, 0x20_0000, 0x10, 0x10),
                (PT_INTERP, PF_R, 0x100, 0, 4, 4),
            ],
            0x1000,
        ),
        Refused::NeedsInterpreter,
    );
    let mut other = program();
    other[18] = 0xFF;
    other[19] = 0x7F;
    refused_before_making(other, Refused::WrongMachine(0x7FFF));
    refused_before_making(std::vec![0; 3], Refused::Malformed(ElfError::TooShort));
    let many: Vec<Header> = (0..=MAX_SEGMENTS as u64)
        .map(|i| (PT_LOAD, PF_R, 0, 0x20_0000 + i * 0x1000, 0x10, 0x10))
        .collect();
    refused_before_making(elf(2, 0x20_0000, &many, 0x1000), Refused::Segments);
}

#[test]
fn an_image_past_the_bound_is_refused_unread() {
    let kernel = Kernel::default();
    let image = image_of(&kernel, std::vec![0; MAX_IMAGE_BYTES as usize + 1]);
    assert_eq!(
        build(&job(&kernel), &image, "big", NATIVE_ELF_MACHINE).err(),
        Some(Refused::TooBig),
    );
    assert!(of(&kernel, nr::VMO_READ).is_empty());
}

#[test]
fn a_call_refused_midway_closes_the_process_and_says_so() {
    let kernel = Kernel::default();
    *kernel.fail.borrow_mut() = Some((nr::SPACE_ALLOCATE, -12));
    let image = image_of(&kernel, program());
    assert_eq!(
        build(&job(&kernel), &image, "limit", NATIVE_ELF_MACHINE).err(),
        Some(Refused::Call(Error::NoMemory)),
    );
    let closed = of(&kernel, nr::HANDLE_CLOSE);
    assert!(
        closed.iter().any(|made| made.args[0] == PROCESS),
        "{closed:?}"
    );
}

#[test]
fn a_short_write_is_a_refusal() {
    let kernel = Kernel::default();
    *kernel.short_write.borrow_mut() = true;
    let image = image_of(&kernel, program());
    assert_eq!(
        build(&job(&kernel), &image, "short", NATIVE_ELF_MACHINE).err(),
        Some(Refused::Call(Error::NoMemory)),
    );
}

#[test]
fn a_long_name_is_refused() {
    let kernel = Kernel::default();
    let image = image_of(&kernel, program());
    let name = std::string::String::from("n").repeat(PROCESS_NAME_MAX + 1);
    assert_eq!(
        build(&job(&kernel), &image, &name, NATIVE_ELF_MACHINE).err(),
        Some(Refused::Name)
    );
}
