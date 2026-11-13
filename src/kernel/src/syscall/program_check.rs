//! A program whose ELF program header table does not end in its first page:
//! `ProgramFile::open` reads on to the table's end, and refuses a table that
//! ends past 64 KiB, Linux's own limit, with `ENOEXEC` rather than reading it.
//!
//! Every program a linker writes keeps its table in the first page, so no
//! program the boot runs takes either path. Two files under `/tmp` do: the
//! same valid image with its table moved two pages in, and seventeen.

use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_errno::Errno;
use ferrix_vfs::NewNode;

use crate::arch;
use crate::fs;
use crate::syscall::image::{self, Shape};

/// The file whose table is read on to.
const FAR: &[u8] = b"/tmp/check-far-headers";
/// The file whose table ends past what is read.
const TOO_FAR: &[u8] = b"/tmp/check-too-far-headers";

/// Run it.
///
/// # Errors
///
/// The first property that did not hold, as a sentence.
pub(crate) fn run() -> Result<(), &'static str> {
    let outcome = check();
    let ns = fs::namespace();
    let ctx = ns.context();
    let _ = ns.unlink(&ctx, None, FAR);
    let _ = ns.unlink(&ctx, None, TOO_FAR);
    outcome
}

/// [`run`], before the files are removed.
fn check() -> Result<(), &'static str> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let good = image::build(class, arch::ARCH.elf_machine(), Shape::Good);
    let fields = Fields::of(class);
    let at = fields.word(&good, fields.phoff)?;
    let len = u64::from(fields.half(&good, fields.phentsize)?)
        * u64::from(fields.half(&good, fields.phnum)?);
    let table = good
        .get(at as usize..(at + len) as usize)
        .ok_or("the image's program header table is not inside it")?
        .to_vec();

    // Two pages in: read on to the table's end, and all of it is the head.
    let far = moved(&good, &table, &fields, 2 * PAGE_SIZE)?;
    write(FAR, &far)?;
    let ctx = fs::namespace().context();
    let (program, _, _) =
        fs::open_program(&ctx, None, FAR).map_err(|_| "a program with a far table did not open")?;
    let start = (2 * PAGE_SIZE) as usize;
    if program.head().get(start..start + table.len()) != Some(table.as_slice()) {
        return Err("a program's table past its first page was not read into its head");
    }

    // Seventeen pages in: past 64 KiB, so refused without being read.
    let too_far = moved(&good, &table, &fields, 17 * PAGE_SIZE)?;
    write(TOO_FAR, &too_far)?;
    match fs::open_program(&ctx, None, TOO_FAR) {
        Err(Errno::ENOEXEC) => Ok(()),
        _ => Err("a program whose table ends past 64 KiB was not refused as ENOEXEC"),
    }
}

/// Where the file header keeps the table's offset and shape, by class.
struct Fields {
    /// `e_phoff`, and its width in bytes.
    phoff: (usize, usize),
    /// `e_phentsize`.
    phentsize: usize,
    /// `e_phnum`.
    phnum: usize,
}

impl Fields {
    /// The ELF specification's offsets for `class`.
    const fn of(class: Class) -> Fields {
        match class {
            Class::Elf64 => Fields {
                phoff: (32, 8),
                phentsize: 54,
                phnum: 56,
            },
            Class::Elf32 => Fields {
                phoff: (28, 4),
                phentsize: 42,
                phnum: 44,
            },
        }
    }

    /// The word at `(at, width)`, little-endian as every Ferrix target is.
    fn word(&self, image: &[u8], (at, width): (usize, usize)) -> Result<u64, &'static str> {
        let bytes = image.get(at..at + width).ok_or("a short ELF header")?;
        Ok(bytes
            .iter()
            .rev()
            .fold(0_u64, |value, &byte| value << 8 | u64::from(byte)))
    }

    /// The half-word at `at`.
    fn half(&self, image: &[u8], at: usize) -> Result<u16, &'static str> {
        let bytes = image.get(at..at + 2).ok_or("a short ELF header")?;
        <[u8; 2]>::try_from(bytes)
            .map(u16::from_le_bytes)
            .map_err(|_| "a short ELF header")
    }
}

/// `image` with its program header table copied to `offset`, the file
/// extended to hold it and `e_phoff` saying so.
fn moved(
    image: &[u8],
    table: &[u8],
    fields: &Fields,
    offset: u64,
) -> Result<Vec<u8>, &'static str> {
    let offset = offset as usize;
    let mut file = image.to_vec();
    if file.len() < offset + table.len() {
        file.resize(offset + table.len(), 0);
    }
    file.get_mut(offset..offset + table.len())
        .ok_or("no room for a moved table")?
        .copy_from_slice(table);
    let (at, width) = fields.phoff;
    let bytes = (offset as u64).to_le_bytes();
    file.get_mut(at..at + width)
        .ok_or("a short ELF header")?
        .copy_from_slice(bytes.get(..width).ok_or("a short ELF header")?);
    Ok(file)
}

/// A new file at `path` holding `data`, executable.
fn write(path: &[u8], data: &[u8]) -> Result<(), &'static str> {
    let ns = fs::namespace();
    let ctx = ns.context();
    let _ = ns.unlink(&ctx, None, path);
    ns.mknod(&ctx, None, path, NewNode::Regular, 0o755)
        .map_err(|_| "a program's file could not be made under /tmp")?;
    let inode = ns
        .resolve(&ctx, None, path, true)
        .and_then(|at| at.inode())
        .map_err(|_| "a program's file just made is gone")?;
    let (written, _) = inode
        .write_at(0, data, false)
        .map_err(|_| "a program's file could not be written")?;
    if written == data.len() {
        Ok(())
    } else {
        Err("a program's file was written short")
    }
}
