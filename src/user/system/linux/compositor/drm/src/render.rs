//! The render node: `/dev/dri/renderD128`, as a client of the GPU opens it.
//!
//! The card next door is the screen; this is where the GPU is
//! (`docs/GPU.md` §3.3). A compositor that draws on the GPU opens both: the
//! card to show a frame, the render node to make one. What is here says the
//! node is real -- who is driving it and what it can do -- and then makes one
//! resource and asks about it, which is the first thing that costs the
//! device a message rather than being answered from the driver's HELLO.
//!
//! # Why this reports rather than fails
//!
//! A card with no GPU behind it has no render node, and that is not an
//! error: it is the ordinary 2D case, and the display test boots it that
//! way on purpose. So the probe answers a line either way, and whoever is
//! judging decides which line it should have been. That is what makes the
//! two boots a pair -- with the 3D device the node must be there, without it
//! must not -- rather than a check that passes whatever happens.

use std::ffi::CStr;
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

use ferrix_linux_abi::drm::{self, GemClose, PrimeHandle, Version};
use ferrix_linux_abi::socket::Width;
use ferrix_linux_abi::virtgpu::{
    self, ContextInit, ContextSetParam, ExecBuffer, Field, GetCaps, GetParam, Layout, Map,
    ResourceCreateBlob, ResourceInfo, TransferFromHost, TransferToHost, Wait,
};
pub use ferrix_linux_abi::virtgpu::{Box3d, ResourceCreate};

/// What the display test reads this program's render line by. Deliberately
/// not the compositor's own prefix: that one is what the boot is watched
/// for, and a second line carrying it would be taken for the scanout's.
const MARKER: &str = "render:";

/// The first render node. Linux numbers them from 128, and so does Ferrix.
const NODE: &CStr = c"/dev/dri/renderD128";

/// The longest driver name read back from `DRM_IOCTL_VERSION`.
const NAME_BYTES: usize = 64;

/// An open render node.
pub struct Render {
    fd: libc::c_int,
}

impl core::fmt::Debug for Render {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Render")
            .field("fd", &self.fd)
            .finish()
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        // SAFETY: the descriptor is this object's own and is open.
        let _ = unsafe { libc::close(self.fd) };
    }
}

impl Render {
    /// Open the render node.
    ///
    /// # Errors
    ///
    /// Whatever `open` said; `ENOENT` when the card has no GPU behind it,
    /// which is the ordinary answer for a 2D device.
    pub fn open() -> io::Result<Self> {
        // Through `sessiond` in a session that runs as its user.
        let fd = compositor_seat::open(NODE, libc::O_RDWR)?.into_raw_fd();
        Ok(Self { fd })
    }

    /// The first render node whose driver is called `driver`, by
    /// `DRM_IOCTL_VERSION`: how a program finds a node whose number depends
    /// on which driver started first (`docs/NVIDIA.md` §4.4, N3b), as
    /// Chrome's path finder and libgbm pick theirs by name.
    ///
    /// # Errors
    ///
    /// `ENOENT` when no node is driven by it.
    pub fn open_driven_by(driver: &str) -> io::Result<Self> {
        // Linux's render minors, 128 to 191.
        for number in 128..192 {
            let Ok(path) = std::ffi::CString::new(format!("/dev/dri/renderD{number}")) else {
                continue;
            };
            let Ok(fd) = compositor_seat::open(&path, libc::O_RDWR) else {
                continue;
            };
            let node = Self {
                fd: fd.into_raw_fd(),
            };
            if node.driver().is_ok_and(|name| name == driver) {
                return Ok(node);
            }
        }
        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }

    /// The width this program's structures are laid out at.
    fn width() -> Width {
        if size_of::<usize>() == 4 {
            Width::Bits32
        } else {
            Width::Bits64
        }
    }

    /// Run `request` with `value` as its argument, and read the answer back
    /// into it. The same shape as the card's, for the structures that are
    /// one size at every width.
    fn ioctl<L: Layout>(&self, request: u32, value: &mut L) -> io::Result<()> {
        let mut bytes = vec![0u8; L::SIZE];
        value
            .write(&mut bytes)
            .ok_or_else(|| io::Error::other("a structure larger than its buffer"))?;
        // SAFETY: `bytes` is a live buffer of exactly the size the request's
        // number encodes, which the kernel reads and writes within.
        let result = unsafe { libc::ioctl(self.fd, request as _, bytes.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        *value = L::read(&bytes).ok_or_else(|| io::Error::other("a short answer"))?;
        Ok(())
    }

    /// Who is driving this node, from `DRM_IOCTL_VERSION`.
    ///
    /// This is what userspace picks a back end by, exactly as on Linux:
    /// `virtio_gpu` speaks virgl, and another name would speak something
    /// else. `struct drm_version` carries pointers and lengths, so it is one
    /// size on a 64-bit program and another on a 32-bit one, and it is
    /// written and read at the width this program was built for.
    ///
    /// # Errors
    ///
    /// Whatever the node said.
    pub fn driver(&self) -> io::Result<String> {
        let width = Self::width();
        let mut name = [0u8; NAME_BYTES];
        let version = Version {
            version_major: 0,
            version_minor: 0,
            version_patchlevel: 0,
            name_len: name.len() as u64,
            name: name.as_mut_ptr() as usize as u64,
            date_len: 0,
            date: 0,
            desc_len: 0,
            desc: 0,
        };
        let mut bytes = vec![0u8; Version::size(width)];
        version
            .write(width, &mut bytes)
            .ok_or_else(|| io::Error::other("a structure larger than its buffer"))?;
        // SAFETY: `bytes` is a live buffer of the size the request's number
        // encodes, and the one pointer in it is to `name`, which outlives
        // the call and whose length is beside it.
        let result =
            unsafe { libc::ioctl(self.fd, drm::ioctl_version(width) as _, bytes.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let answered =
            Version::read(width, &bytes).ok_or_else(|| io::Error::other("a short answer"))?;
        let len = usize::try_from(answered.name_len)
            .unwrap_or(usize::MAX)
            .min(name.len());
        Ok(String::from_utf8_lossy(name.get(..len).unwrap_or(&[])).into_owned())
    }

    /// What `VIRTGPU_GETPARAM` says about `param`.
    ///
    /// The answer goes to an address the request carries, not back into the
    /// structure, so the `u64` it is written into outlives the call here.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EINVAL` for a parameter it does not know.
    pub fn param(&self, param: u64) -> io::Result<u64> {
        let mut answer: u64 = 0;
        let mut request = GetParam {
            param,
            value: (&raw mut answer) as usize as u64,
        };
        self.ioctl(virtgpu::IOCTL_GETPARAM, &mut request)?;
        Ok(answer)
    }

    /// Make a resource of `size` bytes, and answer its object handle and the
    /// resource behind it.
    ///
    /// The target, format and bind words are virgl's and go to the driver
    /// untouched; a plain buffer is what they say here, because a buffer is
    /// the one shape `MAKE_OBJ` carries today. The shape fields are left at
    /// what a buffer means -- one row, one layer, no mip levels.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `ENODEV` when the driver has gone.
    pub fn create_resource(&self, size: u32) -> io::Result<(u32, u32)> {
        let mut request = ResourceCreate {
            target: PIPE_BUFFER,
            format: VIRGL_FORMAT_R8_UNORM,
            bind: VIRGL_BIND_VERTEX_BUFFER,
            width: size,
            height: 1,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
            bo_handle: 0,
            res_handle: 0,
            size,
            stride: size,
        };
        self.ioctl(virtgpu::IOCTL_RESOURCE_CREATE, &mut request)?;
        Ok((request.bo_handle, request.res_handle))
    }

    /// Make the resource `request` describes, and answer its object handle
    /// and the resource behind it, which is the number a command stream
    /// names it by.
    ///
    /// # Errors
    ///
    /// Whatever the node said.
    pub fn create(&self, mut request: ResourceCreate) -> io::Result<(u32, u32)> {
        request.bo_handle = 0;
        request.res_handle = 0;
        self.ioctl(virtgpu::IOCTL_RESOURCE_CREATE, &mut request)?;
        Ok((request.bo_handle, request.res_handle))
    }

    /// Make this open's context for capability set `capset`, with `rings`
    /// rings: `VIRTGPU_CONTEXT_INIT`, which an open does once and before
    /// anything else that needs a context.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EEXIST` for an open that has a context.
    pub fn context_init(&self, capset: u32, rings: u32) -> io::Result<()> {
        let mut params = [0u8; 2 * ContextSetParam::SIZE];
        for (at, (param, value)) in [
            (virtgpu::CONTEXT_PARAM_CAPSET_ID, u64::from(capset)),
            (virtgpu::CONTEXT_PARAM_NUM_RINGS, u64::from(rings)),
        ]
        .into_iter()
        .enumerate()
        {
            let slot = params
                .get_mut(at * ContextSetParam::SIZE..(at + 1) * ContextSetParam::SIZE)
                .ok_or_else(|| io::Error::other("no room for a parameter"))?;
            ContextSetParam { param, value }
                .write(slot)
                .ok_or_else(|| io::Error::other("a parameter larger than its slot"))?;
        }
        let mut request = ContextInit {
            num_params: 2,
            pad: 0,
            ctx_set_params: params.as_ptr() as u64,
        };
        self.ioctl(virtgpu::IOCTL_CONTEXT_INIT, &mut request)
    }

    /// Make a blob of `size` bytes: `VIRTGPU_RESOURCE_CREATE_BLOB`, with no
    /// commands to run first. Answers its handle and resource.
    ///
    /// # Errors
    ///
    /// Whatever the node said.
    pub fn create_blob(
        &self,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
    ) -> io::Result<(u32, u32)> {
        let mut request = ResourceCreateBlob {
            blob_mem,
            blob_flags,
            bo_handle: 0,
            res_handle: 0,
            size,
            pad: 0,
            cmd_size: 0,
            cmd: 0,
            blob_id,
        };
        self.ioctl(virtgpu::IOCTL_RESOURCE_CREATE_BLOB, &mut request)?;
        Ok((request.bo_handle, request.res_handle))
    }

    /// Map `len` bytes of object `handle`'s backing: the bytes a transfer
    /// moves to and from the device, or for a blob, its pages in the
    /// device's window.
    ///
    /// # Errors
    ///
    /// Whatever the node or `mmap` said.
    pub fn map(&self, handle: u32, len: usize) -> io::Result<Mapping> {
        let mut request = Map {
            offset: 0,
            handle,
            pad: 0,
        };
        self.ioctl(virtgpu::IOCTL_MAP, &mut request)?;
        let offset = libc::off_t::try_from(request.offset)
            .map_err(|_| io::Error::other("a map offset `mmap` cannot take"))?;
        // SAFETY: a new shared mapping of this descriptor at an address the
        // kernel chooses; nothing existing is replaced.
        let at = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd,
                offset,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping {
            at: at.cast::<u8>(),
            len,
        })
    }

    /// Copy `region` of object `handle` from its backing, starting `offset`
    /// bytes in with rows `stride` bytes apart, to the device.
    ///
    /// # Errors
    ///
    /// Whatever the node said.
    pub fn transfer_to_host(
        &self,
        handle: u32,
        region: Box3d,
        offset: u32,
        stride: u32,
    ) -> io::Result<()> {
        let mut request = TransferToHost {
            bo_handle: handle,
            r#box: region,
            level: 0,
            offset,
            stride,
            layer_stride: 0,
        };
        self.ioctl(virtgpu::IOCTL_TRANSFER_TO_HOST, &mut request)
    }

    /// The same the other way: the device's copy into the backing. It comes
    /// after every command stream submitted before it, which is what makes
    /// it the way to read a frame back.
    ///
    /// # Errors
    ///
    /// Whatever the node said.
    pub fn transfer_from_host(
        &self,
        handle: u32,
        region: Box3d,
        offset: u32,
        stride: u32,
    ) -> io::Result<()> {
        let mut request = TransferFromHost {
            bo_handle: handle,
            r#box: region,
            level: 0,
            offset,
            stride,
            layer_stride: 0,
        };
        self.ioctl(virtgpu::IOCTL_TRANSFER_FROM_HOST, &mut request)
    }

    /// Run a command stream in this open's context. The words are the
    /// renderer's own language -- virgl's, for `virtio_gpu`.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EINVAL` for a stream longer than it takes.
    pub fn exec(&self, commands: &[u32]) -> io::Result<()> {
        let size = u32::try_from(size_of_val(commands))
            .map_err(|_| io::Error::other("a command stream too long to describe"))?;
        let mut request = ExecBuffer {
            flags: 0,
            size,
            command: commands.as_ptr() as usize as u64,
            bo_handles: 0,
            num_bo_handles: 0,
            fence_fd: -1,
            ring_idx: 0,
            syncobj_stride: 0,
            num_in_syncobjs: 0,
            num_out_syncobjs: 0,
            in_syncobjs: 0,
            out_syncobjs: 0,
        };
        self.ioctl(virtgpu::IOCTL_EXECBUFFER, &mut request)
    }

    /// Capability set `capset`, as the device gave it, up to `room` bytes.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EINVAL` for a set the driver's streams are
    /// not in.
    pub fn caps(&self, capset: u32, room: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0u8; room];
        let mut request = GetCaps {
            cap_set_id: capset,
            cap_set_ver: 0,
            addr: bytes.as_mut_ptr() as usize as u64,
            size: u32::try_from(room).unwrap_or(u32::MAX),
            pad: 0,
        };
        self.ioctl(virtgpu::IOCTL_GET_CAPS, &mut request)?;
        Ok(bytes)
    }

    /// Export object `handle` as a descriptor another node of the card can
    /// be given: `DRM_IOCTL_PRIME_HANDLE_TO_FD`.
    ///
    /// What a compositor does with it is hand it to the card, which then
    /// shows what was drawn without the pixels leaving the device. The
    /// descriptor holds the object alive, so the handle may be closed
    /// afterwards.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EINVAL` for a handle this open has not got.
    pub fn export(&self, handle: u32) -> io::Result<OwnedFd> {
        let mut request = PrimeHandle {
            handle,
            flags: libc::O_CLOEXEC as u32,
            fd: -1,
        };
        self.ioctl(drm::IOCTL_PRIME_HANDLE_TO_FD, &mut request)?;
        if request.fd < 0 {
            return Err(io::Error::other("the node gave no descriptor"));
        }
        #[expect(
            unsafe_code,
            reason = "AUDIT: the descriptor the node just made, which nothing else owns"
        )]
        // SAFETY: as the reason says.
        Ok(unsafe { OwnedFd::from_raw_fd(request.fd) })
    }

    /// Take a buffer another program exported, and answer this open's
    /// handle for it: `DRM_IOCTL_PRIME_FD_TO_HANDLE` (`docs/GPU.md` §3.13).
    ///
    /// The object is then nameable in this open's context, so a stream run
    /// here may sample it where it lies. The descriptor is only read; the
    /// caller keeps it.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EINVAL` for a descriptor that is not a
    /// buffer of this node's device.
    pub fn import(&self, fd: std::os::fd::BorrowedFd<'_>) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        let mut request = PrimeHandle {
            handle: 0,
            flags: 0,
            fd: fd.as_raw_fd(),
        };
        self.ioctl(drm::IOCTL_PRIME_FD_TO_HANDLE, &mut request)?;
        Ok(request.handle)
    }

    /// Wait until the device is done with object `handle`:
    /// `VIRTGPU_WAIT`. An upload and a command stream return once they are
    /// on their way, as they do on Linux, so this is what comes before
    /// writing a backing the device may still be reading, and before
    /// reading one the device was asked to fill.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EBUSY` for a device that did not finish in
    /// the node's patience.
    pub fn wait(&self, handle: u32) -> io::Result<()> {
        let mut request = Wait { handle, flags: 0 };
        self.ioctl(virtgpu::IOCTL_WAIT, &mut request)
    }

    /// Let go of object `handle`: `DRM_IOCTL_GEM_CLOSE`.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `EINVAL` for a handle this open has not got.
    pub fn close(&self, handle: u32) -> io::Result<()> {
        let mut request = GemClose { handle, pad: 0 };
        self.ioctl(drm::IOCTL_GEM_CLOSE, &mut request)
    }

    /// What `VIRTGPU_RESOURCE_INFO` says is behind object `handle`: its
    /// resource and its size.
    ///
    /// # Errors
    ///
    /// Whatever the node said; `ENOENT` for a handle this open has not got.
    pub fn resource_info(&self, handle: u32) -> io::Result<(u32, u32)> {
        let mut request = ResourceInfo {
            bo_handle: handle,
            res_handle: 0,
            size: 0,
            blob_mem: 0,
        };
        self.ioctl(virtgpu::IOCTL_RESOURCE_INFO, &mut request)?;
        Ok((request.res_handle, request.size))
    }
}

/// An object's backing, mapped into this program until it is dropped.
pub struct Mapping {
    at: *mut u8,
    len: usize,
}

impl Mapping {
    /// The whole of the dmabuf `fd`, mapped shared and read-only: what a
    /// compositor drawing in software reads a client's GPU buffer through,
    /// as Linux's `dma_buf_mmap` gives it (`docs/NVIDIA.md` §4.6, N3b). Its
    /// size is the descriptor's end, which `lseek` to `SEEK_END` answers on
    /// a dmabuf.
    ///
    /// # Errors
    ///
    /// What `lseek` or `mmap` said, and `EINVAL` for an empty buffer.
    pub fn of_dmabuf(fd: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        // SAFETY: a descriptor the caller holds open; `lseek` moves only its
        // offset, which nothing reads.
        let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        let len = usize::try_from(end).map_err(|_| io::Error::last_os_error())?;
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: a fresh shared read-only mapping the kernel places; the
        // descriptor names the buffer it maps, and the mapping holds it.
        let at = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            at: at.cast(),
            len,
        })
    }
}

impl core::fmt::Debug for Mapping {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Mapping")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Mapping {
    /// The bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: a live shared mapping of `len` bytes that this object owns;
        // the device writes it only inside a transfer this program waits for.
        unsafe { core::slice::from_raw_parts(self.at, self.len) }
    }

    /// The bytes, to write.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for `bytes`, and unique through `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.at, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: exactly the range `mmap` answered, unmapped once.
        let _ = unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

/// virgl's `PIPE_BUFFER`: a resource with no shape, which is what bytes on
/// their way to a shader are. From Mesa's `p_defines.h`, as `src/user/system/native/drivers/display/virtio-gpu` takes
/// it.
const PIPE_BUFFER: u32 = 0;

/// virgl's `VIRGL_FORMAT_R8_UNORM`: one byte a pixel, which is how a
/// buffer's bytes are counted.
const VIRGL_FORMAT_R8_UNORM: u32 = 64;

/// virgl's `VIRGL_BIND_VERTEX_BUFFER`. virgl numbers some of its bind bits
/// differently from Mesa's `PIPE_BIND_*`, so it is taken from virgl's header.
const VIRGL_BIND_VERTEX_BUFFER: u32 = 1 << 4;

/// How big the resource the probe asks for is: one page, which is enough to
/// be a real resource on the device and small enough to cost nothing. The
/// same size the kernel's own proof uses.
const PROBE_BYTES: u32 = 4096;

/// One line saying what the render node is, or that there is none.
///
/// Never an error: see the module's note. The line is
/// `render: renderD128 <driver> 3d <n> capsets 0x<mask>`, or
/// `render: none <why>`.
#[must_use]
pub fn probe() -> String {
    let node = match Render::open() {
        Ok(node) => node,
        Err(error) => return format!("{MARKER} none {}", reason(&error)),
    };
    let driver = match node.driver() {
        Ok(driver) if !driver.is_empty() => driver,
        Ok(_) => return format!("{MARKER} none the node named no driver"),
        Err(error) => return format!("{MARKER} none version failed: {}", reason(&error)),
    };
    // What the node says it can do. A parameter it does not know is an
    // error rather than a zero, so each is reported as it answered.
    let three_d = node.param(virtgpu::PARAM_3D_FEATURES).unwrap_or(0);
    let capsets = node.param(virtgpu::PARAM_SUPPORTED_CAPSET_IDS).unwrap_or(0);
    format!(
        "{MARKER} renderD128 {driver} 3d {three_d} capsets 0x{capsets:x} {} {} {} {} {} {}",
        object(&node),
        caps(&node, capsets),
        moved(&node),
        drew(&node),
        blob(capsets),
        read(&node),
    )
}

/// What a read of the node said with no event queued: `read EAGAIN`, or
/// `read none <what it did>`.
///
/// Linux's `drm_read` answers a render node as it answers a card, waiting
/// for an event and `EAGAIN` under `O_NONBLOCK` -- measured on a 7.0 host,
/// on amdgpu's and nvidia's render nodes -- and a render node never has an
/// event unless `POLL_RINGS_MASK` asked for them. So this reads once with
/// `O_NONBLOCK` set, which a wait would otherwise hang, and puts the flags
/// back.
fn read(node: &Render) -> String {
    // SAFETY: the descriptor is the node's own and open; F_GETFL takes no
    // argument.
    let flags = unsafe { libc::fcntl(node.fd, libc::F_GETFL) };
    // SAFETY: as above, with the flags it answered and one more.
    if flags < 0 || unsafe { libc::fcntl(node.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return format!("read none fcntl: {}", reason(&io::Error::last_os_error()));
    }
    let mut buffer = [0u8; 4096];
    // SAFETY: `buffer` is live and as long as the count says.
    let got = unsafe { libc::read(node.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    let answer = io::Error::last_os_error();
    // SAFETY: as above, putting back the flags the node had.
    let _ = unsafe { libc::fcntl(node.fd, libc::F_SETFL, flags) };
    match (got, answer.raw_os_error()) {
        (-1, Some(libc::EAGAIN)) => "read EAGAIN".to_owned(),
        (-1, _) => format!("read none {}", reason(&answer)),
        (got, _) => format!("read none {got} bytes"),
    }
}

/// What making a Venus blob said: `blob <n> bytes`, the bytes of a pattern
/// written through the mapping that read back, or `blob none <why>`.
///
/// Venus keeps its rings and every host-visible allocation in blobs of host
/// memory, reached through the device's window (`docs/GPU.md` §6.1). This
/// asks for the kind it asks for first: `blob_id` 0 in a Venus context, which
/// the host's render server answers with plain shared memory. On an open of
/// its own, because an open makes its context once and the probe's has one.
/// A mapping that reached anything but the host's memory -- the window's
/// unassigned space, or the wrong pages -- does not give the pattern back.
fn blob(capsets: u64) -> String {
    if capsets & (1 << virtgpu::CAPSET_VENUS) == 0 {
        return "blob none no Venus".to_owned();
    }
    let attempt = || -> io::Result<usize> {
        let node = Render::open()?;
        node.context_init(virtgpu::CAPSET_VENUS, 64)?;
        let (handle, _) = node.create_blob(
            virtgpu::BLOB_MEM_HOST3D,
            virtgpu::BLOB_FLAG_USE_MAPPABLE,
            0,
            u64::from(PROBE_BYTES),
        )?;
        let len = PROBE_BYTES as usize;
        let mut mapping = node.map(handle, len)?;
        let pattern = |at: usize| (at as u8).wrapping_mul(29) ^ 0xa5;
        for (at, byte) in mapping.bytes_mut().iter_mut().enumerate() {
            *byte = pattern(at);
        }
        Ok(mapping
            .bytes()
            .iter()
            .enumerate()
            .filter(|&(at, &byte)| byte == pattern(at))
            .count())
    };
    match attempt() {
        Ok(bytes) => format!("blob {bytes} bytes"),
        Err(error) => format!("blob none {}", reason(&error)),
    }
}

/// What asking for the capability set said: `caps <n> bytes v<version>`, or
/// `caps none <why>`. The set asked for is the highest the node offers, and
/// its first word is its own version, which is never zero for a set that
/// was really read from a device.
fn caps(node: &Render, offered: u64) -> String {
    if offered == 0 {
        return "caps none offered".to_owned();
    }
    let capset = 63 - offered.leading_zeros();
    match node.caps(capset, CAPS_ROOM) {
        Ok(bytes) => {
            let version = bytes
                .get(..4)
                .and_then(|word| word.try_into().ok())
                .map_or(0, u32::from_le_bytes);
            let used = bytes
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |at| at + 1);
            format!("caps {used} bytes v{version}")
        }
        Err(error) => format!("caps none {}", reason(&error)),
    }
}

/// What moving bytes to the device and back said: `moved <n> bytes`, or
/// `moved none <why>`.
///
/// A buffer is made and mapped, a pattern written into it and sent to the
/// device; the mapping is then wiped and the device's copy fetched back. The
/// pattern coming back proves the backing is the memory the device was
/// given, in both directions: a transfer that went nowhere leaves the wipe.
fn moved(node: &Render) -> String {
    let attempt = || -> io::Result<usize> {
        let (handle, _) = node.create_resource(PROBE_BYTES)?;
        let len = PROBE_BYTES as usize;
        let mut mapping = node.map(handle, len)?;
        let pattern = |at: usize| (at as u8).wrapping_mul(31) ^ 0x5a;
        for (at, byte) in mapping.bytes_mut().iter_mut().enumerate() {
            *byte = pattern(at);
        }
        let whole = Box3d {
            x: 0,
            y: 0,
            z: 0,
            w: PROBE_BYTES,
            h: 1,
            d: 1,
        };
        node.transfer_to_host(handle, whole, 0, 0)?;
        // The upload is on its way, not there: the pattern is the device's
        // to read until the wait says it has.
        node.wait(handle)?;
        mapping.bytes_mut().fill(0);
        node.transfer_from_host(handle, whole, 0, 0)?;
        node.wait(handle)?;
        Ok(mapping
            .bytes()
            .iter()
            .enumerate()
            .filter(|(at, byte)| **byte == pattern(*at))
            .count())
    };
    match attempt() {
        Ok(count) if count == PROBE_BYTES as usize => format!("moved {count} bytes"),
        Ok(count) => format!("moved none only {count} of {PROBE_BYTES} bytes came back"),
        Err(error) => format!("moved none {}", reason(&error)),
    }
}

/// What drawing on the GPU and reading the picture back said: `drew
/// <top-left> <top-right> <bottom-right>` as `0xRRGGBB`, or `drew none
/// <why>`.
///
/// A small texture is cleared to red, and a green rectangle drawn over its
/// top right quarter through the whole pipeline -- a vertex buffer written
/// in the stream, the vertex shader, a fragment shader, blend, rasterizer and
/// viewport state. Three pixels say whether it worked: the clear's colour
/// where nothing was drawn, the draw's where it was, and the clear's again
/// *below* the draw, which is what pins which way up a texture's rows are
/// read back. The judge decides what they should have been.
fn drew(node: &Render) -> String {
    match draw_and_read(node) {
        Ok(pixels) => format!(
            "drew 0x{:06x} 0x{:06x} 0x{:06x}",
            pixels[0], pixels[1], pixels[2]
        ),
        Err(error) => format!("drew none {}", reason(&error)),
    }
}

/// [`drew`]'s work: the three pixels, as `0xRRGGBB`.
fn draw_and_read(node: &Render) -> io::Result<[u32; 3]> {
    use compositor_virgl::{Blend, Rasterizer, Stream, VertexBuffer, VertexElement, pipe, shaders};

    const SIDE: u32 = 64;
    let bytes = SIDE * SIDE * 4;
    let (target, target_id) = node.create(ResourceCreate {
        target: pipe::TEXTURE_2D,
        format: pipe::FORMAT_B8G8R8A8_UNORM,
        bind: pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW,
        width: SIDE,
        height: SIDE,
        depth: 1,
        array_size: 1,
        last_level: 0,
        nr_samples: 0,
        flags: 0,
        bo_handle: 0,
        res_handle: 0,
        size: bytes,
        stride: SIDE * 4,
    })?;
    let (_, vertices_id) = node.create_resource(PROBE_BYTES)?;

    // Handles of state objects are this context's own to number.
    const SURFACE: u32 = 1;
    const BLEND: u32 = 2;
    const DSA: u32 = 3;
    const RASTERIZER: u32 = 4;
    const ELEMENTS: u32 = 5;
    const VERTEX: u32 = 6;
    const FRAGMENT: u32 = 7;

    let mut stream = Stream::new();
    stream.create_surface(SURFACE, target_id, pipe::FORMAT_B8G8R8A8_UNORM);
    stream.set_framebuffer(SURFACE);
    stream.clear([1.0, 0.0, 0.0, 1.0]);

    stream.create_blend(BLEND, Blend::REPLACE);
    stream.bind_object(pipe::object::BLEND, BLEND);
    stream.create_dsa(DSA);
    stream.bind_object(pipe::object::DSA, DSA);
    stream.create_rasterizer(RASTERIZER, Rasterizer { scissor: false });
    stream.bind_object(pipe::object::RASTERIZER, RASTERIZER);
    stream.create_vertex_elements(
        ELEMENTS,
        &[
            VertexElement {
                offset: 0,
                buffer: 0,
                format: pipe::FORMAT_R32G32_FLOAT,
            },
            VertexElement {
                offset: 8,
                buffer: 0,
                format: pipe::FORMAT_R32G32_FLOAT,
            },
        ],
    );
    stream.bind_object(pipe::object::VERTEX_ELEMENTS, ELEMENTS);
    let _ = stream.create_shader(
        VERTEX,
        pipe::SHADER_VERTEX,
        shaders::VERTEX,
        shaders::TOKENS,
    );
    let _ = stream.create_shader(
        FRAGMENT,
        pipe::SHADER_FRAGMENT,
        shaders::SOLID,
        shaders::TOKENS,
    );
    stream.bind_shader(VERTEX, pipe::SHADER_VERTEX);
    stream.bind_shader(FRAGMENT, pipe::SHADER_FRAGMENT);
    let side = SIDE as f32;
    stream.set_constants(pipe::SHADER_VERTEX, &[2.0 / side, 2.0 / side, -1.0, -1.0]);
    // Green, the rectangle it fills, and square corners.
    let half = side / 2.0;
    stream.set_constants(
        pipe::SHADER_FRAGMENT,
        &[
            0.0, 1.0, 0.0, 1.0, half, 0.0, half, half, 0.0, 2.0, 0.0, 0.0,
        ],
    );
    stream.set_viewport(SIDE, SIDE);

    // The top right quarter as a strip, in pixels, each vertex its place
    // and a texture coordinate nothing reads.
    let corners = [[half, 0.0], [side, 0.0], [half, half], [side, half]];
    let mut data = Vec::new();
    for corner in corners {
        for value in [corner[0], corner[1], 0.0, 0.0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }
    let _ = stream.write_buffer(vertices_id, 0, &data);
    stream.set_vertex_buffers(&[VertexBuffer {
        stride: 16,
        offset: 0,
        resource: vertices_id,
    }]);
    stream.draw(pipe::PRIM_TRIANGLE_STRIP, 0, 4);
    node.exec(stream.words())?;

    let mapping = node.map(target, bytes as usize)?;
    node.transfer_from_host(
        target,
        Box3d {
            x: 0,
            y: 0,
            z: 0,
            w: SIDE,
            h: SIDE,
            d: 1,
        },
        0,
        SIDE * 4,
    )?;
    node.wait(target)?;
    let pixel = |x: u32, y: u32| -> u32 {
        let at = ((y * SIDE + x) * 4) as usize;
        // B, G, R, A in memory.
        mapping
            .bytes()
            .get(at..at + 3)
            .map_or(0xdead_beef, |bgr| match bgr {
                [blue, green, red] => {
                    (u32::from(*red) << 16) | (u32::from(*green) << 8) | u32::from(*blue)
                }
                _ => 0xdead_beef,
            })
    };
    Ok([pixel(8, 8), pixel(56, 8), pixel(56, 56)])
}

/// Room for any capability set: virgl's second is under two kilobytes.
const CAPS_ROOM: usize = 4096;

/// What making one resource and asking about it said: `object <handle>/<res>
/// of <size> bytes`, or `object none <why>`.
///
/// This is the half of the line that costs the device a message. Everything
/// before it is answered from the driver's HELLO, so a node that names a
/// driver proves only that the core was told about one; a resource proves
/// the whole path -- the node's handle table, the core's session, the driver
/// and the device.
///
/// The resource is let go of when the node closes, which is the only way an
/// open has to let go of one today.
fn object(node: &Render) -> String {
    let (handle, resource) = match node.create_resource(PROBE_BYTES) {
        Ok(made) => made,
        Err(error) => return format!("object none create failed: {}", reason(&error)),
    };
    match node.resource_info(handle) {
        // The node answers its own table, so a disagreement here is the
        // table being wrong rather than the device saying something else.
        Ok((told, size)) if told == resource && size == PROBE_BYTES => {
            format!("object {handle}/{resource} of {size} bytes")
        }
        Ok((told, size)) => {
            format!("object none info said {told}/{size}, not {resource}/{PROBE_BYTES}")
        }
        Err(error) => format!("object none info failed: {}", reason(&error)),
    }
}

/// An error as one word and its message, with no newline in it.
fn reason(error: &io::Error) -> String {
    error.to_string().replace('\n', " ")
}
