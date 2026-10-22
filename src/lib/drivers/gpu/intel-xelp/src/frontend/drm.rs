//! The chardev core's requests, and `drm_ioctl`'s dispatch: the ioctl's
//! type and number pick an entry, its argument is copied in as far as both
//! the program's size and the entry's allow and zero-extended to the
//! entry's, the render node's permission rule is applied, the handler runs,
//! and the argument is copied back whatever the handler returned
//! (`drm_ioctl.c`, `drm_ioctl` and `drm_ioctl_permit`).
//!
//! Also here: the common ioctls that need no table of their own (`VERSION`,
//! `GET_CAP`, `SET_CLIENT_NAME`), and opening and releasing a file.

use ferrix_linux_abi::errno::Errno;

use super::fence::{self, Hold};
use super::objects::{Context, File, Tables};
use super::{Answer, Backend, Frontend, Host, gem, getparam, query, syncobj, vm};
use crate::uapi::{
    self, DRM_COMMAND_BASE, DRM_COMMAND_END, DRM_IOCTL_BASE, GetCap, IOC_READ, IOC_WRITE, Version,
    core_nr, drm as consts, ioc_dir, ioc_nr, ioc_size, ioc_type, nr,
};

/// The largest argument any entry takes (`drm_ioctl`'s `stack_kdata` is
/// 128 bytes too).
pub const KDATA: usize = 128;

/// One ioctl being answered.
#[derive(Clone, Copy, Debug)]
pub struct Call {
    /// The chardev request's id.
    pub request: u64,
    /// The file's slot.
    pub file: u8,
    /// The ioctl's argument: the program's address of the structure.
    pub arg: u64,
    /// The bytes copied back after the handler (`out_size`).
    pub out_size: usize,
    /// The command as the program gave it.
    pub cmd: u32,
}

/// An entry of the tables: the command as the driver defines it (its size
/// is `drv_size`), and whether a render node may call it.
#[derive(Clone, Copy, Debug)]
struct Entry {
    cmd: u32,
    render: bool,
}

/// The core table's entries a render node may not call
/// (`DRM_IOCTL_DEF` without `DRM_RENDER_ALLOW`, `drm_ioctl.c`): every
/// legacy, master, auth and mode-setting ioctl. Each answers `EACCES`.
const CORE_DENIED: &[u8] = &[
    0x01, 0x02, 0x05, 0x06, 0x07, 0x0a, 0x0b, 0x0d, 0x10, 0x11, 0x12, 0x13, 0x1e, 0x1f, 0x27, 0x28,
    0x2c, 0x3a, 0x3b, 0x3c, 0x3f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa,
    0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba,
    0xbb, 0xbc, 0xbd, 0xbe, 0xc6, 0xc7, 0xc8, 0xc9, 0xce, 0xd0,
];

/// `DRM_IOCTL_SET_CLIENT_NAME`'s number.
const SET_CLIENT_NAME: u8 = 0xd1;
/// `DRM_IOCTL_GEM_CHANGE_HANDLE`'s number.
const GEM_CHANGE_HANDLE: u8 = 0xd2;

/// The number of entries i915's table has (`i915_ioctls[]`, through
/// `DRM_I915_GEM_CREATE_EXT`).
const I915_IOCTLS: u8 = nr::GEM_CREATE_EXT + 1;

/// The i915 entries a render node may call (`DRM_RENDER_ALLOW` in
/// `i915_ioctls[]`, `i915_driver.c`). The rest of the table answers
/// `EACCES`.
const I915_RENDER: &[u8] = &[
    nr::GETPARAM,
    nr::GEM_EXECBUFFER2,
    nr::GEM_BUSY,
    nr::GEM_SET_CACHING,
    nr::GEM_GET_CACHING,
    nr::GEM_THROTTLE,
    nr::GEM_CREATE,
    nr::GEM_CREATE_EXT,
    nr::GEM_PREAD,
    nr::GEM_PWRITE,
    nr::GEM_MMAP,
    nr::GEM_MMAP_OFFSET,
    nr::GEM_SET_DOMAIN,
    nr::GEM_SW_FINISH,
    nr::GEM_SET_TILING,
    nr::GEM_GET_TILING,
    nr::GEM_GET_APERTURE,
    nr::GEM_MADVISE,
    nr::GEM_WAIT,
    nr::GEM_CONTEXT_CREATE,
    nr::GEM_CONTEXT_DESTROY,
    nr::REG_READ,
    nr::GET_RESET_STATS,
    nr::GEM_USERPTR,
    nr::GEM_CONTEXT_GETPARAM,
    nr::GEM_CONTEXT_SETPARAM,
    nr::PERF_OPEN,
    nr::PERF_ADD_CONFIG,
    nr::PERF_REMOVE_CONFIG,
    nr::QUERY,
    nr::GEM_VM_CREATE,
    nr::GEM_VM_DESTROY,
];

/// The entry for `cmd`, or the error `drm_ioctl` gives before any handler.
fn entry(cmd: u32) -> Result<Entry, Errno> {
    if ioc_type(cmd) != DRM_IOCTL_BASE {
        return Err(Errno::ENOTTY);
    }
    let number = ioc_nr(cmd);
    if (DRM_COMMAND_BASE..DRM_COMMAND_END).contains(&number) {
        let index = number - DRM_COMMAND_BASE;
        if index >= I915_IOCTLS {
            return Err(Errno::EINVAL);
        }
        let render = I915_RENDER.contains(&index);
        return Ok(Entry {
            cmd: i915_cmd(index).unwrap_or(0),
            render,
        });
    }
    let defined = core_cmd(number);
    match defined {
        Some(cmd) => Ok(Entry { cmd, render: true }),
        None if CORE_DENIED.contains(&number) => Ok(Entry {
            cmd: 0,
            render: false,
        }),
        None => Err(Errno::EINVAL),
    }
}

/// The core entries a render node may call, as Linux defines them.
fn core_cmd(number: u8) -> Option<u32> {
    use uapi::ioctl;
    Some(match number {
        core_nr::VERSION => ioctl::VERSION,
        core_nr::GET_CAP => ioctl::GET_CAP,
        core_nr::GEM_CLOSE => ioctl::GEM_CLOSE,
        core_nr::PRIME_HANDLE_TO_FD => ioctl::PRIME_HANDLE_TO_FD,
        core_nr::PRIME_FD_TO_HANDLE => ioctl::PRIME_FD_TO_HANDLE,
        core_nr::SYNCOBJ_CREATE => ioctl::SYNCOBJ_CREATE,
        core_nr::SYNCOBJ_DESTROY => ioctl::SYNCOBJ_DESTROY,
        core_nr::SYNCOBJ_HANDLE_TO_FD => ioctl::SYNCOBJ_HANDLE_TO_FD,
        core_nr::SYNCOBJ_FD_TO_HANDLE => ioctl::SYNCOBJ_FD_TO_HANDLE,
        core_nr::SYNCOBJ_TRANSFER => ioctl::SYNCOBJ_TRANSFER,
        core_nr::SYNCOBJ_WAIT => ioctl::SYNCOBJ_WAIT,
        core_nr::SYNCOBJ_TIMELINE_WAIT => ioctl::SYNCOBJ_TIMELINE_WAIT,
        core_nr::SYNCOBJ_EVENTFD => ioctl::SYNCOBJ_EVENTFD,
        core_nr::SYNCOBJ_RESET => ioctl::SYNCOBJ_RESET,
        core_nr::SYNCOBJ_SIGNAL => ioctl::SYNCOBJ_SIGNAL,
        core_nr::SYNCOBJ_TIMELINE_SIGNAL => ioctl::SYNCOBJ_TIMELINE_SIGNAL,
        core_nr::SYNCOBJ_QUERY => ioctl::SYNCOBJ_QUERY,
        // `struct drm_set_client_name` and `struct drm_gem_change_handle`
        // are 16 and 8 bytes; Linux 7.0's `drm.h`.
        SET_CLIENT_NAME => uapi::ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_BASE, SET_CLIENT_NAME, 16),
        GEM_CHANGE_HANDLE => uapi::ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_BASE, GEM_CHANGE_HANDLE, 8),
        _ => return None,
    })
}

/// The i915 entries, as `i915_ioctls[]` defines them, for the sizes.
fn i915_cmd(index: u8) -> Option<u32> {
    use uapi::ioctl;
    Some(match index {
        nr::GETPARAM => ioctl::GETPARAM,
        nr::GEM_EXECBUFFER2 => ioctl::GEM_EXECBUFFER2_WR,
        nr::GEM_BUSY => ioctl::GEM_BUSY,
        nr::GEM_SET_CACHING => ioctl::GEM_SET_CACHING,
        nr::GEM_GET_CACHING => ioctl::GEM_GET_CACHING,
        nr::GEM_THROTTLE => ioctl::GEM_THROTTLE,
        nr::GEM_CREATE => ioctl::GEM_CREATE,
        nr::GEM_CREATE_EXT => ioctl::GEM_CREATE_EXT,
        nr::GEM_PREAD => ioctl::GEM_PREAD,
        nr::GEM_PWRITE => ioctl::GEM_PWRITE,
        nr::GEM_MMAP => ioctl::GEM_MMAP,
        nr::GEM_MMAP_OFFSET => ioctl::GEM_MMAP_OFFSET,
        nr::GEM_SET_DOMAIN => ioctl::GEM_SET_DOMAIN,
        nr::GEM_SW_FINISH => ioctl::GEM_SW_FINISH,
        nr::GEM_SET_TILING => ioctl::GEM_SET_TILING,
        nr::GEM_GET_TILING => ioctl::GEM_GET_TILING,
        nr::GEM_GET_APERTURE => ioctl::GEM_GET_APERTURE,
        nr::GEM_MADVISE => ioctl::GEM_MADVISE,
        nr::GEM_WAIT => ioctl::GEM_WAIT,
        nr::GEM_CONTEXT_CREATE => ioctl::GEM_CONTEXT_CREATE_EXT,
        nr::GEM_CONTEXT_DESTROY => ioctl::GEM_CONTEXT_DESTROY,
        nr::REG_READ => ioctl::REG_READ,
        nr::GET_RESET_STATS => ioctl::GET_RESET_STATS,
        nr::GEM_USERPTR => ioctl::GEM_USERPTR,
        nr::GEM_CONTEXT_GETPARAM => ioctl::GEM_CONTEXT_GETPARAM,
        nr::GEM_CONTEXT_SETPARAM => ioctl::GEM_CONTEXT_SETPARAM,
        nr::PERF_OPEN => ioctl::PERF_OPEN,
        nr::PERF_ADD_CONFIG => uapi::drm_iow(nr::PERF_ADD_CONFIG, 48),
        nr::PERF_REMOVE_CONFIG => uapi::drm_iow(nr::PERF_REMOVE_CONFIG, 8),
        nr::QUERY => ioctl::QUERY,
        nr::GEM_VM_CREATE => ioctl::GEM_VM_CREATE,
        nr::GEM_VM_DESTROY => ioctl::GEM_VM_DESTROY,
        _ => return None,
    })
}

/// What a handler came to.
pub type Outcome = Result<Option<Hold>, Errno>;

impl<B: Backend> Frontend<B> {
    /// An open of the render node: a new file, with its default context and
    /// that context's VM (`drm_file_alloc`, `i915_gem_context_open`).
    pub fn open<H: Host<B::Memory>>(&mut self, host: &mut H, request: u64, file: u64, pid: u32) {
        let status = match self.open_file(file, pid) {
            Ok(()) => 0,
            Err(errno) => -i32::from(errno.0),
        };
        host.reply(request, status);
    }

    fn open_file(&mut self, id: u64, pid: u32) -> Result<(), Errno> {
        let state = &mut self.state;
        let slot = state
            .files
            .insert(File {
                id,
                pid,
                default_context: 0,
            })
            .ok_or(Errno::ENFILE)?;
        let file = u8::try_from(slot).map_err(|_| Errno::ENFILE)?;
        let made = vm::new_vm(state, file).and_then(|vm_slot| {
            vm::new_context(state, file, vm_slot, true).inspect_err(|_| {
                let _ = state.vms.remove(usize::from(vm_slot));
            })
        });
        match made {
            Ok(context) => {
                if let Some(entry) = state.files.get_mut(slot) {
                    entry.default_context = context;
                }
                Ok(())
            }
            Err(errno) => {
                let _ = state.files.remove(slot);
                Err(errno)
            }
        }
    }

    /// The last close of a file: everything it named goes, as
    /// `drm_file_free` and `i915_driver_postclose` let it go. Its requests
    /// still run; their contexts, VMs and objects are kept until they retire.
    pub fn release<H: Host<B::Memory>>(&mut self, host: &mut H, file: u64) {
        let Some(slot) = self.state.file_slot(file) else {
            return;
        };
        self.state.waits.drop_file(slot);
        syncobj::release_file(&mut self.state, slot);
        gem::release_file(&mut self.state, slot);
        vm::release_file(&mut self.state, slot);
        let _ = self.state.files.remove(usize::from(slot));
        fence::settle(self, host);
    }

    /// An ioctl on file `file`.
    pub fn ioctl<H: Host<B::Memory>>(
        &mut self,
        host: &mut H,
        request: u64,
        file: u64,
        cmd: u32,
        arg: u64,
    ) {
        let Some(slot) = self.state.file_slot(file) else {
            host.reply(request, -i32::from(Errno::EBADF.0));
            return;
        };
        let answer = self.dispatch(host, request, slot, cmd, arg);
        if let Answer::Now(status) = answer {
            host.reply(request, status);
        }
        fence::settle(self, host);
    }

    /// `drm_ioctl`, then the handler.
    fn dispatch<H: Host<B::Memory>>(
        &mut self,
        host: &mut H,
        request: u64,
        file: u8,
        cmd: u32,
        arg: u64,
    ) -> Answer {
        let entry = match entry(cmd) {
            Ok(entry) => entry,
            Err(errno) => return Answer::error(errno),
        };
        if !entry.render {
            return Answer::error(Errno::EACCES);
        }
        let drv_size = ioc_size(entry.cmd);
        let mut in_size = ioc_size(cmd);
        let mut out_size = in_size;
        if ioc_dir(cmd) & ioc_dir(entry.cmd) & IOC_WRITE == 0 {
            in_size = 0;
        }
        if ioc_dir(cmd) & ioc_dir(entry.cmd) & IOC_READ == 0 {
            out_size = 0;
        }
        let ksize = in_size.max(out_size).max(drv_size);
        if ksize > KDATA {
            return Answer::error(Errno::ENOMEM);
        }
        let mut kdata = [0_u8; KDATA];
        if in_size > 0 {
            let Some(target) = kdata.get_mut(..in_size) else {
                return Answer::error(Errno::EFAULT);
            };
            if host.copy_in(request, arg, target).is_err() {
                return Answer::error(Errno::EFAULT);
            }
        }
        let call = Call {
            request,
            file,
            arg,
            out_size,
            cmd,
        };
        self.state.why = "";
        let outcome = self.handle(host, &call, &mut kdata);
        let status = match outcome {
            Ok(Some(hold)) => {
                // A full table may be full of waits their programs gave up
                // on (`Waits::sweep_abandoned`); Linux has no such table, so
                // only a table full of live waits refuses.
                let mut held = self.state.waits.hold(&call, &kdata, hold, host.now_ns());
                if held.is_err() && self.state.waits.sweep_abandoned(host) > 0 {
                    held = self.state.waits.hold(&call, &kdata, hold, host.now_ns());
                }
                // A table full of live waits: Linux has none, so the call
                // is interrupted, as a signal would, and libdrm calls again
                // (`drmIoctl`, `intel_ioctl`); the deadline is absolute
                // for a syncobj wait and nothing elapsed for a GEM wait.
                // ENOMEM here was Mesa's VK_ERROR_UNKNOWN: the device lost.
                let held = held.map_err(|_| self.state.refuse("wait table full", Errno::EINTR));
                match held {
                    Ok(()) => {
                        if self.state.trace {
                            host.say(format_args!(
                                "intel: trace: ioctl {cmd:#x} (nr {:#04x}) request {request} held",
                                ioc_nr(cmd)
                            ));
                        }
                        return Answer::Later;
                    }
                    Err(errno) => -i32::from(errno.0),
                }
            }
            Ok(None) => 0,
            Err(errno) => -i32::from(errno.0),
        };
        if status != 0 && status != -i32::from(super::ETIME.0) {
            say_refused(&mut self.state, host, cmd, status);
        }
        // A failure only: every success would flood the console at a
        // frame's worth of ioctls, and a poll's ETIME is no failure.
        if self.state.trace && status != 0 && status != -i32::from(super::ETIME.0) {
            host.say(format_args!(
                "intel: trace: ioctl {cmd:#x} (nr {:#04x}) request {request} -> {status}",
                ioc_nr(cmd)
            ));
        }
        if out_size > 0
            && let Some(source) = kdata.get(..out_size)
            && host.copy_out(request, arg, source).is_err()
        {
            return Answer::error(Errno::EFAULT);
        }
        Answer::Now(status)
    }

    /// The handler for `call`.
    fn handle<H: Host<B::Memory>>(
        &mut self,
        host: &mut H,
        call: &Call,
        data: &mut [u8],
    ) -> Outcome {
        let number = ioc_nr(call.cmd);
        if (DRM_COMMAND_BASE..DRM_COMMAND_END).contains(&number) {
            return self.handle_i915(host, call, data, number - DRM_COMMAND_BASE);
        }
        match number {
            core_nr::VERSION => self.version(host, call, data).map(|()| None),
            core_nr::GET_CAP => get_cap(data).map(|()| None),
            core_nr::GEM_CLOSE => gem::close(self, call, data).map(|()| None),
            core_nr::PRIME_HANDLE_TO_FD => {
                gem::prime_handle_to_fd(self, host, call, data).map(|()| None)
            }
            core_nr::PRIME_FD_TO_HANDLE => {
                gem::prime_fd_to_handle(self, host, call, data).map(|()| None)
            }
            core_nr::SYNCOBJ_CREATE => syncobj::create(self, call, data).map(|()| None),
            core_nr::SYNCOBJ_DESTROY => syncobj::destroy(self, call, data).map(|()| None),
            core_nr::SYNCOBJ_HANDLE_TO_FD => {
                syncobj::handle_to_fd(self, host, call, data).map(|()| None)
            }
            core_nr::SYNCOBJ_FD_TO_HANDLE => {
                syncobj::fd_to_handle(self, host, call, data).map(|()| None)
            }
            core_nr::SYNCOBJ_TRANSFER => syncobj::transfer(self, call, data).map(|()| None),
            core_nr::SYNCOBJ_WAIT => syncobj::wait(self, host, call, data, false),
            core_nr::SYNCOBJ_TIMELINE_WAIT => syncobj::wait(self, host, call, data, true),
            core_nr::SYNCOBJ_RESET => syncobj::reset(self, host, call, data).map(|()| None),
            core_nr::SYNCOBJ_SIGNAL => syncobj::signal(self, host, call, data).map(|()| None),
            core_nr::SYNCOBJ_TIMELINE_SIGNAL => {
                syncobj::timeline_signal(self, host, call, data).map(|()| None)
            }
            core_nr::SYNCOBJ_QUERY => syncobj::query(self, host, call, data).map(|()| None),
            // Linux keeps the name for its debugfs; nothing here reads it.
            SET_CLIENT_NAME => Ok(None),
            _ => Err(unserved(self, host, call, "a DRM core ioctl")),
        }
    }

    fn handle_i915<H: Host<B::Memory>>(
        &mut self,
        host: &mut H,
        call: &Call,
        data: &mut [u8],
        index: u8,
    ) -> Outcome {
        match index {
            nr::GETPARAM => getparam::getparam(self, host, call, data).map(|()| None),
            nr::QUERY => query::query(self, host, call, data).map(|()| None),
            nr::GEM_CREATE => gem::create(self, call, data).map(|()| None),
            nr::GEM_CREATE_EXT => gem::create_ext(self, host, call, data).map(|()| None),
            nr::GEM_MMAP_OFFSET => gem::mmap_offset(self, host, call, data).map(|()| None),
            nr::GEM_SET_TILING => gem::set_tiling(self, call, data).map(|()| None),
            nr::GEM_GET_TILING => gem::get_tiling(self, call, data).map(|()| None),
            nr::GEM_SET_CACHING => gem::set_caching(self, call, data).map(|()| None),
            nr::GEM_GET_CACHING => gem::get_caching(self, call, data).map(|()| None),
            nr::GEM_SET_DOMAIN => gem::set_domain(self, call, data).map(|()| None),
            nr::GEM_SW_FINISH => gem::sw_finish(self, call, data).map(|()| None),
            nr::GEM_MADVISE => gem::madvise(self, call, data).map(|()| None),
            nr::GEM_BUSY => gem::busy(self, call, data).map(|()| None),
            nr::GEM_WAIT => gem::wait(self, host, call, data),
            nr::GEM_GET_APERTURE => gem::get_aperture(self, data).map(|()| None),
            nr::GEM_THROTTLE => Ok(None),
            nr::GEM_EXECBUFFER2 => super::exec::execbuffer2(self, host, call, data).map(|()| None),
            nr::GEM_CONTEXT_CREATE => vm::context_create(self, host, call, data).map(|()| None),
            nr::GEM_CONTEXT_DESTROY => vm::context_destroy(self, call, data).map(|()| None),
            nr::GEM_CONTEXT_GETPARAM => vm::context_getparam(self, call, data).map(|()| None),
            nr::GEM_CONTEXT_SETPARAM => vm::context_setparam(self, host, call, data).map(|()| None),
            nr::GEM_VM_CREATE => vm::vm_create(self, host, call, data).map(|()| None),
            nr::GEM_VM_DESTROY => vm::vm_destroy(self, call, data).map(|()| None),
            nr::GET_RESET_STATS => vm::reset_stats(self, host, call, data).map(|()| None),
            nr::REG_READ => vm::reg_read(self, data).map(|()| None),
            // K7 is not built (ledger 2352: NOT YET); anv uses userptr only
            // for VK_EXT_external_memory_host, and reads ENODEV as absent.
            nr::GEM_USERPTR => Err(unserved_as(
                self,
                host,
                call,
                "GEM_USERPTR (K7 is not built)",
                Errno::ENODEV,
            )),
            // `MMAP_GTT_VERSION` is 4, so Mesa uses `MMAP_OFFSET`.
            nr::GEM_MMAP => Err(unserved_as(
                self,
                host,
                call,
                "the legacy GEM_MMAP",
                Errno::ENODEV,
            )),
            nr::GEM_PREAD | nr::GEM_PWRITE => Err(unserved_as(
                self,
                host,
                call,
                "GEM_PREAD and GEM_PWRITE",
                Errno::ENODEV,
            )),
            // No OA unit is driven.
            nr::PERF_OPEN | nr::PERF_ADD_CONFIG | nr::PERF_REMOVE_CONFIG => Err(unserved_as(
                self,
                host,
                call,
                "i915 perf (no OA)",
                Errno::ENODEV,
            )),
            _ => Err(unserved(self, host, call, "an i915 ioctl")),
        }
    }

    /// `drm_version`: name `i915`, which is what makes Mesa pick its i915
    /// back end (`intel_kmd.c`), and i915's version and date
    /// (`i915_driver.c`: `DRIVER_MAJOR` 1, `DRIVER_MINOR` 6,
    /// `DRIVER_DESC` "Intel Graphics"), and the date `drm_version` now gives
    /// every driver, "0".
    fn version<H: Host<B::Memory>>(
        &mut self,
        host: &mut H,
        call: &Call,
        data: &mut [u8],
    ) -> Result<(), Errno> {
        let mut version = Version::from_bytes(data).ok_or(Errno::EFAULT)?;
        version.version_major = 1;
        version.version_minor = 6;
        version.version_patchlevel = 0;
        version.name_len = copy_string(host, call, version.name, version.name_len, b"i915")?;
        version.date_len = copy_string(host, call, version.date, version.date_len, b"0")?;
        version.desc_len = copy_string(
            host,
            call,
            version.desc,
            version.desc_len,
            b"Intel Graphics",
        )?;
        put(data, &version.to_bytes());
        Ok(())
    }
}

/// `drm_copy_field`: copy as much of `text` as fits in `len`, and tell the
/// whole length.
fn copy_string<M, H: Host<M>>(
    host: &mut H,
    call: &Call,
    to: u64,
    len: u64,
    text: &[u8],
) -> Result<u64, Errno> {
    let fits = usize::try_from(len).unwrap_or(usize::MAX).min(text.len());
    if fits > 0
        && let Some(part) = text.get(..fits)
    {
        host.copy_out(call.request, to, part)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(text.len() as u64)
}

/// `drm_getcap` on a render node: `PRIME` (both ways), `SYNCOBJ`,
/// `SYNCOBJ_TIMELINE` and `TIMESTAMP_MONOTONIC`: the capabilities
/// `drm_getcap` answers for a driver without `DRIVER_MODESET`, as this one
/// is. Every other is `EOPNOTSUPP`, as there.
fn get_cap(data: &mut [u8]) -> Result<(), Errno> {
    let mut cap = GetCap::from_bytes(data).ok_or(Errno::EFAULT)?;
    cap.value = match cap.capability {
        consts::CAP_TIMESTAMP_MONOTONIC => 1,
        consts::CAP_PRIME => consts::PRIME_CAP_IMPORT | consts::PRIME_CAP_EXPORT,
        consts::CAP_SYNCOBJ | consts::CAP_SYNCOBJ_TIMELINE => 1,
        _ => return Err(Errno::EOPNOTSUPP),
    };
    put(data, &cap.to_bytes());
    Ok(())
}

/// Write `bytes` over the start of `data`.
pub fn put(data: &mut [u8], bytes: &[u8]) {
    if let Some(target) = data.get_mut(..bytes.len()) {
        target.copy_from_slice(bytes);
    }
}

/// A call Linux serves and this driver does not: said once a boot, by name,
/// with the program and its arguments (`docs/INTEL-GPU.md` §8, risk 4), and
/// answered `EINVAL`.
pub fn unserved<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    what: &str,
) -> Errno {
    unserved_as(frontend, host, call, what, Errno::EINVAL)
}

/// As [`unserved`], answering `errno`.
pub fn unserved_as<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    what: &str,
    errno: Errno,
) -> Errno {
    let pid = frontend
        .state
        .files
        .get(usize::from(call.file))
        .map_or(0, |file| file.pid);
    if frontend.state.say_once(u32::from(ioc_nr(call.cmd))) {
        host.say(format_args!(
            "intel: unserved {what}: cmd {:#010x} from pid {pid}, answered errno {}",
            call.cmd, errno.0
        ));
    }
    errno
}

/// A new context, for [`vm::new_context`]: i915's proto-context defaults.
#[must_use]
pub const fn default_context(file: u8, vm: u16, finalized: bool) -> Context {
    Context {
        file,
        vm,
        finalized,
        engines: None,
        engine_count: 0,
        hw: [None; super::limits::CONTEXT_ENGINES],
        bannable: true,
        recoverable: true,
        persistence: true,
        no_error_capture: false,
        priority: 0,
        guilty: 0,
        lost: 0,
        banned: false,
        closed: false,
        in_flight: 0,
    }
}

/// `intel: uapi refused <ioctl> <errno> <reason>`, rate-limited, for an
/// error from the ioctls whose failure Mesa takes for a lost device: an
/// execbuffer (anv retries only EINTR, EAGAIN and ENOMEM) and the syncobj
/// waits (ETIME is a timeout, EINTR and EAGAIN are called again).
fn say_refused<M, H: Host<M>>(state: &mut Tables<M>, host: &mut H, cmd: u32, status: i32) {
    let name = match ioc_nr(cmd) {
        number if number == DRM_COMMAND_BASE + nr::GEM_EXECBUFFER2 => {
            if ioc_dir(cmd) & IOC_READ != 0 {
                "EXECBUFFER2_WR"
            } else {
                "EXECBUFFER2"
            }
        }
        core_nr::SYNCOBJ_WAIT => "SYNCOBJ_WAIT",
        core_nr::SYNCOBJ_TIMELINE_WAIT => "SYNCOBJ_TIMELINE_WAIT",
        _ => return,
    };
    if !state.refusals.allow(host.now_ns()) {
        return;
    }
    let quiet = core::mem::take(&mut state.refusals.quiet);
    let why = if state.why.is_empty() { "-" } else { state.why };
    host.say(format_args!(
        "intel: uapi refused {name} {} {why} ({quiet} not said before it)",
        errno_name(-status)
    ));
}

/// An errno's name, for the refusal line.
fn errno_name(errno: i32) -> &'static str {
    let known = [
        (Errno::EPERM, "EPERM"),
        (Errno::ENOENT, "ENOENT"),
        (Errno::EINTR, "EINTR"),
        (Errno::EIO, "EIO"),
        (Errno::E2BIG, "E2BIG"),
        (Errno::EBADF, "EBADF"),
        (Errno::EAGAIN, "EAGAIN"),
        (Errno::ENOMEM, "ENOMEM"),
        (Errno::EACCES, "EACCES"),
        (Errno::EFAULT, "EFAULT"),
        (Errno::EBUSY, "EBUSY"),
        (Errno::ENODEV, "ENODEV"),
        (Errno::EINVAL, "EINVAL"),
        (Errno::ENOSPC, "ENOSPC"),
        (Errno::EDQUOT, "EDQUOT"),
        (Errno::EOPNOTSUPP, "EOPNOTSUPP"),
        (Errno::ENOTTY, "ENOTTY"),
        (Errno::EDEADLK, "EDEADLK"),
    ];
    known
        .iter()
        .find(|(known, _)| i32::from(known.0) == errno)
        .map_or("errno?", |(_, name)| name)
}
