//! A client's shared memory, mapped.
//!
//! `src/user/system/linux/compositor/server` holds a pool's descriptor and the rectangle a buffer
//! cuts out of it, and nothing else: it never touches the memory. This maps
//! it, so the renderer can read the pixels a client drew.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use compositor_wire::Fd;

/// A client's pool, mapped read-only.
///
/// Read-only because the compositor never writes into a client's buffer, and
/// because a mapping a client can grow under the compositor is one where a
/// write could land outside what was agreed.
#[derive(Debug)]
pub struct Mapping {
    /// The descriptor, kept so the mapping can be remade when the pool grows.
    fd: OwnedFd,
    address: *mut core::ffi::c_void,
    len: usize,
}

// SAFETY: the pointer is a private mapping this process owns for the life of
// the value, and nothing hands out a reference that outlives it.
unsafe impl Send for Mapping {}

impl Mapping {
    /// Map `size` bytes of `fd`.
    ///
    /// # Errors
    ///
    /// Whatever `mmap` said. A client whose pool cannot be mapped has sent a
    /// descriptor that is not memory, which is `wl_shm`'s `invalid_fd`.
    pub fn new(fd: Fd, size: i32) -> io::Result<Self> {
        let len = usize::try_from(size)
            .ok()
            .filter(|len| *len > 0)
            .ok_or_else(|| io::Error::other("a pool of no bytes"))?;
        let owned = own(fd.0);
        let address = map(owned.as_raw_fd(), len)?;
        Ok(Self {
            fd: owned,
            address,
            len,
        })
    }

    /// The bytes the pool holds.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        #[expect(
            unsafe_code,
            reason = "AUDIT: the mapping is this value's own and lives exactly as long as it, so the slice cannot outlive it"
        )]
        // SAFETY: `address` is a mapping of `len` bytes made by `Self::new`
        // and unmapped only by `Drop`, so it is live for this borrow. The
        // bytes are a client's to change at any moment, which is why they are
        // read as `u8` and never as a structure.
        unsafe {
            core::slice::from_raw_parts(self.address.cast::<u8>(), self.len)
        }
    }

    /// Map the same pool again, writable, for one screenshot.
    ///
    /// # Errors
    ///
    /// Whatever `mmap` said. A pool a client made read-only cannot be
    /// written into, and a screenshot into it fails rather than the
    /// compositor doing so.
    pub fn writable(&self) -> io::Result<Writable> {
        let address = map_with(
            self.fd.as_raw_fd(),
            self.len,
            libc::PROT_READ | libc::PROT_WRITE,
        )?;
        Ok(Writable {
            address,
            len: self.len,
        })
    }

    /// Map the pool again at `size`, which `wl_shm_pool.resize` may only make
    /// larger.
    ///
    /// # Errors
    ///
    /// Whatever `mmap` said.
    pub fn resize(&mut self, size: i32) -> io::Result<()> {
        let len = usize::try_from(size)
            .ok()
            .filter(|len| *len > self.len)
            .ok_or_else(|| io::Error::other("a pool may only grow"))?;
        let address = map(self.fd.as_raw_fd(), len)?;
        unmap(self.address, self.len);
        self.address = address;
        self.len = len;
        Ok(())
    }
}

/// A second mapping of one pool, writable, for as long as it is held.
///
/// The compositor never writes into a window's buffer -- that is what
/// [`Mapping`] being read-only says -- but a screenshot is the other way
/// round: `zwlr_screencopy_frame_v1.copy` hands over a buffer *for* the
/// compositor to fill. This is that one case, and it is a mapping of its own
/// so the read-only rule still holds everywhere else and the writable window
/// exists only while a screenshot is being taken.
#[derive(Debug)]
pub struct Writable {
    address: *mut core::ffi::c_void,
    len: usize,
}

// SAFETY: as `Mapping`'s: the pointer is this value's own mapping and no
// reference to it outlives the value.
unsafe impl Send for Writable {}

impl Writable {
    /// The bytes, to write into.
    #[must_use]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        #[expect(
            unsafe_code,
            reason = "AUDIT: the mapping is this value's own and lives exactly as long as it, so the slice cannot outlive it"
        )]
        // SAFETY: `address` is a mapping of `len` bytes made by
        // `Mapping::writable` and unmapped only by `Drop`, so it is live for
        // this borrow. The memory is shared with a client, which is why it
        // is written as `u8` and never as a structure.
        unsafe {
            core::slice::from_raw_parts_mut(self.address.cast::<u8>(), self.len)
        }
    }
}

impl Drop for Writable {
    fn drop(&mut self) {
        unmap(self.address, self.len);
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unmap(self.address, self.len);
    }
}

/// Take ownership of a descriptor the server handed over.
pub(crate) fn own(raw: i32) -> OwnedFd {
    #[expect(
        unsafe_code,
        reason = "AUDIT: the server hands a pool's descriptor on once, to whatever maps it; nothing else in this process holds the number"
    )]
    // SAFETY: `raw` came from a `SCM_RIGHTS` control message and was consumed
    // out of the connection, so this is its only owner.
    unsafe {
        OwnedFd::from_raw_fd(raw)
    }
}

/// `mmap` `len` bytes of `fd`, read-only and shared.
fn map(fd: i32, len: usize) -> io::Result<*mut core::ffi::c_void> {
    map_with(fd, len, libc::PROT_READ)
}

/// The same, with the protection said out loud.
fn map_with(fd: i32, len: usize, protection: i32) -> io::Result<*mut core::ffi::c_void> {
    #[expect(
        unsafe_code,
        reason = "AUDIT: mmap is not in std; the call maps a descriptor this process owns at a length it chose, and the result is checked against MAP_FAILED"
    )]
    // SAFETY: a null hint lets the kernel choose the address; `fd` is a
    // descriptor this process owns.
    let address = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            protection,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if address == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(address)
}

/// Give a mapping back.
fn unmap(address: *mut core::ffi::c_void, len: usize) {
    #[expect(
        unsafe_code,
        reason = "AUDIT: munmap of exactly the address and length this value mapped, once, from Drop"
    )]
    // SAFETY: `address` and `len` are what `map` returned for this value and
    // have not been unmapped before, since `Drop` runs once.
    unsafe {
        let _ = libc::munmap(address, len);
    }
}
