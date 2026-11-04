//! Buffers on one card through `OpenCL`, loaded at run time: a host without the
//! library builds this and only fails if asked to use it.
use ferrix_vtier::store::{BLOCK, Backend, Layout};
use std::ffi::{CStr, c_char, c_void};
use std::ptr::null_mut;

unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}
type Handle = *mut c_void;

/// A symbol, typed by the caller.
///
/// # Safety
/// `F` must be the symbol's real function pointer type.
unsafe fn symbol<F: Copy>(library: *mut c_void, name: &CStr) -> Result<F, String> {
    // SAFETY: `library` came from dlopen, and `name` is NUL-terminated.
    let found = unsafe { dlsym(library, name.as_ptr()) };
    if found.is_null() {
        return Err(format!("OpenCL has no {name:?}"));
    }
    // SAFETY: a data pointer from dlsym is the function; the caller vouches for `F`.
    Ok(unsafe { std::mem::transmute_copy(&found) })
}

const CL_DEVICE_TYPE_GPU: u64 = 1 << 2;
const CL_DEVICE_NAME: u32 = 0x102B;
const CL_DEVICE_MAX_MEM_ALLOC_SIZE: u32 = 0x1010;
const CL_PLATFORM_NAME: u32 = 0x0902;
const CL_MEM_READ_WRITE: u64 = 1;
const CL_TRUE: u32 = 1;

type Transfer = unsafe extern "C" fn(
    Handle,
    Handle,
    u32,
    usize,
    usize,
    *mut c_void,
    u32,
    *const Handle,
    *mut Handle,
) -> i32;

/// The card and its buffers.
pub(super) struct Card {
    queue: Handle,
    buffers: Vec<Handle>,
    layout: Layout,
    read: Transfer,
    write: Transfer,
    finish: unsafe extern "C" fn(Handle) -> i32,
    name: String,
    cap: u64,
}

impl Card {
    /// Open the one device visible, which must be named `want`, and allocate
    /// `total` bytes on it, every buffer touched so a card without the room
    /// says so now and not in the middle of somebody's build.
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        clippy::too_many_lines,
        reason = "AUDIT: one sequence of OpenCL calls with one argument: each pointer is a local that outlives its call"
    )]
    pub(super) fn open(want: &str, total: u64) -> Result<Self, String> {
        if !std::env::var("CUDA_VISIBLE_DEVICES").is_ok_and(|v| v.starts_with("GPU-")) {
            return Err("name the card: CUDA_VISIBLE_DEVICES=GPU-<uuid>".into());
        }
        // SAFETY: a NUL-terminated name; RTLD_NOW.
        let library = unsafe { dlopen(c"libOpenCL.so.1".as_ptr(), 2) };
        if library.is_null() {
            return Err("no libOpenCL.so.1 on this host".into());
        }
        // SAFETY: each type below is the OpenCL 1.2 prototype of the symbol it names.
        unsafe {
            let platforms: unsafe extern "C" fn(u32, *mut Handle, *mut u32) -> i32 =
                symbol(library, c"clGetPlatformIDs")?;
            let platform_info: unsafe extern "C" fn(
                Handle,
                u32,
                usize,
                *mut c_void,
                *mut usize,
            ) -> i32 = symbol(library, c"clGetPlatformInfo")?;
            let devices: unsafe extern "C" fn(Handle, u64, u32, *mut Handle, *mut u32) -> i32 =
                symbol(library, c"clGetDeviceIDs")?;
            let device_info: unsafe extern "C" fn(
                Handle,
                u32,
                usize,
                *mut c_void,
                *mut usize,
            ) -> i32 = symbol(library, c"clGetDeviceInfo")?;
            let context: unsafe extern "C" fn(
                *const isize,
                u32,
                *const Handle,
                *const c_void,
                *mut c_void,
                *mut i32,
            ) -> Handle = symbol(library, c"clCreateContext")?;
            let queue: unsafe extern "C" fn(Handle, Handle, u64, *mut i32) -> Handle =
                symbol(library, c"clCreateCommandQueue")?;
            let buffer: unsafe extern "C" fn(Handle, u64, usize, *mut c_void, *mut i32) -> Handle =
                symbol(library, c"clCreateBuffer")?;
            let read: Transfer = symbol(library, c"clEnqueueReadBuffer")?;
            let write: Transfer = symbol(library, c"clEnqueueWriteBuffer")?;
            let finish: unsafe extern "C" fn(Handle) -> i32 = symbol(library, c"clFinish")?;

            let mut found = [null_mut(); 8];
            let mut count = 0;
            if platforms(8, found.as_mut_ptr(), &raw mut count) != 0 {
                return Err("no OpenCL platform".into());
            }
            let mut text = [0u8; 256];
            let read_text = |bytes: &[u8]| {
                CStr::from_bytes_until_nul(bytes)
                    .map_or(String::new(), |c| c.to_string_lossy().into_owned())
            };
            let mut platform = null_mut();
            for &candidate in &found[..(count as usize).min(8)] {
                platform_info(
                    candidate,
                    CL_PLATFORM_NAME,
                    text.len(),
                    text.as_mut_ptr().cast(),
                    null_mut(),
                );
                if read_text(&text).contains("NVIDIA") {
                    platform = candidate;
                }
            }
            if platform.is_null() {
                return Err("no NVIDIA OpenCL platform".into());
            }
            let mut listed = [null_mut(); 8];
            if devices(
                platform,
                CL_DEVICE_TYPE_GPU,
                8,
                listed.as_mut_ptr(),
                &raw mut count,
            ) != 0
                || count != 1
            {
                return Err(format!("{count} cards visible, not the one asked for"));
            }
            let device = listed[0];
            device_info(
                device,
                CL_DEVICE_NAME,
                text.len(),
                text.as_mut_ptr().cast(),
                null_mut(),
            );
            let name = read_text(&text);
            if !name.contains(want) {
                return Err(format!("the card is {name}, not {want}"));
            }
            let mut cap = 0u64;
            device_info(
                device,
                CL_DEVICE_MAX_MEM_ALLOC_SIZE,
                8,
                (&raw mut cap).cast(),
                null_mut(),
            );
            let mut error = 0;
            let ctx = context(
                std::ptr::null(),
                1,
                &raw const device,
                std::ptr::null(),
                null_mut(),
                &raw mut error,
            );
            if error != 0 {
                return Err(format!("clCreateContext: {error}"));
            }
            let q = queue(ctx, device, 0, &raw mut error);
            if error != 0 {
                return Err(format!("clCreateCommandQueue: {error}"));
            }
            let layout = Layout::new(total, cap);
            let mut buffers = Vec::new();
            let mut zeros = vec![0u8; BLOCK];
            for size in layout.buffers() {
                let made = buffer(
                    ctx,
                    CL_MEM_READ_WRITE,
                    size as usize,
                    null_mut(),
                    &raw mut error,
                );
                if error != 0 {
                    return Err(format!("clCreateBuffer of {size} bytes: {error}"));
                }
                // The driver allocates at first use: use it now, from each end.
                for at in [0, size as usize - BLOCK] {
                    let e = write(
                        q,
                        made,
                        CL_TRUE,
                        at,
                        BLOCK,
                        zeros.as_mut_ptr().cast(),
                        0,
                        std::ptr::null(),
                        null_mut(),
                    );
                    if e != 0 {
                        return Err(format!("no room on the card: {e}"));
                    }
                }
                buffers.push(made);
            }
            Ok(Self {
                queue: q,
                buffers,
                layout,
                read,
                write,
                finish,
                name,
                cap,
            })
        }
    }

    /// Buffers allocated.
    pub(super) fn buffers(&self) -> usize {
        self.buffers.len()
    }
    /// The card's name.
    pub(super) fn name(&self) -> &str {
        &self.name
    }
    /// The driver's largest allocation.
    pub(super) fn cap(&self) -> u64 {
        self.cap
    }
}

impl Backend for Card {
    fn read(&mut self, at: u64, to: &mut [u8]) -> Result<(), i32> {
        let (index, offset) = self.layout.place(at);
        let buffer = *self.buffers.get(index).ok_or(-1)?;
        // SAFETY: a blocking read into `to`, which outlives the call, of its length.
        let error = unsafe {
            (self.read)(
                self.queue,
                buffer,
                CL_TRUE,
                offset as usize,
                to.len(),
                to.as_mut_ptr().cast(),
                0,
                std::ptr::null(),
                null_mut(),
            )
        };
        if error == 0 { Ok(()) } else { Err(error) }
    }
    fn write(&mut self, at: u64, from: &[u8]) -> Result<(), i32> {
        let (index, offset) = self.layout.place(at);
        let buffer = *self.buffers.get(index).ok_or(-1)?;
        // SAFETY: a blocking write from `from`, which OpenCL only reads; it outlives the call.
        let error = unsafe {
            (self.write)(
                self.queue,
                buffer,
                CL_TRUE,
                offset as usize,
                from.len(),
                from.as_ptr().cast_mut().cast(),
                0,
                std::ptr::null(),
                null_mut(),
            )
        };
        if error == 0 { Ok(()) } else { Err(error) }
    }
    fn finish(&mut self) -> Result<(), i32> {
        // SAFETY: the queue this card made, alive as long as it is.
        let error = unsafe { (self.finish)(self.queue) };
        if error == 0 { Ok(()) } else { Err(error) }
    }
}
