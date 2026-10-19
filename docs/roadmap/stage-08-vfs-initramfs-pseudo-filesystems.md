# Stage 8 — VFS, initramfs, the pseudo-filesystems ✅

Inode and dentry caches, the mount table, file descriptors and their sharing
rules, tmpfs, devfs, procfs (`self/maps`, `self/exe`, `self/fd`, `cpuinfo`,
`meminfo`), and cpio initramfs unpacking.

**Done — the VFS, host-tested before the kernel calls it.** `src/lib/fs/common/vfs` is
the half of the stage that needs no machine, written first for the reason the
continuous rule gives: path resolution over names a program chose is exactly
the code that should meet a fuzzer before it meets ring 0.

* **Dentries, with negative entries.** A name and the thing it names are
  separate objects, which is what `..`, `getcwd`, mount points and
  `/proc/self/fd` are answered from. A miss is cached like a hit. A child
  holds its parent and a parent holds its children weakly, so what keeps a
  dentry alive is a bounded queue of recent ones — the whole eviction policy,
  replaceable without touching the tree. A lookup racing a create cannot cache
  a stale miss: each directory carries a generation that every change bumps,
  and a lookup inserts only if it is unchanged.
* **Mounts and the walk.** One path walk for every call that takes a path,
  following links from a heap stack rather than by recursion, crossing mounts
  in both directions, and never climbing above a context's root. The
  namespace takes the root and working directory as an argument rather than
  knowing about processes, which is what lets stage 13's mount namespaces be
  more of the same type.
* **Open file descriptions and descriptor tables**, kept apart the way Linux
  keeps them: `dup` and `fork` share an offset, separate `open`s do not, and
  close-on-exec belongs to the number. No spin lock of a description is held
  across a call into its inode, because stage 11's btrfs waits on the disk
  there; the offset is held across the read, write or listing it positions,
  as Linux's `f_pos_lock` is since 3.14, because it is a lock that sleeps
  (`ferrix_sync::SleepLock`, lent a wait queue by the kernel through the
  mount the file was opened on), so two reads racing on one description get
  consecutive bytes. A stream, `pread` and `pwrite` never take it.
* **tmpfs**, whose file contents are not a byte vector but a page store the
  kernel supplies — a VMO there — so that a shared `mmap` of a tmpfs file maps
  the file's own pages. The same store is the page cache of a filesystem on a
  disk: made over a `PageSource`, it fills runs of at most 32 missing pages
  with no lock held, keeps only those still missing, and forgets the source's
  bytes past a truncation. The host tests pin that, and the kernel's own VMO
  store fills the same way: frames allocated and zeroed before the source is
  called, a fill that fails or claims too much keeping nothing, and each page
  read under the VMO's lock so that a disk filesystem, which holds no lock
  across a read, cannot race a truncation into a freed frame. Directory
  cursors are never reused, so `rm -rf` reading a directory it is emptying
  sees every entry exactly once.
* **initramfs unpacking** through the same calls a program makes, hard links
  and device nodes included, and **the `getdents64` packer**, whose names start
  at byte 19 rather than at the structure's size of 24.

59 host tests at the time (101 now), the `vfs_ops` fuzz target — which asserts that every name a
listing reports resolves to the inode the listing gave, the property a stale
cache entry breaks — and a Miri step.

**Done — the root, built at boot from what the loader hands over.** The
loader reads `FERRIX/INITRD.IMG` into memory nothing reclaims; it is optional,
so a card flashed without one boots as it did. xtask writes the archive itself,
the same bytes on every build. The kernel unpacks it into a tmpfs root through
the same VFS calls a program makes, and mounts a second tmpfs on `/tmp`. File
contents are VMO pages, created at a tebibyte and paid for by the page, so the
object `read` copies out of is the one `mmap` of the file will map. The boot
check reads the archive's marker back through its hard link and its symbolic
link, then writes a file across pages under `/tmp`, truncates into it, grows it
and removes it. Then it reads 41 pages of a store over a page source that
fills over nothing, and requires two fills, of 32 and 9. A second read must
not ask again, and a write into the middle of a page must fill the page's
other bytes first. A source that answers three pages at a time must still
fill every page asked for, and a source that fails or claims too much must
be `EIO` and keep nothing. Last, a cut keeps the bytes before it, reads
zeros past it, and never asks the source for what it cut:

```
  initrd   2 KiB unpacked: 6 directories, 2 files, 1 hard links, 1 symbolic links, 0 refused, verified true
  tmpfs    4 pages written through a VMO and read back, 52 filled from a page source in runs and cut, 0 frames leaked
```

**Done — the calls that take a path.** `src/kernel/src/syscall/path.rs` and
`stat.rs`: `mkdirat`, `mknodat` (regular files, pipes, socket names, and
character and block device nodes), `unlinkat`, `renameat2` with
`RENAME_NOREPLACE`, `symlinkat`, `linkat`, `readlinkat`, `chdir`, `fchdir`,
`getcwd`, `faccessat` and `faccessat2`, `chmod`, `chown`, `utimensat`, `umask`,
`getdents64`, and the stat family with `statx` — each with its pre-`*at` form
where the architecture has one. The one architecture-dependent fact is which
`struct stat` a stat call fills: x86-64's own 144 bytes, the generic 128, or
ARMv7-A's 104-byte `stat64`, whose EABI padding `src/lib/proto/linux-abi` now names.
It is `arch::STAT_LAYOUT`, and all three encoders are compiled, and checked, on
every architecture. The umask is per process, 0o022 to start, and applies to
`openat`'s create too. The boot check makes each call by its number against
`/tmp`, decodes every record back out of user memory, lists forty names in
96-byte pieces, and runs twice:

```
  paths    170 path calls under /tmp, 42 names listed in 12 getdents64 calls, 4 device nodes opened by number, 0 frames leaked, dentry cache +0
```

**Done — descriptors, and the console as a file.** Every process has a
descriptor table and a root and working directory, each behind an `Arc` so
that `clone` can share them where `CLONE_FILES` and `CLONE_FS` ask and copy
them where they do not. A new process's descriptors 0, 1 and 2 are one open
description of `/dev/console`. `openat`, `close`, `read`, `write`, `readv`,
`writev`, `pread64`, `pwrite64`, `lseek` and `_llseek`, `dup`, `dup2`, `dup3`,
`fcntl` and `ftruncate` answer through it. `fcntl(F_GETFL)` reports `O_RDWR` on
the console, which is the one thing busybox's `printf` needed before it would
print, and stage 7's shell test has its `printf` line back. The `O_*` bits
x86-64 and the Arm architectures number differently are tables in
`src/lib/proto/linux-abi`, chosen through the architecture facade. `ioctl` on the
console goes to `syscall/tty.rs`. It refused `TCGETS` while the console edited
and echoed every line itself, because `sh -i` would then switch to raw mode
and echo as well, doubling every character. Since stage 7 made the console a
terminal that honours `ICANON` and `ECHO` (bc8c64b, 6de8907), `TCGETS`,
`TCSETS`, `TIOCGWINSZ` and the job-control requests are answered. So are
`TCGETS2` and `TCSETS2`, which a newer glibc's `tcgetattr` asks instead:
refusing them made Ubuntu's static busybox decide it had no terminal, so its
`stty -a`, `tty`, `login` and `less` failed where Alpine's worked.

**Done — `/dev` and `/proc`.** devfs holds `null`, `zero`, `full`, `random`,
`urandom`, `tty` and `console`, numbered as Linux numbers them, and since
stages 17 and 18 `ptmx` and the `dri`, `input` and `pts` directories. It calls itself
`devtmpfs` in `/proc/mounts` and `/proc/filesystems`, the name init scripts and
service managers look for; Linux has had no `devfs` since 2.6.18. A device node
made anywhere else — by `mknod` on tmpfs, or unpacked from the initramfs —
opens as the devfs device with its number, and keeps its own inode for `stat`,
as `/dev/tty` does; a number devfs lacks is `ENXIO`. Disks are registered, not
built in: a driver's kernel side hands `devfs::register_block` a name, a number
and a `fs::block::BlockDevice`, and while the returned registration lives the
disk is a block node in `/dev` and a row in `/proc/partitions`, and
`devfs::block_device` finds it by number for a mount. Dropping the
registration takes all three away, and a device still held after that answers
every read with `EIO`. Opening a block node is `ENXIO` until reads through a
descriptor come, and its `stat` reports no size, as Linux's does. The block
check registers an in-memory disk, lists it in pieces while a second one
arrives, reads its sectors back by number, and checks each refusal and the
drop. The path check makes four such nodes by syscall number,
writes through the null one, reads zeros from the zero one, and checks the
refusals. procfs renders
every file at open, so a program reading `maps` in small pieces sees one
snapshot, and its directories opt out of the dentry cache, so a pid looked up
before its process existed is not remembered as missing. `/proc/self` links to
the caller's pid; each `/proc/<pid>` has `fd`, `status`, `comm`, `cmdline`,
`stat`, `maps`, `exe`, `cwd` and `root`, and later `task` and `mounts`; and
`/proc` has `cpuinfo`, `meminfo`, `mounts`, `stat`, `partitions`,
`filesystems`, `uptime`, `version` and `sys`, and later `sysrq-trigger` and
`net`. The text is `src/lib/fs/common/procfs`, pinned
byte for byte against lines taken from a real Linux `/proc`. `/proc/stat`'s
processor lines are each run queue's busy and idle time, read without charging
anything, so a line never goes backwards between reads; all busy time is
`user`, because the kernel keeps no split between a task's user and kernel
time. `/proc/uptime`'s idle field is the same count. With it, `top`, `mpstat`,
`iostat -c` and `nmeter` run. `/proc/sys` is a tree of the values the kernel
already keeps — `kernel.ostype`, `osrelease`, `version`, `hostname`,
`domainname` and `pid_max`, `fs.file-max` and `fs.nr_open` — in which the
host and domain names write through to what `uname` reports, and every other
value is refused at open with `EACCES`, as Linux refuses it. `/proc/partitions`
lists every registered disk, and is empty, as Linux prints it, when there is
none. With them, `pwdx`,
`sysctl` and `fdisk -l` run. Behind it, every
process now has a pid from a registry that finds a live process by it.

```
  devfs    7 nodes numbered as Linux numbers them; zero, null, full and urandom do what they are for; a disk registered as 254:250 listed, stat'ed, read as a file, found by number, 2 sectors read, one in memory written as a file, gone from /dev and /proc/partitions with its registration; 0 frames leaked
  procfs   36 names listed and walked back to, 4 maps lines parsed, 2 of them named; cwd and root read as getcwd; 8 /proc/sys values read, a host name written there reached uname; partitions empty with no block devices
  procstat /proc/stat read twice 50 ms apart: a cpu line for each of 4 processors, 21 ticks advanced, no counter went backwards
```

**Done — pipes, FIFOs, and the calls about filesystems.** `pipe` and `pipe2`
over `src/lib/fs/common/vfs`'s pipe buffer, with a wait queue for each direction and each
end counted by its inode, so that a reader sees end of file and a writer
`EPIPE` exactly when the last descriptor that could feed or drain the pipe
closes. `O_NONBLOCK` reaches a stream with every read and write, because
`fcntl` can change it between them. A FIFO opens as an end of the one pipe its
node stands for, from a table keyed by device and inode number that `openat`
consults for a FIFO node. `statfs` and `fstatfs` answer in the word size's
layout, and ARMv7-A's packed `statfs64` takes musl's 88 as well as the kernel's
84. Then `sync`, `syncfs`, `fsync` and `fdatasync`; `truncate`, `truncate64`
and `fallocate`; `chroot`; `mount` of `tmpfs`, `proc` and `devtmpfs` — each a fresh instance, stacked over whatever the target showed, and `btrfs` from a block node since stages 11 and 12 — and `umount2`; the
extended-attribute calls, which report none; and `sendfile`, which busybox's
`cat` tries before it falls back to `read`. The boot check drives the handlers
from a process of its own, twice:

```
  pipes    18020 bytes through a pipe, a FIFO and sendfile; statfs, truncate and fallocate answered; proc and devtmpfs mounted, read and unmounted; 0 frames leaked
```

**Done — a file mapped shared.** `mmap` of a file maps the file's own VMO
pages, the ones `read` copies out of. So a write through the mapping is what
the next `read` returns, and a write to the file is what the mapping shows.
An inode offers what can be mapped through `Inode::mapping`: tmpfs its page
store's VMO, btrfs the same. `mmap` refuses in Linux's order: `EBADF` for a
descriptor that names nothing or only a path; `EACCES` for a file not open for
reading, or a writable shared mapping of one not open for writing; `ENODEV` for
an inode with nothing to map. `MAP_FIXED` clears its range only once every
check has passed. The mapping keeps its open file, so the file lives as long as
the mapping, as Linux's `vm_file`, and `/proc/<pid>/maps` names the region by
its path, offset, device and inode. A page wholly past the file's end is never
committed. The filesystem tells the store the file's length, after an extend
and before a cut, and a fault reads that bound under the VMO's lock. A touch
past the end is `SIGBUS` from user mode and `EFAULT` from a system call.
`msync` checks what Linux checks and writes nothing back, because the pages
are the file's. On tmpfs that is all there is. On btrfs, writable since
stage 12, the mount holds a file handed out for mapping while anything maps
it, and once a mapping that may write has been made, the next writeback
writes every page the file holds, since such writes mark none; `msync`
does not bring that commit forward.

**Done — a file mapped privately.** A private mapping of a file shows the
file's pages until it writes one, and that write copies the page into a shadow
object of the mapping's own. So neither the file nor any other mapping sees
it: not a write through the mapping, not a `read` into it, and not a write by
a `fork` child, which inherits the copies and copies again on its own write. A
write to the file still shows through the pages the mapping has not copied.
The region names both objects by one id, the file's VMO attached shared and
the shadow attached privately, so a file's VMO still has only shared mappers.
Every fault checks the file's end first, so a copied page past a cut is
`SIGBUS` too. A truncation takes the copies past the cut, so a file grown back
shows zeros through the mapping, as on Linux. `munmap` gives back the shadow's
pages and never the file's. A writable private mapping of a file opened
read-only maps, as Linux allows. A btrfs file's VMO fills a faulted page
from its source before anything copies it, since stage 16 (`fs/pages.rs`,
`check_a_mapping_faults_in_its_source`). The boot check maps a file under `/tmp` from a process of its
own, shared and private side by side, twice. It also has a program in user
mode write a page of a private mapping it has only read:

```
  mmap     12339 bytes written through a shared file mapping and read back from the file, and the other way; a loader-style fixed RX/RW mapping and RELRO protection worked; refusals, msync and /proc maps answered; a truncation took 3 pages away from the mapping; 2 pages copied into a private mapping and kept from the file, a fork and a user-mode write included; 0 frames leaked
```

**Done — an anonymous file, and its seals.** `memfd_create` makes a regular
file that no directory names, on a tmpfs of its own that nothing mounts, named
`memfd:NAME`, so it reads, writes, truncates and maps as a tmpfs file does.
With `MFD_ALLOW_SEALING` it takes seals through `fcntl(F_ADD_SEALS)`, and
tmpfs enforces them under the file's lock in shmem's order. A shrink seal
refuses truncating downwards, a grow seal refuses extending by truncation,
write or `fallocate`, and a write seal refuses every write. A write seal is
refused with `EBUSY` while any shared mapping may write the file. The file's
VMO counts those mappings, as Linux's `i_mmap_writable`: a shared mapping of a
file open for writing, raised as its id enters a space's tables and lowered as
it leaves, a `fork` child's copy included. The seal is stored and the count
read under the file's lock, and `mmap` counts itself before it takes that lock
to read the seals, so a write seal and a writable shared mapping never both
stand. Once a write seal stands, a shared writable mapping is `EPERM` and
`mprotect` to writable is `EACCES`; a private mapping still maps and keeps its
writes to itself. The same accounting closes a gap: a read-only shared mapping
of a file opened read-only can no longer be made writable. `MFD_HUGETLB` is
`EINVAL`, there being no hugetlbfs. The exec seal of Linux 6.3, as 6.18's
`mm/memfd.c` has it, landed on 2026-10-12: `MFD_NOEXEC_SEAL` makes a file of
mode 0666 carrying `F_SEAL_EXEC`, which no `execve` runs and no `chmod` gives
an execute bit, and which may be sealed further; `MFD_EXEC` makes the 0777
file every memfd was before; `F_SEAL_EXEC` added to a file that still has an
execute bit brings the write seals with it. Neither flag named, the caller's
pid namespace's `vm.memfd_noexec` (`/proc/sys/vm/memfd_noexec`) decides: 0
`MFD_EXEC`, 1 `MFD_NOEXEC_SEAL`, 2 the same with `MFD_EXEC` refused `EACCES`;
a namespace starts at, and cannot be set below, the scope of the one it was
made in. The boot check runs it all twice, from a process of its own:

```
  memfd    6 seals added and enforced, 23 calls refused as Linux refuses them, a write seal refused while a shared mapping could write, a fork's copy included; an exec seal held and vm.memfd_noexec obeyed; 0 frames leaked
```

**Done — packet pipes, `/dev`'s fd links, and `/proc` a 32-bit program can
read (2026-09-28 and 29, for the Steam client).** `pipe2` takes `O_DIRECT`
and makes a packet pipe, as Linux has since 3.4: a write of up to
`PIPE_BUF` bytes is one packet, a larger one is cut into packets of
`PIPE_BUF`, and a read takes one packet at most. `/dev` carries `fd`,
`stdin`, `stdout` and `stderr` as links into `/proc/self/fd`, the links
userspace makes on Linux, so bash's process substitution hands a command a
path that opens. `/proc/<pid>/fd/<n>` is a magic link, as Linux's
`proc_fd_link` makes it: followed, it is the open file itself, so a pipe
opens again through it and `stat` of a socket's link is the socket's
`fstat`; opening a socket or an anonymous file through it is `ENXIO`, as on
Linux. `/proc`'s inode numbers and its directory offsets fit in 32 bits,
each held by a compile-time assertion, since an i386 glibc `readdir` fails
with `EOVERFLOW` at the first number past them.

**The exit test, and how far it gets.** `cargo xtask test-vfs --init PATH`
puts a static musl busybox into the initramfs and has init run the exit
criterion's commands from it, checking their output after the boot marker. It
is a test of its own rather than part of `test-boot` for the reason stage 7's
is: the binary is not the repository's. Measured before a line of it was
written (`docs/STAGE8-WHAT-THE-EXIT-NEEDS.md`), this busybox runs every applet
that touches a file — `mkdir`, `mv`, `ln`, `rm`, `cat` — through `fork`,
`execve` and `wait4`. Until those existed the test ran eleven programs in the
script's place. With stage 7's `fork`, `execve`, `wait4` and `poll` on top, it
is the criterion's three commands: `ls -R /proc`, `cat /proc/self/maps`, and one
`sh -c` script whose file applets are real forked programs. Every command is
judged on its output as well as its status, and the script ends with a status
of its own, so a shell that died and reported success cannot pass. It passes on
all three architectures:

```
  x86_64: stage 8's exit programs all passed
  aarch64: stage 8's exit programs all passed
  armv7a: stage 8's exit programs all passed
```

After the exit programs, the same boot runs a group of applets, reported on a
line of its own and not part of the criterion: `pwdx`, `sysctl` reads with a
refused and an accepted write, `/proc/partitions` and `fdisk -l`, `top`,
`mpstat` and `iostat -c`, and `mknod` of a null, a zero and an unknown device.
They are the applets a sweep of busybox found failing, kept from failing again,
each judged on its output as well as its status.

**What a review found before the stage was called done.** Another session
read the VFS and its self-checks and found five bugs and one gap, three of the
bugs reproduced on the host, and the stage waited on all of it. A lookup that
lost a race with a create handed back a second, uncached dentry for a
directory, whose parent a later rename did not update, so the ancestry check
read a stale chain and a directory could be moved inside itself; a directory
now has one dentry, and tmpfs refuses the move under its own locks as well
(628d546). A racing `open(O_CREAT)` without `O_EXCL` failed with `EEXIST`
(e626316); `..` in a listing carried the directory's own inode number
(ed9f02b); a rename over an empty directory kept it alive (7ab4e3c); and
`openat` at the descriptor limit created the file before failing with
`EMFILE` (16ed9b7). Each fix came with a host test that failed before it. The
gap was that every self-check measured the frames it leaked and printed the
count without failing on it; each now fails on a non-zero count, and each
assertion was shown to fire by leaking a frame on purpose. `vfs_ops` gained
the property the worst bug broke: after every input, the namespace is still a
tree. After the stage was called done, the path check once failed under load
with frames it had not leaked: the programs the syscall check had just run
were reaped inside its measured window. It now waits for the reaper before its
first count, and fails on a count that rises (e8c98da). The pipe and
filesystem call check later failed the same way (FX-0860), so every frame
count in stages 6 to 9 now waits at both edges of its window until no exited
task is left unreaped, a condition rather than a delay, and prints the signed
difference when it fails.

**Left for later stages.**

* `/proc/loadavg`, which `top`'s load average line reads, and
  `/proc/diskstats`, which `iostat` needs past its processor report; stage
  11's block core now has the disks it would count.

**Exit:** `busybox ls -R /proc`, `cat /proc/self/maps` and a shell script that
manipulates files under tmpfs, all under the boot test.

---

