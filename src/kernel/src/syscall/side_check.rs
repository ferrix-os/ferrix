//! The two processes the boot's object checks run (`object::check`), and the
//! scratch region each keeps its buffers in: what the load's checks borrow to
//! drive native calls with raw registers (block ring, character devices,
//! cgroupfs, the btrfs mapping check, the initial calls).
//!
//! Check code of the load ring: cut from the raised `object/check.rs` in S5
//! of the crate split (`docs/microkernel/CRATE-SPLIT.md` §2.6 item 5), bodies
//! unchanged and `pub` where they were `pub(crate)`, because the load cannot
//! name the raised checks' crate and the raised checks name the load.
//! `object::check` re-exports them for its own users.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::sync::SpinLock;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_vma::VmaFlags;

use crate::device::DeviceNode;
use crate::object::job::Job;
use crate::object::{self, Object};
use crate::syscall::process::{self, Process};
use crate::syscall::{SyscallArgs, native, uaccess};

/// Where each check process keeps its buffers. Stage 10's ring check borrows
/// the second page of it.
pub const SCRATCH: u64 = 0x4000_0000;
/// Two pages of it.
pub const SCRATCH_LEN: u64 = 2 * PAGE_SIZE;
/// Handle values, in either direction.
pub const HANDLES: u64 = SCRATCH + 0x200;
/// `channel_create`'s two handles.
pub const PAIR: u64 = SCRATCH + 0x300;
/// A VMO offset.
pub const OFFSET: u64 = SCRATCH + 0x310;

/// One of the two processes.
#[allow(
    missing_debug_implementations,
    reason = "a check's handle on a process, never printed; `pub` only across the raised checks' crate"
)]
pub struct Side {
    /// The process, over its own address space.
    pub process: Arc<Process>,
    /// Where the last child [`Side::built`] made enters.
    last_built: SpinLock<Option<native::EntryPoint>>,
}

impl Side {
    /// A fresh process with a scratch region mapped.
    pub fn new() -> Result<Side, &'static str> {
        let process = process::new_for_check().map_err(|_| "could not make a process")?;
        let _ = process
            .space()
            .map_anonymous(SCRATCH, SCRATCH_LEN, VmaFlags::READ_WRITE)
            .map_err(|_| "could not map a scratch region")?;
        Ok(Side {
            process,
            last_built: SpinLock::new(None),
        })
    }

    /// A side over a process that already exists.
    pub fn of(process: Arc<Process>) -> Side {
        Side {
            process,
            last_built: SpinLock::new(None),
        }
    }

    /// A fresh process, as [`Side::new`], placed in `job` as a new one is.
    pub fn new_in(job: &Arc<Job>) -> Result<Side, &'static str> {
        let side = Side::new()?;
        side.process
            .move_new_to(job)
            .map_err(|_| "could not place a process in its job")?;
        Ok(side)
    }

    /// Make a native call with these registers.
    pub fn call(&self, number: usize, args: &[u64]) -> Result<usize, Errno> {
        let mut registers = [0_u64; 6];
        for (slot, value) in registers.iter_mut().zip(args) {
            *slot = *value;
        }
        let args = SyscallArgs {
            abi: crate::trap::Abi::Native,
            number,
            args: registers,
            ip: 0,
        };
        native::dispatch(&args, Some(&*self.process))
    }

    /// A child made in `job` by `process_make`, with the name staged at `name_at`
    /// and the image in VMO `image` loaded, not started: what a starter does with
    /// the library, here by the kernel's own loader, since these checks are about
    /// what the child does and not how its pages got there
    /// (`docs/microkernel/K15.md`).
    pub fn built(
        &self,
        job: Handle,
        image: Handle,
        name_at: u64,
        name_len: u64,
        what: &'static str,
    ) -> Result<Handle, &'static str> {
        let child = self.handle(nr::PROCESS_MAKE, &[reg(job), name_at, name_len], what)?;
        let (vmo, host) = self
            .process
            .with_handles(|table| {
                let vmo = match table.get(image) {
                    Ok((Object::Vmo(vmo), _)) => Some(Arc::clone(vmo)),
                    _ => None,
                };
                let host = match table.get(child) {
                    Ok((Object::Process(made), _)) => made.control().and_then(|held| held.host()),
                    _ => None,
                };
                vmo.zip(host)
            })
            .ok_or(what)?;
        let bytes = native::image_bytes(&vmo).map_err(|_| what)?;
        let at =
            crate::discovery::first_image_check::load_for_check(host.core().own_space(), &bytes)
                .map_err(|_| what)?;
        *self.last_built.lock() = Some(at);
        Ok(child)
    }

    /// `process_run` on `child`, at the entry the last [`Side::built`] loaded.
    pub fn run(&self, child: Handle, bootstrap: u64) -> Result<usize, Errno> {
        let at = (*self.last_built.lock()).ok_or(status::BAD_STATE)?;
        self.call(
            nr::PROCESS_RUN,
            &[reg(child), at.entry, at.stack, bootstrap],
        )
    }

    /// Make a call that returns a handle.
    pub fn handle(
        &self,
        number: usize,
        args: &[u64],
        what: &'static str,
    ) -> Result<Handle, &'static str> {
        let value = self.call(number, args).map_err(|_| what)?;
        u32::try_from(value).map(Handle).map_err(|_| what)
    }

    /// Put bytes in this process's memory.
    pub fn put(&self, at: u64, bytes: &[u8]) -> Result<(), &'static str> {
        uaccess::copy_to_user(self.process.space(), at, bytes)
            .map_err(|_| "could not stage user memory")
    }

    /// Read bytes back out of it.
    pub fn get(&self, at: u64, len: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = vec![0_u8; len];
        uaccess::copy_from_user(self.process.space(), at, &mut out)
            .map_err(|_| "could not read user memory back")?;
        Ok(out)
    }

    /// A `u32` out of it.
    pub fn get_u32(&self, at: u64) -> Result<u32, &'static str> {
        let bytes = self.get(at, 4)?;
        <[u8; 4]>::try_from(bytes.as_slice())
            .map(u32::from_ne_bytes)
            .map_err(|_| "a short read")
    }

    /// Put handle values at [`HANDLES`].
    pub fn put_handles(&self, handles: &[Handle]) -> Result<(), &'static str> {
        let words: Vec<u8> = handles.iter().flat_map(|h| h.0.to_ne_bytes()).collect();
        self.put(HANDLES, &words)
    }

    /// Put a VMO offset at [`OFFSET`].
    pub fn put_offset(&self, offset: u64) -> Result<(), &'static str> {
        self.put(OFFSET, &offset.to_ne_bytes())
    }

    /// Make a channel inside this process, returning both ends.
    pub fn channel(&self) -> Result<(Handle, Handle), &'static str> {
        let _ = self
            .call(nr::CHANNEL_CREATE, &[PAIR])
            .map_err(|_| "channel_create failed")?;
        Ok((Handle(self.get_u32(PAIR)?), Handle(self.get_u32(PAIR + 4)?)))
    }

    /// Close whatever is left, as a process's exit will.
    pub fn close_everything(&self) {
        object::dispose(self.process.with_handles(object::HandleTable::clear));
    }
}

/// A handle as a register.
pub fn reg(handle: Handle) -> u64 {
    u64::from(handle.0)
}

/// Give `side` a handle to `node`, as `devmgr` will give one to a driver.
pub fn device_handle(side: &Side, node: &Arc<DeviceNode>) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Device(Arc::clone(node)), Rights::DEVICE))
        .map_err(|_| "no room for a device handle")
}
