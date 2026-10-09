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
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
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
const GET_PARAM: u32 = 15;

/// Ferrix's own command, in the server tools/common/fetch/fetch-virgl-server.sh
/// builds (its patch's `vtest_protocol.h`): a texture whose storage is a
/// dmabuf passed with it. A stock server ends the connection on a command
/// it does not know, so it is sent only after [`PARAM_FERRIX_IMPORT_FD`]
/// said it may be.
const FERRIX_RESOURCE_IMPORT_FD: u32 = 64;
/// The `VCMD_GET_PARAM` a patched server answers valid; a stock one answers
/// any parameter it does not know "not valid".
const PARAM_FERRIX_IMPORT_FD: u32 = 0x4658_0001;

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
            // A file this process may not map writable -- one opened for
            // reading only, or sealed against writes -- is memory the
            // texture does without, like an unsealed one.
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::EACCES | libc::EPERM) => Ok(None),
                _ => Err(error),
            };
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
        // owns, of a file sealed against shrinking, so every page is there.
        // That the server writes it only inside a transfer this program
        // waits for is an ASSUMPTION about a cooperating server, not
        // something this process can enforce. A server that writes at
        // other times changes only pixel values: no length, index or branch
        // here is ever taken from the mapped bytes, which are only copied.
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

/// Receive one message of one byte from `socket` and answer every
/// descriptor that came with it, each owned, so that one this program did
/// not ask for is closed when it is dropped. A message whose descriptors did
/// not all fit (`MSG_CTRUNC`) is an error, after those that did fit are
/// closed.
fn receive_fds(socket: std::os::fd::RawFd) -> io::Result<Vec<OwnedFd>> {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&raw mut byte).cast(),
        iov_len: 1,
    };
    // Room for a few descriptors' control messages, aligned as one.
    let mut control = [0_u64; 8];
    // SAFETY: a zeroed `msghdr` is a valid value: null pointers and zero
    // lengths, and musl's padding fields zero as they must be.
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
    // SAFETY: recvmsg writes at most the one byte and the control buffer
    // described above, both alive for the call.
    let got = unsafe { libc::recvmsg(socket, &raw mut message, libc::MSG_CMSG_CLOEXEC) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: the message recvmsg filled in, with its control buffer.
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&raw const message) };
    // SAFETY: a pure computation of a length from a constant.
    let empty = unsafe { libc::CMSG_LEN(0) } as usize;
    while !header.is_null() {
        // SAFETY: a header CMSG_FIRSTHDR or CMSG_NXTHDR found inside the
        // control buffer.
        let cmsg = unsafe { header.read_unaligned() };
        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
            let count = (cmsg.cmsg_len as usize).saturating_sub(empty) / size_of::<libc::c_int>();
            // SAFETY: the data of that control message, inside the buffer.
            let data = unsafe { libc::CMSG_DATA(header) }.cast::<libc::c_int>();
            for index in 0..count {
                // SAFETY: the `index`th of the `count` descriptors the
                // message's length says it holds, inside the control buffer;
                // they may be unaligned in it.
                let at = data.wrapping_add(index);
                // SAFETY: as above, one descriptor.
                let raw = unsafe { at.read_unaligned() };
                if raw >= 0 {
                    // SAFETY: SCM_RIGHTS installed the descriptor in this
                    // process just now, and nothing else owns it.
                    fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
                }
            }
        }
        // SAFETY: the next header of the same message, or null.
        header = unsafe { libc::CMSG_NXTHDR(&raw const message, header) };
    }
    if got == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other(
            "the server sent more descriptors than fit",
        ));
    }
    Ok(fds)
}

/// One byte from `socket` and exactly one descriptor beside it, as the
/// server answers `VCMD_RESOURCE_CREATE2`. Anything else is refused, and
/// every descriptor that came is closed first (`tests/fds.rs`).
///
/// # Errors
///
/// The socket's; no descriptor, or more than one.
pub fn receive_one_fd(socket: std::os::fd::RawFd) -> io::Result<OwnedFd> {
    let mut fds = receive_fds(socket)?;
    match (fds.pop(), fds.is_empty()) {
        (Some(fd), true) => Ok(fd),
        (None, _) => Err(io::Error::other(
            "the server's answer carried no descriptor",
        )),
        (Some(_), false) => Err(io::Error::other(
            "the server sent more descriptors than one",
        )),
    }
}

/// Send one byte on `socket` with `fd` beside it (`SCM_RIGHTS`), as the
/// server's `vtest_receive_fd` takes one.
fn send_fd(socket: std::os::fd::RawFd, fd: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&raw mut byte).cast(),
        iov_len: 1,
    };
    let mut control = [0_u64; 4];
    // SAFETY: a zeroed `msghdr` is a valid value, as in `receive_fds`.
    let mut message: libc::msghdr = unsafe { core::mem::zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // SAFETY: a pure computation of a length from a constant.
    let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) };
    #[allow(
        clippy::useless_conversion,
        reason = "a usize on glibc and a socklen_t on musl"
    )]
    {
        message.msg_controllen = (space as usize).try_into().unwrap_or(0);
    }
    // SAFETY: the first header of the control buffer set up above, which
    // has room for it.
    let header = unsafe { libc::CMSG_FIRSTHDR(&raw const message) };
    if header.is_null() {
        return Err(io::Error::other("no room for a descriptor's message"));
    }
    // SAFETY: a pure computation of a length from a constant.
    let len = unsafe { libc::CMSG_LEN(size_of::<libc::c_int>() as u32) };
    // A zeroed header first: musl's has a padding field of its own.
    let mut cmsg = zeroed_cmsghdr();
    #[allow(
        clippy::useless_conversion,
        reason = "its type differs between C libraries"
    )]
    {
        cmsg.cmsg_len = (len as usize).try_into().unwrap_or(0);
    }
    cmsg.cmsg_level = libc::SOL_SOCKET;
    cmsg.cmsg_type = libc::SCM_RIGHTS;
    // SAFETY: the header's place inside the control buffer.
    unsafe { header.write_unaligned(cmsg) };
    // SAFETY: the data of that header, inside the buffer.
    let data = unsafe { libc::CMSG_DATA(header) }.cast::<libc::c_int>();
    // SAFETY: room for one descriptor there, which CMSG_SPACE counted.
    unsafe { data.write_unaligned(fd.as_raw_fd()) };
    // SAFETY: sendmsg reads the byte and the control buffer, both alive.
    let sent = unsafe { libc::sendmsg(socket, &raw const message, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A zeroed `cmsghdr`, for its padding fields on musl.
fn zeroed_cmsghdr() -> libc::cmsghdr {
    // SAFETY: all of its fields are integers, for which zero is valid.
    unsafe { core::mem::zeroed() }
}

/// The protocol a new connection asks for: [`SHARED_MEMORY`] unless the
/// environment says `0`.
fn wanted_protocol() -> u32 {
    match std::env::var(PROTOCOL_ENV) {
        Ok(value) if value.trim() == "0" => 0,
        _ => SHARED_MEMORY,
    }
}

/// A buffer made outside the server -- by the GPU's own driver, where a
/// display engine can scan it out -- for a frame to be drawn into.
#[derive(Debug)]
pub struct Scanout {
    /// The dmabuf.
    pub fd: OwnedFd,
    /// Bytes from one row to the next.
    pub stride: u32,
    /// Where the first pixel is in the dmabuf.
    pub offset: u32,
    /// Its DRM format modifier (`0` for linear).
    pub modifier: u64,
    /// Whatever must live as long as the buffer: the driver's handle, say.
    pub keep: Box<dyn core::fmt::Debug>,
}

/// Where [`Scanout`] buffers come from: the compositor's, which knows the
/// card.
pub trait ScanoutSource: core::fmt::Debug {
    /// A buffer for a `width` x `height` frame, four bytes a pixel.
    ///
    /// # Errors
    ///
    /// The driver's; the frame is then drawn into a texture of the server's
    /// own, and fetched.
    fn allocate(&mut self, width: u32, height: u32) -> io::Result<Scanout>;

    /// What became of a frame buffer: drawn into one of [`Self::allocate`]'s,
    /// or why not. For the compositor's log; the default says nothing.
    fn note(&mut self, line: String) {
        let _ = line;
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
    /// Where a frame's buffers come from, if anywhere.
    scanouts: Option<Box<dyn ScanoutSource>>,
    /// Whether the server takes [`FERRIX_RESOURCE_IMPORT_FD`], once asked.
    importable: Option<bool>,
    /// The textures whose storage is a [`Scanout`], with it.
    imported: HashMap<u32, Scanout>,
    /// Reads of imported textures so far, which picks the ones checked
    /// against the buffer's own memory ([`Vtest::check_in_place`]).
    imported_reads: u64,
    /// Why the last frame buffer was not imported, for the compositor to say.
    import_failure: Option<String>,
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
        Self::start_program(std::path::Path::new(SERVER), name, version)
    }

    /// [`Vtest::start_with`], with the server at `program` rather than the
    /// one on `PATH`: how a test runs Ferrix's patched build beside the
    /// host's stock one.
    ///
    /// # Errors
    ///
    /// As [`Vtest::start`]'s.
    pub fn start_program(
        program: &std::path::Path,
        name: &str,
        version: u32,
    ) -> io::Result<Option<Self>> {
        let socket =
            std::env::temp_dir().join(format!("ferrix-vtest-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let spawned = Command::new(program)
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
            scanouts: None,
            importable: None,
            imported: HashMap::new(),
            imported_reads: 0,
            import_failure: None,
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

    /// The descriptor that comes with `VCMD_RESOURCE_CREATE2`'s answer.
    fn receive_fd(&mut self) -> io::Result<OwnedFd> {
        receive_one_fd(self.stream.as_raw_fd())
    }

    /// Draw frames -- textures a screen may be shown -- into buffers from
    /// `source` where the server can take them ([`Vtest::can_import`]):
    /// what [`Device::export`] then answers, for a card to show without a
    /// copy. Where either cannot, a frame is a texture of the server's own,
    /// as before.
    pub fn set_scanout_source(&mut self, source: Box<dyn ScanoutSource>) {
        self.scanouts = Some(source);
    }

    /// Why the last frame was not drawn into a [`Scanout`], once.
    pub fn take_import_failure(&mut self) -> Option<String> {
        self.import_failure.take()
    }

    /// How many textures are imported buffers.
    #[must_use]
    pub fn imported_textures(&self) -> usize {
        self.imported.len()
    }

    /// Whether the server takes dmabufs (Ferrix's patched server): asked
    /// once with `VCMD_GET_PARAM`, which a stock server answers "not valid"
    /// and keeps the connection.
    ///
    /// # Errors
    ///
    /// The socket's.
    pub fn can_import(&mut self) -> io::Result<bool> {
        if let Some(known) = self.importable {
            return Ok(known);
        }
        self.header(1, GET_PARAM)?;
        self.words(&[PARAM_FERRIX_IMPORT_FD])?;
        let [len, command, valid, value] = self.reply_words::<4>()?;
        if len != 2 || command != GET_PARAM {
            return Err(io::Error::other("the server's parameter answer is not one"));
        }
        let known = valid != 0 && value >= 1;
        self.importable = Some(known);
        Ok(known)
    }

    /// Make a `width` x `height` texture whose storage is the dmabuf `fd`,
    /// rows `stride` apart from `offset`. With protocol 2 and a four-byte
    /// format it is given shared memory for transfers too, as a texture of
    /// [`Vtest::create_shared`]'s is, so a frame drawn into it can still be
    /// fetched.
    ///
    /// # Errors
    ///
    /// The socket's; a server without the command (`EOPNOTSUPP`, nothing
    /// sent); the server's refusal, as its errno (the connection stays
    /// usable).
    #[expect(clippy::too_many_arguments, reason = "the command's own fields")]
    pub fn import(
        &mut self,
        fd: std::os::fd::BorrowedFd<'_>,
        format: u32,
        bind: u32,
        (width, height): (u32, u32),
        stride: u32,
        offset: u32,
        modifier: u64,
    ) -> io::Result<u32> {
        if !self.can_import()? {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        let shared = self.version >= SHARED_MEMORY && four_bytes(format);
        let size = if shared {
            width
                .checked_mul(height)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?
        } else {
            0
        };
        let resource = self.next;
        self.next += 1;
        self.header(10, FERRIX_RESOURCE_IMPORT_FD)?;
        #[expect(clippy::cast_possible_truncation, reason = "the modifier's two halves")]
        self.words(&[
            resource,
            format,
            bind,
            width,
            height,
            stride,
            offset,
            modifier as u32,
            (modifier >> 32) as u32,
            size,
        ])?;
        send_fd(self.stream.as_raw_fd(), fd)?;
        let [len, command] = self.reply_words::<2>()?;
        if command != FERRIX_RESOURCE_IMPORT_FD || !(1..=2).contains(&len) {
            return Err(io::Error::other("the server's import answer is not one"));
        }
        let mut status = [0_u32; 2];
        for word in status.iter_mut().take(len as usize) {
            *word = self.reply_words::<1>()?[0];
        }
        if status[0] != 0 {
            let errno = i32::try_from(status[0]).unwrap_or(libc::EIO);
            return Err(io::Error::from_raw_os_error(errno));
        }
        if size != 0 {
            let memory = self.receive_fd()?;
            match Shared::adopt(&memory, width, height) {
                Ok(Some(shared)) => {
                    let _ = self.shared.insert(resource, shared);
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = Device::release(self, resource);
                    return Err(error);
                }
            }
        }
        Ok(resource)
    }

    /// A frame texture drawn into a buffer from the scanout source, or the
    /// reason it is not.
    fn import_scanout(&mut self, texture: Texture) -> Result<u32, String> {
        if !self.can_import().map_err(|error| error.to_string())? {
            return Err("the test server takes no dmabufs (a stock virgl_test_server)".to_owned());
        }
        let source = self
            .scanouts
            .as_mut()
            .ok_or_else(|| "no scanout source".to_owned())?;
        let scanout = source
            .allocate(texture.width, texture.height)
            .map_err(|error| format!("the driver gave no buffer: {error}"))?;
        let resource = self
            .import(
                scanout.fd.as_fd(),
                texture.format,
                texture.bind,
                (texture.width, texture.height),
                scanout.stride,
                scanout.offset,
                scanout.modifier,
            )
            .map_err(|error| format!("the server would not import the buffer: {error}"))?;
        let _ = self.imported.insert(resource, scanout);
        Ok(resource)
    }

    /// Count a read of an imported texture, and early and once a while
    /// later say whether the driver draws into the buffer it was given or
    /// into a copy of its own.
    fn after_imported_read(&mut self, resource: u32, region: Region, read: &[u8], stride: usize) {
        self.imported_reads += 1;
        if !matches!(self.imported_reads, 60 | 3600) {
            return;
        }
        let line = match self.check_in_place(resource, region, read, stride) {
            Ok((same, all)) => format!(
                "the driver's buffer holds {same} of {all} sampled pixels of the frame read back ({})",
                if same == all {
                    "drawn in place"
                } else {
                    "drawn into a copy"
                }
            ),
            Err(error) => format!("the driver's buffer could not be looked at: {error}"),
        };
        if let Some(source) = self.scanouts.as_mut() {
            source.note(line);
        }
    }

    /// Compare `region` of an imported texture, as just read into `read`
    /// (rows `stride` apart), with the same pixels in the imported buffer's
    /// own memory, mapped read-only for the look: every 97th pixel of
    /// every 7th row, the low three bytes (an X channel may be anything).
    /// Answers how many matched of how many were looked at.
    fn check_in_place(
        &self,
        resource: u32,
        region: Region,
        read: &[u8],
        stride: usize,
    ) -> io::Result<(usize, usize)> {
        let scanout = self
            .imported
            .get(&resource)
            .ok_or_else(|| io::Error::other("not an imported texture"))?;
        let raw = scanout.fd.as_raw_fd();
        // SAFETY: lseek on a descriptor this object holds; it moves only the
        // descriptor's offset, which nothing reads.
        let end = unsafe { libc::lseek(raw, 0, libc::SEEK_END) };
        let len = usize::try_from(end).map_err(|_| io::Error::last_os_error())?;
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: a fresh shared read-only mapping the kernel places, of
        // the buffer's own size, unmapped below.
        let at = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mut copy = vec![0_u8; len];
        // SAFETY: `len` readable bytes at `at`, the mapping just made, into
        // a vector of that length; the two do not overlap.
        unsafe { core::ptr::copy_nonoverlapping(at.cast::<u8>(), copy.as_mut_ptr(), len) };
        // SAFETY: exactly the range mmap answered, unmapped once.
        let _ = unsafe { libc::munmap(at, len) };
        let mut matches = Vec::new();
        for row in (0..region.height as usize).step_by(7) {
            for column in (0..region.width as usize).step_by(97) {
                let theirs = (region.y as usize + row)
                    .checked_mul(scanout.stride as usize)
                    .and_then(|start| {
                        start
                            .checked_add(scanout.offset as usize + (region.x as usize + column) * 4)
                    })
                    .and_then(|start| copy.get(start..start + 3));
                let at = row * stride + column * 4;
                let ours = read.get(at..at + 3);
                if let (Some(theirs), Some(ours)) = (theirs, ours) {
                    matches.push(theirs == ours);
                }
            }
        }
        let same = matches.iter().filter(|&&same| same).count();
        let all = matches.len();
        Ok((same, all))
    }

    /// Read a reply of `N` words.
    fn reply_words<const N: usize>(&mut self) -> io::Result<[u32; N]> {
        let mut words = [0_u32; N];
        let mut bytes = [0_u8; 4];
        for word in &mut words {
            self.stream.read_exact(&mut bytes)?;
            *word = u32::from_le_bytes(bytes);
        }
        Ok(words)
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
        match Shared::adopt(&fd, width, height) {
            Ok(Some(shared)) => {
                let _ = self.shared.insert(resource, shared);
            }
            Ok(None) => {}
            Err(error) => {
                // The server made the resource; it goes again with the error.
                let _ = Device::release(self, resource);
                return Err(error);
            }
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
            self.get_shared(resource, region, into, stride)?;
            if self.imported.contains_key(&resource) {
                self.after_imported_read(resource, region, into, stride);
            }
            return Ok(());
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
        if texture.scanout && self.scanouts.is_some() {
            match self.import_scanout(texture) {
                Ok(resource) => {
                    if let Some(source) = self.scanouts.as_mut() {
                        source.note(format!(
                            "a {}x{} frame is drawn into the driver's own buffer",
                            texture.width, texture.height
                        ));
                    }
                    return Ok(resource);
                }
                Err(why) => {
                    if let Some(source) = self.scanouts.as_mut() {
                        source.note(format!(
                            "a {}x{} frame is drawn into the server's texture: {why}",
                            texture.width, texture.height
                        ));
                    }
                    self.import_failure = Some(why);
                }
            }
        }
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
        self.words(&[resource])?;
        // The buffer goes once the server has let go of it.
        if self.imported.contains_key(&resource) {
            self.sync()?;
            let _ = self.imported.remove(&resource);
        }
        Ok(())
    }

    fn export(&mut self, resource: u32) -> io::Result<Option<OwnedFd>> {
        self.imported
            .get(&resource)
            .map(|scanout| scanout.fd.try_clone())
            .transpose()
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
