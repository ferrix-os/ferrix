//! Inode numbers and listing cursors, made from names.
//!
//! sysfs stores nothing, so there is no table of numbers to hand out from.
//! A node's inode number is a hash of its path, the same every time the
//! same path is looked at; `find` and `du` need two names to have two
//! numbers, and a 63-bit hash gives the few thousand names a machine has
//! distinct ones with a margin no machine comes near.
//!
//! A directory lists its names in the order of a hash of each name, and a
//! listing resumes at the hash after the last one it gave, as kernfs lists
//! Linux's sysfs: a device appearing or going between two `getdents64` calls
//! then neither repeats nor hides any name that stayed.

/// FNV-1a's offset basis and prime, 64-bit.
const FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over `bytes`, continuing from `hash`.
const fn fnv(mut hash: u64, mut bytes: &[u8]) -> u64 {
    while let [first, rest @ ..] = bytes {
        hash ^= *first as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        bytes = rest;
    }
    hash
}

/// The inode number of the node at `path`, its components beneath the root:
/// never zero, and below 2⁶³ so that no program reading it as signed sees a
/// negative number. The root, with no components, is 1, as Linux numbers a
/// kernfs root.
#[must_use]
pub fn inode(path: &[&[u8]]) -> u64 {
    if path.is_empty() {
        return 1;
    }
    let hash = path
        .iter()
        .fold(FNV_BASIS, |hash, component| fnv(fnv(hash, b"/"), component));
    let number = hash >> 1;
    if number < 2 { number + 2 } else { number }
}

/// Where listings begin: cursors 0 and 1 are `.` and `..`, which the VFS
/// gives itself (`ferrix_vfs::FIRST_CURSOR`).
pub const FIRST_CURSOR: u64 = 2;

/// The cursor a name is listed at: its hash in 62 bits, above
/// [`FIRST_CURSOR`]. A listing gives its names in cursor order and resumes
/// at the cursor after the last one it gave.
#[must_use]
pub const fn cursor(name: &[u8]) -> u64 {
    (fnv(FNV_BASIS, name) >> 2) + FIRST_CURSOR
}
