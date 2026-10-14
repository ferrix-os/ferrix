//! Structure layouts and flag constants of the Linux system call ABI.
//!
//! Every structure here is `#[repr(C)]` and matches, byte for byte, what the
//! kernel writes into a user buffer on a 64-bit target. The two architectures
//! agree on everything except `stat` and `epoll_event`, which are given per
//! architecture in [`x86_64`] and [`aarch64`]; the rest is shared, because the
//! generic UAPI headers are what both use.
//!
//! Sizes and field offsets are asserted in this crate's tests rather than
//! trusted. The failure mode of a wrong layout is not a compile error: it is a
//! program that reads a file size out of the padding, and there is nothing in
//! the resulting behaviour that points back at this file.

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// A time in seconds and nanoseconds, `struct timespec`.
///
/// Both fields are 64-bit on every target Ferrix supports, so there is no
/// 2038 problem and no compat variant to carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Timespec {
    /// Whole seconds.
    pub tv_sec: i64,
    /// Nanoseconds within the second, `0..1_000_000_000`.
    pub tv_nsec: i64,
}

/// A time in seconds and microseconds, `struct timeval`.
///
/// Used by `gettimeofday` and by the two fields of [`Rusage`]; new interfaces
/// use [`Timespec`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Timeval {
    /// Whole seconds.
    pub tv_sec: i64,
    /// Microseconds within the second, `0..1_000_000`.
    pub tv_usec: i64,
}

// ---------------------------------------------------------------------------
// Scatter/gather
// ---------------------------------------------------------------------------

/// One segment of a scatter/gather list, `struct iovec`.
///
/// `iov_base` is a `u64` rather than a pointer because it is a user address the
/// kernel has not validated; keeping it a plain integer means it cannot be
/// dereferenced by accident and this crate stays free of `unsafe`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Iovec {
    /// User address of the segment.
    pub iov_base: u64,
    /// Length of the segment in bytes.
    pub iov_len: u64,
}

// ---------------------------------------------------------------------------
// Per-architecture layouts
// ---------------------------------------------------------------------------

/// Layouts that are specific to x86-64.
pub mod x86_64 {
    /// File metadata as x86-64 lays it out, `struct stat` from
    /// `arch/x86/include/uapi/asm/stat.h`.
    ///
    /// 144 bytes, and *not* interchangeable with [`super::aarch64::Stat`]:
    /// x86-64 puts `st_nlink` before `st_mode` and pads differently. This is
    /// the layout `stat`, `lstat`, `fstat` and `newfstatat` fill in on x86-64.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[repr(C)]
    pub struct Stat {
        /// Identifier of the device holding the file.
        pub st_dev: u64,
        /// Inode number within that device.
        pub st_ino: u64,
        /// Number of hard links.
        pub st_nlink: u64,
        /// File type and permission bits; see the `S_IF*` constants.
        pub st_mode: u32,
        /// Owning user identifier.
        pub st_uid: u32,
        /// Owning group identifier.
        pub st_gid: u32,
        /// Padding the kernel writes as zero.
        pub __pad0: u32,
        /// Device this file represents, for block and character devices.
        pub st_rdev: u64,
        /// Size in bytes, or the link target length for a symbolic link.
        pub st_size: i64,
        /// Preferred block size for input and output.
        pub st_blksize: i64,
        /// Number of 512-byte blocks allocated.
        pub st_blocks: i64,
        /// Seconds of the last access time.
        pub st_atime: i64,
        /// Nanoseconds of the last access time.
        pub st_atime_nsec: i64,
        /// Seconds of the last modification time.
        pub st_mtime: i64,
        /// Nanoseconds of the last modification time.
        pub st_mtime_nsec: i64,
        /// Seconds of the last status change time.
        pub st_ctime: i64,
        /// Nanoseconds of the last status change time.
        pub st_ctime_nsec: i64,
        /// Reserved space, written as zero.
        pub __unused: [i64; 3],
    }

    /// One epoll registration, `struct epoll_event` as x86-64 defines it.
    ///
    /// x86-64 defines `EPOLL_PACKED`, so the structure is 12 bytes with
    /// `data` at offset 4 and no trailing padding. This exists so that a
    /// 32-bit x86 process sees the same layout as a 64-bit one; AArch64 never
    /// had that compatibility problem and uses the natural 16-byte layout in
    /// [`super::aarch64::EpollEvent`]. Getting this wrong misplaces the user
    /// cookie by four bytes on every event delivered.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[repr(C, packed)]
    pub struct EpollEvent {
        /// Requested or reported event mask; see the `EPOLL*` constants.
        pub events: u32,
        /// Opaque value the caller registered and the kernel hands back.
        pub data: u64,
    }
}

/// Layouts that are specific to AArch64.
pub mod aarch64 {
    /// File metadata as the generic UAPI lays it out, `struct stat` from
    /// `include/uapi/asm-generic/stat.h`.
    ///
    /// 128 bytes, sixteen fewer than [`super::x86_64::Stat`], with `st_mode`
    /// and `st_nlink` in the opposite order and `st_blksize` only 32 bits
    /// wide. AArch64 has no `stat` or `lstat` call; this is what `fstat` and
    /// `newfstatat` fill in.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[repr(C)]
    pub struct Stat {
        /// Identifier of the device holding the file.
        pub st_dev: u64,
        /// Inode number within that device.
        pub st_ino: u64,
        /// File type and permission bits; see the `S_IF*` constants.
        pub st_mode: u32,
        /// Number of hard links.
        pub st_nlink: u32,
        /// Owning user identifier.
        pub st_uid: u32,
        /// Owning group identifier.
        pub st_gid: u32,
        /// Device this file represents, for block and character devices.
        pub st_rdev: u64,
        /// Padding the kernel writes as zero.
        pub __pad1: u64,
        /// Size in bytes, or the link target length for a symbolic link.
        pub st_size: i64,
        /// Preferred block size for input and output.
        pub st_blksize: i32,
        /// Padding the kernel writes as zero.
        pub __pad2: i32,
        /// Number of 512-byte blocks allocated.
        pub st_blocks: i64,
        /// Seconds of the last access time.
        pub st_atime: i64,
        /// Nanoseconds of the last access time.
        pub st_atime_nsec: u64,
        /// Seconds of the last modification time.
        pub st_mtime: i64,
        /// Nanoseconds of the last modification time.
        pub st_mtime_nsec: u64,
        /// Seconds of the last status change time.
        pub st_ctime: i64,
        /// Nanoseconds of the last status change time.
        pub st_ctime_nsec: u64,
        /// Reserved space, written as zero.
        pub __unused: [u32; 2],
    }

    /// One epoll registration, `struct epoll_event` as AArch64 defines it.
    ///
    /// Unpacked, so `data` is naturally aligned at offset 8 and the structure
    /// is 16 bytes. Contrast [`super::x86_64::EpollEvent`], which is packed.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[repr(C)]
    pub struct EpollEvent {
        /// Requested or reported event mask; see the `EPOLL*` constants.
        pub events: u32,
        /// Opaque value the caller registered and the kernel hands back.
        pub data: u64,
    }
}

/// Layouts that are specific to ARMv7-A.
pub mod arm {
    /// File metadata as ARMv7-A lays it out for the `64` calls, `struct stat64`
    /// from `arch/arm/include/uapi/asm/stat.h`.
    ///
    /// 104 bytes. What `stat64`, `lstat64`, `fstat64` and `fstatat64` fill in;
    /// ARMv7-A has no `newfstatat`, because its plain `struct stat` cannot
    /// carry a 64-bit size. Three things about it are surprising, and each is
    /// a way to misread it:
    ///
    /// * **The inode number is there twice.** `__st_ino` near the top is the
    ///   32-bit number glibc 2.1 read, and the real one is `st_ino`, the last
    ///   field. A reader that takes the first gets a truncated inode, and
    ///   `find` and `du` then take distinct files for hard links.
    /// * **The header's padding is not all of it.** The EABI aligns a 64-bit
    ///   field to eight bytes, so after `__pad3` and after `st_blksize` the C
    ///   compiler inserts four bytes the header never names. They are named
    ///   here — `__pad4` and `__pad5` — so that the layout does not depend on
    ///   the host's alignment rules and so the kernel writes them as zero.
    ///   QEMU's `target_eabi_stat64`, which is packed, spells them the same
    ///   way.
    /// * **The times are 32 bits.** `unsigned long` is 32 bits here, so a
    ///   timestamp past 2106 cannot be reported through this structure;
    ///   `statx` is the call that can.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[repr(C)]
    pub struct Stat64 {
        /// Identifier of the device holding the file.
        pub st_dev: u64,
        /// Padding the kernel writes as zero.
        pub __pad0: [u8; 4],
        /// The low 32 bits of the inode number, for readers that predate
        /// [`Stat64::st_ino`].
        pub __st_ino: u32,
        /// File type and permission bits; see the `S_IF*` constants.
        pub st_mode: u32,
        /// Number of hard links.
        pub st_nlink: u32,
        /// Owning user identifier.
        pub st_uid: u32,
        /// Owning group identifier.
        pub st_gid: u32,
        /// Device this file represents, for block and character devices.
        pub st_rdev: u64,
        /// Padding the kernel writes as zero.
        pub __pad3: [u8; 4],
        /// The alignment padding the EABI inserts before `st_size`.
        pub __pad4: u32,
        /// Size in bytes, or the link target length for a symbolic link.
        pub st_size: i64,
        /// Preferred block size for input and output.
        pub st_blksize: u32,
        /// The alignment padding the EABI inserts before `st_blocks`.
        pub __pad5: u32,
        /// Number of 512-byte blocks allocated.
        pub st_blocks: u64,
        /// Seconds of the last access time.
        pub st_atime: u32,
        /// Nanoseconds of the last access time.
        pub st_atime_nsec: u32,
        /// Seconds of the last modification time.
        pub st_mtime: u32,
        /// Nanoseconds of the last modification time.
        pub st_mtime_nsec: u32,
        /// Seconds of the last status change time.
        pub st_ctime: u32,
        /// Nanoseconds of the last status change time.
        pub st_ctime_nsec: u32,
        /// The inode number, all 64 bits of it.
        pub st_ino: u64,
    }
}

/// Structures laid out for i386, as a 32-bit x86 program on the x86-64 kernel
/// passes them (`docs/I386.md`).
pub mod i386 {
    /// i386's `struct stat64`, from `arch/x86/include/uapi/asm/stat.h`: 96
    /// bytes, which old glibc's `__xstat64` and the `stat64` calls fill.
    ///
    /// Not ARMv7-A's [`super::arm::Stat64`], though the header reads the same:
    /// the i386 ABI aligns a `long long` to four bytes inside a structure, so
    /// `st_size` follows `__pad3` directly at 44 and nothing pads it, where the
    /// EABI puts it at 48. Packed to four here to say so on any host; Linux's
    /// compat `stat64` in `asm/ia32.h` is packed for the same reason. The
    /// times and `__st_ino` are 32 bits, as on ARMv7-A.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[repr(C, packed(4))]
    pub struct Stat64 {
        /// Identifier of the device holding the file.
        pub st_dev: u64,
        /// Padding the kernel writes as zero.
        pub __pad0: [u8; 4],
        /// The low 32 bits of the inode number, for readers that predate
        /// [`Stat64::st_ino`].
        pub __st_ino: u32,
        /// File type and permission bits; see the `S_IF*` constants.
        pub st_mode: u32,
        /// Number of hard links.
        pub st_nlink: u32,
        /// Owning user identifier.
        pub st_uid: u32,
        /// Owning group identifier.
        pub st_gid: u32,
        /// Device this file represents, for block and character devices.
        pub st_rdev: u64,
        /// Padding the kernel writes as zero.
        pub __pad3: [u8; 4],
        /// Size in bytes, or the link target length for a symbolic link.
        pub st_size: i64,
        /// Preferred block size for input and output.
        pub st_blksize: u32,
        /// Number of 512-byte blocks allocated.
        pub st_blocks: u64,
        /// Seconds of the last access time.
        pub st_atime: u32,
        /// Nanoseconds of the last access time.
        pub st_atime_nsec: u32,
        /// Seconds of the last modification time.
        pub st_mtime: u32,
        /// Nanoseconds of the last modification time.
        pub st_mtime_nsec: u32,
        /// Seconds of the last status change time.
        pub st_ctime: u32,
        /// Nanoseconds of the last status change time.
        pub st_ctime_nsec: u32,
        /// The inode number, all 64 bits of it.
        pub st_ino: u64,
    }
}

// ---------------------------------------------------------------------------
// statx
// ---------------------------------------------------------------------------

/// A `statx` timestamp, `struct statx_timestamp`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct StatxTimestamp {
    /// Whole seconds since the epoch, signed so pre-1970 times survive.
    pub tv_sec: i64,
    /// Nanoseconds within the second.
    pub tv_nsec: u32,
    /// Reserved, written as zero.
    pub __reserved: i32,
}

/// Extended file metadata, `struct statx`.
///
/// The same on both architectures, which is half the reason it was added: it
/// carries its own field mask, so the kernel need not fill in what the caller
/// did not ask for, and it is 256 bytes with reserved space for growth rather
/// than a new structure per architecture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Statx {
    /// Which fields the kernel actually filled in; see the `STATX_*` masks.
    pub stx_mask: u32,
    /// Preferred block size for input and output.
    pub stx_blksize: u32,
    /// File attribute flags the file system reports.
    pub stx_attributes: u64,
    /// Number of hard links.
    pub stx_nlink: u32,
    /// Owning user identifier.
    pub stx_uid: u32,
    /// Owning group identifier.
    pub stx_gid: u32,
    /// File type and permission bits.
    pub stx_mode: u16,
    /// Padding to the next 64-bit boundary.
    pub __spare0: [u16; 1],
    /// Inode number.
    pub stx_ino: u64,
    /// Size in bytes.
    pub stx_size: u64,
    /// Number of 512-byte blocks allocated.
    pub stx_blocks: u64,
    /// Which bits of `stx_attributes` the file system supports at all.
    pub stx_attributes_mask: u64,
    /// Last access time.
    pub stx_atime: StatxTimestamp,
    /// Creation time, if the file system records one.
    pub stx_btime: StatxTimestamp,
    /// Last status change time.
    pub stx_ctime: StatxTimestamp,
    /// Last modification time.
    pub stx_mtime: StatxTimestamp,
    /// Major number of the represented device, for device nodes.
    pub stx_rdev_major: u32,
    /// Minor number of the represented device, for device nodes.
    pub stx_rdev_minor: u32,
    /// Major number of the device holding the file.
    pub stx_dev_major: u32,
    /// Minor number of the device holding the file.
    pub stx_dev_minor: u32,
    /// Identifier of the mount the file was found through.
    pub stx_mnt_id: u64,
    /// Memory alignment required for direct input and output.
    pub stx_dio_mem_align: u32,
    /// File offset alignment required for direct input and output.
    pub stx_dio_offset_align: u32,
    /// Reserved space that keeps the structure at 256 bytes.
    pub __spare3: [u64; 12],
}

// ---------------------------------------------------------------------------
// statfs
// ---------------------------------------------------------------------------

/// A filesystem identifier, `__kernel_fsid_t` from
/// `include/uapi/asm-generic/posix_types.h`: two `int`s on every architecture.
pub type Fsid = [i32; 2];

/// What `statfs` and `fstatfs` fill in on both 64-bit architectures, `struct
/// statfs` from `include/uapi/asm-generic/statfs.h`.
///
/// 120 bytes. Neither x86-64 nor AArch64 overrides the generic header's
/// `struct statfs` -- their own `asm/statfs.h` only packs the 32-bit compat
/// structure -- and on a 64-bit build `__statfs_word` is `__kernel_long_t`, so
/// every count is a signed 64-bit word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Statfs {
    /// The filesystem's magic number: `TMPFS_MAGIC` and the like.
    pub f_type: i64,
    /// The block size the counts are in.
    pub f_bsize: i64,
    /// Blocks in total.
    pub f_blocks: i64,
    /// Blocks free.
    pub f_bfree: i64,
    /// Blocks free to an unprivileged user.
    pub f_bavail: i64,
    /// Inodes in total.
    pub f_files: i64,
    /// Inodes free.
    pub f_ffree: i64,
    /// Filesystem identifier.
    pub f_fsid: Fsid,
    /// The longest name a directory entry may have.
    pub f_namelen: i64,
    /// Fragment size, which Linux reports as the block size when a filesystem
    /// gives none.
    pub f_frsize: i64,
    /// Mount flags, `ST_*`.
    pub f_flags: i64,
    /// Reserved, written as zero.
    pub f_spare: [i64; 4],
}

/// ARMv7-A's `struct statfs`: the same generic header's, with `__statfs_word`
/// a `__u32`. 64 bytes.
///
/// No count wider than 32 bits fits, which is why musl on this architecture
/// never asks for it and calls `statfs64` instead. It is written down so that
/// a 32-bit `statfs` has a layout to be answered in rather than a guess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ArmStatfs {
    /// The filesystem's magic number.
    pub f_type: u32,
    /// The block size the counts are in.
    pub f_bsize: u32,
    /// Blocks in total.
    pub f_blocks: u32,
    /// Blocks free.
    pub f_bfree: u32,
    /// Blocks free to an unprivileged user.
    pub f_bavail: u32,
    /// Inodes in total.
    pub f_files: u32,
    /// Inodes free.
    pub f_ffree: u32,
    /// Filesystem identifier.
    pub f_fsid: Fsid,
    /// The longest name a directory entry may have.
    pub f_namelen: u32,
    /// Fragment size.
    pub f_frsize: u32,
    /// Mount flags, `ST_*`.
    pub f_flags: u32,
    /// Reserved, written as zero.
    pub f_spare: [u32; 4],
}

/// ARMv7-A's `struct statfs64`: the counts widened to 64 bits and the rest
/// left at 32. 84 bytes.
///
/// **Packed to four.** `arch/arm/include/uapi/asm/statfs.h` defines
/// `ARCH_PACK_STATFS64` as `__attribute__((packed,aligned(4)))`, because the
/// EABI would otherwise pad the structure to 88 so that its 64-bit fields
/// align, and the kernel wanted one size for both ARM ABIs. User space does
/// not pack it: musl's structure is 88 bytes, and 88 is the size it passes --
/// see [`ARM_STATFS64_UNPACKED_SIZE`]. `repr(C, packed(4))` says the same
/// thing in Rust on whatever host the tests run on: no field is placed
/// further apart than four bytes, and the structure aligns to four.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C, packed(4))]
pub struct ArmStatfs64 {
    /// The filesystem's magic number.
    pub f_type: u32,
    /// The block size the counts are in.
    pub f_bsize: u32,
    /// Blocks in total.
    pub f_blocks: u64,
    /// Blocks free.
    pub f_bfree: u64,
    /// Blocks free to an unprivileged user.
    pub f_bavail: u64,
    /// Inodes in total.
    pub f_files: u64,
    /// Inodes free.
    pub f_ffree: u64,
    /// Filesystem identifier.
    pub f_fsid: Fsid,
    /// The longest name a directory entry may have.
    pub f_namelen: u32,
    /// Fragment size.
    pub f_frsize: u32,
    /// Mount flags, `ST_*`.
    pub f_flags: u32,
    /// Reserved, written as zero.
    pub f_spare: [u32; 4],
}

/// The size musl passes to ARMv7-A's `statfs64` and `fstatfs64`: its own
/// unpacked `struct statfs64`, four bytes longer than the kernel's.
///
/// Linux's ARM entry code (`sys_statfs64_wrapper` in
/// `arch/arm/kernel/entry-common.S`) turns 88 into 84 before the size is
/// checked, for exactly this reason. Measured rather than remembered: Alpine's
/// static busybox 1.37 for armv7 loads `#88` into `r1` before both of its
/// `svc` calls with 266 in `r7`.
pub const ARM_STATFS64_UNPACKED_SIZE: usize = 88;

/// `f_flags`: the flags field means something, which Linux sets on every
/// answer. From `include/linux/statfs.h` -- not a UAPI header, but its values
/// are the ABI `statvfs` decodes.
pub const ST_VALID: u64 = 0x0020;
/// `f_flags`: the mount is read-only. This and the `ST_*` values after it are
/// Linux's `flags_by_mnt` answers, the same bits glibc's `<bits/statvfs.h>`
/// names.
pub const ST_RDONLY: u64 = 0x0001;
/// `f_flags`: set-id bits are ignored on the mount.
pub const ST_NOSUID: u64 = 0x0002;
/// `f_flags`: device nodes on the mount cannot be opened.
pub const ST_NODEV: u64 = 0x0004;
/// `f_flags`: programs on the mount cannot be run.
pub const ST_NOEXEC: u64 = 0x0008;
/// `f_flags`: no access times.
pub const ST_NOATIME: u64 = 0x0400;
/// `f_flags`: no access times for directories.
pub const ST_NODIRATIME: u64 = 0x0800;
/// `f_flags`: access times relative to the modification time.
pub const ST_RELATIME: u64 = 0x1000;

// ---------------------------------------------------------------------------
// mount, umount2 and fallocate
// ---------------------------------------------------------------------------

/// `mount`: mount read-only. Every `MS_*` here is from
/// `include/uapi/linux/mount.h`.
pub const MS_RDONLY: u32 = 1;
/// `mount`: ignore set-user-ID and set-group-ID bits on the mount.
pub const MS_NOSUID: u32 = 2;
/// `mount`: refuse to open device nodes on the mount.
pub const MS_NODEV: u32 = 4;
/// `mount`: refuse to run programs from the mount.
pub const MS_NOEXEC: u32 = 8;
/// `mount`: update access times only when older than the modification time.
pub const MS_RELATIME: u32 = 1 << 21;
/// `mount`: keep no access times.
pub const MS_NOATIME: u32 = 1024;
/// `mount`: keep no access times for directories.
pub const MS_NODIRATIME: u32 = 2048;
/// `mount`: always update access times, overriding the default `relatime`.
pub const MS_STRICTATIME: u32 = 1 << 24;
/// `mount`: with `MS_BIND`, the mounts inside too; with a propagation flag,
/// every mount below.
pub const MS_REC: u32 = 16384;
/// `mount`: say less in the kernel log about a failure.
pub const MS_SILENT: u32 = 32768;
/// `mount`: change the flags of an existing mount rather than make one.
pub const MS_REMOUNT: u32 = 32;
/// `mount`: make a directory visible at a second place.
pub const MS_BIND: u32 = 4096;
/// `mount`: move an existing mount to another place.
pub const MS_MOVE: u32 = 8192;
/// `mount`: make a mount unbindable.
pub const MS_UNBINDABLE: u32 = 1 << 17;
/// `mount`: make a mount private, receiving no mount events from its peers.
pub const MS_PRIVATE: u32 = 1 << 18;
/// `mount`: make a mount a slave of its peer group.
pub const MS_SLAVE: u32 = 1 << 19;
/// `mount`: make a mount shared with its peer group.
pub const MS_SHARED: u32 = 1 << 20;
/// The magic number programs from before Linux 2.4 put in `mount`'s flags,
/// which Linux still strips.
pub const MS_MGC_VAL: u32 = 0xC0ED_0000;
/// The bits [`MS_MGC_VAL`] occupies.
pub const MS_MGC_MSK: u32 = 0xFFFF_0000;

/// `umount2`: unmount even if busy.
///
/// The `MNT_*` and `UMOUNT_*` values are in `include/linux/fs.h` rather than a
/// UAPI header. They are the ABI all the same: every C library carries its own
/// copy in `<sys/mount.h>`.
pub const MNT_FORCE: u32 = 1;
/// `umount2`: detach from the tree now, and go when no longer busy.
pub const MNT_DETACH: u32 = 2;
/// `umount2`: mark for expiry, to be unmounted by a later call if unused.
pub const MNT_EXPIRE: u32 = 4;
/// `umount2`: do not follow a final symbolic link.
pub const UMOUNT_NOFOLLOW: u32 = 8;

/// `fallocate`: allocate without changing the file's size. From
/// `include/uapi/linux/falloc.h`.
pub const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
/// `fallocate`: deallocate a range, leaving a hole. Only with
/// [`FALLOC_FL_KEEP_SIZE`].
pub const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

/// The fixed head of a `getdents64` entry, `struct linux_dirent64`.
///
/// The real entry ends with a NUL-terminated name of variable length, so the
/// kernel walks the buffer by `d_reclen` rather than by `size_of`. This type
/// describes only the head; [`DIRENT64_NAME_OFFSET`] is where the name starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
#[allow(
    clippy::trailing_empty_array,
    reason = "the zero-length array is what makes the C flexible array member's \
              offset visible to offset_of!, and it is exactly what the UAPI \
              header declares"
)]
pub struct LinuxDirent64 {
    /// Inode number of the entry.
    pub d_ino: u64,
    /// Offset to pass to `lseek` to resume reading after this entry.
    pub d_off: i64,
    /// Length of this whole entry, including the name and its padding.
    pub d_reclen: u16,
    /// File type of the entry; see the `DT_*` constants.
    pub d_type: u8,
    /// Start of the NUL-terminated name, which extends past this structure.
    pub d_name: [u8; 0],
}

/// Byte offset of `d_name` within a `getdents64` entry.
///
/// Named because `size_of::<LinuxDirent64>()` is 24, not 19: Rust rounds the
/// structure up to its alignment exactly as C does, and using the size here
/// would skip five bytes of every name.
pub const DIRENT64_NAME_OFFSET: usize = 19;

/// Directory entry type: unknown, so the caller must `stat` to find out.
pub const DT_UNKNOWN: u8 = 0;
/// Directory entry type: named pipe.
pub const DT_FIFO: u8 = 1;
/// Directory entry type: character device.
pub const DT_CHR: u8 = 2;
/// Directory entry type: directory.
pub const DT_DIR: u8 = 4;
/// Directory entry type: block device.
pub const DT_BLK: u8 = 6;
/// Directory entry type: regular file.
pub const DT_REG: u8 = 8;
/// Directory entry type: symbolic link.
pub const DT_LNK: u8 = 10;
/// Directory entry type: socket.
pub const DT_SOCK: u8 = 12;

// ---------------------------------------------------------------------------
// System identity and resources
// ---------------------------------------------------------------------------

/// System identification strings, `struct new_utsname`.
///
/// Six fixed 65-byte fields, each holding a NUL-terminated string. The odd
/// length is `__NEW_UTS_LEN + 1`, which is why the structure is 390 bytes and
/// has an alignment of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Utsname {
    /// Operating system name, which Ferrix reports as `Ferrix`. Build systems
    /// that map it to a target have to be told which one to use.
    pub sysname: [u8; 65],
    /// Host name.
    pub nodename: [u8; 65],
    /// Kernel release, the field configure scripts compare against.
    pub release: [u8; 65],
    /// Kernel version string.
    pub version: [u8; 65],
    /// Machine hardware name, such as `x86_64` or `aarch64`.
    pub machine: [u8; 65],
    /// NIS or YP domain name, almost always `(none)`.
    pub domainname: [u8; 65],
}

/// A resource limit pair, `struct rlimit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Rlimit {
    /// Soft limit: the value actually enforced.
    pub rlim_cur: u64,
    /// Hard limit: the ceiling an unprivileged process may raise the soft
    /// limit to.
    pub rlim_max: u64,
}

/// Accumulated resource usage, `struct rusage`.
///
/// Linux fills in the two times, `ru_maxrss`, the fault counts and the context
/// switch counts; the remaining fields exist for source compatibility with
/// older UNIX systems and are always zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Rusage {
    /// Processor time spent in user mode.
    pub ru_utime: Timeval,
    /// Processor time spent in the kernel on this process's behalf.
    pub ru_stime: Timeval,
    /// Maximum resident set size, in kilobytes.
    pub ru_maxrss: i64,
    /// Integral shared memory size; unused on Linux.
    pub ru_ixrss: i64,
    /// Integral unshared data size; unused on Linux.
    pub ru_idrss: i64,
    /// Integral unshared stack size; unused on Linux.
    pub ru_isrss: i64,
    /// Page faults served without reading from storage.
    pub ru_minflt: i64,
    /// Page faults that required reading from storage.
    pub ru_majflt: i64,
    /// Swap count; unused on Linux.
    pub ru_nswap: i64,
    /// Block input operations.
    pub ru_inblock: i64,
    /// Block output operations.
    pub ru_oublock: i64,
    /// Messages sent; unused on Linux.
    pub ru_msgsnd: i64,
    /// Messages received; unused on Linux.
    pub ru_msgrcv: i64,
    /// Signals received; unused on Linux.
    pub ru_nsignals: i64,
    /// Voluntary context switches.
    pub ru_nvcsw: i64,
    /// Involuntary context switches.
    pub ru_nivcsw: i64,
}

/// System-wide statistics, `struct sysinfo`.
///
/// All the memory fields are counted in `mem_unit` bytes, not in bytes; the
/// unit exists so that the 32-bit form of the structure could describe more
/// than 4 GiB of memory, and 64-bit callers still have to multiply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Sysinfo {
    /// Seconds since boot.
    pub uptime: i64,
    /// One, five and fifteen minute load averages, scaled by 65536.
    pub loads: [u64; 3],
    /// Total usable main memory.
    pub totalram: u64,
    /// Available main memory.
    pub freeram: u64,
    /// Memory used by shared mappings.
    pub sharedram: u64,
    /// Memory used by buffers.
    pub bufferram: u64,
    /// Total swap space.
    pub totalswap: u64,
    /// Free swap space.
    pub freeswap: u64,
    /// Number of processes.
    pub procs: u16,
    /// Padding the kernel writes as zero.
    pub pad: u16,
    /// Total high memory; zero on 64-bit, which has no high memory.
    pub totalhigh: u64,
    /// Free high memory; zero on 64-bit.
    pub freehigh: u64,
    /// Size in bytes of the unit the memory fields are counted in.
    pub mem_unit: u32,
    /// Historical padding, empty on 64-bit because the fields above already
    /// fill the reserved twenty bytes.
    pub _f: [u8; 0],
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// A set of signals, the kernel's `sigset_t`.
///
/// One 64-bit word, because `_NSIG` is 64. This is *not* the C library's
/// `sigset_t`, which musl and glibc both pad to 128 bytes; the system call
/// takes the size as an argument precisely so the two can differ, and the
/// kernel rejects anything but 8.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Sigset {
    /// Bit `n - 1` is set when signal `n` is a member.
    pub sig: [u64; 1],
}

/// A signal disposition, the kernel's `struct sigaction`.
///
/// The field order is the kernel's, not POSIX's: the mask comes last so the
/// structure could be extended, and `sa_restorer` sits in the middle. Both
/// architectures use this layout — `arch/arm64/include/uapi/asm/signal.h`
/// defines `SA_RESTORER` for AArch32 compatibility, so the AArch64 structure
/// keeps the field even though 64-bit code sets [`SA_RESTORER`] and returns
/// through the trampoline the C library supplies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Sigaction {
    /// User address of the handler, or [`SIG_DFL`] or [`SIG_IGN`].
    pub sa_handler: u64,
    /// Behaviour flags; see the `SA_*` constants.
    pub sa_flags: u64,
    /// User address of the trampoline that issues `rt_sigreturn`.
    pub sa_restorer: u64,
    /// Signals blocked for the duration of the handler.
    pub sa_mask: Sigset,
}

/// An alternate signal stack, `stack_t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Stack {
    /// User address of the stack's lowest byte.
    pub ss_sp: u64,
    /// [`SS_ONSTACK`] or [`SS_DISABLE`].
    pub ss_flags: i32,
    /// Size of the stack in bytes.
    pub ss_size: u64,
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

/// One entry of a `poll` or `ppoll` array, `struct pollfd`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Pollfd {
    /// File descriptor to watch; a negative value makes the entry inert.
    pub fd: i32,
    /// Events the caller is interested in.
    pub events: i16,
    /// Events that occurred, written by the kernel.
    pub revents: i16,
}

// ---------------------------------------------------------------------------
// The auxiliary vector
// ---------------------------------------------------------------------------

/// End of the auxiliary vector.
pub const AT_NULL: u64 = 0;
/// Entry to be ignored.
pub const AT_IGNORE: u64 = 1;
/// File descriptor of the program, for the long-obsolete `execfd` path.
pub const AT_EXECFD: u64 = 2;
/// User address of the program headers.
pub const AT_PHDR: u64 = 3;
/// Size of one program header entry.
pub const AT_PHENT: u64 = 4;
/// Number of program header entries.
pub const AT_PHNUM: u64 = 5;
/// System page size.
pub const AT_PAGESZ: u64 = 6;
/// Base address at which the interpreter was loaded.
pub const AT_BASE: u64 = 7;
/// Flags; always zero on Linux.
pub const AT_FLAGS: u64 = 8;
/// Entry point of the program.
pub const AT_ENTRY: u64 = 9;
/// Non-zero if the program is not an ELF file.
pub const AT_NOTELF: u64 = 10;
/// Real user identifier.
pub const AT_UID: u64 = 11;
/// Effective user identifier.
pub const AT_EUID: u64 = 12;
/// Real group identifier.
pub const AT_GID: u64 = 13;
/// Effective group identifier.
pub const AT_EGID: u64 = 14;
/// String naming the platform, such as `x86_64`.
pub const AT_PLATFORM: u64 = 15;
/// Processor capability bits.
pub const AT_HWCAP: u64 = 16;
/// Clock ticks per second, the unit of `times`.
pub const AT_CLKTCK: u64 = 17;
/// Non-zero when the program was executed set-user-ID or set-group-ID.
///
/// musl reads this before deciding whether to honour environment variables
/// such as `LD_PRELOAD`, so a kernel that omits it is choosing the unsafe
/// default.
pub const AT_SECURE: u64 = 23;
/// Address of sixteen random bytes, which musl uses to seed the stack guard.
pub const AT_RANDOM: u64 = 25;
/// Second word of processor capability bits.
pub const AT_HWCAP2: u64 = 26;
/// Address of the path name the program was executed with.
pub const AT_EXECFN: u64 = 31;
/// Address of the vDSO's ELF header.
pub const AT_SYSINFO_EHDR: u64 = 33;
/// Minimum stack size a signal frame needs on this machine.
pub const AT_MINSIGSTKSZ: u64 = 51;

// ---------------------------------------------------------------------------
// open flags
// ---------------------------------------------------------------------------

/// Open for reading only.
pub const O_RDONLY: u32 = 0;
/// Open for writing only.
pub const O_WRONLY: u32 = 1;
/// Open for reading and writing.
pub const O_RDWR: u32 = 2;
/// Create the file if it does not exist.
pub const O_CREAT: u32 = 0o100;
/// With [`O_CREAT`], fail if the file already exists.
pub const O_EXCL: u32 = 0o200;
/// Do not make the file the process's controlling terminal.
pub const O_NOCTTY: u32 = 0o400;
/// Truncate the file to zero length on open.
pub const O_TRUNC: u32 = 0o1000;
/// Append every write to the end of the file.
pub const O_APPEND: u32 = 0o2000;
/// Fail rather than block on operations that would wait.
pub const O_NONBLOCK: u32 = 0o4000;
/// Fail unless the path names a directory.
///
/// The generic header's value, which x86-64 uses. Both Arm architectures
/// override it: see [`OPEN_FLAGS_ARM`].
pub const O_DIRECTORY: u32 = 0o200000;
/// Fail if the final component is a symbolic link.
///
/// The generic header's value, which x86-64 uses. Both Arm architectures
/// override it: see [`OPEN_FLAGS_ARM`].
pub const O_NOFOLLOW: u32 = 0o400000;
/// Close the descriptor automatically on `execve`.
pub const O_CLOEXEC: u32 = 0o2000000;
/// Open only to name the file: no reading, writing or access check.
pub const O_PATH: u32 = 0o10000000;
/// The bits of an `open` flag word that carry the access mode.
///
/// Not a bit set: `O_RDONLY` is zero, so the mode is the masked value.
pub const O_ACCMODE: u32 = 3;

/// The four `open` flags whose bits are not the same on every architecture.
///
/// `include/uapi/asm-generic/fcntl.h` defines all of them under `#ifndef`, and
/// `arch/arm/include/uapi/asm/fcntl.h` and `arch/arm64/include/uapi/asm/fcntl.h`
/// define all four first with the same set of bits in a different order. So
/// `0o200000` asks for a directory from an x86-64 program and for direct I/O
/// from an Arm one, and `0o400000` -- `O_LARGEFILE`, which a 32-bit musl sets
/// on every open -- is `O_NOFOLLOW` on x86-64. A kernel that decoded an Arm
/// program's flags with the generic table would refuse every symbolic link a
/// 32-bit program opens.
///
/// A table rather than per-architecture constants because the kernel chooses
/// one at one place, its architecture facade, and everything after that is
/// written once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlagBits {
    /// `O_DIRECT`: bypass the page cache.
    pub direct: u32,
    /// `O_LARGEFILE`: offsets past 2 GiB are allowed.
    pub largefile: u32,
    /// `O_DIRECTORY`: fail unless the path names a directory.
    pub directory: u32,
    /// `O_NOFOLLOW`: fail if the final component is a symbolic link.
    pub nofollow: u32,
}

/// The generic header's bits, which x86-64 uses unchanged.
pub const OPEN_FLAGS_GENERIC: OpenFlagBits = OpenFlagBits {
    direct: 0o40000,
    largefile: 0o100000,
    directory: 0o200000,
    nofollow: 0o400000,
};

/// The bits AArch64 and ARMv7-A use, which their two headers agree on.
pub const OPEN_FLAGS_ARM: OpenFlagBits = OpenFlagBits {
    directory: 0o40000,
    nofollow: 0o100000,
    direct: 0o200000,
    largefile: 0o400000,
};

// ---------------------------------------------------------------------------
// Memory mapping
// ---------------------------------------------------------------------------

/// The mapping may not be accessed at all.
pub const PROT_NONE: u32 = 0;
/// The mapping may be read.
pub const PROT_READ: u32 = 1;
/// The mapping may be written.
pub const PROT_WRITE: u32 = 2;
/// The mapping may be executed.
pub const PROT_EXEC: u32 = 4;
/// Pages may be used for atomic operations; accepted and meaningless, as on
/// Linux. From `asm-generic/mman-common.h`, as are the two below.
pub const PROT_SEM: u32 = 0x8;
/// `mprotect`: extend the change to the start of a region that grows down.
pub const PROT_GROWSDOWN: u32 = 0x0100_0000;
/// `mprotect`: extend the change to the end of a region that grows up.
pub const PROT_GROWSUP: u32 = 0x0200_0000;

/// Writes are visible to other mappers of the same object.
pub const MAP_SHARED: u32 = 0x1;
/// Writes are private, taken by copy on write.
pub const MAP_PRIVATE: u32 = 0x2;
/// Place the mapping exactly at the given address, replacing what is there.
pub const MAP_FIXED: u32 = 0x10;
/// Map zero-filled memory rather than a file.
pub const MAP_ANONYMOUS: u32 = 0x20;
/// Do not reserve swap space for the mapping.
pub const MAP_NORESERVE: u32 = 0x4000;
/// Populate the page tables immediately rather than faulting them in.
pub const MAP_POPULATE: u32 = 0x8000;
/// Mark the mapping as a stack, which on some architectures affects growth.
pub const MAP_STACK: u32 = 0x20000;
/// Like [`MAP_FIXED`], but fail rather than replace an existing mapping.
pub const MAP_FIXED_NOREPLACE: u32 = 0x100000;

/// `mremap` may move the mapping to a new address.
pub const MREMAP_MAYMOVE: u32 = 1;
/// `mremap` must place the mapping at the given address.
pub const MREMAP_FIXED: u32 = 2;

/// `msync`: schedule the write-back and return.
pub const MS_ASYNC: u32 = 1;
/// `msync`: invalidate other mappings of the same file.
pub const MS_INVALIDATE: u32 = 2;
/// `msync`: write back and wait for it.
pub const MS_SYNC: u32 = 4;

// `madvise`'s advice, from `include/uapi/asm-generic/mman-common.h`, which
// x86-64, AArch64 and the ARM EABI all include unchanged: none of their
// `asm/mman.h` defines an `MADV_*` of its own. `madvise` takes it as an `int`.

/// `madvise`: no special treatment.
pub const MADV_NORMAL: i32 = 0;
/// `madvise`: expect references in no particular order.
pub const MADV_RANDOM: i32 = 1;
/// `madvise`: expect references in order.
pub const MADV_SEQUENTIAL: i32 = 2;
/// `madvise`: the pages will be needed soon.
pub const MADV_WILLNEED: i32 = 3;
/// `madvise`: the pages are not needed; drop them, so that private
/// anonymous memory reads as zeros and a private file mapping as the file.
pub const MADV_DONTNEED: i32 = 4;
/// `madvise`: private anonymous pages may be dropped whenever memory is
/// wanted, until they are written again.
pub const MADV_FREE: i32 = 8;
/// `madvise`: punch a hole in what a shared mapping maps.
pub const MADV_REMOVE: i32 = 9;
/// `madvise`: leave the range out of a `fork` child.
pub const MADV_DONTFORK: i32 = 10;
/// `madvise`: undo [`MADV_DONTFORK`].
pub const MADV_DOFORK: i32 = 11;
/// `madvise`: let KSM merge identical pages.
pub const MADV_MERGEABLE: i32 = 12;
/// `madvise`: undo [`MADV_MERGEABLE`].
pub const MADV_UNMERGEABLE: i32 = 13;
/// `madvise`: worth backing with huge pages.
pub const MADV_HUGEPAGE: i32 = 14;
/// `madvise`: not worth backing with huge pages.
pub const MADV_NOHUGEPAGE: i32 = 15;
/// `madvise`: leave the range out of a core dump.
pub const MADV_DONTDUMP: i32 = 16;
/// `madvise`: undo [`MADV_DONTDUMP`].
pub const MADV_DODUMP: i32 = 17;
/// `madvise`: a `fork` child gets the range as zeros.
pub const MADV_WIPEONFORK: i32 = 18;
/// `madvise`: undo [`MADV_WIPEONFORK`].
pub const MADV_KEEPONFORK: i32 = 19;
/// `madvise`: the pages are cold; reclaim them before others.
pub const MADV_COLD: i32 = 20;
/// `madvise`: reclaim the pages now.
pub const MADV_PAGEOUT: i32 = 21;
/// `madvise`: fault the range in, readable.
pub const MADV_POPULATE_READ: i32 = 22;
/// `madvise`: fault the range in, writable.
pub const MADV_POPULATE_WRITE: i32 = 23;
/// `madvise`: [`MADV_DONTNEED`], on `mlock`ed memory too.
pub const MADV_DONTNEED_LOCKED: i32 = 24;
/// `madvise`: collapse the range into huge pages now.
pub const MADV_COLLAPSE: i32 = 25;

// ---------------------------------------------------------------------------
// clone
// ---------------------------------------------------------------------------

/// Share the address space with the parent.
pub const CLONE_VM: u64 = 0x100;
/// Share file system information: working directory, root and umask.
pub const CLONE_FS: u64 = 0x200;
/// Share the file descriptor table.
pub const CLONE_FILES: u64 = 0x400;
/// Share the table of signal handlers.
pub const CLONE_SIGHAND: u64 = 0x800;
/// Allow the parent's tracer to trace the child.
pub const CLONE_PTRACE: u64 = 0x2000;
/// Suspend the parent until the child execs or exits.
pub const CLONE_VFORK: u64 = 0x4000;
/// Make the child a sibling rather than a child of the caller.
pub const CLONE_PARENT: u64 = 0x8000;
/// Put the child in the caller's thread group; what makes a thread a thread.
pub const CLONE_THREAD: u64 = 0x10000;
/// Give the child a new mount namespace.
pub const CLONE_NEWNS: u64 = 0x20000;
/// Share System V semaphore adjustment state.
pub const CLONE_SYSVSEM: u64 = 0x40000;
/// Set the child's thread pointer from the `tls` argument.
pub const CLONE_SETTLS: u64 = 0x80000;
/// Store the child's thread identifier at the parent's address.
pub const CLONE_PARENT_SETTID: u64 = 0x100000;
/// Clear and wake the address in the child when it exits; the futex a
/// `pthread_join` waits on.
pub const CLONE_CHILD_CLEARTID: u64 = 0x200000;
/// Store the child's thread identifier at the child's address.
pub const CLONE_CHILD_SETTID: u64 = 0x1000000;
/// Give the child a new user namespace.
pub const CLONE_NEWUSER: u64 = 0x10000000;
/// Give the child a new process identifier namespace.
pub const CLONE_NEWPID: u64 = 0x20000000;
/// Give the child a new network namespace.
pub const CLONE_NEWNET: u64 = 0x40000000;

// ---------------------------------------------------------------------------
// futex
// ---------------------------------------------------------------------------

/// Sleep if the futex word still holds the expected value.
pub const FUTEX_WAIT: u32 = 0;
/// Wake up to the given number of waiters.
pub const FUTEX_WAKE: u32 = 1;
/// Requeue waiters onto a second futex.
pub const FUTEX_REQUEUE: u32 = 3;
/// Requeue waiters, but only if the first futex still holds a given value.
pub const FUTEX_CMP_REQUEUE: u32 = 4;
/// Wake waiters on one futex after an atomic operation on a second.
pub const FUTEX_WAKE_OP: u32 = 5;
/// Wait, matching only the given bits, with an absolute timeout.
pub const FUTEX_WAIT_BITSET: u32 = 9;
/// Wake only waiters that registered matching bits.
pub const FUTEX_WAKE_BITSET: u32 = 10;
/// The futex is private to one address space, which skips the shared lookup.
pub const FUTEX_PRIVATE_FLAG: u32 = 128;
/// Interpret the timeout against `CLOCK_REALTIME` rather than the monotonic
/// clock.
pub const FUTEX_CLOCK_REALTIME: u32 = 256;
/// A `futex_waitv` element's word is 32 bits wide: `FUTEX2_SIZE_U32`, which
/// `linux/futex.h` also names `FUTEX_32`. The only size `futex_waitv` takes.
pub const FUTEX_32: u32 = 0x02;
/// A `futex_waitv` element's word is private to the caller's address space
/// (`FUTEX2_PRIVATE`, the same bit as [`FUTEX_PRIVATE_FLAG`]).
pub const FUTEX2_PRIVATE: u32 = FUTEX_PRIVATE_FLAG;
/// The most words one `futex_waitv` waits on (`FUTEX_WAITV_MAX`).
pub const FUTEX_WAITV_MAX: usize = 128;
/// Bytes in one `struct futex_waitv`: `u64 val`, `u64 uaddr`, `u32 flags`,
/// `u32 __reserved`, the same on every architecture.
pub const FUTEX_WAITV_SIZE: usize = 24;

// ---------------------------------------------------------------------------
// The *at family
// ---------------------------------------------------------------------------

/// Directory file descriptor meaning "relative to the working directory".
///
/// Negative, and deliberately not `-1`, so that passing a closed descriptor by
/// accident cannot be mistaken for it.
pub const AT_FDCWD: i32 = -100;
/// Do not follow a final symbolic link.
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
/// Remove a directory rather than a link, for `unlinkat`.
pub const AT_REMOVEDIR: u32 = 0x200;
/// Follow a final symbolic link, for the calls that default to not.
pub const AT_SYMLINK_FOLLOW: u32 = 0x400;
/// Do not trigger an automount on the last component.
pub const AT_NO_AUTOMOUNT: u32 = 0x800;
/// Operate on the directory file descriptor itself when the path is empty.
pub const AT_EMPTY_PATH: u32 = 0x1000;
/// `statx`'s two-bit synchronisation request. Both bits at once is `EINVAL`.
pub const AT_STATX_SYNC_TYPE: u32 = 0x6000;
/// Check access with the effective rather than the real identity, for
/// `faccessat2`. The same bit as [`AT_REMOVEDIR`], which only `unlinkat` reads.
pub const AT_EACCESS: u32 = 0x200;
/// `name_to_handle_at`: a handle to compare, never to open. The same bit as
/// [`AT_REMOVEDIR`] again.
pub const AT_HANDLE_FID: u32 = 0x200;
/// `name_to_handle_at`: report the 64-bit unique mount identifier.
pub const AT_HANDLE_MNT_ID_UNIQUE: u32 = 0x001;
/// `name_to_handle_at`: a handle that opens back onto a connected path.
pub const AT_HANDLE_CONNECTABLE: u32 = 0x002;

/// `renameat2`: refuse with `EEXIST` rather than replace the target.
pub const RENAME_NOREPLACE: u32 = 1 << 0;
/// `renameat2`: swap the two names atomically.
pub const RENAME_EXCHANGE: u32 = 1 << 1;
/// `renameat2`: leave a whiteout behind at the source, for overlay filesystems.
pub const RENAME_WHITEOUT: u32 = 1 << 2;

/// `utimensat`: a `tv_nsec` meaning "set this time to now".
pub const UTIME_NOW: i64 = (1 << 30) - 1;
/// `utimensat`: a `tv_nsec` meaning "leave this time alone".
pub const UTIME_OMIT: i64 = (1 << 30) - 2;

/// `access`: the file exists.
pub const F_OK: u32 = 0;
/// `access`: execute or search permission.
pub const X_OK: u32 = 1;
/// `access`: write permission.
pub const W_OK: u32 = 2;
/// `access`: read permission.
pub const R_OK: u32 = 4;

// ---------------------------------------------------------------------------
// File modes
// ---------------------------------------------------------------------------

/// File type: named pipe.
pub const S_IFIFO: u32 = 0o010000;
/// File type: character device.
pub const S_IFCHR: u32 = 0o020000;
/// File type: directory.
pub const S_IFDIR: u32 = 0o040000;
/// File type: block device.
pub const S_IFBLK: u32 = 0o060000;
/// File type: regular file.
pub const S_IFREG: u32 = 0o100000;
/// File type: symbolic link.
pub const S_IFLNK: u32 = 0o120000;
/// File type: socket.
pub const S_IFSOCK: u32 = 0o140000;
/// Mask selecting the file type bits of a mode.
pub const S_IFMT: u32 = 0o170000;

// ---------------------------------------------------------------------------
// Seeking
// ---------------------------------------------------------------------------

/// Seek relative to the start of the file.
pub const SEEK_SET: u32 = 0;
/// Seek relative to the current position.
pub const SEEK_CUR: u32 = 1;
/// Seek relative to the end of the file.
pub const SEEK_END: u32 = 2;
/// Seek to the next byte that is not a hole.
pub const SEEK_DATA: u32 = 3;
/// Seek to the next hole.
pub const SEEK_HOLE: u32 = 4;

// ---------------------------------------------------------------------------
// Signal numbers and dispositions
// ---------------------------------------------------------------------------

/// Hangup on the controlling terminal.
pub const SIGHUP: u32 = 1;
/// Interrupt from the keyboard.
pub const SIGINT: u32 = 2;
/// Quit from the keyboard.
pub const SIGQUIT: u32 = 3;
/// Illegal instruction.
pub const SIGILL: u32 = 4;
/// Breakpoint or trace trap.
pub const SIGTRAP: u32 = 5;
/// Abort, as raised by `abort` and by a failed Rust assertion.
pub const SIGABRT: u32 = 6;
/// Bus error: an access the hardware could not complete.
pub const SIGBUS: u32 = 7;
/// Arithmetic exception.
pub const SIGFPE: u32 = 8;
/// Kill, which cannot be caught, blocked or ignored.
pub const SIGKILL: u32 = 9;
/// First user-defined signal.
pub const SIGUSR1: u32 = 10;
/// Invalid memory reference.
pub const SIGSEGV: u32 = 11;
/// Second user-defined signal.
pub const SIGUSR2: u32 = 12;
/// Write to a pipe with no reader.
pub const SIGPIPE: u32 = 13;
/// Timer signal from `alarm`.
pub const SIGALRM: u32 = 14;
/// Termination request.
pub const SIGTERM: u32 = 15;
/// Stack fault on a coprocessor; unused on Linux.
pub const SIGSTKFLT: u32 = 16;
/// A child stopped or terminated.
pub const SIGCHLD: u32 = 17;
/// Continue if stopped.
pub const SIGCONT: u32 = 18;
/// Stop, which cannot be caught, blocked or ignored.
pub const SIGSTOP: u32 = 19;
/// Stop typed at the terminal.
pub const SIGTSTP: u32 = 20;
/// Background process read from the terminal.
pub const SIGTTIN: u32 = 21;
/// Background process wrote to the terminal.
pub const SIGTTOU: u32 = 22;
/// Urgent data on a socket.
pub const SIGURG: u32 = 23;
/// Processor time limit exceeded.
pub const SIGXCPU: u32 = 24;
/// File size limit exceeded.
pub const SIGXFSZ: u32 = 25;
/// Virtual alarm clock.
pub const SIGVTALRM: u32 = 26;
/// Profiling timer expired.
pub const SIGPROF: u32 = 27;
/// Terminal window size changed.
pub const SIGWINCH: u32 = 28;
/// Input or output is now possible.
pub const SIGIO: u32 = 29;
/// Power failure.
pub const SIGPWR: u32 = 30;
/// Bad system call, which is what a seccomp filter raises.
pub const SIGSYS: u32 = 31;
/// Lowest real-time signal as the kernel numbers them.
///
/// The C library reserves the first two for its own use and reports 34 as
/// `SIGRTMIN` to programs; the kernel's own floor is 32.
pub const SIGRTMIN: u32 = 32;
/// One past the highest signal number, and the number of bits in a
/// [`Sigset`].
pub const NSIG: u32 = 64;

/// Take the default action for the signal.
pub const SIG_DFL: u64 = 0;
/// Ignore the signal.
pub const SIG_IGN: u64 = 1;

/// `sigprocmask` operation: add the given signals to the blocked set.
pub const SIG_BLOCK: u32 = 0;
/// `sigprocmask` operation: remove the given signals from the blocked set.
pub const SIG_UNBLOCK: u32 = 1;
/// `sigprocmask` operation: replace the blocked set.
pub const SIG_SETMASK: u32 = 2;

/// Do not raise `SIGCHLD` when a child merely stops.
pub const SA_NOCLDSTOP: u64 = 0x0000_0001;
/// Do not turn children into zombies.
pub const SA_NOCLDWAIT: u64 = 0x0000_0002;
/// Deliver the three-argument handler with a `siginfo_t`.
pub const SA_SIGINFO: u64 = 0x0000_0004;
/// The handler address in `sa_restorer` is valid.
pub const SA_RESTORER: u64 = 0x0400_0000;
/// Run the handler on the alternate signal stack.
pub const SA_ONSTACK: u64 = 0x0800_0000;
/// Restart interruptible system calls rather than failing them with `EINTR`.
pub const SA_RESTART: u64 = 0x1000_0000;
/// Do not block the signal inside its own handler.
pub const SA_NODEFER: u64 = 0x4000_0000;
/// Reset the disposition to the default when the handler is entered.
pub const SA_RESETHAND: u64 = 0x8000_0000;

/// Alternate stack flag: the process is currently executing on it.
pub const SS_ONSTACK: i32 = 1;
/// Alternate stack flag: no alternate stack is installed.
pub const SS_DISABLE: i32 = 2;

// ---------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------

/// Wall clock, which can jump when the time is set.
pub const CLOCK_REALTIME: u32 = 0;
/// Monotonic clock, which never jumps and does not count suspended time.
pub const CLOCK_MONOTONIC: u32 = 1;
/// Processor time consumed by the whole process.
pub const CLOCK_PROCESS_CPUTIME_ID: u32 = 2;
/// Processor time consumed by the calling thread.
pub const CLOCK_THREAD_CPUTIME_ID: u32 = 3;
/// Monotonic clock without the adjustments `adjtime` applies.
pub const CLOCK_MONOTONIC_RAW: u32 = 4;
/// Wall clock read at tick granularity, which is cheap.
pub const CLOCK_REALTIME_COARSE: u32 = 5;
/// Monotonic clock read at tick granularity, which is cheap.
pub const CLOCK_MONOTONIC_COARSE: u32 = 6;
/// Monotonic clock that does count time spent suspended.
pub const CLOCK_BOOTTIME: u32 = 7;
/// International Atomic Time: real time plus the TAI offset.
pub const CLOCK_TAI: u32 = 11;

// ---------------------------------------------------------------------------
// poll and epoll
// ---------------------------------------------------------------------------

/// Data may be read without blocking.
pub const POLLIN: i16 = 0x001;
/// Urgent data may be read.
pub const POLLPRI: i16 = 0x002;
/// Data may be written without blocking.
pub const POLLOUT: i16 = 0x004;
/// An error occurred; reported whether or not it was requested.
pub const POLLERR: i16 = 0x008;
/// The peer closed its end; reported whether or not it was requested.
pub const POLLHUP: i16 = 0x010;
/// The descriptor is not open; reported whether or not it was requested.
pub const POLLNVAL: i16 = 0x020;

/// Epoll event: data may be read.
pub const EPOLLIN: u32 = 0x001;
/// Epoll event: urgent data may be read.
pub const EPOLLPRI: u32 = 0x002;
/// Epoll event: data may be written.
pub const EPOLLOUT: u32 = 0x004;
/// Epoll event: an error occurred.
pub const EPOLLERR: u32 = 0x008;
/// Epoll event: the descriptor hung up.
pub const EPOLLHUP: u32 = 0x010;
/// Epoll event: the descriptor is not open. Never reported: a closed
/// descriptor's registration goes with its file.
pub const EPOLLNVAL: u32 = 0x020;
/// Epoll event: normal data may be read.
pub const EPOLLRDNORM: u32 = 0x040;
/// Epoll event: priority data may be read.
pub const EPOLLRDBAND: u32 = 0x080;
/// Epoll event: normal data may be written.
pub const EPOLLWRNORM: u32 = 0x100;
/// Epoll event: priority data may be written.
pub const EPOLLWRBAND: u32 = 0x200;
/// Epoll event: a message is available; unused by Linux.
pub const EPOLLMSG: u32 = 0x400;
/// Epoll event: the peer shut down the writing half.
pub const EPOLLRDHUP: u32 = 0x2000;
/// Epoll flag: wake only one of the epoll sets waiting on the same file.
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
/// Epoll flag: hold off system suspend while the event is being handled.
pub const EPOLLWAKEUP: u32 = 1 << 29;
/// Epoll flag: report this descriptor once, then disarm it.
pub const EPOLLONESHOT: u32 = 1 << 30;
/// Epoll flag: report edges rather than levels.
pub const EPOLLET: u32 = 1 << 31;

/// Add a descriptor to an epoll set.
pub const EPOLL_CTL_ADD: u32 = 1;
/// Remove a descriptor from an epoll set.
pub const EPOLL_CTL_DEL: u32 = 2;
/// Change the registration of a descriptor already in an epoll set.
pub const EPOLL_CTL_MOD: u32 = 3;

/// Create the epoll descriptor with the close-on-exec flag set.
pub const EPOLL_CLOEXEC: u32 = O_CLOEXEC;

// ---------------------------------------------------------------------------
// fcntl
// ---------------------------------------------------------------------------

/// Duplicate onto the lowest free descriptor at or above the argument.
pub const F_DUPFD: u32 = 0;
/// Read the descriptor flags.
pub const F_GETFD: u32 = 1;
/// Set the descriptor flags.
pub const F_SETFD: u32 = 2;
/// Read the file status flags.
pub const F_GETFL: u32 = 3;
/// Set the file status flags.
pub const F_SETFL: u32 = 4;
/// Read a record lock.
pub const F_GETLK: u32 = 5;
/// Set a record lock without blocking.
pub const F_SETLK: u32 = 6;
/// Set a record lock, waiting if necessary.
pub const F_SETLKW: u32 = 7;
/// Read a record lock through a `struct flock64`. ARMv7-A's `fcntl64` only.
pub const F_GETLK64: u32 = 12;
/// Set a record lock through a `struct flock64`, without blocking.
/// ARMv7-A's `fcntl64` only.
pub const F_SETLK64: u32 = 13;
/// Set a record lock through a `struct flock64`, waiting if necessary.
/// ARMv7-A's `fcntl64` only.
pub const F_SETLKW64: u32 = 14;
/// Read an open file description's record lock.
pub const F_OFD_GETLK: u32 = 36;
/// Set an open file description's record lock without blocking.
pub const F_OFD_SETLK: u32 = 37;
/// Set an open file description's record lock, waiting if necessary.
pub const F_OFD_SETLKW: u32 = 38;
/// A record lock's type: shared, for reading.
pub const F_RDLCK: i16 = 0;
/// A record lock's type: exclusive, for writing.
pub const F_WRLCK: i16 = 1;
/// A record lock's type: none, or release.
pub const F_UNLCK: i16 = 2;
/// Like [`F_DUPFD`], but set close-on-exec on the new descriptor.
pub const F_DUPFD_CLOEXEC: u32 = 1030;
/// `fcntl`: add seals to a file that allows sealing (`F_LINUX_SPECIFIC_BASE + 9`).
pub const F_ADD_SEALS: u32 = 1033;
/// `fcntl`: the seals a file carries (`F_LINUX_SPECIFIC_BASE + 10`).
pub const F_GET_SEALS: u32 = 1034;
/// Seal: no further seals may be added.
pub const F_SEAL_SEAL: u32 = 0x0001;
/// Seal: the file may not shrink.
pub const F_SEAL_SHRINK: u32 = 0x0002;
/// Seal: the file may not grow.
pub const F_SEAL_GROW: u32 = 0x0004;
/// Seal: the file's contents may not change; refused while it is mapped
/// shared and writable.
pub const F_SEAL_WRITE: u32 = 0x0008;
/// Seal: no new write may start, while writable mappings made before stay.
pub const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
/// Seal: `chmod` may not change the file's execute bits (Linux 6.3).
pub const F_SEAL_EXEC: u32 = 0x0020;
/// The only descriptor flag: close this descriptor on `execve`.
pub const FD_CLOEXEC: u32 = 1;

// ---------------------------------------------------------------------------
// flock
// ---------------------------------------------------------------------------

/// `flock`: a shared lock.
pub const LOCK_SH: u32 = 1;
/// `flock`: an exclusive lock.
pub const LOCK_EX: u32 = 2;
/// `flock`: or'd with `LOCK_SH` or `LOCK_EX`, refuse rather than wait.
pub const LOCK_NB: u32 = 4;
/// `flock`: release the lock.
pub const LOCK_UN: u32 = 8;

// ---------------------------------------------------------------------------
// Terminal ioctls
// ---------------------------------------------------------------------------

/// Read a terminal's settings, `struct termios`. What `isatty` asks.
///
/// From `include/uapi/asm-generic/ioctls.h`, which all three architectures
/// use: x86-64 and AArch64 include it unchanged, and ARMv7-A's own header adds
/// only `FIOQSIZE` before including it.
pub const TCGETS: u32 = 0x5401;
/// Change a terminal's settings now.
pub const TCSETS: u32 = 0x5402;
/// Change a terminal's settings once written output has drained.
pub const TCSETSW: u32 = 0x5403;
/// Change a terminal's settings after draining output and discarding input.
pub const TCSETSF: u32 = 0x5404;
/// Suspend or restart output: `tcflow`.
pub const TCXONC: u32 = 0x540A;
/// Discard queued input, output or both: `tcflush`.
pub const TCFLSH: u32 = 0x540B;
/// Make the terminal the caller's controlling terminal.
pub const TIOCSCTTY: u32 = 0x540E;
/// Read the terminal's foreground process group: `tcgetpgrp`.
pub const TIOCGPGRP: u32 = 0x540F;
/// Set the terminal's foreground process group: `tcsetpgrp`.
pub const TIOCSPGRP: u32 = 0x5410;
/// How many bytes are written and not yet sent: the output-side counterpart
/// of [`TIOCINQ`].
pub const TIOCOUTQ: u32 = 0x5411;
/// Read a terminal's size in rows and columns, `struct winsize`.
pub const TIOCGWINSZ: u32 = 0x5413;
/// Set a terminal's size.
pub const TIOCSWINSZ: u32 = 0x5414;
/// Which pseudoterminal pair a master is: `_IOR('T', 0x30, unsigned int)`,
/// which is `(2 << 30) | (4 << 16) | ('T' << 8) | 0x30`.
pub const TIOCGPTN: u32 = 0x8004_5430;
/// Lock or unlock a pseudoterminal's slave: `_IOW('T', 0x31, int)`. A pair
/// starts locked, and `openpty` unlocks it with a zero before it opens the
/// slave.
pub const TIOCSPTLCK: u32 = 0x4004_5431;
/// How many bytes a read would return without waiting.
pub const FIONREAD: u32 = 0x541B;
/// Set or clear `O_NONBLOCK` from the `int` the argument points at: any
/// file, not only a terminal.
pub const FIONBIO: u32 = 0x5421;
/// Clear the descriptor's close-on-exec flag: any file.
pub const FIONCLEX: u32 = 0x5450;
/// Set the descriptor's close-on-exec flag: any file.
pub const FIOCLEX: u32 = 0x5451;

// Block devices' requests, from `include/uapi/linux/fs.h`. Type `0x12`;
// the ones built with `_IO` carry no size, and the two built with
// `_IOR(.., size_t)` carry the pointer width in bits 16 to 29, so they are
// `0x8008_12xx` on the 64-bit architectures and `0x8004_12xx` on ARMv7-A.

/// Whether the disk refuses writes, as an `int`: `_IO(0x12, 94)`.
pub const BLKROGET: u32 = 0x125E;
/// Read the partition table again: `_IO(0x12, 95)`.
pub const BLKRRPART: u32 = 0x125F;
/// The size in 512-byte sectors, as an `unsigned long`: `_IO(0x12, 96)`.
pub const BLKGETSIZE: u32 = 0x1260;
/// Write back and drop what is cached of the disk: `_IO(0x12, 97)`.
pub const BLKFLSBUF: u32 = 0x1261;
/// The logical sector size, as an `int`: `_IO(0x12, 104)`.
pub const BLKSSZGET: u32 = 0x1268;
/// The block size the page cache uses, as an `int`: `_IOR(0x12, 112, size_t)`,
/// although the kernel writes an `int` there.
pub const BLKBSZGET: u32 = ior_size_t(112);
/// The size in bytes, as a `u64`: `_IOR(0x12, 114, size_t)`.
pub const BLKGETSIZE64: u32 = ior_size_t(114);
/// The smallest I/O the disk does without a read-modify-write, as an
/// `unsigned int`: `_IO(0x12, 120)`.
pub const BLKIOMIN: u32 = 0x1278;
/// The I/O size the disk does best, as an `unsigned int`: `_IO(0x12, 121)`.
pub const BLKIOOPT: u32 = 0x1279;
/// How far the first aligned sector is from the start, as an `int`:
/// `_IO(0x12, 122)`.
pub const BLKALIGNOFF: u32 = 0x127A;
/// The physical sector size, as an `unsigned int`: `_IO(0x12, 123)`.
pub const BLKPBSZGET: u32 = 0x127B;
/// Whether the disk is a spinning one, as an `unsigned short`:
/// `_IO(0x12, 126)`.
pub const BLKROTATIONAL: u32 = 0x127E;

/// `_IOR(0x12, number, size_t)` for this build's pointer width.
const fn ior_size_t(number: u32) -> u32 {
    // `_IOC_READ` is 2, in the top two bits.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "usize is at most 8 bytes, so the cast cannot truncate"
    )]
    let size = size_of::<usize>() as u32;
    (2 << 30) | (size << 16) | (0x12 << 8) | number
}
/// The same request as [`FIONREAD`], under its terminal name.
pub const TIOCINQ: u32 = FIONREAD;
/// Give up the controlling terminal.
pub const TIOCNOTTY: u32 = 0x5422;
/// Read the session the terminal controls: `tcgetsid`.
pub const TIOCGSID: u32 = 0x5429;
/// Read a terminal's settings with their speeds, `struct termios2`. What
/// glibc's `tcgetattr` asks since glibc stopped translating `struct termios`.
///
/// `_IOR('T', 0x2A, struct termios2)` in `asm-generic/ioctls.h`: direction
/// `_IOC_READ` (2) in the top two bits, then [`TERMIOS2_BYTES`] in the next
/// fourteen, then `'T'` and the number. The size is part of the number, so it
/// is the same on all three architectures because the structure is.
pub const TCGETS2: u32 = 0x802C_542A;
/// [`TCSETS`], with speeds: `_IOW('T', 0x2B, struct termios2)`.
pub const TCSETS2: u32 = 0x402C_542B;
/// [`TCSETSW`], with speeds: `_IOW('T', 0x2C, struct termios2)`.
pub const TCSETSW2: u32 = 0x402C_542C;
/// [`TCSETSF`], with speeds: `_IOW('T', 0x2D, struct termios2)`.
pub const TCSETSF2: u32 = 0x402C_542D;

/// Bytes in the kernel's `struct termios` on all three architectures: four
/// `unsigned int` flag words, `c_line`, and `NCCS` (19) control characters.
///
/// Not the C library's `struct termios`, which glibc makes 60 bytes with 32
/// control characters and two speeds: the library translates. x86-64, AArch64
/// and ARMv7-A all take it from `asm-generic/termbits.h`.
pub const TERMIOS_BYTES: usize = 36;
/// Control characters in the kernel's `struct termios`.
pub const NCCS: usize = 19;
/// Bytes in `struct termios2`: `struct termios`, then `c_ispeed` and
/// `c_ospeed`, two `speed_t` (`unsigned int`). [`TERMIOS_BYTES`] is already a
/// multiple of four, so there is no padding before them on any of the three.
pub const TERMIOS2_BYTES: usize = 44;

/// `c_cc` index: the character that raises `SIGINT`.
pub const VINTR: usize = 0;
/// `c_cc` index: the character that raises `SIGQUIT`.
pub const VQUIT: usize = 1;
/// `c_cc` index: erase the character before the cursor.
pub const VERASE: usize = 2;
/// `c_cc` index: erase the whole line.
pub const VKILL: usize = 3;
/// `c_cc` index: end of file, or end of a line without a newline.
pub const VEOF: usize = 4;
/// `c_cc` index: a non-canonical read's timeout, in tenths of a second.
pub const VTIME: usize = 5;
/// `c_cc` index: how many bytes a non-canonical read waits for.
pub const VMIN: usize = 6;
/// `c_cc` index: the character that raises `SIGTSTP`.
pub const VSUSP: usize = 10;
/// `c_cc` index: an extra end-of-line character.
pub const VEOL: usize = 11;
/// `c_cc` index: erase the word before the cursor.
pub const VWERASE: usize = 14;
/// `c_cc` index: a second extra end-of-line character.
pub const VEOL2: usize = 16;

/// `c_iflag`: strip the eighth bit.
pub const ISTRIP: u32 = 0x020;
/// `c_iflag`: map newline to carriage return on input.
pub const INLCR: u32 = 0x040;
/// `c_iflag`: discard carriage returns on input.
pub const IGNCR: u32 = 0x080;
/// `c_iflag`: map carriage return to newline on input.
pub const ICRNL: u32 = 0x100;
/// `c_iflag`: XON/XOFF output flow control.
pub const IXON: u32 = 0x400;

/// `c_oflag`: process output at all.
pub const OPOST: u32 = 0x01;
/// `c_oflag`: map newline to carriage return and newline on output.
pub const ONLCR: u32 = 0x04;

/// `c_cflag`: 115200 baud.
pub const B115200: u32 = 0x1002;
/// `c_cflag`: the bits that name the output speed.
pub const CBAUD: u32 = 0x100F;
/// `c_cflag`: a speed given as a number, in `struct termios2`'s `c_ospeed`
/// (or `c_ispeed`, shifted by [`IBSHIFT`]), rather than as a `B` code.
pub const BOTHER: u32 = 0x1000;
/// How far `CIBAUD`, the input speed's bits, sit above [`CBAUD`]'s. Zero
/// there means the input speed is the output speed.
pub const IBSHIFT: u32 = 16;
/// Every `B` speed code with the bits a second it names, from
/// `asm-generic/termbits-common.h` (`B0` to `B38400`) and
/// `asm-generic/termbits.h` (`B57600` to `B4000000`), which all three
/// architectures use.
pub const BAUD_RATES: [(u32, u32); 31] = [
    (0x0000, 0),
    (0x0001, 50),
    (0x0002, 75),
    (0x0003, 110),
    (0x0004, 134),
    (0x0005, 150),
    (0x0006, 200),
    (0x0007, 300),
    (0x0008, 600),
    (0x0009, 1200),
    (0x000A, 1800),
    (0x000B, 2400),
    (0x000C, 4800),
    (0x000D, 9600),
    (0x000E, 19200),
    (0x000F, 38400),
    (0x1001, 57600),
    (B115200, 115_200),
    (0x1003, 230_400),
    (0x1004, 460_800),
    (0x1005, 500_000),
    (0x1006, 576_000),
    (0x1007, 921_600),
    (0x1008, 1_000_000),
    (0x1009, 1_152_000),
    (0x100A, 1_500_000),
    (0x100B, 2_000_000),
    (0x100C, 2_500_000),
    (0x100D, 3_000_000),
    (0x100E, 3_500_000),
    (0x100F, 4_000_000),
];
/// `c_cflag`: eight bits a character.
pub const CS8: u32 = 0x30;
/// `c_cflag`: the receiver is enabled.
pub const CREAD: u32 = 0x80;
/// `c_cflag`: ignore modem control lines.
pub const CLOCAL: u32 = 0x800;

/// `c_lflag`: the interrupt, quit and suspend characters raise signals.
pub const ISIG: u32 = 0x00001;
/// `c_lflag`: canonical mode, a line at a time with editing.
pub const ICANON: u32 = 0x00002;
/// `c_lflag`: echo input.
pub const ECHO: u32 = 0x00008;
/// `c_lflag`: echo an erase as backspace, space, backspace.
pub const ECHOE: u32 = 0x00010;
/// `c_lflag`: echo a newline after a line kill.
pub const ECHOK: u32 = 0x00020;
/// `c_lflag`: echo a newline even when `ECHO` is off.
pub const ECHONL: u32 = 0x00040;
/// `c_lflag`: keep queued input when a signal character arrives.
pub const NOFLSH: u32 = 0x00080;
/// `c_lflag`: echo control characters as `^X`.
pub const ECHOCTL: u32 = 0x00200;
/// `c_lflag`: erase a killed line character by character.
pub const ECHOKE: u32 = 0x00800;
/// `c_lflag`: the extended characters, `VWERASE` and `VEOL2` among them.
pub const IEXTEN: u32 = 0x08000;

/// `TCFLSH`: discard received input not yet read.
pub const TCIFLUSH: u64 = 0;
/// `TCFLSH`: discard written output not yet sent.
pub const TCOFLUSH: u64 = 1;
/// `TCFLSH`: both.
pub const TCIOFLUSH: u64 = 2;
/// `TCXONC`: the largest action, `TCION`.
pub const TCION: u64 = 3;

// ---------------------------------------------------------------------------
// Miscellaneous flags
// ---------------------------------------------------------------------------

/// Fail rather than block when the entropy pool is not ready.
pub const GRND_NONBLOCK: u32 = 0x0001;
/// Draw from the blocking pool rather than the non-blocking one.
pub const GRND_RANDOM: u32 = 0x0002;

/// Set close-on-exec on the descriptor `memfd_create` returns.
pub const MFD_CLOEXEC: u32 = 0x0001;
/// Allow the memfd to be sealed against further change.
pub const MFD_ALLOW_SEALING: u32 = 0x0002;
/// Make the memfd unexecutable for good: no execute bits, `F_SEAL_EXEC`, and
/// sealable (Linux 6.3).
pub const MFD_NOEXEC_SEAL: u32 = 0x0008;
/// Make the memfd executable, mode 0777, as before Linux 6.3; refused under
/// `vm.memfd_noexec` 2.
pub const MFD_EXEC: u32 = 0x0010;

/// Set close-on-exec on the descriptor `eventfd2` returns.
pub const EFD_CLOEXEC: u32 = O_CLOEXEC;
/// Make the descriptor `eventfd2` returns non-blocking.
pub const EFD_NONBLOCK: u32 = O_NONBLOCK;
/// Make the eventfd count semaphore-style, decrementing by one per read.
pub const EFD_SEMAPHORE: u32 = 0x0000_0001;

/// Set close-on-exec on the descriptor `timerfd_create` returns. `O_CLOEXEC`,
/// by `linux/timerfd.h`, which is the same number on all three architectures:
/// neither `asm-arm/fcntl.h` nor `asm-arm64/fcntl.h` overrides it.
pub const TFD_CLOEXEC: u32 = O_CLOEXEC;
/// Make the descriptor `timerfd_create` returns non-blocking. `O_NONBLOCK`,
/// generic on all three architectures for the same reason.
pub const TFD_NONBLOCK: u32 = O_NONBLOCK;
/// `timerfd_settime`: the new value is a time on the timer's clock, not a
/// duration from now.
pub const TFD_TIMER_ABSTIME: u32 = 1 << 0;
/// `timerfd_settime`: with `TFD_TIMER_ABSTIME` on a real-time clock, end the
/// timer with `ECANCELED` when the clock is set.
pub const TFD_TIMER_CANCEL_ON_SET: u32 = 1 << 1;

/// `splice`: move pages rather than copy them, a hint Linux has ignored since
/// 2.6.21 and Ferrix ignores too.
pub const SPLICE_F_MOVE: u32 = 0x0001;
/// `splice`: do not wait on the pipe.
pub const SPLICE_F_NONBLOCK: u32 = 0x0002;
/// `splice`: more is coming, a hint for a socket.
pub const SPLICE_F_MORE: u32 = 0x0004;
/// `vmsplice`'s gift of its pages, which `splice` takes and ignores.
pub const SPLICE_F_GIFT: u32 = 0x0008;
/// Every flag `splice` takes; any other bit is `EINVAL`.
pub const SPLICE_F_ALL: u32 = SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT;
/// Set close-on-exec on the descriptor `signalfd4` makes. `O_CLOEXEC`, by
/// `linux/signalfd.h`, generic on all three architectures as [`TFD_CLOEXEC`]
/// is.
pub const SFD_CLOEXEC: u32 = O_CLOEXEC;
/// Make the descriptor `signalfd4` makes non-blocking. `O_NONBLOCK`, by the
/// same header.
pub const SFD_NONBLOCK: u32 = O_NONBLOCK;
/// Bytes in `struct signalfd_siginfo`, what one read of a signalfd returns
/// per signal: fixed-width fields padded to 128 on every architecture.
pub const SIGNALFD_SIGINFO_BYTES: usize = 128;

// ---------------------------------------------------------------------------
// statx field mask
// ---------------------------------------------------------------------------

/// Want `stx_mode`'s file type bits.
pub const STATX_TYPE: u32 = 0x0000_0001;
/// Want `stx_mode`'s permission bits.
pub const STATX_MODE: u32 = 0x0000_0002;
/// Want `stx_nlink`.
pub const STATX_NLINK: u32 = 0x0000_0004;
/// Want `stx_uid`.
pub const STATX_UID: u32 = 0x0000_0008;
/// Want `stx_gid`.
pub const STATX_GID: u32 = 0x0000_0010;
/// Want `stx_atime`.
pub const STATX_ATIME: u32 = 0x0000_0020;
/// Want `stx_mtime`.
pub const STATX_MTIME: u32 = 0x0000_0040;
/// Want `stx_ctime`.
pub const STATX_CTIME: u32 = 0x0000_0080;
/// Want `stx_ino`.
pub const STATX_INO: u32 = 0x0000_0100;
/// Want `stx_size`.
pub const STATX_SIZE: u32 = 0x0000_0200;
/// Want `stx_blocks`.
pub const STATX_BLOCKS: u32 = 0x0000_0400;
/// Want everything the old `stat` call returned.
pub const STATX_BASIC_STATS: u32 = 0x0000_07ff;
/// Want `stx_btime`.
pub const STATX_BTIME: u32 = 0x0000_0800;
/// Got `stx_mnt_id`.
pub const STATX_MNT_ID: u32 = 0x0000_1000;
/// Reserved for a future extension of the structure; a request carrying it is
/// `EINVAL`.
pub const STATX_RESERVED: u32 = 0x8000_0000;
