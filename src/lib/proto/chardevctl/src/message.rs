//! The bytes on the control channel: each message a fixed little-endian
//! structure whose first byte is its kind, decoded strictly -- a length
//! other than the kind's, an unknown kind or operation, or a field out of
//! range is refused.

use crate::session::Refusal;

/// HELLO, the driver's first message.
pub const KIND_HELLO: u8 = 1;
/// READY, the kernel's answer to a HELLO it took.
pub const KIND_READY: u8 = 2;
/// REFUSED, the kernel's answer to one it did not.
pub const KIND_REFUSED: u8 = 3;
/// REQUEST, the kernel's for each open, ioctl and release.
pub const KIND_REQUEST: u8 = 4;

/// The protocol version a HELLO names.
pub const VERSION: u8 = 1;

/// The most minors one HELLO lists.
pub const MAX_NODES: usize = 8;

/// HELLO's bytes before its minors.
const HELLO_HEAD: usize = 8;
/// READY's and REFUSED's length.
const SHORT: usize = 4;
/// REQUEST's length.
pub const REQUEST_BYTES: usize = 48;

/// The longest message.
pub const MAX_BYTES: usize = REQUEST_BYTES;

/// What a REQUEST asks the driver to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// An open of a node: a new file.
    Open,
    /// An ioctl on a file: `cmd` and `arg` as the program passed them.
    Ioctl,
    /// The last close of a file. Answered by nobody.
    Release,
    /// An mmap of a file: `arg` the file offset, `cmd` the protection and
    /// flags (`PROT_*` and `MAP_*` << 8), `pages` its length in pages.
    /// Answered with one of the [`MAP_VMO`] or [`MAP_APERTURE`] kinds.
    Mmap,
}

/// A reply's kind for an [`Op::Mmap`]: the reply's value is a VMO handle in
/// the driver's table and its fifth register the byte offset in it.
pub const MAP_VMO: u64 = 1;
/// A reply's kind for an [`Op::Mmap`]: the reply's value is the physical
/// address of a range wholly inside one of the device's memory apertures.
pub const MAP_APERTURE: u64 = 2;
/// With [`MAP_APERTURE`]: write-combining rather than uncached, which only a
/// prefetchable aperture may be.
pub const MAP_WRITE_COMBINING: u64 = 1 << 8;
/// The bits of a reply's sixth register that name the kind.
pub const MAP_KIND: u64 = 0xff;

impl Op {
    const fn code(self) -> u8 {
        match self {
            Op::Open => 1,
            Op::Ioctl => 2,
            Op::Release => 3,
            Op::Mmap => 4,
        }
    }

    const fn from_code(code: u8) -> Option<Op> {
        match code {
            1 => Some(Op::Open),
            2 => Some(Op::Ioctl),
            3 => Some(Op::Release),
            4 => Some(Op::Mmap),
            _ => None,
        }
    }
}

/// The driver's HELLO: the device it drives, and the minors it serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hello {
    /// The protocol version it speaks.
    pub version: u8,
    /// The device's PCI location word, as `device_info` gave it.
    pub location: u32,
    /// How many of `minors` it lists.
    pub count: usize,
    /// The minors, in the order listed.
    pub minors: [u16; MAX_NODES],
}

/// One request, as the kernel writes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request {
    /// The request's id, which the reply and the copies name. Never reused
    /// on a control.
    pub id: u64,
    /// The file's identity: one per open, never reused on a control.
    pub file: u64,
    /// What is asked.
    pub op: Op,
    /// The node's minor.
    pub minor: u16,
    /// The caller's process id.
    pub pid: u32,
    /// The caller's effective user id.
    pub euid: u32,
    /// The caller's effective group id.
    pub egid: u32,
    /// The ioctl's command, or zero.
    pub cmd: u32,
    /// The ioctl's argument, raw, or zero; an mmap's file offset.
    pub arg: u64,
    /// An mmap's length in pages, or zero.
    pub pages: u32,
}

/// A message on the control channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    /// The driver's first.
    Hello(Hello),
    /// The kernel took the HELLO and published this many nodes.
    Ready(u8),
    /// The kernel did not.
    Refused(Refusal),
    /// A program's request.
    Request(Request),
}

/// Why bytes did not decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Malformed;

/// An encoded message: its bytes and their length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    bytes: [u8; MAX_BYTES],
    len: usize,
}

impl Encoded {
    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// A little-endian writer over a fixed buffer.
struct Writer {
    bytes: [u8; MAX_BYTES],
    at: usize,
}

impl Writer {
    const fn new() -> Self {
        Self {
            bytes: [0; MAX_BYTES],
            at: 0,
        }
    }

    fn put(&mut self, source: &[u8]) {
        if let Some(slot) = self.bytes.get_mut(self.at..self.at + source.len()) {
            slot.copy_from_slice(source);
        }
        self.at += source.len();
    }

    fn done(self) -> Encoded {
        Encoded {
            bytes: self.bytes,
            len: self.at.min(MAX_BYTES),
        }
    }
}

/// A little-endian reader.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let slice = self.bytes.get(self.at..self.at + N).ok_or(Malformed)?;
        self.at += N;
        let mut out = [0; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(u8::from_le_bytes(self.take()?))
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_le_bytes(self.take()?))
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_le_bytes(self.take()?))
    }
}

impl Message {
    /// The message's bytes.
    #[must_use]
    pub fn encode(&self) -> Encoded {
        let mut out = Writer::new();
        match *self {
            Message::Hello(hello) => {
                let count = hello.count.min(MAX_NODES);
                out.put(&[
                    KIND_HELLO,
                    hello.version,
                    u8::try_from(count).unwrap_or(0),
                    0,
                ]);
                out.put(&hello.location.to_le_bytes());
                for minor in hello.minors.iter().take(count) {
                    out.put(&minor.to_le_bytes());
                }
            }
            Message::Ready(count) => out.put(&[KIND_READY, count, 0, 0]),
            Message::Refused(refusal) => out.put(&[KIND_REFUSED, refusal.code(), 0, 0]),
            Message::Request(request) => {
                out.put(&[KIND_REQUEST, request.op.code()]);
                out.put(&request.minor.to_le_bytes());
                out.put(&request.pid.to_le_bytes());
                out.put(&request.euid.to_le_bytes());
                out.put(&request.egid.to_le_bytes());
                out.put(&request.id.to_le_bytes());
                out.put(&request.file.to_le_bytes());
                out.put(&request.cmd.to_le_bytes());
                out.put(&request.pages.to_le_bytes());
                out.put(&request.arg.to_le_bytes());
            }
        }
        out.done()
    }

    /// Decode `bytes`, strictly.
    ///
    /// # Errors
    ///
    /// [`Malformed`] for a length other than the kind's, an unknown kind,
    /// operation or refusal, a nonzero reserved byte, or a HELLO listing no
    /// minor or more than [`MAX_NODES`].
    pub fn decode(bytes: &[u8]) -> Result<Message, Malformed> {
        let mut read = Reader { bytes, at: 0 };
        let kind = read.u8()?;
        let message = match kind {
            KIND_HELLO => {
                let version = read.u8()?;
                let count = usize::from(read.u8()?);
                if read.u8()? != 0 || count == 0 || count > MAX_NODES {
                    return Err(Malformed);
                }
                if bytes.len() != HELLO_HEAD + 2 * count {
                    return Err(Malformed);
                }
                let location = read.u32()?;
                let mut minors = [0; MAX_NODES];
                for slot in minors.iter_mut().take(count) {
                    *slot = read.u16()?;
                }
                Message::Hello(Hello {
                    version,
                    location,
                    count,
                    minors,
                })
            }
            KIND_READY | KIND_REFUSED => {
                if bytes.len() != SHORT {
                    return Err(Malformed);
                }
                let value = read.u8()?;
                if read.u16()? != 0 {
                    return Err(Malformed);
                }
                if kind == KIND_READY {
                    Message::Ready(value)
                } else {
                    Message::Refused(Refusal::from_code(value).ok_or(Malformed)?)
                }
            }
            KIND_REQUEST => {
                if bytes.len() != REQUEST_BYTES {
                    return Err(Malformed);
                }
                let op = Op::from_code(read.u8()?).ok_or(Malformed)?;
                let minor = read.u16()?;
                let pid = read.u32()?;
                let euid = read.u32()?;
                let egid = read.u32()?;
                let id = read.u64()?;
                let file = read.u64()?;
                let cmd = read.u32()?;
                let pages = read.u32()?;
                if pages != 0 && op != Op::Mmap {
                    return Err(Malformed);
                }
                let arg = read.u64()?;
                Message::Request(Request {
                    id,
                    file,
                    op,
                    minor,
                    pid,
                    euid,
                    egid,
                    cmd,
                    arg,
                    pages,
                })
            }
            _ => return Err(Malformed),
        };
        Ok(message)
    }
}
