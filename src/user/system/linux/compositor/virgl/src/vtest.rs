//! A client of virglrenderer's own test server: on the host, for tests, and
//! in a guest whose GPU has no virtio-gpu in front of it (`--renderer
//! vtest`, the RTX 3060 through NVIDIA's EGL).
//!
//! `virgl_test_server` is virglrenderer with a Unix socket where QEMU's
//! virtio-gpu would be: it takes the same command streams a guest's
//! `VIRTGPU_EXECBUFFER` carries and runs them on the host's GL. So what this
//! crate writes -- and the shaders above all, which are text only
//! virglrenderer can judge -- is tested in a second against the very code
//! that will run it, where a guest boot takes a minute and shows a black
//! screen for every mistake.
//!
//! # Two protocols
//!
//! The server starts in protocol version 0: the client numbers its own
//! resources and every pixel rides in the socket, both ways. A frame read
//! back at 1080p is 8 MB through the socket, copied by the server into a
//! buffer of its own first.
//!
//! Version 2 gives a texture that pixels move to or from a shared memory
//! file the server makes and hands over with the reply to
//! `VCMD_RESOURCE_CREATE2`. A transfer then names a place in that memory
//! and no pixel crosses the socket: the server's `glReadPixels` writes
//! straight into memory this process reads, and an upload is written here
//! and read there. This asks for version 2 and keeps version 0 for a server
//! that answers less, for a texture whose memory it cannot trust (below),
//! and for `FERRIX_VTEST_PROTOCOL=0`, which is how the two are measured
//! against each other.
//!
//! # Trusting the server's memory
//!
//! The memory is the server's file. A file that could shrink under this
//! process would make a read of its mapping a `SIGBUS`, so before mapping
//! one this seals it against shrinking and growing and then asks which
//! seals it carries: a file without both is not mapped, and that texture
//! moves its pixels through the socket as in version 0. Nothing is mapped
//! past the size `fstat` gives, and every place a transfer names is worked
//! out here from the texture's own size and checked against the mapping
//! before a byte is read or written. Nothing the server sends is used as a
//! size, an offset or a stride.
//!
//! A host without the server has no tests of it, and [`Vtest::start`] says
//! so with `None` rather than failing.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Device, Region, Texture, pipe};

/// The server's name, looked for on `PATH`.
const SERVER: &str = "virgl_test_server";

/// Set to `0` to keep to protocol version 0, which is how the shared
/// memory's gain is measured.
const PROTOCOL_ENV: &str = "FERRIX_VTEST_PROTOCOL";

/// `VCMD_*`, from virglrenderer 1.2.0's `vtest_protocol.h`.
const RESOURCE_CREATE: u32 = 2;
const RESOURCE_UNREF: u32 = 3;
const TRANSFER_GET: u32 = 4;
const TRANSFER_PUT: u32 = 5;
const SUBMIT_CMD: u32 = 6;
const RESOURCE_BUSY_WAIT: u32 = 7;
const CREATE_RENDERER: u32 = 8;
const PROTOCOL_VERSION: u32 = 11;
const RESOURCE_CREATE2: u32 = 12;
const TRANSFER_GET2: u32 = 13;
const TRANSFER_PUT2: u32 = 14;

/// The version asked for. Version 3 has the server number resources, which
/// buys nothing here, so this stops at the one that brought shared memory.
const SHARED_MEMORY: u32 = 2;

/// One texture's shared memory: the server's file, mapped here.
///
/// Its rows are the texture's width apart, four bytes a pixel, which is the
/// layout the server uses when a transfer gives no stride
/// (`vrend_renderer.c`: the level's width times the block size).
struct Shared {
    at: *mut u8,
    len: usize,
    width: u32,
    height: u32,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Shared")
            .field("len", &self.len)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// Map `fd` for a `width` by `height` texture, if it can be trusted:
    /// sealed against shrinking and growing (this seals it first, and then
    /// asks), and at least the texture's size. `None` is memory the
    /// texture does without.
    ///
    /// # Errors
    ///
    /// What `fcntl`, `fstat` or `mmap` said where the answer is not simply
    /// "this file will not do".
    fn adopt(fd: &OwnedFd, width: u32, height: u32) -> io::Result<Option<Self>> {
        let Some(len) = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|&len| len > 0)
        else {
            return Ok(None);
        };
        let raw = fd.as_raw_fd();
        // A file made without MFD_ALLOW_SEALING refuses this; the question
        // below is what decides.
        // SAFETY: fcntl on a descriptor this process holds; it changes only
        // which seals the file carries.
        let _ = unsafe {
            libc::fcntl(
                raw,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW,
            )
        };
        // SAFETY: as above; this only asks.
        let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
        let wanted = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
        if seals < 0 || seals & wanted != wanted {
            return Ok(None);
        }
        // SAFETY: a zeroed `stat` is a valid value of the type, all integers.
        let mut stat: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: fstat writes the struct it is given and nothing else.
        if unsafe { libc::fstat(raw, &raw mut stat) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let size = usize::try_from(stat.st_size).unwrap_or(0);
        if size < len {
            return Ok(None);
        }
        // SAFETY: a fresh shared mapping the kernel places, of no more than
        // the file's sealed size; the mapping holds the file.
        let at = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(Self {
            at: at.cast(),
            len,
            width,
            height,
        }))
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: a live shared mapping of `len` bytes that this object
        // owns, of a file sealed against shrinking; the server writes it
        // only inside a transfer this program waits for.
        unsafe { core::slice::from_raw_parts(self.at, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for `bytes`, and unique through `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.at, self.len) }
    }

    /// Where `region` starts in the memory, and its rows' length, if it is
    /// inside the texture: the offset a version-2 transfer names.
    fn place(&self, region: Region) -> Option<(usize, usize)> {
        let right = region.x.checked_add(region.width)?;
        let bottom = region.y.checked_add(region.height)?;
        if region.width == 0 || region.height == 0 || right > self.width || bottom > self.height {
            return None;
        }
        let stride = self.stride();
        let offset = (region.y as usize)
            .checked_mul(stride)?
            .checked_add((region.x as usize).checked_mul(4)?)?;
        let row = (region.width as usize).checked_mul(4)?;
        // The last row's end, which the checks above keep inside `len`;
        // asked again so that no slice below rests on arithmetic alone.
        let end = offset
            .checked_add((region.height as usize - 1).checked_mul(stride)?)?
            .checked_add(row)?;
        (end <= self.len).then_some((offset, row))
    }

    fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: exactly the range `mmap` answered, unmapped once.
        let _ = unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

/// Copy `rows` rows of `row` bytes from `from`, rows `from_stride` apart,
/// into `into`, rows `into_stride` apart. Short slices are an error rather
/// than a partial copy.
fn copy_rows(
    from: &[u8],
    from_stride: usize,
    into: &mut [u8],
    into_stride: usize,
    row: usize,
    rows: usize,
) -> io::Result<()> {
    let short = || io::Error::other("pixels too short for their region");
    for index in 0..rows {
        let source = index
            .checked_mul(from_stride)
            .and_then(|start| from.get(start..start.checked_add(row)?))
            .ok_or_else(short)?;
        let target = index
            .checked_mul(into_stride)
            .and_then(|start| into.get_mut(start..start.checked_add(row)?))
            .ok_or_else(short)?;
        target.copy_from_slice(source);
    }
    Ok(())
}

/// The protocol a new connection asks for: [`SHARED_MEMORY`] unless the
/// environment says `0`.
fn wanted_protocol() -> u32 {
    match std::env::var(PROTOCOL_ENV) {
        Ok(value) if value.trim() == "0" => 0,
        _ => SHARED_MEMORY,
    }
}

/// A running server and the connection to it. The server is this object's
/// own and goes when it does.
#[derive(Debug)]
pub struct Vtest {
    child: Child,
    stream: UnixStream,
    socket: PathBuf,
    next: u32,
    /// The protocol the server agreed to.
    version: u32,
    /// The shared memory of every texture that has some.
    shared: HashMap<u32, Shared>,
    /// Textures whose memory an upload wrote that the server may not have
    /// read yet: the next write to one waits for it first.
    unread: HashSet<u32>,
}

impl Drop for Vtest {
    fn drop(&mut self) {
        // The mappings first, then the server that made their files.
        self.shared.clear();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Vtest {
    /// Start a server of this program's own and connect to it, in the
    /// protocol the environment asks for (version 2 unless
    /// `FERRIX_VTEST_PROTOCOL=0`).
    ///
    /// `None` when the host has no `virgl_test_server`, or no GL for it to
    /// stand on: a test that cannot run is skipped and says so, because the
    /// encoder's own word-for-word tests do not need it.
    ///
    /// # Errors
    ///
    /// The server started and then could not be spoken to.
    pub fn start(name: &str) -> io::Result<Option<Self>> {
        Self::start_with(name, wanted_protocol())
    }

    /// [`Vtest::start`], asking for protocol `version`: `0` keeps every
    /// pixel in the socket, anything higher asks for shared memory.
    ///
    /// # Errors
    ///
    /// As [`Vtest::start`]'s.
    pub fn start_with(name: &str, version: u32) -> io::Result<Option<Self>> {
        let socket =
            std::env::temp_dir().join(format!("ferrix-vtest-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let spawned = Command::new(SERVER)
            .arg("--no-fork")
            .arg("--use-egl-surfaceless")
            .arg("--socket-path")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // The socket appears once the server is listening.
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            if let Ok(stream) = UnixStream::connect(&socket) {
                break stream;
            }
            // A server that has already gone had no GL to start on.
            if child.try_wait()?.is_some() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut vtest = Self {
            child,
            stream,
            socket,
            next: 1,
            version: 0,
            shared: HashMap::new(),
            unread: HashSet::new(),
        };
        // The one command whose length is in bytes: the client's name.
        vtest.header(u32::try_from(name.len()).unwrap_or(0), CREATE_RENDERER)?;
        vtest.stream.write_all(name.as_bytes())?;
        if version > 0 {
            vtest.version = vtest.negotiate(version.min(SHARED_MEMORY))?;
        }
        Ok(Some(vtest))
    }

    /// The protocol the server agreed to: `0` or `2`.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// How many textures have shared memory, which a test reads to know
    /// which road its pixels took.
    #[must_use]
    pub fn shared_textures(&self) -> usize {
        self.shared.len()
    }

    /// Ask for `version` and answer what the server agreed to. It answers
    /// the lower of its own and the one asked, and 0 where it has no shared
    /// memory (it says so on its standard output); anything but 0 or the
    /// version asked is a server this does not understand.
    fn negotiate(&mut self, version: u32) -> io::Result<u32> {
        self.header(1, PROTOCOL_VERSION)?;
        self.words(&[version])?;
        let [len, command, agreed] = self.reply()?;
        if len != 1 || command != PROTOCOL_VERSION {
            return Err(io::Error::other("the server's protocol answer is not one"));
        }
        if agreed == version || agreed == 0 {
            Ok(agreed)
        } else {
            Err(io::Error::other(format!(
                "the server agreed to protocol {agreed}, asked {version}"
            )))
        }
    }

    /// Read a reply of a header and one word.
    fn reply(&mut self) -> io::Result<[u32; 3]> {
        let mut bytes = [0_u8; 12];
        self.stream.read_exact(&mut bytes)?;
        let mut words = [0_u32; 3];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(4)) {
            let mut le = [0_u8; 4];
            le.copy_from_slice(chunk);
            *word = u32::from_le_bytes(le);
        }
        Ok(words)
    }

    /// Wait until the server has run everything sent before this: it
    /// answers `VCMD_RESOURCE_BUSY_WAIT` in order, and a transfer runs
    /// whole before the next command is read, so the answer means every
    /// transfer before it has finished with its memory. Without the wait
    /// flag it does not also wait for the GPU, which a transfer already
    /// did.
    fn sync(&mut self) -> io::Result<()> {
        self.header(2, RESOURCE_BUSY_WAIT)?;
        self.words(&[0, 0])?;
        let [len, command, _busy] = self.reply()?;
        if len != 1 || command != RESOURCE_BUSY_WAIT {
            return Err(io::Error::other("the server's wait answer is not one"));
        }
        self.unread.clear();
        Ok(())
    }

    /// The descriptor that comes with `VCMD_RESOURCE_CREATE2`'s answer: one
    /// byte, and the file beside it.
    fn receive_fd(&mut self) -> io::Result<OwnedFd> {
        let mut byte = 0_u8;
        let mut iov = libc::iovec {
            iov_base: (&raw mut byte).cast(),
            iov_len: 1,
        };
        // Room for one descriptor's control message, aligned as one.
        let mut control = [0_u64; 4];
        // SAFETY: a zeroed `msghdr` is a valid value: null pointers and
        // zero lengths, and musl's padding fields zero as they must be.
        let mut message: libc::msghdr = unsafe { core::mem::zeroed() };
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        #[allow(
            clippy::useless_conversion,
            reason = "a usize on glibc and a socklen_t on musl"
        )]
        {
            message.msg_controllen = size_of_val(&control).try_into().unwrap_or(0);
        }
        // SAFETY: recvmsg writes at most the one byte and the control
        // buffer described above, both alive for the call.
        let got = unsafe {
            libc::recvmsg(
                self.stream.as_raw_fd(),
                &raw mut message,
                libc::MSG_CMSG_CLOEXEC,
            )
        };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        if got == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other(
                "the server sent more descriptors than one",
            ));
        }
        // SAFETY: the message recvmsg filled in, with its control buffer.
        let header = unsafe { libc::CMSG_FIRSTHDR(&raw const message) };
        if header.is_null() {
            return Err(io::Error::other(
                "the server's answer carried no descriptor",
            ));
        }
        // SAFETY: a header CMSG_FIRSTHDR found inside the control buffer.
        let cmsg = unsafe { header.read_unaligned() };
        // SAFETY: a pure computation of a length from a constant.
        let one = unsafe { libc::CMSG_LEN(size_of::<libc::c_int>() as u32) };
        if cmsg.cmsg_level != libc::SOL_SOCKET
            || cmsg.cmsg_type != libc::SCM_RIGHTS
            || cmsg.cmsg_len as usize != one as usize
        {
            return Err(io::Error::other(
                "the server's answer carried no descriptor",
            ));
        }
        // SAFETY: the data of a control message whose length says it holds
        // exactly one descriptor, inside the control buffer.
        let data = unsafe { libc::CMSG_DATA(header) };
        // SAFETY: as above; the descriptor may be unaligned in the buffer.
        let raw = unsafe { data.cast::<libc::c_int>().read_unaligned() };
        if raw < 0 {
            return Err(io::Error::other("the server sent no descriptor"));
        }
        // SAFETY: SCM_RIGHTS installed the descriptor in this process just
        // now, and nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    fn header(&mut self, len: u32, command: u32) -> io::Result<()> {
        self.words(&[len, command])
    }

    fn words(&mut self, words: &[u32]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for word in words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        send(&mut self.stream, &bytes)
    }

    /// Make a resource and answer its number, which is what a stream names
    /// it by. A texture is `width` by `height`; a buffer is `width` bytes
    /// and one high. Its pixels move through the socket.
    ///
    /// # Errors
    ///
    /// The socket's.
    pub fn create(
        &mut self,
        target: u32,
        format: u32,
        bind: u32,
        width: u32,
        height: u32,
    ) -> io::Result<u32> {
        let resource = self.next;
        self.next += 1;
        self.header(10, RESOURCE_CREATE)?;
        // Depth and array size of one, no mip levels, no samples.
        self.words(&[resource, target, format, bind, width, height, 1, 1, 0, 0])?;
        Ok(resource)
    }

    /// Make a four-byte-a-pixel texture with shared memory, in protocol
    /// version 2. One whose memory cannot be trusted ([`Shared::adopt`])
    /// is still made, and moves its pixels through the socket.
    fn create_shared(
        &mut self,
        format: u32,
        bind: u32,
        width: u32,
        height: u32,
    ) -> io::Result<u32> {
        let size = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| io::Error::other("a texture too big for shared memory"))?;
        let resource = self.next;
        self.next += 1;
        self.header(11, RESOURCE_CREATE2)?;
        self.words(&[
            resource,
            pipe::TEXTURE_2D,
            format,
            bind,
            width,
            height,
            1,
            1,
            0,
            0,
            size,
        ])?;
        // Version 2 answers with the descriptor alone.
        let fd = self.receive_fd()?;
        if let Some(shared) = Shared::adopt(&fd, width, height)? {
            let _ = self.shared.insert(resource, shared);
        }
        Ok(resource)
    }

    /// Run a stream.
    ///
    /// # Errors
    ///
    /// The socket's. What the *renderer* made of the stream it says on its
    /// standard error and nowhere else, which is why a test reads pixels.
    pub fn submit(&mut self, words: &[u32]) -> io::Result<()> {
        self.header(u32::try_from(words.len()).unwrap_or(0), SUBMIT_CMD)?;
        self.words(words)
    }

    /// Write `data`, rows `stride` bytes apart, into `region` of a texture.
    ///
    /// # Errors
    ///
    /// The socket's.
    pub fn put(
        &mut self,
        resource: u32,
        region: Region,
        stride: u32,
        data: &[u8],
    ) -> io::Result<()> {
        if self.shared.contains_key(&resource) {
            return self.put_shared(resource, region, stride, data);
        }
        let len = u32::try_from(data.len()).unwrap_or(0);
        self.header(11 + len.div_ceil(4), TRANSFER_PUT)?;
        self.words(&[
            resource,
            0,
            stride,
            0,
            region.x,
            region.y,
            0,
            region.width,
            region.height,
            1,
            len,
        ])?;
        send(&mut self.stream, data)
    }

    /// [`Vtest::put`] through shared memory: the rows written into the
    /// texture's own place in it, and the server told where.
    fn put_shared(
        &mut self,
        resource: u32,
        region: Region,
        stride: u32,
        data: &[u8],
    ) -> io::Result<()> {
        if region.width == 0 || region.height == 0 {
            return Ok(());
        }
        // The memory may still hold an upload the server has not read.
        if self.unread.contains(&resource) {
            self.sync()?;
        }
        let outside = || io::Error::other("a region outside its texture");
        let shared = self.shared.get_mut(&resource).ok_or_else(outside)?;
        let (offset, row) = shared.place(region).ok_or_else(outside)?;
        let into_stride = shared.stride();
        let into = shared.bytes_mut().get_mut(offset..).ok_or_else(outside)?;
        copy_rows(
            data,
            stride as usize,
            into,
            into_stride,
            row,
            region.height as usize,
        )?;
        let span = u32::try_from(into_stride * (region.height as usize - 1) + row)
            .map_err(|_| outside())?;
        let offset = u32::try_from(offset).map_err(|_| outside())?;
        self.header(10, TRANSFER_PUT2)?;
        self.words(&[
            resource,
            0,
            region.x,
            region.y,
            0,
            region.width,
            region.height,
            1,
            span,
            offset,
        ])?;
        let _ = self.unread.insert(resource);
        Ok(())
    }

    /// Read `region` of a texture back, four bytes a pixel, rows packed.
    /// This comes after every stream submitted before it.
    ///
    /// # Errors
    ///
    /// The socket's, including a server that died of the stream.
    pub fn get(&mut self, resource: u32, region: Region) -> io::Result<Vec<u8>> {
        let row = region.width as usize * 4;
        let mut data = vec![0_u8; row * region.height as usize];
        self.get_into(resource, region, &mut data, row)?;
        Ok(data)
    }

    /// Read `region` of a texture back into `into`, rows `stride` bytes
    /// apart: with shared memory, the one copy from the server's
    /// `glReadPixels` to wherever the caller wants the pixels.
    ///
    /// # Errors
    ///
    /// The socket's; an `into` too short for the region.
    pub fn get_into(
        &mut self,
        resource: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        if self.shared.contains_key(&resource) {
            return self.get_shared(resource, region, into, stride);
        }
        let row = region.width as usize * 4;
        let len = region.width * region.height * 4;
        self.header(11, TRANSFER_GET)?;
        self.words(&[
            resource,
            0,
            region.width * 4,
            0,
            region.x,
            region.y,
            0,
            region.width,
            region.height,
            1,
            len,
        ])?;
        if stride == row {
            let target = into
                .get_mut(..len as usize)
                .ok_or_else(|| io::Error::other("pixels too short for their region"))?;
            return receive(&mut self.stream, target);
        }
        let mut data = vec![0_u8; len as usize];
        receive(&mut self.stream, &mut data)?;
        copy_rows(&data, row, into, stride, row, region.height as usize)
    }

    fn get_shared(
        &mut self,
        resource: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        if region.width == 0 || region.height == 0 {
            return Ok(());
        }
        let outside = || io::Error::other("a region outside its texture");
        let (offset, row) = self
            .shared
            .get(&resource)
            .and_then(|shared| shared.place(region))
            .ok_or_else(outside)?;
        let span = self
            .shared
            .get(&resource)
            .map(|shared| shared.stride() * (region.height as usize - 1) + row)
            .and_then(|span| u32::try_from(span).ok())
            .ok_or_else(outside)?;
        self.header(10, TRANSFER_GET2)?;
        self.words(&[
            resource,
            0,
            region.x,
            region.y,
            0,
            region.width,
            region.height,
            1,
            span,
            u32::try_from(offset).map_err(|_| outside())?,
        ])?;
        // The answer comes after the transfer has written the memory.
        self.sync()?;
        let shared = self.shared.get(&resource).ok_or_else(outside)?;
        let from = shared.bytes().get(offset..).ok_or_else(outside)?;
        copy_rows(
            from,
            shared.stride(),
            into,
            stride,
            row,
            region.height as usize,
        )
    }
}

/// Whether a texture's pixels are four bytes, the only kind given shared
/// memory: the layout above counts four.
const fn four_bytes(format: u32) -> bool {
    matches!(
        format,
        pipe::FORMAT_B8G8R8A8_UNORM | pipe::FORMAT_B8G8R8X8_UNORM | pipe::FORMAT_R8G8B8A8_UNORM
    )
}

/// The most one write or read on the socket moves: a 1080p frame through
/// the socket (protocol 0) is 8 MB, and Ferrix answered such a transfer with
/// `ENOMEM` on the RTX 3060 (N3c), so the bytes go in pieces and an error
/// says how many were asked for.
const PIECE: usize = 256 << 10;

/// Write all of `bytes`, [`PIECE`] at a time.
fn send(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    bytes
        .chunks(PIECE)
        .try_for_each(|piece| stream.write_all(piece))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("vtest: writing {} bytes: {error}", bytes.len()),
            )
        })
}

/// Fill `data`, [`PIECE`] at a time.
fn receive(stream: &mut UnixStream, data: &mut [u8]) -> io::Result<()> {
    let len = data.len();
    data.chunks_mut(PIECE)
        .try_for_each(|piece| stream.read_exact(piece))
        .map_err(|error| {
            io::Error::new(error.kind(), format!("vtest: reading {len} bytes: {error}"))
        })
}

impl Device for Vtest {
    fn texture(&mut self, texture: Texture) -> io::Result<u32> {
        if self.version >= SHARED_MEMORY && texture.moved && four_bytes(texture.format) {
            return self.create_shared(texture.format, texture.bind, texture.width, texture.height);
        }
        self.create(
            pipe::TEXTURE_2D,
            texture.format,
            texture.bind,
            texture.width,
            texture.height,
        )
    }

    fn buffer(&mut self, bytes: u32) -> io::Result<u32> {
        self.create(
            pipe::BUFFER,
            pipe::FORMAT_R8_UNORM,
            pipe::BIND_VERTEX_BUFFER,
            bytes,
            1,
        )
    }

    fn upload(
        &mut self,
        resource: u32,
        region: Region,
        stride: u32,
        data: &[u8],
    ) -> io::Result<()> {
        // The socket carries exactly the bytes the region covers: whole
        // strides for every row but the last, and that one's pixels.
        let rows = region.height.saturating_sub(1) as usize;
        let needed = rows * stride as usize + region.width as usize * 4;
        let data = data
            .get(..needed)
            .ok_or_else(|| io::Error::other("pixels too short for their region"))?;
        self.put(resource, region, stride, data)
    }

    fn submit(&mut self, words: &[u32]) -> io::Result<()> {
        Self::submit(self, words)
    }

    fn read(&mut self, resource: u32, region: Region) -> io::Result<Vec<u8>> {
        self.get(resource, region)
    }

    fn read_into(
        &mut self,
        resource: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        self.get_into(resource, region, into, stride)
    }

    fn release(&mut self, resource: u32) -> io::Result<()> {
        // The server may still be reading an upload from the memory; the
        // mapping goes only once it has.
        if self.unread.contains(&resource) {
            self.sync()?;
        }
        let _ = self.shared.remove(&resource);
        self.header(1, RESOURCE_UNREF)?;
        self.words(&[resource])
    }
}

/// `pub(crate)` for the tests: the memory checks without a server.
#[cfg(test)]
pub(crate) fn adopt_for_test(
    fd: std::os::fd::BorrowedFd<'_>,
    width: u32,
    height: u32,
) -> io::Result<bool> {
    let owned = fd.try_clone_to_owned()?;
    Ok(Shared::adopt(&owned, width, height)?.is_some())
}
