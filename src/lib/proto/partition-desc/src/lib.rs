//! `.ferrix/partitions`: the static description of a machine's safety
//! partitions (`docs/microkernel/BOOT-AUTHORITY.md` §5).
//!
//! Fixed-width and little-endian, with no pointers, so that a reader needs no
//! memory and walks it by bounded arithmetic. The kernel reads the magic, the
//! bounds and the device lists; the partition manager reads the rest. The file
//! is security-policy data: `gen-partitions.py` writes it, uncertified, and
//! `check-partitions.py`, separate code, reads the bytes in the built image and
//! refuses what is wrong. A malformed one stops the boot, since reading it as
//! empty would hand a safety device to `devmgr`.
//!
//! ```text
//! header, 16 bytes:  magic "FXPD", version u16 = 1, count u16,
//!                    max_processes u32, ready_millis u32 (0: the kernel's default)
//! count partitions, PARTITION_BYTES each:
//!   0    name          32 bytes, NUL-padded
//!   32   digest        32 bytes: the SHA-256 K14 measures
//!   64   image         64 bytes: its path in the archive, NUL-padded
//!   128  reserve_pages u64
//!   136  budget_ns     u64  (K9; zero until it lands)
//!   144  window_ns     u64  (K9; zero until it lands)
//!   152  restarts      u32  how many times it may be started again
//!   156  devices       u32  how many of the next MAX_DEVICES are in use
//!   160  role          u32  ROLE_PLAIN, or ROLE_FILES: the file server of its disks
//!   164  reserved      u32  zero
//!   168  MAX_DEVICES devices, DEVICE_BYTES each:
//!        0   kind      u32  KIND_PCI or KIND_TREE
//!        4   flags     u32  FLAG_NEEDS_CERTIFIED; a description adds it, never removes it
//!        8   bdf       u32  segment 31:16, bus 15:8, devfn 7:0
//!        12  ids       u32  vendor 15:0, device 31:16
//!        16  path      64 bytes, NUL-padded: the tree node's whole path
//!        80  compatible 32 bytes, NUL-padded
//! ```

#![no_std]

pub mod certified;
#[cfg(test)]
mod tests;

/// The file's first four bytes.
pub const MAGIC: [u8; 4] = *b"FXPD";
/// The one version.
pub const VERSION: u16 = 1;
/// Bytes of the header.
pub const HEADER_BYTES: usize = 16;
/// The most partitions a description names.
pub const MAX_PARTITIONS: usize = 16;
/// The most devices one partition holds.
pub const MAX_DEVICES: usize = 8;
/// Bytes of a device.
pub const DEVICE_BYTES: usize = 112;
/// Bytes of a partition.
pub const PARTITION_BYTES: usize = DEVICES_AT + MAX_DEVICES * DEVICE_BYTES;
/// Where a partition's devices start.
const DEVICES_AT: usize = 168;
/// The largest file.
pub const MAX_BYTES: usize = HEADER_BYTES + MAX_PARTITIONS * PARTITION_BYTES;
/// A partition with nothing more to be given.
pub const ROLE_PLAIN: u32 = 0;
/// A file server (`docs/microkernel/BTRFSD.md`): its start carries the block
/// cores' channel, its client's channel and its own space. At most one.
pub const ROLE_FILES: u32 = 1;
/// A further Linux partition (`docs/microkernel/M8.md` §3): the kernel starts it, as
/// it starts the first `linuxd`, with no cores, no file server and no console. Its
/// `reserve_pages` is its memory, in frames.
pub const ROLE_LINUX: u32 = 2;
/// A PCI function.
pub const KIND_PCI: u32 = 1;
/// A device-tree node.
pub const KIND_TREE: u32 = 2;
/// A driver of this device must pass K14's grant.
pub const FLAG_NEEDS_CERTIFIED: u32 = 1;

/// The handles of the manager's first message before its images and devices, as
/// `ferrix-native-abi`'s `first` has it.
const MANAGER_FIXED_HANDLES: usize = 3;
/// A channel message's most handles.
const MANAGER_MOST_HANDLES: usize = 64;

/// Why a file is no description.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Shorter than its header, or not the size its count says.
    Length,
    /// Not the magic.
    Magic,
    /// Another version.
    Version,
    /// More partitions than [`MAX_PARTITIONS`].
    TooMany,
    /// A reserved word is not zero.
    Reserved,
    /// A partition says more devices than [`MAX_DEVICES`].
    Devices,
    /// A device of a kind this version does not have, or a flag it does not.
    Device,
    /// A name, path or compatible that does not end in NUL padding.
    Text,
    /// A role this version does not have, or a second file server.
    Role,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .and_then(|four| <[u8; 4]>::try_from(four).ok())
        .map_or(0, u32::from_le_bytes)
}

fn long(bytes: &[u8], at: usize) -> u64 {
    bytes
        .get(at..at + 8)
        .and_then(|eight| <[u8; 8]>::try_from(eight).ok())
        .map_or(0, u64::from_le_bytes)
}

/// `field` up to its padding; `None` when bytes follow a NUL, or none ends it.
fn text(field: &[u8]) -> Option<&[u8]> {
    let end = field.iter().position(|&byte| byte == 0)?;
    let (name, padding) = field.split_at(end);
    padding.iter().all(|&byte| byte == 0).then_some(name)
}

/// A description, checked whole.
#[derive(Clone, Copy, Debug)]
pub struct Description<'a> {
    bytes: &'a [u8],
    count: usize,
    max_processes: u32,
    ready_millis: u32,
}

impl<'a> Description<'a> {
    /// Read `bytes`, every bound and every field's form checked once, here.
    ///
    /// # Errors
    ///
    /// [`Malformed`].
    pub fn parse(bytes: &'a [u8]) -> Result<Description<'a>, Malformed> {
        let header = bytes.get(..HEADER_BYTES).ok_or(Malformed::Length)?;
        if header.get(..4) != Some(&MAGIC[..]) {
            return Err(Malformed::Magic);
        }
        if word(header, 4) & 0xFFFF != u32::from(VERSION) {
            return Err(Malformed::Version);
        }
        let count = (word(header, 4) >> 16) as usize;
        if count > MAX_PARTITIONS {
            return Err(Malformed::TooMany);
        }
        if bytes.len() != HEADER_BYTES + count * PARTITION_BYTES {
            return Err(Malformed::Length);
        }
        let description = Description {
            bytes,
            count,
            max_processes: word(header, 8),
            ready_millis: word(header, 12),
        };
        let mut servers = 0;
        for index in 0..count {
            let partition = description.partition(index).ok_or(Malformed::Length)?;
            partition.check()?;
            servers += u32::from(partition.role() == ROLE_FILES);
        }
        if servers > 1 {
            return Err(Malformed::Role);
        }
        // Until a volume is bound by the description (BTRFSD.md, BD.3), no file server
        // beside a safety device: every disk devmgr opens would be its.
        if servers != 0
            && description
                .partitions()
                .any(|partition| partition.devices().next().is_some())
        {
            return Err(Malformed::Role);
        }
        let devices: usize = description
            .partitions()
            .map(|partition| partition.devices().count())
            .sum();
        if MANAGER_FIXED_HANDLES + 2 * servers as usize + count + devices > MANAGER_MOST_HANDLES {
            return Err(Malformed::TooMany);
        }
        // Nothing gives a further partition more than its memory, and it must say that.
        if description
            .partitions()
            .any(|partition| partition.role() == ROLE_LINUX && partition.reserve_pages() == 0)
        {
            return Err(Malformed::Role);
        }
        Ok(description)
    }

    /// How many partitions.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The boot's bound on processes; 0 for the kernel's default (K16.9).
    #[must_use]
    pub const fn max_processes(&self) -> u32 {
        self.max_processes
    }

    /// How long the manager has to say READY, in milliseconds; 0 for the kernel's
    /// default (BA.13).
    #[must_use]
    pub const fn ready_millis(&self) -> u32 {
        self.ready_millis
    }

    /// Partition `index`.
    #[must_use]
    pub fn partition(&self, index: usize) -> Option<Partition<'a>> {
        if index >= self.count {
            return None;
        }
        let at = HEADER_BYTES + index * PARTITION_BYTES;
        self.bytes
            .get(at..at + PARTITION_BYTES)
            .map(|bytes| Partition { bytes })
    }

    /// Every partition.
    pub fn partitions(&self) -> impl Iterator<Item = Partition<'a>> + use<'a> {
        let description = *self;
        (0..self.count).filter_map(move |index| description.partition(index))
    }
}

/// One partition.
#[derive(Clone, Copy, Debug)]
pub struct Partition<'a> {
    bytes: &'a [u8],
}

impl<'a> Partition<'a> {
    fn check(&self) -> Result<(), Malformed> {
        if text(self.bytes.get(..32).unwrap_or(&[])).is_none()
            || text(self.bytes.get(64..128).unwrap_or(&[])).is_none()
        {
            return Err(Malformed::Text);
        }
        if word(self.bytes, 156) as usize > MAX_DEVICES {
            return Err(Malformed::Devices);
        }
        if !matches!(self.role(), ROLE_PLAIN | ROLE_FILES | ROLE_LINUX) {
            return Err(Malformed::Role);
        }
        if word(self.bytes, 164) != 0 {
            return Err(Malformed::Reserved);
        }
        for device in self.devices() {
            if !matches!(device.kind(), KIND_PCI | KIND_TREE)
                || device.flags() & !FLAG_NEEDS_CERTIFIED != 0
            {
                return Err(Malformed::Device);
            }
            if text(device.bytes.get(16..80).unwrap_or(&[])).is_none()
                || text(device.bytes.get(80..112).unwrap_or(&[])).is_none()
            {
                return Err(Malformed::Text);
            }
        }
        Ok(())
    }

    /// Its name.
    #[must_use]
    pub fn name(&self) -> &'a [u8] {
        text(self.bytes.get(..32).unwrap_or(&[])).unwrap_or(&[])
    }

    /// The digest its image must have.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        self.bytes
            .get(32..64)
            .and_then(|digest| <[u8; 32]>::try_from(digest).ok())
            .unwrap_or([0; 32])
    }

    /// Its image's path in the archive.
    #[must_use]
    pub fn image(&self) -> &'a [u8] {
        text(self.bytes.get(64..128).unwrap_or(&[])).unwrap_or(&[])
    }

    /// The pages reserved for it (K12).
    #[must_use]
    pub fn reserve_pages(&self) -> u64 {
        long(self.bytes, 128)
    }

    /// Its budget (K9).
    #[must_use]
    pub fn budget_ns(&self) -> u64 {
        long(self.bytes, 136)
    }

    /// Its window (K9).
    #[must_use]
    pub fn window_ns(&self) -> u64 {
        long(self.bytes, 144)
    }

    /// How many times it may be started again.
    #[must_use]
    pub fn restarts(&self) -> u32 {
        word(self.bytes, 152)
    }

    /// What it is to the others: [`ROLE_PLAIN`] or [`ROLE_FILES`].
    #[must_use]
    pub fn role(&self) -> u32 {
        word(self.bytes, 160)
    }

    /// Its devices.
    pub fn devices(&self) -> impl Iterator<Item = Device<'a>> + use<'a> {
        let bytes = self.bytes;
        let count = (word(bytes, 156) as usize).min(MAX_DEVICES);
        (0..count).filter_map(move |index| {
            let at = DEVICES_AT + index * DEVICE_BYTES;
            bytes
                .get(at..at + DEVICE_BYTES)
                .map(|bytes| Device { bytes })
        })
    }
}

/// One device of a partition.
#[derive(Clone, Copy, Debug)]
pub struct Device<'a> {
    bytes: &'a [u8],
}

impl<'a> Device<'a> {
    /// [`KIND_PCI`] or [`KIND_TREE`].
    #[must_use]
    pub fn kind(&self) -> u32 {
        word(self.bytes, 0)
    }

    /// Its flags.
    #[must_use]
    pub fn flags(&self) -> u32 {
        word(self.bytes, 4)
    }

    /// A PCI function's address.
    #[must_use]
    pub fn bdf(&self) -> u32 {
        word(self.bytes, 8)
    }

    /// A PCI function's vendor, then device, in one word.
    #[must_use]
    pub fn ids(&self) -> u32 {
        word(self.bytes, 12)
    }

    /// A tree node's whole path.
    #[must_use]
    pub fn path(&self) -> &'a [u8] {
        text(self.bytes.get(16..80).unwrap_or(&[])).unwrap_or(&[])
    }

    /// A tree node's compatible.
    #[must_use]
    pub fn compatible(&self) -> &'a [u8] {
        text(self.bytes.get(80..112).unwrap_or(&[])).unwrap_or(&[])
    }
}
