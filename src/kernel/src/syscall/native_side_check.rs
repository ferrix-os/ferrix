//! A caller of native calls for a boot check, in the item: a [`Bare`] process, with no Linux
//! process under it, which a user kernel does not have (ledger 7184 B4.1). What
//! `object::check::Side` is to the load's checks, as far as `region_check` needs one.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_native_abi::handle::Handle;

use crate::object::bare::Bare;
use crate::object::job::Job;
use crate::object::process::Host;
use crate::trap::SyscallArgs;
use crate::syscall::native;
use crate::syscall::uaccess;
use ferrix_linux_abi::errno::Errno;
use crate::user::space::AddressSpace;

/// A process that makes native calls from the kernel's side.
pub(crate) struct NativeSide {
    host: Arc<Bare>,
}

impl NativeSide {
    /// A fresh process, in a job of its own, with nothing mapped.
    pub(crate) fn new() -> Result<NativeSide, &'static str> {
        let space = AddressSpace::new().map_err(|_| "could not make a space")?;
        let job = Job::new_root().map_err(|_| "could not make a job")?;
        let host = Bare::new(space, job, None).map_err(|_| "could not make a process")?;
        Ok(NativeSide { host })
    }

    /// Its address space.
    pub(crate) fn space(&self) -> &Arc<AddressSpace> {
        self.host.core().own_space()
    }

    /// Its handle table.
    pub(crate) fn with_handles<R>(
        &self,
        body: impl FnOnce(&mut crate::object::HandleTable) -> R,
    ) -> R {
        self.host.core().with_handles(body)
    }

    /// Make a native call with these registers.
    pub(crate) fn call(&self, number: usize, args: &[u64]) -> Result<usize, Errno> {
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
        native::dispatch(&args, Some(&*self.host))
    }

    /// A call that makes a handle.
    pub(crate) fn handle(
        &self,
        number: usize,
        args: &[u64],
        why: &'static str,
    ) -> Result<Handle, &'static str> {
        self.call(number, args)
            .map(|value| Handle(value as u32))
            .map_err(|_| why)
    }

    /// Put bytes in its memory.
    pub(crate) fn put(&self, at: u64, bytes: &[u8]) -> Result<(), &'static str> {
        uaccess::copy_to_user(self.space(), at, bytes).map_err(|_| "could not stage user memory")
    }

    /// Read bytes back out of it.
    pub(crate) fn get(&self, at: u64, len: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = vec![0_u8; len];
        uaccess::copy_from_user(self.space(), at, &mut out)
            .map_err(|_| "could not read user memory back")?;
        Ok(out)
    }

    /// A `u32` out of it.
    pub(crate) fn get_u32(&self, at: u64) -> Result<u32, &'static str> {
        let bytes = self.get(at, 4)?;
        <[u8; 4]>::try_from(bytes.as_slice())
            .map(u32::from_ne_bytes)
            .map_err(|_| "a short read")
    }
}
