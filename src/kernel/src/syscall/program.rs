//! A program's file, as `execve` loads it: read for its headers, mapped for
//! its pages.
//!
//! Until 2026-09-24 `execve` read the whole file into kernel memory and the
//! loader copied each segment out of that copy, which held a program to the
//! 64 MiB `crate::fs::read_file` will read and paid for every page of it
//! whether the program touched it or not. Chrome is 198 MB. What the kernel
//! needs from the file before it can decide anything is only the first bytes
//! -- the `#!` line of a script, or an ELF's file header and program header
//! table -- and what it needs afterwards is the file's pages, which the page
//! cache already has as an object a mapping can name. So that is what this
//! holds: the open file, its length, those first bytes, and the object.
//!
//! # Which files are mapped, and which copied
//!
//! A file whose pages are one VMO -- tmpfs, the initramfs unpacked into it,
//! and btrfs through its page cache -- is mapped (see `load.rs`). A file with
//! no such object is still loaded, a segment at a time through a small buffer,
//! so it too can be any size; it is only not paged in on demand.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Elf;
use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::OpenFile;

use crate::user::vmo::Vmo;

/// The first bytes read of every program: a page, which holds a script's
/// `#!` line and the headers of every ELF file a linker writes.
const HEAD_BYTES: u64 = PAGE_SIZE;

/// The most read to reach the end of an ELF's program header table, if it
/// does not end in the first page: 64 KiB, Linux's own limit on the table's
/// size. A file claiming more is refused with `ENOEXEC` rather than read.
const HEADERS_MOST: u64 = 64 * 1024;

/// An open program file.
#[derive(Debug)]
pub struct ProgramFile {
    /// The file, open for reading. Kept by every mapping made of it, as
    /// Linux's `vm_file` keeps it, and what `/proc/<pid>/maps` names them by.
    file: Arc<OpenFile>,
    /// Its length when it was opened.
    len: u64,
    /// Its first bytes: at least a page of it, or all of a shorter file, and
    /// the whole program header table of an ELF file.
    head: Vec<u8>,
    /// The object a mapping of the file maps, and the byte of it the file's
    /// first byte is; `None` for a file with no object to map.
    object: Option<(Arc<Vmo>, u64)>,
}

impl ProgramFile {
    /// Read the first bytes of `file`, which must be a regular file open for
    /// reading, and find its object.
    ///
    /// # Errors
    ///
    /// `ENOEXEC` for an ELF file whose program header table ends past
    /// [`HEADERS_MOST`]; `ENOMEM`; and whatever reading refuses.
    pub fn open(file: Arc<OpenFile>) -> Result<ProgramFile, Errno> {
        let len = file.io().metadata().size;
        let mut head = zeroed(len.min(HEAD_BYTES))?;
        let got = read_at(&file, 0, &mut head)?;
        head.truncate(got);

        // An ELF file whose table runs past the first page: read on to its
        // end. Anything that is not an ELF file, or is one too short to say,
        // is left to whoever reads the head to refuse.
        if let Ok(end) = Elf::headers_len(&head)
            && end > head.len() as u64
            && end <= len
        {
            if end > HEADERS_MOST {
                return Err(Errno::ENOEXEC);
            }
            let mut rest = zeroed(end - head.len() as u64)?;
            let got = read_at(&file, head.len() as u64, &mut rest)?;
            rest.truncate(got);
            crate::fallible::try_extend_from_slice(&mut head, &rest).map_err(|_| Errno::ENOMEM)?;
        }

        let object = file
            .io()
            .mapping_at(0)
            .or_else(|| file.inode().mapping_at(0))
            .and_then(|(object, offset)| Some((object.downcast::<Vmo>().ok()?, offset)))
            .filter(|(_, offset)| offset.is_multiple_of(PAGE_SIZE));
        Ok(ProgramFile {
            file,
            len,
            head,
            object,
        })
    }

    /// The file's first bytes: at least a page, or the whole of a shorter
    /// file, and all of an ELF file's program headers.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// The file's length when it was opened.
    #[allow(
        clippy::len_without_is_empty,
        reason = "callers ask how many, never whether none"
    )]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// The open file.
    pub fn file(&self) -> &Arc<OpenFile> {
        &self.file
    }

    /// The object a mapping of the file maps, and the byte of it that is the
    /// file's first; `None` if the file has none, and must be copied.
    pub fn object(&self) -> Option<(&Arc<Vmo>, u64)> {
        self.object.as_ref().map(|(vmo, offset)| (vmo, *offset))
    }

    /// Fill `buf` from byte `offset` of the file.
    ///
    /// # Errors
    ///
    /// `EIO` for a file that ends before `buf` is full -- it was cut after the
    /// headers said where its segments are -- and whatever reading refuses.
    pub fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Errno> {
        if read_at(&self.file, offset, buf)? == buf.len() {
            Ok(())
        } else {
            Err(Errno::EIO)
        }
    }
}

/// Read into `buf` from byte `offset` of `file` until it is full or the file
/// ends, and say how much was read.
fn read_at(file: &OpenFile, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    let mut done = 0;
    while let Some(slot) = buf.get_mut(done..).filter(|slot| !slot.is_empty()) {
        let at = offset.checked_add(done as u64).ok_or(Errno::EFBIG)?;
        let count = file.read_at(at, slot)?;
        if count == 0 {
            break;
        }
        done += count;
    }
    Ok(done)
}

/// A zeroed buffer of `len` bytes, or `ENOMEM`.
fn zeroed(len: u64) -> Result<Vec<u8>, Errno> {
    let len = usize::try_from(len).map_err(|_| Errno::ENOMEM)?;
    crate::fallible::try_filled(0, len).map_err(|_| Errno::ENOMEM)
}

/// How much of a segment's contents is copied at a time, where it is copied
/// rather than mapped: 64 KiB, so a file of any size is copied through one
/// small buffer rather than read whole.
const COPY_CHUNK: u64 = 64 * 1024;

/// A buffer to copy a segment's contents through.
///
/// # Errors
///
/// `ENOMEM`.
pub fn copy_buffer() -> Result<Vec<u8>, Errno> {
    zeroed(COPY_CHUNK)
}
