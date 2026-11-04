//! Classes: how a driver's device becomes visible to programs
//! (`docs/DRIVER-MODEL.md` §6). This slice carries the character-device
//! class's shape -- [`CharacterDevice`], [`UserMemory`] and its reader and
//! writer, [`CommandTable`] -- as the design page draws it, and stubs what
//! needs `CHARDEV.md`'s grant to a driver, which devmgr does not give a
//! generic node's driver yet: [`ClassPublisher`] answers `ENOSYS`, and so does
//! every access to a caller's memory.

use core::ops::BitOr;

use crate::model::{Errno, Flow, Result};

/// Whether a file can be read or written without waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Readiness(u8);

impl Readiness {
    /// Nothing ready.
    pub const NONE: Readiness = Readiness(0);
    /// A read will not wait.
    pub const READABLE: Readiness = Readiness(1);
    /// A write will not wait.
    pub const WRITABLE: Readiness = Readiness(2);

    /// Whether every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Readiness) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Readiness {
    type Output = Readiness;

    fn bitor(self, other: Readiness) -> Readiness {
        Readiness(self.0 | other.0)
    }
}

/// An open file of the device.
#[derive(Debug, Default)]
pub struct File {
    /// The file's number among the device's open files.
    pub number: u32,
}

/// Which way a followed pointer's memory moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the caller to the driver.
    FromCaller,
    /// From the driver to the caller.
    ToCaller,
}

/// A pointer and its length inside a caller's record, for
/// [`UserMemory::follow_pointer`]: the bytes from the record's start to each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerField {
    /// Where the pointer is.
    pub pointer: usize,
    /// Where its length is, a 32-bit count of bytes.
    pub length: usize,
}

/// A value that is its bytes: one a driver may read out of a caller's memory.
pub trait Plain: Copy {
    /// The value `bytes` hold, native-endian, as many as it takes.
    fn from_bytes(bytes: &[u8]) -> Option<Self>;
}

macro_rules! plain {
    ($($kind:ty),*) => {$(
        impl Plain for $kind {
            fn from_bytes(bytes: &[u8]) -> Option<Self> {
                Some(<$kind>::from_ne_bytes(bytes.get(..size_of::<$kind>())?.try_into().ok()?))
            }
        }
    )*};
}

plain!(u8, u16, u32, u64, i32, i64);

/// The window of a caller's memory an ioctl's argument names (tier 0), and
/// the pointers inside it the kernel follows (tier 1, `CHARDEV.md`). A stub
/// until the class is granted: it holds no bytes.
#[derive(Debug, Default)]
pub struct UserMemory {
    length: usize,
}

impl UserMemory {
    /// Bytes in the window.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Whether the window is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// The value at `offset`.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until the class is granted.
    pub fn read_value<T: Plain>(&self, _offset: usize) -> Result<T> {
        Err(Errno::ENOSYS)
    }

    /// Read `buffer.len()` bytes at `offset`.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until the class is granted.
    pub fn read_at(&self, _offset: usize, _buffer: &mut [u8]) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// Write `buffer` at `offset`.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until the class is granted.
    pub fn write_at(&self, _offset: usize, _buffer: &[u8]) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// The memory the pointer at `field` of the record at `at` names: the
    /// kernel reads the pointer and its length from the caller's memory
    /// itself, so the driver reaches only what the caller pointed to.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until the class is granted.
    pub fn follow_pointer(
        &self,
        _field: PointerField,
        _at: usize,
        _direction: Direction,
    ) -> Result<UserMemory> {
        Err(Errno::ENOSYS)
    }
}

/// The caller's buffer of a `write`, to read from.
#[derive(Debug, Default)]
pub struct UserMemoryReader {
    length: usize,
}

impl UserMemoryReader {
    /// Bytes the caller wrote.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Whether it is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Copy the caller's bytes into `buffer`: how many.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until the class is granted.
    pub fn read(&self, _buffer: &mut [u8]) -> Result<usize> {
        Err(Errno::ENOSYS)
    }
}

/// The caller's buffer of a `read`, to write to.
#[derive(Debug, Default)]
pub struct UserMemoryWriter {
    length: usize,
}

impl UserMemoryWriter {
    /// Bytes the caller has room for.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Whether there is no room.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Copy `bytes` to the caller: how many.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until the class is granted.
    pub fn write(&mut self, _bytes: &[u8]) -> Result<usize> {
        Err(Errno::ENOSYS)
    }
}

/// The ioctls whose size their command does not encode, and how big each
/// one's argument is: `CHARDEV.md`'s HELLO v3 table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommandTable {
    commands: &'static [(u32, u32)],
}

impl CommandTable {
    /// No command needs an entry.
    #[must_use]
    pub const fn empty() -> Self {
        CommandTable { commands: &[] }
    }

    /// Each `(command, argument bytes)`.
    #[must_use]
    pub const fn of(commands: &'static [(u32, u32)]) -> Self {
        CommandTable { commands }
    }

    /// The entries.
    #[must_use]
    pub const fn commands(&self) -> &'static [(u32, u32)] {
        self.commands
    }
}

/// A character device's driver: what each file operation does.
pub trait CharacterDevice {
    /// The ioctls that need a table entry.
    fn commands(&self) -> CommandTable {
        CommandTable::empty()
    }

    /// A program opened the device.
    ///
    /// # Errors
    ///
    /// The errno the open answers.
    fn open(&mut self, _file: &mut File) -> Result<Readiness> {
        Ok(Readiness::READABLE)
    }

    /// A read: `EINVAL` unless the driver says otherwise.
    ///
    /// # Errors
    ///
    /// The errno the read answers.
    fn read(&mut self, _file: &mut File, _output: UserMemoryWriter, _offset: u64) -> Result<usize> {
        Err(Errno::EINVAL)
    }

    /// A write: `EINVAL` unless the driver says otherwise.
    ///
    /// # Errors
    ///
    /// The errno the write answers.
    fn write(&mut self, _file: &mut File, _input: UserMemoryReader, _offset: u64) -> Result<usize> {
        Err(Errno::EINVAL)
    }

    /// An ioctl: `ENOTTY` unless the driver says otherwise.
    ///
    /// # Errors
    ///
    /// The errno the ioctl answers.
    fn ioctl(&mut self, _file: &mut File, _command: u32, _argument: UserMemory) -> Result<i64> {
        Err(Errno::ENOTTY)
    }
}

/// One request on a driver's character device. A stub until the class is
/// granted: none arrives.
#[derive(Debug)]
pub struct Request(());

impl Request {
    /// Serve it with `device`'s operations.
    pub fn serve<D: CharacterDevice>(self, _device: &mut D) -> Flow {
        Flow::Continue
    }
}

/// Where a driver publishes its device to programs: through the grant
/// devmgr gave it, under the name devmgr chose. A stub until devmgr grants a
/// generic node's driver a class (`CHARDEV.md`).
#[derive(Debug, Default)]
pub struct ClassPublisher(());

impl ClassPublisher {
    /// Publish `device` as the character device devmgr's grant names.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until devmgr gives the grant.
    pub fn character_device_granted<D: CharacterDevice>(&self, _device: &D) -> Result<()> {
        Err(Errno::ENOSYS)
    }
}
