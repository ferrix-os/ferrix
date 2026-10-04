//! The initramfs: a cpio "newc" archive the loader hands the kernel.
//!
//! Written here rather than by `cpio` or `bsdcpio` for the reason `fat.rs`
//! writes FAT32: one fewer host tool, and an archive that is the same bytes on
//! every machine, because every timestamp, inode number and owner below is a
//! constant rather than something read off the build host's filesystem.
//!
//! # What is in it
//!
//! The directories a Linux userland expects to find — `/bin`, `/dev`, `/etc`,
//! `/proc`, `/tmp` — and a few files the kernel's stage 8 self-check reads back
//! through the VFS: a marker with known contents, a hard link to it and a
//! symbolic link to it. Each is a shape the unpacker has to get right, placed
//! where a check that the unpack happened can find it.
//!
//! Given a program — `test-vfs`, or `build` and `run` with `--init` or
//! `FERRIX_INIT` — the archive also carries it at `/bin/busybox`, zinc at
//! `/bin/zinc` and uutils/coreutils at `/bin/coreutils`, and a link in `/bin`
//! for every name each of them owns: `sh` is zinc's, the hundred names in
//! [`UTILITIES`] are uutils', and busybox gets the rest of [`APPLETS`].
//! Beside zinc it carries oh-my-zsh, at [`omz::DIRECTORY`] with the
//! `/etc/zshrc` that sources it and zsh's function tree at
//! [`omz::FUNCTIONS`], so the shell comes up configured.
//! Without a program it is the same bytes it was before programs could be
//! added.

use std::path::Path;

use crate::{Error, Result};
use crate::{native, omz, ports};

/// Every timestamp in the archive: 2026-01-01 00:00:00 UTC, the same instant
/// `fat.rs` stamps the boot image with.
pub(crate) const FIXED_MTIME: u32 = 1_767_225_600;

/// Where the marker is unpacked, relative to the root.
pub(crate) const MARKER_PATH: &str = "etc/ferrix/initramfs";

/// The marker's contents. The kernel's self-check compares them byte for byte,
/// and `src/kernel/src/fs/check.rs` carries the same string.
pub(crate) const MARKER: &[u8] =
    b"unpacked by the kernel from a cpio archive the loader handed it\n";

/// The magic of a plain newc header.
const MAGIC: &[u8] = b"070701";

/// The owner of everything in the archive but a user's own home directory:
/// uid and gid zero, as an archive built on any host must be.
const ROOT: (u32, u32) = (0, 0);

/// The user the image carries beside root, for `test-vfs` to become.
pub(crate) const USER: (u32, u32) = (1000, 1000);

/// `c_mode` type bits.
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// A newc archive being written.
struct Newc {
    bytes: Vec<u8>,
    next_ino: u32,
}

impl Newc {
    fn new() -> Newc {
        Newc {
            bytes: Vec::new(),
            next_ino: 1,
        }
    }

    fn pad(&mut self) {
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
    }

    fn entry(
        &mut self,
        name: &str,
        mode: u32,
        ino: u32,
        nlink: u32,
        (uid, gid): (u32, u32),
        data: &[u8],
    ) -> Result<()> {
        let size = u32::try_from(data.len())
            .map_err(|_| Error::new(format!("{name} is too large for a newc entry")))?;
        let name_size = u32::try_from(name.len() + 1)
            .map_err(|_| Error::new(format!("{name} is too long for a newc entry")))?;
        // ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor,
        // rdevmajor, rdevminor, namesize, check.
        let fields = [
            ino,
            mode,
            uid,
            gid,
            nlink,
            FIXED_MTIME,
            size,
            0,
            0,
            0,
            0,
            name_size,
            0,
        ];
        self.bytes.extend_from_slice(MAGIC);
        for field in fields {
            self.bytes
                .extend_from_slice(format!("{field:08X}").as_bytes());
        }
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.push(0);
        self.pad();
        self.bytes.extend_from_slice(data);
        self.pad();
        Ok(())
    }

    fn ino(&mut self) -> u32 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn directory(&mut self, name: &str, permissions: u32) -> Result<()> {
        self.directory_owned(name, permissions, ROOT)
    }

    /// A directory belonging to `owner`: a user's home, which has to be the
    /// user's or they cannot write in it.
    fn directory_owned(&mut self, name: &str, permissions: u32, owner: (u32, u32)) -> Result<()> {
        let ino = self.ino();
        self.entry(name, S_IFDIR | permissions, ino, 2, owner, &[])
    }

    fn file(&mut self, name: &str, permissions: u32, data: &[u8]) -> Result<()> {
        let ino = self.ino();
        self.entry(name, S_IFREG | permissions, ino, 1, ROOT, data)
    }

    fn symlink(&mut self, name: &str, target: &str) -> Result<()> {
        let ino = self.ino();
        self.entry(name, S_IFLNK | 0o777, ino, 1, ROOT, target.as_bytes())
    }

    /// One file with several names. newc repeats the inode number on each and
    /// carries the data on the last, which is what GNU cpio writes and what an
    /// unpacker has to cope with.
    fn hard_linked(&mut self, names: &[&str], permissions: u32, data: &[u8]) -> Result<()> {
        let ino = self.ino();
        let nlink = u32::try_from(names.len()).map_err(|_| Error::new("too many hard links"))?;
        for (at, name) in names.iter().enumerate() {
            let body = if at + 1 == names.len() { data } else { &[] };
            self.entry(name, S_IFREG | permissions, ino, nlink, ROOT, body)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>> {
        self.entry("TRAILER!!!", 0, 0, 1, ROOT, &[])?;
        Ok(self.bytes)
    }
}

/// An archive of `directories` and then `files`, each a path, its
/// permission bits and its bytes: for a guest that is not Ferrix, such as the
/// stock Linux kernel the seam's measurement compares against
/// (`crate::seam`).
///
/// # Errors
///
/// A name or a file too large for a newc entry.
pub(crate) fn plain(directories: &[&str], files: &[(&str, u32, &[u8])]) -> Result<Vec<u8>> {
    let mut archive = Newc::new();
    for directory in directories {
        archive.directory(directory, 0o755)?;
    }
    for (name, permissions, data) in files {
        archive.file(name, *permissions, data)?;
    }
    archive.finish()
}

/// An archive of `entries`: a package (`crate::apps`). Every directory an
/// entry is in comes first, in name order, then the files and links in the
/// order given.
///
/// # Errors
///
/// A name or a file too large for a newc entry.
pub(crate) fn package(entries: &[ports::File]) -> Result<Vec<u8>> {
    let mut directories: Vec<&str> = Vec::new();
    for entry in entries {
        if entry.content == ports::Content::Directory {
            directories.push(&entry.path);
        }
        let mut at = entry.path.as_str();
        while let Some((parent, _)) = at.rsplit_once('/') {
            directories.push(parent);
            at = parent;
        }
    }
    directories.sort_unstable();
    directories.dedup();
    let mut archive = Newc::new();
    for directory in directories {
        archive.directory(directory, 0o755)?;
    }
    for entry in entries {
        match &entry.content {
            ports::Content::Bytes(data) => archive.file(&entry.path, entry.mode, data)?,
            ports::Content::Link(target) => archive.symlink(&entry.path, target)?,
            ports::Content::Directory => {}
        }
    }
    archive.finish()
}

/// Where an image carries pid 1's inputs: the kernel reads them from the
/// archive and nothing unpacks them (`src/kernel/src/init.rs`,
/// `src/lib/fs/vfs/src/initramfs.rs`).
pub(crate) const INIT_DIRECTORY: &str = ".ferrix/init";

/// `archive` with pid 1's inputs added under [`INIT_DIRECTORY`]: each of
/// `inputs` is a name there (`program`, `script` or `commands`) and its
/// bytes. Every entry of `archive` stays as it was; the directories and the
/// files are appended before the trailer, each file with one link.
///
/// # Errors
///
/// An archive that does not parse, or one that carries the directory already.
pub(crate) fn with_init_inputs(archive: &[u8], inputs: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut out = Newc::new();
    let mut last_ino = 0;
    for entry in ferrix_cpio::Archive::new(archive).entries() {
        let entry = entry.map_err(|error| Error::new(format!("reading the archive: {error:?}")))?;
        if entry.name.trim_start_matches("./").starts_with(".ferrix") {
            return Err(Error::new(format!(
                "the initramfs carries {} already, where pid 1's inputs go",
                entry.name
            )));
        }
        last_ino = last_ino.max(entry.ino);
        out.entry(
            entry.name,
            entry.mode,
            entry.ino,
            entry.nlink,
            (entry.uid, entry.gid),
            entry.data,
        )?;
    }
    if inputs.is_empty() {
        return out.finish();
    }
    out.entry(".ferrix", 0o040_755, last_ino + 1, 2, (0, 0), b"")?;
    out.entry(INIT_DIRECTORY, 0o040_755, last_ino + 2, 2, (0, 0), b"")?;
    for (offset, (name, data)) in (3..).zip(inputs) {
        out.entry(
            &format!("{INIT_DIRECTORY}/{name}"),
            0o100_644,
            last_ino + offset,
            1,
            (0, 0),
            data,
        )?;
    }
    out.finish()
}

/// Pid 1's inputs as `archive` carries them, by name under
/// [`INIT_DIRECTORY`].
///
/// # Errors
///
/// An archive that does not parse.
pub(crate) fn init_inputs(archive: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let prefix = format!("{INIT_DIRECTORY}/");
    let mut found = Vec::new();
    for entry in ferrix_cpio::Archive::new(archive).entries() {
        let entry = entry.map_err(|error| Error::new(format!("reading the archive: {error:?}")))?;
        if let Some(name) = entry.name.trim_start_matches("./").strip_prefix(&prefix) {
            found.push((name.to_owned(), entry.data.to_vec()));
        }
    }
    Ok(found)
}

/// `archive` again, with each regular file's contents replaced where `change`
/// returns new ones, and every other entry and header field as it was.
///
/// For what a card is given that an image is not -- its programs without
/// their symbols, which `flash` strips -- after the archive is built, so that
/// [`build`] and every image stay the bytes they are. A hard link's names
/// are left alone: only the entry carrying the data has any to change.
///
/// # Errors
///
/// An archive that does not parse, or what `change` returned.
pub(crate) fn with_files_changed(
    archive: &[u8],
    mut change: impl FnMut(&str, &[u8]) -> Result<Option<Vec<u8>>>,
) -> Result<Vec<u8>> {
    let mut out = Newc::new();
    for entry in ferrix_cpio::Archive::new(archive).entries() {
        let entry = entry.map_err(|error| Error::new(format!("reading the archive: {error:?}")))?;
        let changed = if entry.mode & 0o170_000 == S_IFREG && !entry.data.is_empty() {
            change(entry.name, entry.data)?
        } else {
            None
        };
        out.entry(
            entry.name,
            entry.mode,
            entry.ino,
            entry.nlink,
            (entry.uid, entry.gid),
            changed.as_deref().unwrap_or(entry.data),
        )?;
    }
    out.finish()
}

/// Where a program given with `--init` goes, relative to the root.
pub(crate) const PROGRAM_PATH: &str = "bin/busybox";

/// Every applet a busybox 1.37 may provide, each linked in `/bin` beside the
/// program: the union of `busybox --list` from Ubuntu's build and from Alpine's
/// `busybox-static`, less `busybox` itself, whose name the program already has.
///
/// Written out rather than asked of the program, because the program is built
/// for the target and this runs on the host. A link to an applet a given
/// binary lacks costs one directory entry, and running it prints `applet not
/// found`. The kernel starts a program by `argv[0]` and needs none of them;
/// they are there so that the shell's `PATH=/bin` finds a command where a
/// person types it, and so that `cargo xtask test-vfs` can name its programs.
/// Sorted, so that a name is added in one obvious place.
#[rustfmt::skip]
pub(crate) const APPLETS: &[&str] = &[
    "[", "[[", "acpid", "add-shell", "addgroup", "adduser", "adjtimex", "ar", "arch",
    "arp", "arping", "ascii", "ash", "awk", "base64", "basename", "bbconfig", "bc",
    "beep", "blkdiscard", "blkid", "blockdev", "brctl", "bunzip2", "bzcat", "bzip2",
    "cal", "cat", "chattr", "chgrp", "chmod", "chown", "chpasswd", "chroot", "chvt",
    "cksum", "clear", "cmp", "comm", "cp", "cpio", "crc32", "crond", "crontab",
    "cryptpw", "cttyhack", "cut", "date", "dc", "dd", "deallocvt", "delgroup",
    "deluser", "depmod", "devmem", "df", "diff", "dirname", "dmesg", "dnsdomainname",
    "dos2unix", "dpkg", "dpkg-deb", "du", "dumpkmap", "dumpleases", "echo", "ed",
    "egrep", "eject", "env", "ether-wake", "expand", "expr", "factor", "fallocate",
    "false", "fatattr", "fbset", "fbsplash", "fdflush", "fdisk", "fgrep", "find",
    "findfs", "flock", "fold", "free", "freeramdisk", "fsck", "fsfreeze", "fstrim",
    "fsync", "ftpget", "ftpput", "fuser", "getfattr", "getopt", "getty", "grep",
    "groups", "gunzip", "gzip", "halt", "hd", "head", "hexdump", "hostid", "hostname",
    "httpd", "hwclock", "i2cdetect", "i2cdump", "i2cget", "i2cset", "i2ctransfer", "id",
    "ifconfig", "ifdown", "ifenslave", "ifup", "init", "inotifyd", "insmod", "install",
    "ionice", "iostat", "ip", "ipaddr", "ipcalc", "ipcrm", "ipcs", "iplink", "ipneigh",
    "iproute", "iprule", "iptunnel", "kbd_mode", "kill", "killall", "killall5", "klogd",
    "last", "less", "link", "linux32", "linux64", "linuxrc", "ln", "loadfont",
    "loadkmap", "logger", "login", "logname", "logread", "losetup", "ls", "lsattr",
    "lsmod", "lsof", "lsscsi", "lsusb", "lzcat", "lzma", "lzop", "lzopcat", "makemime",
    "md5sum", "mdev", "mesg", "microcom", "mim", "mkdir", "mkdosfs", "mke2fs", "mkfifo",
    "mkfs.vfat", "mknod", "mkpasswd", "mkswap", "mktemp", "modinfo", "modprobe", "more",
    "mount", "mountpoint", "mpstat", "mt", "mv", "nameif", "nanddump", "nandwrite",
    "nbd-client", "nc", "netstat", "nice", "nl", "nmeter", "nohup", "nologin", "nproc",
    "nsenter", "nslookup", "ntpd", "nuke", "od", "openvt", "partprobe", "passwd",
    "paste", "patch", "pgrep", "pidof", "ping", "ping6", "pipe_progress", "pivot_root",
    "pkill", "pmap", "poweroff", "printenv", "printf", "ps", "pscan", "pstree", "pwd",
    "pwdx", "raidautorun", "rdate", "rdev", "readahead", "readlink", "realpath",
    "reboot", "reformime", "remove-shell", "renice", "reset", "resize", "resume", "rev",
    "rfkill", "rm", "rmdir", "rmmod", "route", "rpm", "rpm2cpio", "run-init",
    "run-parts", "sed", "sendmail", "seq", "setconsole", "setfont", "setkeycodes",
    "setlogcons", "setpriv", "setserial", "setsid", "sh", "sha1sum", "sha256sum",
    "sha3sum", "sha512sum", "showkey", "shred", "shuf", "slattach", "sleep", "sort",
    "split", "ssl_client", "start-stop-daemon", "stat", "static-sh", "strings", "stty",
    "su", "sulogin", "sum", "svc", "svok", "swapoff", "swapon", "switch_root", "sync",
    "sysctl", "syslogd", "tac", "tail", "tar", "taskset", "tc", "tee", "telnet",
    "telnetd", "test", "tftp", "time", "timeout", "top", "touch", "tr", "traceroute",
    "traceroute6", "tree", "true", "truncate", "ts", "tty", "ttysize", "tunctl",
    "ubirename", "udhcpc", "udhcpc6", "udhcpd", "uevent", "umount", "uname",
    "uncompress", "unexpand", "uniq", "unix2dos", "unlink", "unlzma", "unlzop",
    "unshare", "unxz", "unzip", "uptime", "usleep", "uudecode", "uuencode", "vconfig",
    "vi", "vlock", "volname", "w", "watch", "watchdog", "wc", "wget", "which", "who",
    "whoami", "whois", "xargs", "xxd", "xz", "xzcat", "yes", "zcat", "zcip",
];

/// One program the uutils family installs, and the names it answers to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binary {
    /// Its own name, which is also where it goes: `bin/<name>`.
    pub(crate) name: &'static str,
    /// The other names linked to it in `/bin`. Empty for a program that
    /// answers only to its own, as findutils' four do.
    pub(crate) links: &'static [&'static str],
}

/// The family, as `src/user/system/linux/ferrousli/tools/uutils/` builds it.
///
/// Three projects and six programs. coreutils and diffutils are multicall
/// binaries, which pick their utility from `argv[0]`; findutils builds one
/// program per utility instead, so each answers only to its own name.
pub(crate) const FAMILY: &[Binary] = &[
    Binary {
        name: "coreutils",
        links: UTILITIES,
    },
    Binary {
        name: "diffutils",
        links: &["diff", "cmp"],
    },
    Binary {
        name: "find",
        links: &[],
    },
    Binary {
        name: "xargs",
        links: &[],
    },
    Binary {
        name: "locate",
        links: &[],
    },
    Binary {
        name: "updatedb",
        links: &[],
    },
];

/// The directory its utility names are linked in: `/bin`, which is `PATH`.
///
/// It was `/usr/bin` for one slice, so that uutils could be in the image
/// without any gate running it by accident. Now a name uutils provides is
/// uutils': `ls`, `cat`, `cp` and the hundred others are the Rust
/// implementations, and busybox keeps only the names uutils has not got --
/// `sysctl`, `fdisk`, `top`, `su`, the networking, and the rest of the list
/// in `docs/UUTILS.md` §5.
pub(crate) const UUTILS_DIR: &str = "bin";

/// Every utility uutils/coreutils 0.9.0 provides, each linked in
/// [`UUTILS_DIR`] beside the program: `coreutils --list`, as the binary this
/// tree pins prints it.
///
/// Written out rather than asked of the program, for the reason [`APPLETS`]
/// is: the program is built for the target and this runs on the host, which
/// may be Windows. A link to a utility a given binary lacks costs one
/// directory entry.
#[rustfmt::skip]
pub(crate) const UTILITIES: &[&str] = &[
    "[", "arch", "b2sum", "base32", "base64", "basename", "basenc", "cat",
    "chgrp", "chmod", "chown", "chroot", "cksum", "comm", "cp", "csplit",
    "cut", "date", "dd", "df", "dir", "dircolors", "dirname", "du", "echo",
    "env", "expand", "expr", "factor", "false", "fmt", "fold", "groups",
    "head", "hostid", "hostname", "id", "install", "join", "kill", "link",
    "ln", "logname", "ls", "md5sum", "mkdir", "mkfifo", "mknod", "mktemp",
    "more", "mv", "nice", "nl", "nohup", "nproc", "numfmt", "od", "paste",
    "pathchk", "pinky", "pr", "printenv", "printf", "ptx", "pwd",
    "readlink", "realpath", "rm", "rmdir", "seq", "sha1sum", "sha224sum",
    "sha256sum", "sha384sum", "sha512sum", "shred", "shuf", "sleep", "sort",
    "split", "stat", "stty", "sum", "sync", "tac", "tail", "tee", "test",
    "timeout", "touch", "tr", "true", "truncate", "tsort", "tty", "uname",
    "unexpand", "uniq", "unlink", "uptime", "users", "vdir", "wc", "who",
    "whoami", "yes",
];

/// Where zinc, the zsh-compatible shell, goes beside a program.
pub(crate) const ZINC_PATH: &str = "bin/zinc";

/// The names zinc owns in `/bin` when it is in the image, and busybox
/// therefore does not get a link for.
///
/// `sh` is the one that matters: it is what every script in the image and in
/// `cargo xtask test-vfs` is run by, so this line is what makes zinc the
/// shell rather than a program sitting beside busybox. `zsh` is zinc's own
/// name, which busybox never answers to.
///
/// Not `ash` or `static-sh`: those are busybox's own names for its own shell,
/// and a person who asks for busybox's shell by name should get it.
pub(crate) const ZINC_NAMES: &[&str] = &["sh", "zsh"];

/// Where busybox's `udhcpc` looks for the script it runs as a lease changes.
const UDHCPC_SCRIPT_PATH: &str = "usr/share/udhcpc/default.script";

/// That script: `udhcpc` runs it with `deconfig` before it asks, and with
/// `bound` or `renew` and the lease in its environment -- `ip`, `mask` as a
/// prefix length, `router` and `dns` as lists -- once it has one. Written for
/// `ip`, which is how every other part of this image configures a network, and
/// after busybox's own `examples/udhcp/simple.script`.
const UDHCPC_SCRIPT: &[u8] = b"\
#!/bin/sh
# Written by cargo xtask: apply what udhcpc was given to the interface.
case \"$1\" in
deconfig)
\tip link set \"$interface\" up
\tfor old in $(ip -o -4 addr show dev \"$interface\" | sed -n 's/.* inet \\([^ ]*\\).*/\\1/p'); do
\t\tip addr del \"$old\" dev \"$interface\"
\tdone
\t;;
bound|renew)
\tip addr add \"$ip/${mask:-24}\" dev \"$interface\" 2>/dev/null
\tif [ -n \"$router\" ]; then
\t\tip route del default dev \"$interface\" 2>/dev/null
\t\tfor gateway in $router; do
\t\t\tip route add default via \"$gateway\" dev \"$interface\" && break
\t\tdone
\tfi
\tif [ -n \"$dns\" ]; then
\t\t: > /etc/resolv.conf
\t\tfor server in $dns; do
\t\t\techo \"nameserver $server\" >> /etc/resolv.conf
\t\tdone
\tfi
\techo \"$interface: $ip/${mask:-24} by DHCP, router ${router:-none}, DNS ${dns:-none}\"
\t;;
esac
";

/// `/etc/fstab`: the two btrfs volumes the kernel mounts itself, when their
/// disks are there. The device is written as `/proc/mounts` writes it; a
/// reader that finds a mount point here that is not mounted skips it.
const FSTAB: &[u8] = b"\
# Written by cargo xtask. Ferrix mounts these itself, from the kernel's
# command line, and nothing mounts from this file: it says what they are.
# <device> <mount point> <type> <options> <dump> <pass>
btrfs / btrfs rw 0 0
btrfs /data btrfs rw 0 0
";

/// `/etc/profile`, which the kernel's interactive shell reads through `ENV`.
///
/// Configures `eth0` by DHCP, as a distribution's network setup would: busybox's
/// `udhcpc` asks, and [`UDHCPC_SCRIPT`] applies the address, the route and the
/// resolvers the lease names. Only when there is an `eth0` and it has no IPv4
/// address, so a boot without `--net` is quiet, a nested `sh -i` changes
/// nothing, and neither does an interface somebody configured by hand first.
/// `udhcpc`'s own progress lines go to its standard error and are dropped; the
/// script's one line says what was configured, and a failure says so too.
const PROFILE: &[u8] = b"\
# Written by cargo xtask: configure eth0 by DHCP, once.
if ip link show eth0 >/dev/null 2>&1 && ! ip -o addr show eth0 | grep -q ' inet '; then
\tudhcpc -i eth0 -n -q -t 5 -T 2 2>/dev/null || echo 'eth0: no DHCP lease'
fi
";

/// The names linked to the family program called `name`, or none if the
/// family has no such program.
fn links_of(name: &str) -> &'static [&'static str] {
    FAMILY
        .iter()
        .find(|binary| binary.name == name)
        .map_or(&[], |binary| binary.links)
}

/// Whether `applet` is a name one of the carried family programs owns, and so
/// one busybox does not get a link for.
///
/// A program that is not carried owns nothing: an image for an architecture
/// the family is not built for still has busybox's `ls`.
fn owned_by_family(carried: &[(&str, Vec<u8>)], applet: &str) -> bool {
    carried
        .iter()
        .any(|(name, _)| *name == applet || links_of(name).contains(&applet))
}

/// The archive every image carries: the tree's native programs in `/sbin`,
/// and `program` at `/bin/busybox` when one is given, with `zinc` at
/// `/bin/zinc` and `/bin/zsh` beside it when that is given too, and the
/// installed `ports` at their own paths.
pub(crate) fn build(
    program: Option<&Path>,
    natives: &[native::Built],
    zinc: Option<&[u8]>,
    ports: &[ports::File],
) -> Result<Vec<u8>> {
    let program = program
        .map(|path| {
            std::fs::read(path)
                .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
        })
        .transpose()?;
    build_with_shell(program.as_deref(), natives, zinc, &[], ports)
}

/// [`build`], with uutils/coreutils carried too.
///
/// A separate entry point rather than a fifth argument to [`build`]: most
/// callers carry no program at all, and would all have to say `None` twice.
pub(crate) fn build_with_utilities(
    program: Option<&Path>,
    natives: &[native::Built],
    zinc: Option<&[u8]>,
    uutils: &[(&str, Vec<u8>)],
    ports: &[ports::File],
) -> Result<Vec<u8>> {
    let program = program
        .map(|path| {
            std::fs::read(path)
                .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
        })
        .transpose()?;
    build_with_shell(program.as_deref(), natives, zinc, uutils, ports)
}

/// Whether busybox gets the name `applet` in `/bin`. A name there can only be
/// one program: zinc owns `sh` when it is carried, uutils owns every name it
/// implements, and a carried program owns its own -- the desktop's `reboot`,
/// which can say where the machine comes back up. busybox gets the rest,
/// which is still most of the list.
fn busybox_owns(
    applet: &str,
    zinc: bool,
    uutils: &[(&str, Vec<u8>)],
    ports: &[ports::File],
) -> bool {
    let path = format!("bin/{applet}");
    !(zinc && ZINC_NAMES.contains(&applet)
        || owned_by_family(uutils, applet)
        || ports.iter().any(|file| file.path == path))
}

/// [`build`], with the program's bytes rather than its path and no zinc.
#[cfg(test)]
fn build_with(program: Option<&[u8]>, natives: &[native::Built]) -> Result<Vec<u8>> {
    build_with_shell(program, natives, None, &[], &[])
}

/// [`build`], with the program's bytes rather than its path.
fn build_with_shell(
    program: Option<&[u8]>,
    natives: &[native::Built],
    zinc: Option<&[u8]>,
    uutils: &[(&str, Vec<u8>)],
    ports: &[ports::File],
) -> Result<Vec<u8>> {
    // A sabotaged authd goes into test-auth's control images and no other.
    crate::auth::refuse_sabotaged(ports)?;
    let mut archive = Newc::new();
    archive.directory(".", 0o755)?;
    for (name, permissions) in [
        ("bin", 0o755),
        ("dev", 0o755),
        ("etc", 0o755),
        ("etc/ferrix", 0o755),
        ("proc", 0o555),
        ("tmp", 0o1777),
    ] {
        archive.directory(name, permissions)?;
    }
    archive.file("etc/hostname", 0o644, b"ferrix\n")?;
    let link = format!("{MARKER_PATH}.link");
    archive.hard_linked(&[MARKER_PATH, &link], 0o644, MARKER)?;
    archive.symlink(&format!("{MARKER_PATH}.symlink"), "initramfs")?;
    // Only when there are any, so an archive without them is the bytes it was
    // before native programs existed.
    if !natives.is_empty() {
        archive.directory(native::DIRECTORY, 0o755)?;
        archive.directory("lib", 0o755)?;
        archive.directory(native::DRIVERS, 0o755)?;
        let mut manifest = String::new();
        for built in natives {
            let path = format!("{}/{}", built.directory, built.name);
            archive.file(&path, 0o755, &built.bytes)?;
            if built.directory == native::DRIVERS {
                manifest.push_str(built.name);
                manifest.push('\n');
            }
        }
        // What the kernel reads to know the drivers, one name per line.
        let path = format!("{}/{}", native::DRIVERS, native::MANIFEST);
        archive.file(&path, 0o644, manifest.as_bytes())?;
    }
    if let Some(program) = program {
        // Who uid 0 is, for the applets that ask by name -- `whoami`, `id`,
        // `ls -l` -- and a user who is not root, whom `test-vfs` becomes with
        // `su` to be refused what only root may do. Only beside a program, so
        // the archive without one stays the bytes the kernel's boot check
        // reads.
        archive.file(
            "etc/passwd",
            0o644,
            b"root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/home/ferrix:/bin/zsh\n",
        )?;
        archive.file("etc/group", 0o644, b"root:x:0:\nferrix:x:1000:\n")?;
        // Where the C library's resolver asks. `10.0.2.3` is the gateway's
        // forwarder, which is where slirp puts one too, so a guest configured
        // by DHCP and a guest configured by hand agree.
        archive.file("etc/resolv.conf", 0o644, b"nameserver 10.0.2.3\n")?;
        // What is mounted, for the programs that read it. Ferrix mounts `/`
        // and `/data` itself, from the command line (`src/kernel/src/fs/root_disk.rs`,
        // `data_disk.rs`), and nothing here mounts from this file. But every
        // Linux has one, and btop reads its list of disks from it by default:
        // with none, its memory panel's collection failed every time on the
        // file's missing timestamp, and the disks box stayed empty.
        archive.file("etc/fstab", 0o644, FSTAB)?;
        // What the interactive shell runs first, through `ENV`: `eth0` by
        // DHCP, when there is one and nothing has configured it, with the
        // script `udhcpc` runs as the lease comes. `test-net` runs `udhcpc`
        // itself and never starts an interactive shell.
        archive.file("etc/profile", 0o644, PROFILE)?;
        for directory in ["usr", "usr/share", "usr/share/udhcpc"] {
            archive.directory(directory, 0o755)?;
        }
        archive.file(UDHCPC_SCRIPT_PATH, 0o755, UDHCPC_SCRIPT)?;
        // A home the user owns: `su - ferrix` starts there, and a home root
        // owned would be a home its user cannot write in.
        archive.directory("home", 0o755)?;
        archive.directory_owned("home/ferrix", 0o755, USER)?;
        archive.file(PROGRAM_PATH, 0o755, program)?;
        for applet in APPLETS {
            if busybox_owns(applet, zinc.is_some(), uutils, ports) {
                archive.symlink(&format!("bin/{applet}"), "busybox")?;
            }
        }
    }
    // uutils/coreutils, whether or not a program is beside it, for zinc's
    // reason below: an image whose pid 1 is `/sbin/init` carries no program,
    // and its getty's shell found no `sleep` while the archive had been
    // handed every utility (`docs/INIT.md`, L10). The boot check carries none
    // and its archive stays the bytes it was.
    for (name, bytes) in uutils {
        archive.file(&format!("{UUTILS_DIR}/{name}"), 0o755, bytes)?;
        // Relative, as busybox's are: the links are in the directory the
        // program is in.
        for link in links_of(name) {
            archive.symlink(&format!("{UUTILS_DIR}/{link}"), name)?;
        }
    }
    // zinc, whether or not busybox is beside it: `run-compositor` boots the
    // compositor as init, with no program carried at all, and starts zinc on
    // a pseudoterminal directly (`docs/DISPLAY.md`'s desktop). It was written
    // only inside the branch above, which needs a program, so that boot
    // forked a shell that was never actually in the archive -- the same
    // mistake the ports below were already moved out of. Outside the branch,
    // because a caller that hands over zinc's bytes means it to be there
    // whether or not busybox is too; the boot check carries neither and its
    // archive stays the bytes it was.
    if let Some(zinc) = zinc {
        archive.file(ZINC_PATH, 0o755, zinc)?;
        for name in ZINC_NAMES {
            archive.symlink(&format!("bin/{name}"), "zinc")?;
        }
    }
    // oh-my-zsh, wherever zinc is: the configuration the shell starts with,
    // carried by the image rather than installed into a guest by hand, since
    // a guest may have no network to install it over and would lose it at the
    // next boot in any case. `/etc/zshrc`, which sources it, comes with it.
    let configuration = omz::beside(zinc)?;
    // The files a caller asked to carry: the ports, and whatever else a test
    // needs beside init -- `cargo xtask test-compositor` carries the
    // compositor's clients this way. Outside the branch above, because a file
    // named here is one the caller means to be there whether or not a shell
    // is; the boot check's archive is unchanged either way, since it names
    // none. `bin` and `etc` are made above; any other directory a file is in
    // is made the first time one needs it.
    let mut made = vec!["bin".to_owned(), "etc".to_owned()];
    // The native programs' directories are made above too, and a library
    // `test-shell` carries goes in `lib` beside the drivers.
    if !natives.is_empty() {
        made.extend([native::DIRECTORY, "lib", native::DRIVERS].map(str::to_owned));
    }
    for file in ports.iter().chain(&configuration) {
        let mut directory = String::new();
        let parents = file.path.split('/').collect::<Vec<_>>();
        for name in parents.iter().take(parents.len().saturating_sub(1)) {
            if !directory.is_empty() {
                directory.push('/');
            }
            directory.push_str(name);
            if !made.contains(&directory) {
                archive.directory(&directory, 0o755)?;
                made.push(directory.clone());
            }
        }
        match &file.content {
            ports::Content::Bytes(bytes) => archive.file(&file.path, file.mode, bytes)?,
            ports::Content::Link(target) => archive.symlink(&file.path, target)?,
            ports::Content::Directory => {
                if !made.contains(&file.path) {
                    archive.directory(&file.path, file.mode)?;
                    made.push(file.path.clone());
                }
            }
        }
    }
    archive.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_carried_program_keeps_its_name_from_busybox() {
        let reboot = [ports::File {
            path: "bin/reboot".to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(b"\x7fELF".to_vec()),
        }];
        assert!(
            !busybox_owns("reboot", false, &[], &reboot),
            "the carried reboot"
        );
        assert!(
            busybox_owns("reboot", false, &[], &[]),
            "busybox's otherwise"
        );
        assert!(busybox_owns("ls", false, &[], &reboot), "every other name");
        assert!(!busybox_owns("sh", true, &[], &[]), "zinc's sh");
    }

    #[test]
    fn a_file_asked_for_is_carried_without_a_shell_beside_it() {
        // `cargo xtask test-compositor` boots the compositor as init, with no
        // busybox at all, and its clients have to be in the archive. They were
        // not until this moved out of the branch that needs a program.
        let program: &[u8] = b"\x7fELF not really";
        let carried = [ports::File {
            path: "bin/pattern".to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(program.to_vec()),
        }];
        let bytes = build(None, &[], None, &carried).unwrap();
        let archive = ferrix_cpio::Archive::new(&bytes);
        let found = archive.find("bin/pattern").unwrap().unwrap();
        assert_eq!(found.data, program);
        assert_eq!(found.mode & 0o777, 0o755, "it has to be runnable");
        // And an archive that was asked for none is the one it always was,
        // which is what keeps the boot check's bytes the same.
        assert_eq!(
            build(None, &[], None, &[]).unwrap(),
            build(None, &[], None, &[]).unwrap()
        );
        assert!(
            ferrix_cpio::Archive::new(&build(None, &[], None, &[]).unwrap())
                .find("bin/pattern")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_file_changed_after_the_archive_is_built_reads_back_changed_and_nothing_else_moves() {
        let program: &[u8] = b"\x7fELF with its symbols";
        let bytes = build_with(Some(program), &[]).unwrap();
        // Nothing changed is the same bytes, so the rewrite loses no field.
        assert_eq!(with_files_changed(&bytes, |_, _| Ok(None)).unwrap(), bytes);

        let changed = with_files_changed(&bytes, |name, data| {
            Ok((name == PROGRAM_PATH).then(|| data[..4].to_vec()))
        })
        .unwrap();
        let archive = ferrix_cpio::Archive::new(&changed);
        assert_eq!(
            archive.find(PROGRAM_PATH).unwrap().unwrap().data,
            b"\x7fELF"
        );
        // The hard link's data rides on its last name.
        let link = format!("{MARKER_PATH}.link");
        assert_eq!(archive.find(&link).unwrap().unwrap().data, MARKER);
        assert_eq!(
            archive.summary().unwrap().entries,
            ferrix_cpio::Archive::new(&bytes).summary().unwrap().entries
        );
    }

    #[test]
    fn the_archive_is_the_same_bytes_every_time() {
        assert_eq!(
            build(None, &[], None, &[]).unwrap(),
            build(None, &[], None, &[]).unwrap()
        );
        let program: &[u8] = b"\x7fELF not really";
        assert_eq!(
            build_with(Some(program), &[]).unwrap(),
            build_with(Some(program), &[]).unwrap()
        );
    }

    #[test]
    fn a_program_goes_in_bin_with_its_applets_beside_it() {
        let program: &[u8] = b"\x7fELF not really";
        let bytes = build_with(Some(program), &[]).unwrap();
        let archive = ferrix_cpio::Archive::new(&bytes);
        assert_eq!(archive.find(PROGRAM_PATH).unwrap().unwrap().data, program);
        for applet in APPLETS {
            let link = archive.find(&format!("bin/{applet}")).unwrap().unwrap();
            assert_eq!(link.symlink_target(), Some("busybox"), "bin/{applet}");
        }
        let in_bin = archive
            .entries()
            .map(|entry| entry.unwrap().name)
            .filter(|name| name.starts_with("bin/"))
            .count();
        assert_eq!(
            in_bin,
            APPLETS.len() + 1,
            "the program and one link per applet"
        );
        assert!(
            archive
                .entries()
                .all(|entry| ferrix_cpio::is_safe_path(entry.unwrap().name)),
            "every applet name joins to /bin without escaping it"
        );
    }

    #[test]
    fn zinc_goes_in_bin_as_zinc_and_zsh_whether_or_not_a_program_is_beside_it() {
        let with = build_with_shell(Some(b"program"), &[], Some(b"shell"), &[], &[]).unwrap();
        let without_program = build_with_shell(None, &[], Some(b"shell"), &[], &[]).unwrap();
        let without_either = build_with_shell(None, &[], None, &[], &[]).unwrap();
        for archive in [&with, &without_program] {
            assert!(
                archive
                    .windows(ZINC_PATH.len())
                    .any(|w| w == ZINC_PATH.as_bytes()),
                "bin/zinc is in the archive whether or not a program is beside it"
            );
            assert!(
                archive.windows(b"bin/zsh".len()).any(|w| w == b"bin/zsh"),
                "bin/zsh links to it"
            );
        }
        assert!(
            !without_either
                .windows(ZINC_PATH.len())
                .any(|w| w == ZINC_PATH.as_bytes()),
            "without zinc's bytes the archive is the one the boot check reads"
        );
    }

    #[test]
    fn sh_is_zinc_when_zinc_is_there_and_busybox_when_it_is_not() {
        let with = build_with_shell(Some(b"program"), &[], Some(b"shell"), &[], &[]).unwrap();
        let archive = ferrix_cpio::Archive::new(&with);
        for name in ZINC_NAMES {
            assert_eq!(
                archive
                    .find(&format!("bin/{name}"))
                    .unwrap()
                    .unwrap_or_else(|| panic!("bin/{name} is linked"))
                    .symlink_target(),
                Some("zinc"),
                "bin/{name}"
            );
        }
        // busybox's own name for its own shell is still busybox's.
        assert_eq!(
            archive.find("bin/ash").unwrap().unwrap().symlink_target(),
            Some("busybox")
        );
        // And with no zinc in the image, `sh` is busybox's again, so an image
        // built for an architecture zinc is not compiled for still has one.
        let without = build_with_shell(Some(b"program"), &[], None, &[], &[]).unwrap();
        assert_eq!(
            ferrix_cpio::Archive::new(&without)
                .find("bin/sh")
                .unwrap()
                .unwrap()
                .symlink_target(),
            Some("busybox")
        );
    }

    #[test]
    fn uutils_owns_the_names_it_provides_and_busybox_keeps_the_rest() {
        let carried: Vec<(&str, Vec<u8>)> = FAMILY
            .iter()
            .map(|binary| (binary.name, binary.name.as_bytes().to_vec()))
            .collect();
        let with = build_with_shell(Some(b"program"), &[], None, &carried, &[]).unwrap();
        let archive = ferrix_cpio::Archive::new(&with);

        // Every program in the family is carried under its own name, and each
        // of its own links points at it.
        for binary in FAMILY {
            let path = format!("bin/{}", binary.name);
            let found = archive
                .find(&path)
                .unwrap()
                .unwrap_or_else(|| panic!("{path} is carried"));
            assert_eq!(found.data, binary.name.as_bytes());
            assert_eq!(found.mode & 0o777, 0o755, "{path} has to be runnable");
            for link in binary.links {
                assert_eq!(
                    archive
                        .find(&format!("bin/{link}"))
                        .unwrap()
                        .unwrap_or_else(|| panic!("bin/{link} is linked"))
                        .symlink_target(),
                    Some(binary.name),
                    "bin/{link}"
                );
            }
        }

        // Every name uutils provides is a link to it.
        for utility in ["ls", "cat", "uname", "wc"] {
            let link = archive
                .find(&format!("bin/{utility}"))
                .unwrap()
                .unwrap_or_else(|| panic!("{utility} is linked"));
            assert_eq!(link.symlink_target(), Some("coreutils"), "bin/{utility}");
        }

        // And a name it does not provide is still busybox's.
        for applet in ["sysctl", "fdisk", "top", "su", "ifconfig"] {
            assert!(
                !UTILITIES.contains(&applet),
                "{applet} is not uutils' to give"
            );
            assert_eq!(
                archive
                    .find(&format!("bin/{applet}"))
                    .unwrap()
                    .unwrap()
                    .symlink_target(),
                Some("busybox"),
                "bin/{applet}"
            );
        }

        // Without uutils the name is busybox's, so an image for an
        // architecture without it still has an `ls`.
        let no_uutils = build_with_shell(Some(b"program"), &[], None, &[], &[]).unwrap();
        assert_eq!(
            ferrix_cpio::Archive::new(&no_uutils)
                .find("bin/ls")
                .unwrap()
                .unwrap()
                .symlink_target(),
            Some("busybox")
        );

        // And without it the archive is the one it was, which is what keeps
        // the boot check's bytes the same.
        let without = build_with_shell(Some(b"program"), &[], None, &[], &[]).unwrap();
        assert!(
            ferrix_cpio::Archive::new(&without)
                .find("bin/coreutils")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn every_utility_name_is_a_name_and_they_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for utility in UTILITIES {
            assert!(seen.insert(utility), "{utility} is listed twice");
            assert!(!utility.is_empty() && !utility.contains('/'), "{utility}");
        }
        assert_eq!(seen.len(), 106, "coreutils 0.9.0 provides 106 utilities");
    }

    #[test]
    fn ports_go_at_their_paths_with_their_directories_made_once() {
        let files = [
            ports::File {
                path: "bin/curl".to_owned(),
                mode: 0o755,
                content: ports::Content::Bytes(b"curl".to_vec()),
            },
            ports::File {
                path: "etc/ssl/certs/ca-certificates.crt".to_owned(),
                mode: 0o644,
                content: ports::Content::Bytes(b"certificates".to_vec()),
            },
            ports::File {
                path: "bin/git".to_owned(),
                mode: 0o777,
                content: ports::Content::Link("../usr/bin/git".to_owned()),
            },
        ];
        let bytes = build_with_shell(Some(b"program"), &[], None, &[], &files).unwrap();
        let archive = ferrix_cpio::Archive::new(&bytes);
        assert_eq!(archive.find("bin/curl").unwrap().unwrap().data, b"curl");
        let bundle = archive
            .find("etc/ssl/certs/ca-certificates.crt")
            .unwrap()
            .unwrap();
        assert_eq!(bundle.data, b"certificates");
        let git = archive.find("bin/git").unwrap().unwrap();
        assert_eq!(git.symlink_target(), Some("../usr/bin/git"));
        let names: Vec<&str> = archive.entries().map(|entry| entry.unwrap().name).collect();
        for directory in ["etc/ssl", "etc/ssl/certs"] {
            assert_eq!(
                names.iter().filter(|name| **name == directory).count(),
                1,
                "{directory} is made once"
            );
        }
        // A file named here is carried whether or not a shell is beside it.
        // It used not to be, and the reason given was that the boot check's
        // archive must not change -- but the boot check names no files at
        // all, so what kept its bytes the same was the empty list and never
        // the branch. `cargo xtask test-compositor` boots the compositor as
        // init with no busybox, and its clients have to be somewhere.
        let without = build_with_shell(None, &[], None, &[], &files).unwrap();
        assert_eq!(
            ferrix_cpio::Archive::new(&without)
                .find("bin/curl")
                .unwrap()
                .unwrap()
                .data,
            b"curl"
        );
    }

    #[test]
    fn without_a_program_bin_is_empty() {
        let bytes = build(None, &[], None, &[]).unwrap();
        let archive = ferrix_cpio::Archive::new(&bytes);
        assert!(archive.find(PROGRAM_PATH).unwrap().is_none());
        assert!(
            archive
                .entries()
                .all(|entry| !entry.unwrap().name.starts_with("bin/")),
            "without --init there is nothing in /bin"
        );
    }

    #[test]
    fn native_programs_go_in_sbin_and_only_when_there_are_some() {
        let natives = [native::Built {
            name: "channel-echo",
            directory: native::DIRECTORY,
            bytes: b"\x7fELF native".to_vec(),
        }];
        let bytes = build_with(None, &natives).unwrap();
        let archive = ferrix_cpio::Archive::new(&bytes);
        let entry = archive.find("sbin/channel-echo").unwrap().unwrap();
        assert_eq!(entry.data, b"\x7fELF native");
        assert!(
            archive.find("sbin").unwrap().is_some(),
            "its directory is made"
        );
        assert!(
            build(None, &[], None, &[])
                .unwrap()
                .windows(4)
                .all(|window| window != b"sbin"),
            "an archive without native programs has no /sbin"
        );
    }

    #[test]
    fn the_applets_are_the_ones_a_shell_needs_and_collide_with_nothing() {
        for needed in [
            "sh", "ash", "ls", "cat", "mkdir", "uname", "echo", "[", "[[",
        ] {
            assert!(APPLETS.contains(&needed), "{needed} is missing");
        }
        assert!(
            APPLETS.windows(2).all(|pair| pair[0] < pair[1]),
            "APPLETS is sorted and has no name twice"
        );
        for applet in APPLETS {
            assert!(
                !applet.is_empty() && !applet.contains('/') && *applet != "." && *applet != "..",
                "{applet:?} is not a name in /bin"
            );
            assert_ne!(
                format!("bin/{applet}"),
                PROGRAM_PATH,
                "a link must not replace the program"
            );
        }
    }

    #[test]
    fn the_archive_reads_back_with_the_kernels_own_reader() {
        let bytes = build(None, &[], None, &[]).unwrap();
        let archive = ferrix_cpio::Archive::new(&bytes);
        let names: Vec<&str> = archive.entries().map(|entry| entry.unwrap().name).collect();
        assert_eq!(names.first(), Some(&"."));
        assert!(names.contains(&"tmp"));

        let marker = archive.find(MARKER_PATH).unwrap().unwrap();
        assert!(marker.data.is_empty(), "the data belongs on the last link");
        assert_eq!(marker.nlink, 2);
        let link = archive
            .find(&format!("{MARKER_PATH}.link"))
            .unwrap()
            .unwrap();
        assert_eq!(link.data, MARKER);
        assert_eq!(link.ino, marker.ino);

        let symlink = archive
            .find(&format!("{MARKER_PATH}.symlink"))
            .unwrap()
            .unwrap();
        assert_eq!(symlink.symlink_target(), Some("initramfs"));
        assert!(names.iter().all(|name| ferrix_cpio::is_safe_path(name)));
    }
}
