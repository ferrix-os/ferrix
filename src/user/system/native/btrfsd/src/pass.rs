//! A file's pages in btrfsd: none. `linuxd` keeps the page cache of a served file
//! (`docs/microkernel/BTRFSD.md` §3), so here a read goes to the file's source and
//! a page is not kept twice. The metadata cache stays where the crates have it.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::tmpfs::{PAGE_SIZE, PageSource, Pages, Storage};

/// The most pages asked of a source at once, as a heap store does.
const RUN_PAGES: usize = 32;
const PAGE: usize = PAGE_SIZE as usize;

/// A store for a mount that writes nothing.
#[derive(Debug)]
pub(crate) struct PassStorage;

impl Storage for PassStorage {
    fn allocate(&self) -> Result<Box<dyn Pages>, Errno> {
        Err(Errno::EROFS)
    }

    fn allocate_with(&self, source: Arc<dyn PageSource>) -> Result<Box<dyn Pages>, Errno> {
        Ok(Box::new(PassPages { source }))
    }

    fn max_file_size(&self) -> u64 {
        u64::MAX >> 1
    }
}

#[derive(Debug)]
struct PassPages {
    source: Arc<dyn PageSource>,
}

impl Pages for PassPages {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let mut done = 0_usize;
        while done < buf.len() {
            let at = offset.checked_add(done as u64).ok_or(Errno::EIO)?;
            let first = at / PAGE_SIZE;
            let within = (at % PAGE_SIZE) as usize;
            let wanted = (within + buf.len() - done).div_ceil(PAGE).min(RUN_PAGES);
            let mut run = vec![0_u8; wanted * PAGE];
            let mut pages: Vec<&mut [u8]> = run.chunks_mut(PAGE).collect();
            let filled = self.source.fill_range(first, &mut pages)?;
            drop(pages);
            if filled == 0 {
                return Err(Errno::EIO);
            }
            let have = (filled * PAGE).saturating_sub(within);
            let take = have.min(buf.len() - done);
            let from = run.get(within..within + take).ok_or(Errno::EIO)?;
            buf.get_mut(done..done + take)
                .ok_or(Errno::EIO)?
                .copy_from_slice(from);
            done += take;
        }
        Ok(())
    }

    fn write(&self, _offset: u64, _data: &[u8]) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn discard_from(&self, _offset: u64) {}

    fn committed_bytes(&self) -> u64 {
        0
    }
}
