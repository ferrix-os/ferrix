//! The connection to one running hyprix-gpu, which every screen's
//! [`super::Canvas`] draws through: the socket, the process, the arena and
//! the textures the helper holds.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use compositor_gpu_protocol::{self as proto, Message, Plane};
use compositor_render::{Rect, Surface};
use compositor_shm::Shared;

use super::conn::{Received, Socket};

/// How long a helper has to say it is ready: Vulkan's start, anv's
/// compiler included.
const READY_DEADLINE: Duration = Duration::from_secs(5);

/// How long the modifier lists that follow `Ready` are waited for.
const MODIFIERS_DEADLINE: Duration = Duration::from_millis(300);

/// The first arena's size: a 1920x1200 screen's pixels four times over.
/// A memfd's pages are made only when written.
const FIRST_ARENA: usize = 1920 * 1200 * 4 * 4;

/// What a texture holds, as the helper was told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// A client's dmabuf.
    Dmabuf,
    /// An image the arena fills.
    Pixels(u32, u32, u32),
}

/// One running helper.
#[derive(Debug)]
pub struct Link {
    socket: Socket,
    child: Option<Child>,
    /// Why the helper is no use any more, once it is not.
    dead: Option<String>,
    /// The device and the driver, as the helper named them.
    pub(crate) device: String,
    /// See [`Link::device`].
    pub(crate) driver: String,
    /// The modifiers a client's dmabuf of each fourcc is sampled with.
    pub(crate) modifiers: BTreeMap<u32, Vec<(u64, bool)>>,
    /// Every arena sent, oldest first; the last is written.
    arenas: Vec<(u32, Shared, usize)>,
    cursor: usize,
    textures: BTreeMap<u64, (Held, u64)>,
    frame: u64,
    transient: u64,
}

impl Link {
    /// Start `program` with the socket's other end as descriptor 3 and
    /// `node` as the render node to choose the device by, and wait for it
    /// to be ready.
    ///
    /// # Errors
    ///
    /// No such program, or a helper that did not say it was ready in time.
    pub fn spawn(program: &Path, node: Option<&str>) -> io::Result<Self> {
        use std::os::unix::process::CommandExt as _;

        let (socket, theirs) = Socket::pair()?;
        let raw = theirs.as_raw_fd();
        let mut command = Command::new(program);
        let _ = command
            .env("HYPRIX_GPU_FD", "3")
            .stdin(Stdio::null());
        if let Some(node) = node {
            let _ = command.env("HYPRIX_GPU_NODE", node);
        }
        let place = move || place_socket(raw);
        #[expect(
            unsafe_code,
            reason = "AUDIT: pre_exec's closure runs between fork and exec; place_socket calls only async-signal-safe functions"
        )]
        // SAFETY: the closure allocates nothing and calls only dup2 and fcntl.
        let _ = unsafe { command.pre_exec(place) };
        let child = command.spawn()?;
        drop(theirs);
        let mut link = Self::new(socket, Some(child));
        link.handshake()?;
        Ok(link)
    }

    /// A helper already at the other end of `socket`: the tests' fake.
    ///
    /// # Errors
    ///
    /// As [`Link::spawn`]'s.
    #[cfg(test)]
    pub(crate) fn over(socket: Socket) -> io::Result<Self> {
        let mut link = Self::new(socket, None);
        link.handshake()?;
        Ok(link)
    }

    fn new(socket: Socket, child: Option<Child>) -> Self {
        Self {
            socket,
            child,
            dead: None,
            device: String::new(),
            driver: String::new(),
            modifiers: BTreeMap::new(),
            arenas: Vec::new(),
            cursor: 0,
            textures: BTreeMap::new(),
            frame: 0,
            transient: 0,
        }
    }

    fn handshake(&mut self) -> io::Result<()> {
        self.send(
            &Message::Hello {
                version: proto::VERSION,
            },
            &[],
        )?;
        let deadline = Instant::now() + READY_DEADLINE;
        loop {
            let received = self
                .receive(deadline)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "hyprix-gpu was not ready in time"))?;
            match received.message {
                Message::Ready {
                    version,
                    device,
                    driver,
                    ..
                } => {
                    if version != proto::VERSION {
                        return Err(io::Error::other(format!(
                            "hyprix-gpu speaks version {version}, not {}",
                            proto::VERSION
                        )));
                    }
                    self.device = device;
                    self.driver = driver;
                    break;
                }
                Message::Failed { why, .. } => {
                    return Err(io::Error::other(format!("hyprix-gpu: {why}")));
                }
                _ => {}
            }
        }
        let deadline = Instant::now() + MODIFIERS_DEADLINE;
        while self.modifiers.len() < 2 {
            let Some(received) = self.receive(deadline)? else {
                break;
            };
            if let Message::Modifiers { fourcc, modifiers } = received.message {
                let _ = self.modifiers.insert(fourcc, modifiers);
            }
        }
        Ok(())
    }

    /// Why the helper is no use, once it is not.
    pub fn dead(&self) -> Option<&str> {
        self.dead.as_deref()
    }

    /// The helper is no use any more, for `why`.
    pub(crate) fn lose(&mut self, why: String) {
        if self.dead.is_none() {
            self.dead = Some(why);
        }
    }

    fn check(&self) -> io::Result<()> {
        match &self.dead {
            Some(why) => Err(io::Error::new(io::ErrorKind::BrokenPipe, why.clone())),
            None => Ok(()),
        }
    }

    /// Send `message`; any failure leaves the helper dead.
    pub(crate) fn send(&mut self, message: &Message, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        self.check()?;
        match self.socket.send(message, fds) {
            Ok(_) => Ok(()),
            Err(error) => {
                self.lose(format!("hyprix-gpu: {error}"));
                Err(error)
            }
        }
    }

    /// The next record before `deadline`; any failure but the time running
    /// out leaves the helper dead.
    pub(crate) fn receive(&mut self, deadline: Instant) -> io::Result<Option<Received>> {
        self.check()?;
        self.socket.receive(deadline).inspect_err(|error| {
            self.lose(format!("hyprix-gpu: {error}"));
        })
    }

    /// The next frame's number.
    pub(crate) fn next_frame(&mut self) -> u64 {
        self.frame = self.frame.wrapping_add(1);
        self.frame
    }

    /// A name for a texture of one frame.
    pub(crate) fn transient_name(&mut self, bit: u64) -> u64 {
        self.transient = self.transient.wrapping_add(1);
        bit | (self.transient & !bit)
    }

    /// Whether the helper samples a dmabuf of `fourcc` with `modifier`.
    pub(crate) fn samples(&self, fourcc: u32, modifier: u64) -> bool {
        self.modifiers
            .get(&fourcc)
            .is_some_and(|list| list.iter().any(|&(held, _)| held == modifier))
    }

    /// The texture `name` was sampled in this frame.
    pub(crate) fn used(&mut self, name: u64) {
        if let Some(held) = self.textures.get_mut(&name) {
            held.1 = self.frame;
        }
    }

    /// Have the helper sample `surface`'s dmabuf `fd` as texture `name`,
    /// once.
    pub(crate) fn import(
        &mut self,
        surface: &Surface<'_>,
        fd: BorrowedFd<'_>,
        name: u64,
        fourcc: u32,
    ) -> io::Result<()> {
        if self.textures.get(&name).is_some_and(|held| held.0 == Held::Dmabuf) {
            return Ok(());
        }
        let Some(on) = surface.device() else {
            return Err(io::Error::other("a surface with no dmabuf"));
        };
        self.send(
            &Message::ImportDmabuf {
                texture: name,
                width: on.width,
                height: on.height,
                fourcc,
                modifier: on.modifier,
                planes: vec![Plane {
                    stride: surface.stride(),
                    offset: on.offset,
                }],
            },
            &[fd.as_fd()],
        )?;
        let _ = self.textures.insert(name, (Held::Dmabuf, self.frame));
        Ok(())
    }

    /// Make texture `name` a `width` × `height` image of `fourcc` unless it
    /// is one: whether it was made, and so holds nothing yet.
    pub(crate) fn ensure_texture(
        &mut self,
        name: u64,
        width: u32,
        height: u32,
        fourcc: u32,
    ) -> io::Result<bool> {
        let wanted = Held::Pixels(width, height, fourcc);
        match self.textures.get(&name) {
            Some((held, _)) if *held == wanted => return Ok(false),
            Some(_) => self.forget(name),
            None => {}
        }
        self.send(
            &Message::CreateTexture {
                texture: name,
                width,
                height,
                fourcc,
            },
            &[],
        )?;
        let _ = self.textures.insert(name, (wanted, self.frame));
        Ok(true)
    }

    /// Tell the helper texture `name` is done with.
    pub(crate) fn forget(&mut self, name: u64) {
        if self.textures.remove(&name).is_some() {
            let _ = self.send(&Message::Forget { texture: name }, &[]);
        }
    }

    /// Forget every texture not sampled in the last `frames` frames.
    pub(crate) fn forget_unused(&mut self, frames: u64) {
        let now = self.frame;
        let stale: Vec<u64> = self
            .textures
            .iter()
            .filter(|(_, (_, last))| last.saturating_add(frames) < now)
            .map(|(&name, _)| name)
            .collect();
        for name in stale {
            self.forget(name);
        }
    }

    /// Start writing the arena from its beginning again, every older arena
    /// let go: the frame that read them is done.
    pub(crate) fn reset_arena(&mut self) {
        if self.arenas.len() > 1 {
            let newest = self.arenas.split_off(self.arenas.len() - 1);
            self.arenas = newest;
        }
        self.cursor = 0;
    }

    /// Room for `len` bytes in the arena: its number and the offset. A new,
    /// larger arena is made and sent when the one written has no room.
    pub(crate) fn reserve(&mut self, len: usize) -> io::Result<(u32, usize)> {
        let start = self.cursor.next_multiple_of(64);
        if let Some((id, _, size)) = self.arenas.last()
            && start.saturating_add(len) <= *size
        {
            self.cursor = start + len;
            return Ok((*id, start));
        }
        let size = self
            .arenas
            .last()
            .map_or(FIRST_ARENA, |(_, _, size)| size.saturating_mul(2))
            .max(len.saturating_mul(2));
        let id = self.arenas.last().map_or(1, |(id, _, _)| id.wrapping_add(1));
        let shared = Shared::new(size)?;
        let raw = shared.as_raw_fd();
        #[expect(
            unsafe_code,
            reason = "AUDIT: the descriptor is the Shared value's own, held across this call"
        )]
        // SAFETY: `shared` outlives the borrow, which ends with the send.
        let fd = unsafe { BorrowedFd::borrow_raw(raw) };
        self.send(
            &Message::Arena {
                arena: id,
                size: size as u64,
            },
            &[fd],
        )?;
        self.arenas.push((id, shared, size));
        self.cursor = len;
        Ok((id, 0))
    }

    /// `part` of `surface`'s pixels, copied into the arena: where, and the
    /// rows' stride.
    pub(crate) fn upload(&mut self, surface: &Surface<'_>, part: Rect) -> io::Result<(u32, usize, u32)> {
        let (Ok(x), Ok(y), Ok(width), Ok(height)) = (
            usize::try_from(part.x),
            usize::try_from(part.y),
            usize::try_from(part.width),
            usize::try_from(part.height),
        ) else {
            return Err(io::Error::other("a part outside the surface"));
        };
        let row = width * 4;
        let (arena, offset) = self.reserve(row * height)?;
        let stride = surface.stride() as usize;
        let data = surface.data();
        let Some((_, shared, _)) = self.arenas.iter_mut().find(|(id, _, _)| *id == arena) else {
            return Err(io::Error::other("no arena"));
        };
        let bytes = shared.bytes_mut();
        for line in 0..height {
            let from = (y + line) * stride + x * 4;
            let to = offset + line * row;
            let (Some(source), Some(target)) =
                (data.get(from..from + row), bytes.get_mut(to..to + row))
            else {
                return Err(io::Error::other("a surface shorter than its rows"));
            };
            target.copy_from_slice(source);
        }
        Ok((arena, offset, u32::try_from(row).unwrap_or(u32::MAX)))
    }

    /// `out.len()` bytes of arena `arena` from `offset`.
    pub(crate) fn copy_out(&mut self, arena: u32, offset: usize, out: &mut [u8]) {
        if let Some((_, shared, _)) = self.arenas.iter_mut().find(|(id, _, _)| *id == arena)
            && let Some(from) = shared.bytes_mut().get(offset..offset + out.len())
        {
            out.copy_from_slice(from);
        }
    }
}

/// Put the helper's end of the socket at descriptor 3 in the child, without
/// close-on-exec.
fn place_socket(raw: std::os::fd::RawFd) -> io::Result<()> {
    if raw == 3 {
        #[expect(
            unsafe_code,
            reason = "AUDIT: fcntl is async-signal-safe and only clears a descriptor flag in the child"
        )]
        // SAFETY: as the reason says.
        let cleared = unsafe { libc::fcntl(3, libc::F_SETFD, 0) };
        if cleared < 0 {
            return Err(io::Error::last_os_error());
        }
    } else {
        #[expect(
            unsafe_code,
            reason = "AUDIT: dup2 is async-signal-safe and only changes the child's descriptor table"
        )]
        // SAFETY: as the reason says.
        let placed = unsafe { libc::dup2(raw, 3) };
        if placed < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl Drop for Link {
    fn drop(&mut self) {
        // By its pid, never by name: it is this process's child.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
