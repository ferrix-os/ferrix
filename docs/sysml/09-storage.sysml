/*
 * Ferrix — SysML v2 model, part 9: storage.
 *
 * docs/ARCHITECTURE.md §8. Block core, VFS, the small in-kernel filesystems
 * and btrfs in three stages. What exists today is marked on each part.
 */
package FerrixStorage {
    private import ScalarValues::*;
    private import FerrixLifecycle::*;
    private import FerrixMemory::Vmo;

    // -----------------------------------------------------------------------
    // Block core, stage 11
    // -----------------------------------------------------------------------

    #implemented part def BlockCore {
        @stage { number = 11; }
        doc /* Request queues, merging, an I/O scheduler with per-cgroup
             * bandwidth, and the ring protocol to userspace block drivers. A
             * read into a page-cache page fills the VMO the cache already
             * holds. The queue itself (merging, flush and FUA barriers,
             * deadline scheduling) is src/lib/fs/block, in front of every block
             * ring (src/kernel/src/interfaces/block_ring, src/lib/proto/blkring): one kernel task
             * per ring moves requests onto it and copies payloads through
             * the ring's data VMO, and publishes the disk in devfs's
             * registry as a BlockDevice (src/kernel/src/fs/block.rs) that
             * reads, writes and flushes whole sectors. No per-cgroup
             * bandwidth: cgroups are stage 13's. */
        part queues : RequestQueue[0..*];
        part ioScheduler : IoScheduler;
        action submit;
        action flush;
        action fua;
    }
    part def RequestQueue;
    part def IoScheduler;

    #implemented part def NetCore {
        doc /* The networking stage (docs/ROADMAP.md, Networking), which is
             * not on the path to rustc and was taken after stage 11: AF_INET
             * and AF_INET6 sockets over src/lib/network/net, AF_NETLINK route sockets
             * and the ifreq ioctls for configuring an interface, /proc/net
             * for the programs that read it, the net ring, and virtio-net as
             * its first driver, in ring 3, with AF_UNIX names under it all.
             * Its exit criterion is met: cargo xtask test-net configures
             * eth0, resolves a name and fetches a file byte for byte on
             * every architecture. */
    }

    // -----------------------------------------------------------------------
    // VFS, stage 8
    // -----------------------------------------------------------------------

    #implemented part def Inode {
        @stage { number = 8; }
        doc /* src/lib/fs/vfs's Inode trait, which tmpfs, devfs, procfs, pipes,
             * sockets and btrfs implement; Inode::mapping offers what mmap
             * maps. */
        ref part pages : Vmo {
            doc /* The page cache is the inode's VMO; a mapped file and a read
                 * file are the same pages. */
        }
    }

    #implemented part def Dentry {
        @stage { number = 8; }
        attribute name : String;
        ref part inode : Inode[0..1] {
            doc /* Absent for a negative entry, which is what makes a failed
                 * lookup cheap the second time. */
        }
    }

    #implemented part def Mount {
        @stage { number = 8; }
        ref part root : Dentry;
        ref part filesystem : Filesystem;
    }

    #implemented part def Vfs {
        @stage { number = 8; }
        doc /* rustc opens tens of thousands of files during a build; this is a
             * performance requirement. Inode cache, dentry cache with negative
             * entries, mount table per mount namespace, fd tables with their
             * sharing rules.
             *
             * src/lib/fs/vfs, host-tested and fuzzed by vfs_ops: dentries with
             * negative entries kept alive by a bounded queue of recent ones,
             * mounts and one path walk for every call that takes a path,
             * open file descriptions apart from descriptor tables, and the
             * permission checks where Linux makes them. src/kernel/src/fs builds
             * the one namespace every process resolves paths in; mount
             * namespaces are stage 13's. */
        part inodes : Inode[0..*];
        part dentries : Dentry[0..*];
        part mounts : Mount[0..*];
        action openat;
        action getdents64;
        action statx;
        action renameat2;
        action pread64;
    }

    #implemented part def PageCache {
        @stage { number = 8; }
        doc /* Unified with VMOs: not a separate cache but the set of file
             * VMOs, subject to reclaim's LRU. src/kernel/src/fs/pages.rs: each
             * file's VMO, filled from a disk filesystem's PageSource in runs
             * of at most 32 missing pages with no lock held, and by a fault
             * through a mapping as by a read. A read fills to its own last
             * page; on a sequential stream (it begins where the last read
             * ended, in a later page) at least MAX_FILL_RUN pages from its
             * first, never past the end of the file, and to its own last
             * page again if that run fails; a fault fills a run, as before. Reclaim is stage 13's and not
             * built, and no page is marked dirty, so a page written through
             * MAP_SHARED reaches a btrfs disk only if write writes it too. */
        ref part vmos : Vmo[0..*];
    }

    // -----------------------------------------------------------------------
    // Filesystems
    // -----------------------------------------------------------------------

    abstract #implemented part def Filesystem {
        doc /* src/lib/fs/vfs's FileSystem trait. */
        action mount;
        action lookup;
        action read;
        action write;
    }

    #implemented part def Tmpfs :> Filesystem {
        @stage { number = 8; }
        doc /* src/lib/fs/vfs/src/tmpfs.rs, whose file contents are a page store
             * the kernel supplies as a VMO, so a shared mmap of a tmpfs file
             * maps the file's own pages. The root and /tmp are tmpfs, and
             * mount -t tmpfs makes a fresh one. */
    }
    #implemented part def Devfs :> Filesystem {
        @stage { number = 8; }
        doc /* src/kernel/src/fs/devfs.rs: null, zero, full, random, urandom,
             * tty and console, numbered as Linux numbers them, ptmx and the
             * dri, input and pts directories since stages 17 and 18, and
             * the disks a driver's kernel side registers. It calls itself
             * devtmpfs, the name init scripts look for. */
    }
    #implemented part def Procfs :> Filesystem {
        @stage { number = 8; }
        doc /* self/maps, self/exe, self/fd backed by the real VM and fd
             * table; cpuinfo, meminfo. src/kernel/src/fs/procfs.rs over
             * src/lib/fs/procfs's text, rendered at open: each /proc/<pid> with
             * its fd, status, comm, cmdline, stat, maps, exe, cwd, root,
             * task and mounts, and /proc's mounts, stat, partitions,
             * filesystems, uptime, version, sys, sysrq-trigger and net
             * beside them. */
    }
    #implemented part def Sysfs :> Filesystem {
        doc /* src/kernel/src/fs/sysfs.rs over src/lib/fs/sysfs's text, a view that
             * stores nothing, fed by the services that own each fact
             * (docs/SYSFS.md): enumeration's device nodes under
             * devices/pci and platform, the disks, interfaces, cards,
             * render nodes and input devices the ring-3 drivers published,
             * each inside the node its driver serves, and devmgr's word on
             * which driver drives which device. A write to a driver's bind
             * or unbind is sent to devmgr to decide. Mounted on /sys at
             * boot; cgroup2 mounts on its fs/cgroup. */
    }
    #implemented part def Cgroupfs :> Filesystem {
        @stage { number = 13; }
        doc /* src/lib/fs/cgroupfs, mounted as cgroup2 since 2026-09-23: a
             * view of the job tree that stores nothing, with cgroup.events,
             * delegation by chown, and the pids, memory and cpu files over
             * each job's quota slot (docs/CGROUPS.md). */
    }

    #implemented part def InitramfsUnpack {
        @stage { number = 8; }
        doc /* src/lib/fs/cpio: the "newc" reader. Borrows, copies nothing,
             * allocates nothing, rejects unsafe paths. Unpacked into tmpfs
             * by src/lib/fs/vfs's initramfs module through the same calls a
             * program makes, hard links and device nodes included. */
    }

    #implemented part def BtrfsParsing {
        @stage { number = 11; }
        doc /* src/lib/fs/btrfs: superblock, sys chunk array and chunk map
             * (logical to physical), B-tree nodes and leaves, item payloads
             * (inode, inode ref, dir, extent data), crc32c. Pure functions over
             * bytes, forbid(unsafe_code); no cache, no transactions. Fuzzed
             * from images mkfs.btrfs produced. */
    }

    #implemented part def Btrfs :> Filesystem {
        @stage { number = 11; }
        doc /* The real filesystem, read and write, single device, no RAID
             * 5/6. The largest single piece of work in the project.
             * src/kernel/src/fs/btrfs.rs mounts it from a block node, read-only
             * over src/lib/fs/btrfs-vfs or writable over src/lib/fs/btrfs-write, one
             * inode object per inode so two names share one page cache. `/`
             * is a btrfs volume under cargo xtask run, and test-rustc's
             * sysroot is one at /data. */
        part parsing : BtrfsParsing;
        part read : BtrfsRead;
        part write : BtrfsWrite;
        #planned part subvolumes : BtrfsSubvolumes;
    }

    #implemented part def BtrfsRead {
        @stage { number = 11; }
        doc /* Stage A: superblock, chunk tree, root tree, fs trees, extent
             * data inline and regular, directory and inode items, crc32c
             * verification, zstd/zlib/lzo. Enough to mount what mkfs.btrfs
             * produced and read a sysroot out of it.
             *
             * src/lib/fs/btrfs and src/lib/fs/btrfs-vfs, host-tested against real
             * images, with every data sector checked against the checksum
             * tree before its bytes are used and a bounded cache of
             * metadata reads. Exit met on all three architectures: a
             * mkfs.btrfs fixture on a second virtio-blk disk, served by the
             * ring-3 driver, mounted at /mnt and read back against its
             * manifest. A file on the read-only mount cannot be mapped yet
             * (ENODEV); one on the writable mount can. */
    }

    #implemented part def BtrfsWrite {
        @stage { number = 12; }
        doc /* Stage B: copy-on-write allocation through the extent tree,
             * delayed refs, transaction commit against both superblock copies
             * with the right flush/FUA ordering, the free-space tree, the log
             * tree and its replay.
             *
             * All three are in src/lib/fs/btrfs-write, mounted writable by the
             * kernel through src/lib/fs/btrfs-vfs, and clean under host btrfs
             * check: over what every boot writes (cargo xtask test-btrfs),
             * and before and after the log is replayed when QEMU is killed
             * inside a transaction (cargo xtask test-powerfail). Owed beside
             * the stage: writeback of pages written through MAP_SHARED. */
        action allocateCow;
        action commitTransaction;
        action replayLog;
    }

    #planned part def BtrfsSubvolumes {
        doc /* Stage C: subvolumes and snapshots, then the rest. The reader
             * mounts the default subvolume; entering others, subvol= and
             * subvolid= are owed, and the writer refuses subvolumes and
             * snapshots. */
    }

    #implemented part def Filesystems {
        part tmpfs : Tmpfs;
        part devfs : Devfs;
        part procfs : Procfs;
        part sysfs : Sysfs;
        part cgroupfs : Cgroupfs;
        part btrfs : Btrfs;
        part initramfs : InitramfsUnpack;
    }
}
