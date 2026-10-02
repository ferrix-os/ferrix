//! Serving fault windows: the driver's side of `docs/NVIDIA.md` §11.4 (K1).
//!
//! A client maps a device file whose driver serves its faults, and the kernel
//! makes a fault window for the mapping. The driver -- the window's server --
//! hears of each fault as a `PACKET_WINDOW_FAULT` on its port, puts pages of
//! its own VMOs into the window with [`WindowServer::insert`], takes them out
//! again with [`WindowServer::revoke`], and answers the fault with
//! [`WindowServer::answer`]. When no client maps a window any more the port
//! gets one `PACKET_WINDOW_UNMAPPED`.
//!
//! The server handle is given by the subsystem that serves the device file.
//! It carries `Rights::WINDOW_SERVER`: it can be neither duplicated nor sent,
//! and closing it is the server's death, which fails every fault waiting on
//! it and revokes every page it lent.

use ferrix_native_abi::nr;
use ferrix_native_abi::types::{WINDOW_ENTRY_WRITE, WindowEntry};

use crate::call::{Call, Syscall};
use crate::error::{Error, decode_unit};
use crate::handle::{Object, object_handle, register};
use crate::vmo::Vmo;

object_handle!(
    /// A fault window server's identity. Closing it is the server's death.
    WindowServer
);

/// One page [`WindowServer::insert`] puts in: page `index` of `vmo`, at page
/// `offset` of the window, writable by clients or not.
#[derive(Debug, Clone, Copy)]
pub struct Page<'a, S: Syscall> {
    /// The page offset in the window.
    pub offset: u64,
    /// The VMO the page is of.
    pub vmo: &'a Vmo<S>,
    /// The page index in the VMO.
    pub index: u64,
    /// Whether clients may write it.
    pub write: bool,
}

/// The bytes of one [`WindowEntry`], as the kernel reads it.
const ENTRY_BYTES: usize = size_of::<WindowEntry>();

impl<S: Syscall> WindowServer<S> {
    /// `window_insert`: put `pages`, at most `nr::WINDOW_INSERT_MAX`, into
    /// window `window`, all or none.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] past the limit; [`Error::InvalidArgs`] for no pages,
    /// a page offset outside the window, a page not committed or of a file;
    /// [`Error::AccessDenied`] for a VMO handle without `READ`, or without
    /// `WRITE` for a writable page; [`Error::BadState`] for a window that is
    /// dead or no client maps; [`Error::NoMemory`].
    pub fn insert(&self, window: u64, pages: &[Page<'_, S>]) -> Result<(), Error> {
        let mut bytes = [0_u8; ENTRY_BYTES * nr::WINDOW_INSERT_MAX as usize];
        if pages.len() > nr::WINDOW_INSERT_MAX as usize {
            return Err(Error::TooBig);
        }
        for (page, slot) in pages.iter().zip(bytes.chunks_exact_mut(ENTRY_BYTES)) {
            let flags = if page.write { WINDOW_ENTRY_WRITE } else { 0 };
            slot[0..8].copy_from_slice(&page.offset.to_le_bytes());
            slot[8..16].copy_from_slice(&page.index.to_le_bytes());
            slot[16..20].copy_from_slice(&page.vmo.handle().0.to_le_bytes());
            slot[20..24].copy_from_slice(&flags.to_le_bytes());
        }
        let used = bytes.get(..pages.len() * ENTRY_BYTES).unwrap_or(&[]);
        decode_unit(
            Call::new(nr::WINDOW_INSERT)
                .value(register(self.handle()))
                .value(window as usize)
                .input(used)
                .value(pages.len())
                .make(self.syscall()),
        )
    }

    /// `window_revoke`: take pages `first..first + pages` out of window
    /// `window` and out of every client, and only then let them go.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] for no pages, or a first page outside the
    /// window.
    pub fn revoke(&self, window: u64, first: u64, pages: u64) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::WINDOW_REVOKE)
                .value(register(self.handle()))
                .value(window as usize)
                .value(first as usize)
                .value(pages as usize)
                .make(self.syscall()),
        )
    }

    /// `window_answer`: answer the fault `token` of window `window` -- retry
    /// the access when `retry`, `SIGBUS` otherwise.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] for a window the server has no more.
    pub fn answer(&self, window: u64, token: u64, retry: bool) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::WINDOW_ANSWER)
                .value(register(self.handle()))
                .value(window as usize)
                .value(token as usize)
                .value(usize::from(!retry))
                .make(self.syscall()),
        )
    }
}
