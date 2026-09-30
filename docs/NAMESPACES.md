# Namespaces: user and mount, as far as Steam needs them

Customer decision, 2026-09-28 ~20:30: build user and mount namespaces in the
kernel, enough for Steam -- its requirements check and its container,
pressure-vessel over bubblewrap (`bwrap`), in which `steamwebhelper`
normally runs. This is the first half of stage 13's namespaces
(`docs/roadmap/stage-13-namespaces-cgroups-v2-seccomp.md`); pid, network,
IPC, UTS, cgroup and time namespaces, `setns` and seccomp stay out of it.

Status: **reviewed and being built** (§12): N1 landed 2026-09-28.

---

## 1. What Steam asks for, measured

Two host traces, 2026-09-28, on nazuna (Linux 7.0, AppArmor restricting
unprivileged user namespaces), kept in
`~/.local/share/ferrix/steam-ref/userns/`:

* `req.strace` -- `strace -f` of scout's `steam-runtime-check-requirements`,
  the check `steam.sh` runs before every start and whose exit 71 ("Steam now
  requires user namespaces to be enabled") is fatal.
* `pv.strace.xz`, `pv.out`, `pv-bwrap-opts.txt` -- `strace -f` of
  `steamrt64/pv-runtime/steam-runtime-steamrt/_v2-entry-point` with the
  environment `ubuntu12_64/steamwebhelper.sh` sets (`PRESSURE_VESSEL_BATCH=1`,
  `PRESSURE_VESSEL_SHARE_HOME=1`, `STEAM_COMPAT_FLAGS=search-cwd,search-cwd-first`,
  `STEAM_COMPAT_INSTALL_PATH=.../ubuntu12_64`), running a probe
  (`/bin/sh -c 'id; cat /proc/self/uid_map ...; cat /proc/self/mountinfo'`)
  in place of `steamwebhelper`, which must never be traced (it executes
  `int3` when `TracerPid` is set; `SPIKE.md`, trap 1). pressure-vessel
  builds the same container whatever the command is.

### 1.1 The requirements check

It runs `srt-bwrap` (scout's bubblewrap, beside the checker) four times,
with the variants pressure-vessel probes too, each running `true`:

```
bwrap [--not-a-security-boundary] [--level-prefix] [--perms 0700 --dir /]
      --ro-bind /etc /etc --symlink usr/bin /bin --symlink usr/lib /lib
      --symlink usr/lib32 /lib32 --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin
      --ro-bind /usr /usr --ro-bind-try /gnu/store /gnu/store
      --ro-bind-try /nix/store /nix/store --bind /proc /proc --dev-bind /dev /dev true
```

and passes when the plain one exits 0. On the host `srt-bwrap` is refused
by AppArmor (`setting up uid map: Permission denied`) and the checker falls
back to `/usr/bin/bwrap`, which succeeds; on Ferrix there is only
`srt-bwrap`. The checker also reads `/proc/sys/kernel/unprivileged_userns_clone`
and `/proc/sys/user/max_user_namespaces`, only to word its error message.

### 1.2 What bwrap does, in order (uid 1000, the successful run)

```
getuid/getgid/geteuid; capget                    -> all sets empty
prctl(PR_CAPBSET_READ, 40/48/44/42/41)            libcap finding CAP_LAST_CAP
prctl(PR_SET_NO_NEW_PRIVS, 1)
read /proc/sys/kernel/overflowuid, overflowgid   -> "65534"  (fatal if missing)
openat("/proc", O_PATH) = 3
clone(CLONE_NEWNS|CLONE_NEWUSER|SIGCHLD)
  parent: openat(3, "<pid>/ns", O_PATH)          (fatal if missing), close;
          capset(0,0,0); PR_SET_DUMPABLE 1; waits on a signalfd
  child:  prctl(PR_CAPBSET_DROP, 0..40)           each = 0
          openat(3, "self", O_PATH)
          write self/uid_map "1000 1000 1"
          write self/setgroups "deny"
          write self/gid_map "1000 1000 1"
          mount(NULL, "/", NULL, MS_REC|MS_SLAVE)
          mount("tmpfs", "/tmp", "tmpfs", MS_NOSUID|MS_NODEV)
          chdir /tmp; mkdir newroot; mount("newroot","newroot",MS_BIND|MS_REC)
          mkdir oldroot; pivot_root("/tmp", "oldroot"); chdir "/"
          for each bind:  mkdir /newroot/X (or creat, for a file)
                          mount("/oldroot/X", "/newroot/X", MS_BIND|MS_REC)
                          open("/newroot/X", O_PATH) = 4
                          readlink("/oldroot/proc/self/fd/4") == "/newroot/X"
                          read  3:"self/mountinfo"; find the line whose mount
                            point is "/newroot/X", and every mount under it
                          mount("none", each, MS_REMOUNT|MS_BIND|<its flags>|MS_NOSUID|MS_NODEV[|MS_RDONLY])
                            (EACCES on a submount is ignored; anything else is fatal)
          symlink usr/bin /newroot/bin ... ; mkdir /newroot/{proc,dev}
          mount(oldroot/proc -> newroot/proc, MS_BIND|MS_REC) (+ remounts)
          mount(oldroot/dev  -> newroot/dev,  MS_BIND|MS_REC) (+ remounts)
          mount("oldroot", "oldroot", MS_REC|MS_PRIVATE)
          umount2("oldroot", MNT_DETACH)
          fd = open("/"); chdir("/newroot"); pivot_root(".", "."); fchdir(fd)
          umount2(".", MNT_DETACH); chdir "/"
          capset(0,0,0); PR_SET_DUMPABLE 1; umask; chdir back; execve
```

The remount flags bwrap passes are the ones it read from the options field
of `mountinfo` plus the ones it adds, so it never asks to clear one; a
remount that did clear a locked flag would be refused (§4).

### 1.3 The container pressure-vessel builds

Same machinery, larger: one `clone(CLONE_NEWNS|CLONE_NEWUSER)`, 85
recursive binds (`--bind`, `--ro-bind`, `--dev-bind` of directories, of
single files such as `/etc/machine-id`, `/etc/resolv.conf`, and of Unix
sockets such as `/run/user/1000/pulse/native` and the D-Bus sockets), 118
bind-remounts, two `tmpfs` mounts with `mode=0755`
(`/var/pressure-vessel/ldso`, `/tmp/.X11-unix`), seven `--ro-bind-data`
files (a file `creat`ed in the new tmpfs root and bound onto
`/newroot/etc/passwd`, `/etc/group`, ...), `--proc /proc` done as a bind
of the old `/proc` (no pid namespace), `--new-session` (`setsid`), and the
arguments passed through `--args <memfd>`. Inside, the probe saw:

```
uid=1000 gid=1000 groups=1000,65534      uid_map/gid_map "1000 1000 1", setgroups "deny"
CapInh/CapPrm/CapEff/CapBnd/CapAmb 0     NoNewPrivs 1     Seccomp 0
134 lines of mountinfo, every bind "nosuid,nodev", the runtime's /usr "ro"
root-owned files shown as nobody:nogroup (65534, the overflow ids)
```

Inside, `pv-adverb` uses `PR_SET_CHILD_SUBREAPER`, `PR_SET_PDEATHSIG`,
`F_OFD_SETLK`, `close_range`, and runs `ldconfig` into the ldso tmpfs.
Before bwrap, `pressure-vessel-wrap` builds a mutable copy of the runtime
with 6,601 `linkat` calls, `utimensat`, `fchmod` and `getxattr`. None of
that is a namespace; §9's last slice finds what of it Ferrix lacks.

### 1.4 As root it is a different program

Steam runs as root on Ferrix today (`tools/common/xtask/src/steam.rs`: `USER=root`).
bwrap turns a user namespace on only for a caller that is neither setuid
nor uid 0 (`if (!is_privileged && getuid () != 0) opt_unshare_user = TRUE`);
as root it keeps its capabilities and makes a mount namespace alone:
`clone(CLONE_NEWNS|SIGCHLD)`, no maps, and `capset` back to everything. The
bind, remount and `pivot_root` sequence is the same. So:

* **`test-steam-bootstrap` as it runs today needs mount namespaces, not user
  namespaces.** Its requirements check passes once M3 lands (§9).
* A desktop session, which `docs/AUTH.md` phase 2 moves off root, runs Steam
  as uid 1000 and needs both. The customer's decision covers both; the gate
  for the unprivileged path is `test-bwrap` as uid 1000 (§8), and
  `test-steam-bootstrap` runs the client as uid 1000 from N6 on (§11): the
  client needs to be uid 1000 throughout for its semaphores too.
* bwrap as root also makes `/proc/sys`, `/proc/sysrq-trigger` read-only by
  binding each onto itself and remounting it `ro` when `access(W_OK)`
  passes, which for root it does. Ferrix has `/proc/sysrq-trigger`, so a
  bind of one file of procfs onto itself, and read-only enforced on
  procfs, are needed for the root path too.

### 1.5 The minimal set

| Needed | Not needed (stays as today) |
|---|---|
| `clone`/`clone3`/`unshare` with `CLONE_NEWNS`, `CLONE_NEWUSER`, and both | `CLONE_NEWPID`, `NEWNET`, `NEWIPC`, `NEWUTS`, `NEWCGROUP`, `NEWTIME`: `EINVAL` |
| `/proc/<pid>/uid_map`, `gid_map`, `setgroups`, Linux's write rules | `setns`: `EINVAL`; nothing asks for it |
| `/proc/<pid>/ns/` with `mnt` and `user` (opened `O_PATH`, `fstatat`) | `MS_MOVE`, `MS_SHARED` and propagation: no mount is ever shared |
| `mount` with `MS_BIND`, `MS_REC`, `MS_REMOUNT`, `MS_REMOUNT|MS_BIND`, `MS_RDONLY`, `MS_NOSUID`, `MS_NODEV`, `MS_NOEXEC`, atime flags, `MS_SILENT`, `MS_PRIVATE`/`MS_SLAVE`/`MS_UNBINDABLE` as no-ops | the new mount API (`open_tree`, `fsopen`, `mount_setattr`): `ENOSYS` |
| binds of directories, files and sockets, subtrees included | `mount -t proc` inside a user namespace (bwrap binds instead) |
| `tmpfs` mounted from a user namespace, with its options (`mode=`) | any other type from a user namespace: `EPERM` |
| `umount2` with `MNT_DETACH` of a whole subtree | seccomp: pressure-vessel installs none for the helper (`Seccomp 0`) |
| `pivot_root`, including `pivot_root(".", ".")` | Chromium's own sandbox: Steam is run with `-no-cef-sandbox` (`SPIKE.md`) |
| `/proc/self/mountinfo`, true per namespace | ambient capabilities, file capabilities, securebits |
| `capget`/`capset`, `PR_CAPBSET_READ`/`PR_CAPBSET_DROP` in a user namespace | nested user namespaces beyond what costs nothing (§2.2) |
| `/proc/sys/kernel/overflowuid`, `overflowgid`, `ngroups_max`, `cap_last_cap`; `/proc/sys/user/max_user_namespaces`, `max_mnt_namespaces` | `/proc/sys/kernel/unprivileged_userns_clone` (Debian's, not Linux's) |
| ids shown through the namespace's maps: 65534 for an unmapped one | |

Numbers, from the local UAPI headers (QEMU's `linux-headers/`, and the
host's `linux/sched.h`, `linux/mount.h`, `linux/prctl.h`,
`linux/capability.h`), all already in Ferrix's tables and routed:

| Call | x86-64 | i386 | AArch64 | ARMv7-A |
|---|---:|---:|---:|---:|
| `unshare` | 272 | 310 | 97 | 337 |
| `setns` | 308 | 346 | 268 | 375 |
| `pivot_root` | 155 | 217 | 41 | 218 |
| `mount` / `umount2` | 165 / 166 | 21 / 52 | 40 / 39 | 21 / 52 |
| `capget` / `capset` | 125 / 126 | 184 / 185 | 90 / 91 | 184 / 185 |
| `prctl` | 157 | 172 | 167 | 172 |
| `clone` / `clone3` | 56 / 435 | 120 / 435 | 220 / 435 | 120 / 435 |

`CLONE_NEWNS 0x20000`, `CLONE_NEWUSER 0x10000000`, `CLONE_FS 0x200`;
`MS_RDONLY 1`, `NOSUID 2`, `NODEV 4`, `NOEXEC 8`, `REMOUNT 32`, `BIND 4096`,
`MOVE 8192`, `REC 16384`, `SILENT 32768`, `UNBINDABLE 1<<17`, `PRIVATE 1<<18`,
`SLAVE 1<<19`, `SHARED 1<<20`, `RELATIME 1<<21`; `PR_CAPBSET_READ 23`,
`PR_CAPBSET_DROP 24`; `CAP_SETGID 6`, `CAP_SETUID 7`, `CAP_SETPCAP 8`,
`CAP_SYS_CHROOT 18`, `CAP_SYS_ADMIN 21`, `CAP_LAST_CAP 40`.

---

## 2. The model

### 2.1 Mount namespaces

Today there is one `ferrix_vfs::Namespace` (`fs::namespace()`): a root
mount, a table of mounts keyed by `(mount id, dentry id)` of the place each
covers, a dentry cache and a rename lock. A `Location` is `(Arc<Mount>,
Arc<Dentry>)`, and most of `Namespace`'s methods already act on a location
without looking at the table; only the walk's crossing of mount points
(`mounted_on`), the list of mounts and `mount`/`unmount` read it. That is
what stage 8 kept apart for this stage, and it makes the change small:

* **A mount namespace is a `Namespace`**, and a process's is in its fs
  context beside its root and working directory: the kernel's
  `SpinLock<Context>` gains `ns: Arc<Namespace>`. `CLONE_FS` already shares
  that context, so threads share a namespace, as they do on Linux in every
  case Steam meets. `fs::namespace()` stays: it is the first namespace, the
  one the kernel's own walks, `root_disk`, `devmgr` and the boot checks use.
  The walks a program's call makes use the namespace in its context
  (`syscall/path.rs`, `fd.rs`, `fsctl.rs`, `exec.rs`, `fs/sockname.rs`,
  procfs's `mounts`).
* **A mount knows its namespace** (`Weak<Namespace>`), and crossing a mount
  point looks in *that* namespace's table, not the walker's. That is Linux's
  rule (the mount hash is keyed by mount, not by who walks), and it keeps a
  descriptor opened before an `unshare`, or `fchdir` to one, working in the
  tree it was opened in. Mount ids come from one kernel-wide counter, so a
  key never names a mount of another namespace.
* **The dentry cache and the rename lock become the kernel's, shared by
  every namespace** (an `Arc` the copies hold). A cache per namespace would
  keep an unlinked file's dentry, and its pages, alive in every namespace
  but the one that unlinked it; a rename lock per namespace would let two
  renames on the same filesystem, from two namespaces, invalidate each
  other's ancestry check and move a directory under itself.
* **Per-mount flags are real** (§2.3): an `AtomicU32` in `Mount` with
  read-only, nosuid, nodev, noexec, the atime mode, and the lock bits.
* **A mount's parent can change** (`pivot_root`), so it moves from an
  immutable field to one read and written under its namespace's table lock.
  Every parent pointer is written only under its namespace's table lock,
  so the table and the parents change together; each pointer also has a
  leaf lock of its own, which is all a walk going `..` (`up`, `path_of`)
  takes. So a walk may see a change half made, and what keeps it safe is
  an invariant every change holds at each step: **the graph of parents
  stays acyclic**, so a walk upward always ends. N2's binds and detaches
  hold it trivially; N3's `pivot_root` must order its pointer writes to
  hold it too.
* **A namespace's recorded root is a `Location`,** not only a root mount:
  the root switch (`fs/root_disk.rs`) re-roots pid 1 at `/sysroot`, and in
  the same step now records that place as the first namespace's root. It is
  what "chrooted" is measured against (§4, rule U6) and what `mountinfo`
  prints from.

**Copying** (`clone(CLONE_NEWNS)`, `unshare(CLONE_NEWNS)`): under the
source namespace's change lock (§6), every mount in the source is given a
new `Mount` -- same filesystem, same root dentry, same flags, a new id --
parents mapped to the copies, in the order that keeps a parent before its
children. Every copy is charged to the job of the process asking (§5). The
calling process's root and working directory are mapped to the copies of
their mounts; open descriptors keep the old ones, as on Linux. A copy for a
namespace owned by a less privileged user namespace locks what it copies
(§4, rule M3).

**Teardown**: a namespace ends when the last fs context holding it goes.
Its table is dropped, and each mount with it unless an open file or a
`Location` still holds one; the parent `Arc`s run child-to-parent only, so
nothing cycles.

**The mount operations**, each a method of `Namespace`, each refused
`EINVAL` on a mount not in the caller's namespace (Linux's `check_mnt`):

* `bind(source, target, recursive)`: a new mount on `target` whose
  filesystem is the source's and whose root dentry is the source's dentry,
  so a subdirectory or a single file can be bound. A file binds onto a file
  and a directory onto a directory (`ENOTDIR` otherwise; `mount` today
  refuses every non-directory). With `MS_REC`, every mount below the source
  place is copied under the new one. Without it, a source with locked
  children is `EINVAL` (rule M4). The new mount's flags are the source
  mount's, lock bits included.
* `remount(target, flags)` for `MS_REMOUNT|MS_BIND`: the target must be a
  mount's root; the per-mount flags become `flags`, refused `EPERM` if they
  clear a locked one. Plain `MS_REMOUNT` (Linux's remount of the
  superblock) sets the same per-mount flags on that mount and also sets or
  clears read-only on the filesystem itself -- a superblock every bind and
  copy of it shares, the one piece of mount state they share -- so every
  bind of it refuses writes, or takes them again, as `SB_RDONLY` does on
  Linux; no other filesystem option exists to change. A remount to
  read-only writes the filesystem out first, as `umount2` does, so a btrfs `/` or `/data` remounted read-only
  at shutdown (`docs/INIT.md` §8.2, step 2, refused `EINVAL` until now) is
  committed and then refuses writes through that mount. Linux refuses a
  read-only remount while files are open for writing (`EBUSY`); nothing
  here counts writers, so it is accepted and the open files keep writing,
  as they would through a read-only bind.
* `unmount(target, detach)`: without `MNT_DETACH` as today (`EBUSY` with
  mounts inside); with it, the whole subtree leaves the table at once and
  each mount goes when its last user does. Every filesystem of the subtree
  is written out first, once each, as the target's alone was before. The top of a detached subtree
  gets no parent, as Linux gives it, so `..` from inside it stops at its
  root. A locked mount cannot be the target (`EINVAL`); it goes only with
  its parent.
* `pivot_root(new_root, put_old)`: Linux's checks and order (below), under
  the table lock in one step, then every process of the namespace whose
  root or working directory was the old root is moved to the new one
  (`chroot_fs_refs`), from the process registry.
* `MS_PRIVATE`, `MS_SLAVE`, `MS_UNBINDABLE`, with or without `MS_REC`, on a
  mount's root: accepted, nothing to do -- no mount here is ever shared, so
  every mount already is what they ask. `MS_SHARED` stays `EINVAL`, and so
  does `MS_MOVE`.

`pivot_root`'s checks, from `fs/namespace.c`: both paths directories, both
in the caller's namespace (`EINVAL`); `new_root` the root of a mount that is
not locked and not the caller's root mount (`EBUSY` if it is, or if
`put_old`'s mount is), with a parent (`EINVAL`); the caller's root the root
of its mount (`EINVAL`); `put_old` reachable from `new_root` and `new_root`
from the caller's root (`EINVAL`). Then: `new_root` leaves its parent and
takes the old root mount's place, the old root mount is attached at
`put_old`, and a lock on the old root moves to the new one. For
`pivot_root(".", ".")`, `put_old` is `new_root`'s own root, so the old root
lands stacked on top of it, which is what bwrap then unmounts.

### 2.2 User namespaces

A `UserNamespace` is an object of its own (a new file,
`syscall/userns.rs`):

```
parent: Option<Arc<UserNamespace>>    None for the first
level: u8                             0 for the first, at most 32
owner: (kuid, kgid)                   the creator's effective ids, kernel ids
uid_map, gid_map: Once<IdMap>         written once; read without a lock
setgroups: AtomicBool                 allowed until "deny"; a child inherits deny
id: u64                               what /proc/<pid>/ns/user names
charge: Charge                        F-37
```

An `IdMap` is up to five extents `(inside, outside, count)`, `outside`
already a kernel id (a write is translated through the parent's map as it
is parsed). Five is Linux's small-map limit; an unprivileged writer may only
write one.

**Credentials hold kernel ids, always.** A process's `Credentials` gains
`user_ns: Arc<UserNamespace>` and `caps: CapSets` (effective, permitted,
inheritable, bounding, as four `u64`s). Nothing a user namespace does
changes a kernel id; it changes how ids are shown and accepted at the
system-call boundary, and it adds capabilities that reach only the objects
that namespace owns. A kernel id of 0 is reachable only through a map
written by a writer whose own kernel id is 0.

**Capabilities.** In the first namespace nothing changes: an effective uid
of 0 stands for every capability, as `credentials.rs` says today, and
`capset` from root is accepted and not enforced. In any other namespace the
sets are real:

* A process that makes a user namespace (`clone` or `unshare` with
  `CLONE_NEWUSER`) gets every capability in it, and none outside it.
* `ns_capable(process, ns, cap)` is Linux's `cap_capable`: true if the
  process is in `ns` and `cap` is in its effective set (in the first
  namespace: effective uid 0); or if `ns` descends from the process's
  namespace through a child whose owner is the process's effective kernel
  uid (a namespace's owner has every capability in it from outside).
* **`privileged()` -- the check behind every `require_privilege` in the
  personality -- becomes "effective uid 0 *and* in the first user
  namespace".** Every call that is root-only today is root-only in the
  whole system's sense (reboot, time, `mknod`, set-id to any id, `setgroups`
  to anything), and stays so for a process in a child namespace whatever
  its ids say. That one line is most of the security argument (§4, U1).
* `execve` in a child namespace recomputes the sets as Linux's
  `cap_bprm_creds_from_file` does without file capabilities: a process
  whose effective id is the namespace's root (inside id 0) gets the bounding
  set as permitted and effective; any other gets none. bwrap's helper ends
  with `capset` to nothing and `execve`s as uid 1000, so the program runs
  with no capability, as `CapEff 0` showed on the host.
* `capget`, `capset` (Linux's subset rules), `PR_CAPBSET_READ` and
  `PR_CAPBSET_DROP` (needs `CAP_SETPCAP` in the caller's namespace;
  `EINVAL` above `CAP_LAST_CAP`) act on the sets in a child namespace. In the
  first namespace `PR_CAPBSET_DROP` is accepted from root and recorded, and
  `PR_CAPBSET_READ` reports it, which is today's "accepted and not
  enforced" for `capset`, stated once more. `/proc/<pid>/status` gains
  `CapInh`, `CapPrm`, `CapEff`, `CapBnd`, `CapAmb` lines.

What a capability in a child namespace allows, and nothing else:

| Capability, in the namespace that owns the object | Allows |
|---|---|
| `CAP_SYS_ADMIN` over the mount namespace's owner | `mount` (bind, remount, `tmpfs`), `umount2`, `pivot_root`, `MS_PRIVATE`/`SLAVE`; `CLONE_NEWNS` |
| `CAP_SYS_CHROOT` in the caller's namespace | `chroot` |
| `CAP_SETUID` / `CAP_SETGID` in the caller's namespace | `set*uid`/`set*gid` to any id *mapped* in it; `setgroups` if the namespace still allows it |
| `CAP_SETPCAP` in the caller's namespace | `PR_CAPBSET_DROP`, raising inheritable within bounding |

No other capability is honoured in a child namespace: not `CAP_DAC_OVERRIDE`,
`CAP_FOWNER`, `CAP_CHOWN`, `CAP_KILL` or `CAP_MKNOD`. Linux honours the
first four for files whose owner is mapped in; bwrap's helper touches only
files it made, so refusing them costs Steam nothing and removes a class of
past CVEs (§4). The VFS's `Access::privileged` keeps meaning a filesystem
uid of 0 as a kernel id, which a child namespace cannot produce unless root
made it.

**Ids at the boundary.** `from_kuid(ns, kuid)` answers the inside id, or
`overflowuid` (65534) for an id the namespace does not map;
`make_kuid(ns, id)` answers the kernel id, or `EINVAL` for one it does not
map. Applied at: `getuid` and its kin, `getresuid`/`getresgid`, `getgroups`,
the `stat` family's `st_uid`/`st_gid` and `statx`'s, `chown` and its kin
(inputs), `setuid` and its kin (inputs), `setgroups` (inputs),
`/proc/<pid>/status`'s `Uid`/`Gid`/`Groups` lines, `SO_PEERCRED` and
`SCM_CREDENTIALS` as the reader's namespace sees them, `si_uid` in
`waitid` and signal information, System V IPC's `ipc_perm` when it lands.
For bwrap's identity map (`1000 1000 1`) every mapped id reads the same and
only unmapped ones change, to 65534, which is exactly what the container
showed on the host. A site that is missed shows a kernel id: wrong, never
more privilege; §8 lists a check per site.

**Making one.** `CLONE_NEWUSER` from `clone`, `clone3` or `unshare`, alone
or with `CLONE_NEWNS` (the user namespace is made first and owns the mount
namespace made with it). Refused as Linux refuses:

* `EINVAL` with `CLONE_FS` in `clone` (CVE-2013-1858), and from `unshare`
  when the fs context or the thread group is shared;
* `EPERM` from a process whose root is not its mount namespace's recorded
  root -- a chrooted process (CVE-2013-1956);
* `EUSERS` past level 32; `ENOMEM` past the job's memory (§5);
* `EPERM` if the creator's effective uid or gid is not mapped in its own
  namespace.

**The map files** (`/proc/<pid>/uid_map`, `gid_map`, `setgroups`, mode 0644,
owned by the process), a new procfs content kind. Reading shows each extent
from the reader's namespace, as Linux does. A write, Linux's `map_write`
and `new_idmap_permitted`, checking both the *opener* (recorded when the
file is opened, Linux's `f_cred`) and the writer:

1. one write, at offset 0, complete, at most a page; a map already written
   is `EPERM`;
2. the opener in the target namespace or its parent, and with
   `CAP_SYS_ADMIN` over the target;
3. extents parsed, non-overlapping, counts non-zero, no overflow; each
   outside range mapped through the parent's map;
4. then either **unprivileged**: one extent of count 1 whose outside id is
   the opener's effective uid (for `gid_map`: effective gid, and
   `setgroups` already `deny`), and the opener is the namespace's owner --
   or **privileged**: opener and writer both with `CAP_SETUID`
   (`CAP_SETGID`) over the parent, which for the first namespace means root.

`setgroups` takes `allow` or `deny`; `deny` is refused once `gid_map` is
written, and `allow` once `deny` was written (CVE-2014-8989).

### 2.3 Per-mount flags, enforced

Accepted and ignored today; Steam's binds add `nosuid,nodev` everywhere and
`ro` to half of them, and a user namespace's safety rests on them:

| Flag | Enforced at | Answer |
|---|---|---|
| read-only | `open` for writing or truncating, and every change through a path or a descriptor: `mkdir`, `mknod`, `symlink`, `link`, `unlink`, `rmdir`, `rename`, `chmod`, `chown`, `utimensat`, `truncate`, `fallocate`, `setxattr` | `EROFS`; files already open for writing keep writing, as on Linux |
| nodev | `open` of a character or block device | `EACCES` |
| noexec | `execve` of the file or its interpreter; `mmap` with `PROT_EXEC` of a file on it | `EACCES`; `EPERM` for `mmap` |
| nosuid | `execve`'s set-user-id and set-group-id bits | ignored, as `SetIds::NONE` |
| atime modes | recorded and shown | no effect: no access time is kept apart |

The checks sit at the `Location` each operation already has, in
`Namespace`'s methods and the fd calls, so there is one place per
operation. `tmpfs`, `proc` and `devtmpfs` stop refusing `MS_RDONLY`, since a
read-only mount can now refuse a write. `/proc/mounts` prints the flags,
and `/proc/<pid>/mountinfo` (new) prints
`id parent major:minor root point options - type source super-options`,
with `root` the path of the mount's root dentry inside its filesystem (`/`,
or `/etc` for a bind of it), mount points from the reader's root, and mounts
outside the reader's root left out, as Linux leaves them.

### 2.4 procfs additions

* `/proc/sys/kernel/overflowuid`, `overflowgid` (65534), `ngroups_max`
  (65536), `cap_last_cap` (40): read-only.
* `/proc/sys/user/max_user_namespaces`, `max_mnt_namespaces`: what §7's
  limits are, read-only. `/proc/sys/fs/mount-max`: the mounts per namespace.
* `/proc/<pid>/uid_map`, `gid_map`, `setgroups` (§2.2), `mountinfo` (§2.3).
* `/proc/<pid>/ns/`, holding `mnt` and `user`: links whose text is
  `mnt:[<id>]`, which `fstatat` follows to an object whose inode number is
  the id, and which open only `O_PATH` (there is no `setns` to use them
  with). A process's directory holds no subdirectory today (`PER_PROCESS`
  is flat and asserted so), so this is a content kind of its own, as `fd`
  and `task` are.

### 2.5 The two places the item meets it

**Native `process_create`** (`syscall/native.rs`, in the item) makes a
process "running as its creator does" through the personality's
`LoadNative` (`syscall/launch.rs::load_native`, load), which today copies
the creator's `Credentials` and starts the child in
`root_disk::process_context()` -- the first namespace's root. From N3 on
that would be an escape: a process inside bwrap's container, its `/` a
tmpfs of read-only binds, could make a native child whose `/` is the real
one, in one call. So **a native child inherits its creator's mount
namespace, root and working directory, as it already inherits its
credentials (and with them, from N4, its user namespace and capability
sets).** `load_native` takes the creator's fs context beside its
credentials and hands both to `exec::load_native`; a kernel-made process
(`devmgr`, no creator) keeps the first namespace's root and root's ids.
`native.rs` is unchanged: it already passes the creator (`Some(caller)`),
and the item's decision -- who may make a process in which job -- does not
move. This also closes the same escape from a `chroot`, which needs no
namespace. N3 carries it, with a boot check: a process in a copied
namespace whose `/` has been pivoted to a tmpfs makes a native child, and
the child must find a file that exists only in that tmpfs and must not find
one that exists only in the first namespace; its `/proc/<pid>/ns/mnt` must
name its creator's namespace. The negative control is today's
`process_context()`.

**Audit** (`audit.rs`, in the item, FAU under O.AUDIT). A record's subject
is the pid and job, which the TSF attests, and a `uid` field that is
personality-supplied (`docs/certification/AUDIT.md` §2); today every
record carries `NO_UID` there. **Whenever the personality supplies a uid,
it is the kernel id held in `Credentials`, never a namespace's view of
it** (`from_kuid` is for what a program is told, not for what the kernel
records). N4's check makes an audited call -- a native call refused for
rights -- from a process that is root inside a child namespace (kernel uid
1000) and requires the record's `uid` to be `NO_UID` or 1000, never 0, and
its pid and job to be the caller's.

These are the only two contact points: nothing else in `core` or `item`
reads a process's ids or its fs context.

---

## 3. Where it lives: the certified item and the load

**All of it is load.** Every file it touches is in the `load` ring of
`tools/common/data/certification-item.json`: `syscall/namespace.rs`,
`credentials.rs`, `family.rs`, `fsctl.rs`, `path.rs`, `fd.rs`, `exec.rs`,
`memory.rs`, `attributes.rs`, `process.rs`, `fs/**` (procfs, sockname,
root_disk), and the `src/lib/fs/vfs` library outside the kernel crate. The one
new kernel file, `syscall/userns.rs`, and the new boot check,
`syscall/namespace_check.rs`, go into the `load` ring's list, and the check
into `composition_root.load_modules`, where `main.rs` names it (39 then).
The FX codes the checks use are added to `panic/catalog.rs`, a `core` file,
as data -- a table entry, the same kind of edit every boot check makes.

Nothing in `core` or `item` consults POSIX credentials (measured: no
`with_credentials`, `require_privilege` or `privileged()` in either ring),
and the Security Target claims nothing about them (§3.2 names the Linux
personality a threat agent outside the TSF). So a user namespace cannot
move what the item enforces: address spaces, handles and rights, job
quotas, the IOMMU. What it can move is what `docs/AUTH.md` relies on from
the environment (OE.AUTH): who may read which file, who is root. §4 is the
argument that it does not. The item's own enforcement stays unaffected on
one condition, §2.5's: a native process made from inside a namespace stays
inside it. `docs/AUTH.md` records that OE.AUTH relies on U1, and
`docs/certification/VULNERABILITY-ANALYSIS.md` lists the defect classes
below with their countermeasures and evidence.

The certification consultant reviews it all the same, the design before
code and each landing's diff before it lands, for three reasons: it is a structural change to the VFS every program walks
through; it adds kernel heap a program can make and keep (F-37, §5); and it
changes `privileged()`, which every root-only call in the personality
answers by.

---

## 4. Security: a user namespace grants nothing outside itself

The rules, each with the Linux defect it is the fix for, and each with a
boot check that tries the attack and must be refused (§8).

**User namespaces**

* **U1. Privilege outside is the first namespace's.** `privileged()` requires
  the first user namespace; a capability in a child reaches only objects it
  owns (§2.2's table). Fake root -- inside id 0 -- is a kernel uid of 1000.
* **U2. Kernel ids never change.** Maps only translate. An unprivileged
  writer maps exactly its own id (count 1). Only a writer that is root
  outside can map kernel id 0 in.
* **U3. The map is written once, checked against opener and writer**
  (a map fd passed to a set-uid program cannot be used to write a wider map;
  Linux's `f_cred` fix).
* **U4. `setgroups` is denied before an unprivileged `gid_map`**, and
  `setgroups(2)` in such a namespace is `EPERM` forever, so nobody drops a
  group whose members a file's mode excludes (CVE-2014-8989).
* **U5. `CLONE_NEWUSER` with `CLONE_FS` is `EINVAL`** (CVE-2013-1858: a
  root shared with a process outside, then `chroot` inside).
* **U6. A chrooted process cannot make a user namespace** (CVE-2013-1956:
  `CAP_SYS_CHROOT` inside to walk out of the jail).
* **U7. Set-id programs give nothing in a child namespace.** `execve` in a
  child namespace ignores set-user-id and set-group-id bits outright
  (stricter than Linux, which honours an owner mapped in); `nosuid` mounts
  and `PR_SET_NO_NEW_PRIVS` ignore them too, as today. The classic attack --
  fake root binds a forged `/etc/shadow` in its own mount namespace, then
  runs `su` -- finds `su` running as the caller.
* **U8. No `CAP_DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_CHOWN`, `CAP_MKNOD` or
  `CAP_KILL` from a child namespace** (CVE-2014-4014 was `chmod` of a file
  whose owner was not mapped; here there is no such override at all).

  **U7 and U8 are deliberate** (the customer, 2026-09-28, subject to the
  certification consultant's view): each refuses less than Linux allows,
  and nothing Steam, pressure-vessel or bwrap runs needs what they refuse.
  If a real program ever does, they relax one step at a time, each a
  landing with its own boot check: U7 by honouring a set-id bit whose
  owner (or group) is mapped in the caller's namespace, on a mount without
  `nosuid`, without `PR_SET_NO_NEW_PRIVS` -- Linux's `bprm_fill_uid` with
  `kuid_has_mapping`; U8 by honouring the one capability the program needs
  for an inode only when both its uid and its gid are mapped in the
  caller's namespace -- Linux's `capable_wrt_inode_uidgid` -- never for
  `CAP_MKNOD`, which Linux too keeps to the first namespace. Neither
  relaxation touches U1 or U2, which carry the argument.

  **What they cost in compatibility,** and the argument rests on paying it
  (the consultant, 2026-09-28): a set-id program inside a container does
  not change ids -- `sudo`, `su`, `newgrp` or a set-group-id `ssh-agent`
  run inside bwrap run as their caller, even where Linux would honour an
  owner mapped in; and a process that is root inside a child namespace
  cannot override file permissions (`CAP_DAC_OVERRIDE`,
  `CAP_DAC_READ_SEARCH`, `CAP_FOWNER`), change a file's owner
  (`CAP_CHOWN`), make a device node (`CAP_MKNOD`), or signal a process of
  another uid (`CAP_KILL`), even on files and processes it maps -- so
  `unshare -r` followed by `chown` or `apt` inside it fails where it works
  on Linux. Rootless container builders (podman, buildah) therefore do not
  work until U8 is relaxed for them.
* **U9. Ids not mapped are shown as 65534 and refused as input** (`EINVAL`),
  so an unmapped id can never be written into a file's owner.

**Mount namespaces**

* **M1. Mounting needs `CAP_SYS_ADMIN` over the mount namespace's owner.** A
  process that shares the first namespace's mounts can never change them
  without being root.
* **M2. From a child user namespace only `tmpfs` and binds**; `proc`,
  `devtmpfs`, `sysfs`, `cgroup2` and `btrfs` are `EPERM`. A `tmpfs` mounted
  there is always `nodev` and `nosuid` (Linux forces `nodev`, `SB_I_NODEV`;
  `nosuid` adds U7's belt to its braces). No filesystem parser sees an image
  an unprivileged user chose.
* **M3. Copying into a less privileged owner locks.** Every copied mount
  gets its flags locked (read-only if it was, `nosuid`, `nodev`, `noexec`,
  the atime mode) and is locked to its parent (CVE-2014-5206, -5207: a
  remount in a user namespace clearing `ro` or `nodev` on the host's mount).
* **M4. A locked mount cannot be separated from its parent**: not unmounted
  alone, not bound without `MS_REC`, not a `pivot_root` target -- so nothing
  an administrator hid under an over-mount is revealed. Unmounting its
  parent with `MNT_DETACH` takes it along; `pivot_root` moves the old root's
  lock to the new root, so bwrap's detach of `oldroot` is allowed.
* **M5. A bind's source must be in the caller's namespace** (`EINVAL`), so
  a descriptor kept from before an `unshare`, or `/proc/<pid>/root` of
  another namespace, cannot bind that namespace's mounts into this one.
* **M6. Flags are enforced** (§2.3), so `ro,nosuid,nodev,noexec` on a bind
  mean what bwrap and pressure-vessel expect of them.
* **M7. A mount namespace is visible only to its members.** Mount changes
  never reach another namespace (no propagation exists). A process in the
  first namespace reaching a child's tree through `/proc/<pid>/root` sees
  files it could see anyway, at its own permissions; M2 means nothing in
  that tree is a device node or set-id file the child made.
* **M8. `/proc/<pid>/root`, `cwd`, `exe` and `fd/*` cross namespaces only
  where Linux's `ptrace_may_access` allows** (landing NP, before N5). These
  links reach into another process's tree, so they are gated as Linux gates
  them, not by the directory's owner alone as today (`docs/BACKLOG.md`,
  the `proc_fd_link` row): allowed when the caller's kernel uid and gid
  equal every one of the target's real, effective and saved ids *and* the
  target is dumpable, or when the caller has `CAP_SYS_PTRACE` over the
  target's user namespace (root in the first namespace; nothing from a
  child namespace into a process its namespace does not own). A target that
  `execve`d a set-id program, or cleared `PR_SET_DUMPABLE`, is not dumpable.
  So a process inside a container cannot use `/proc/<pid>/root` of a
  process outside it to walk out, and a same-uid process cannot reach into
  a non-dumpable one's descriptors.

**What stays as it is:** `setns` (`EINVAL`), pid, network, IPC, UTS and
cgroup namespaces (`EINVAL`), so the attack surface of joining someone
else's namespace does not exist.

---

## 5. F-37: every kernel heap a program can keep is charged

New kinds, each charged with a `Charge` from `src/lib/kernel/kmem` to the job
of the task whose call makes it, kept inside what it pays for:

| Kind | Made by | Charged |
|---|---|---|
| user namespace, with its two maps | `CLONE_NEWUSER` | at creation; the maps are inline, written once |
| mount namespace | `CLONE_NEWNS` | at creation |
| a copied mount | `CLONE_NEWNS` copying the tree | each, as `mount` already charges one |
| a bind mount, and each copy `MS_REC` makes | `mount(MS_BIND)` | each (the existing mount charge) |
| a map file's opener record | `open` of `uid_map`/`gid_map` | with the open's rendered snapshot, already charged |

A namespace held by a descriptor (`/proc/<pid>/ns/*` opened `O_PATH`) is
the charged object kept alive by an open file that is itself charged. The
boot check `fs/kmem_check.rs` gains two fills: user namespaces made by
`unshare` in a looping child until `ENOMEM`, and mount namespaces made the
same way over a tree of 64 mounts. Each must be refused by the job's limit,
a sibling job must then make one, and both jobs must read zero after.

---

## 6. Locks

| Lock | Kind | Guards | Taken after |
|---|---|---|---|
| namespace change lock | `SleepLock`, per mount namespace | every change to the tree: mount, bind, remount, unmount, `pivot_root`, copying (held on the source) | nothing; never two at once |
| the shared rename lock | `SleepLock`, one | as today | the change lock |
| namespace table | `SpinLock`, per mount namespace | the table, and the writing of every parent pointer of its mounts | the change lock |
| a mount's parent | `SpinLock`, per mount | its parent pointer, read by walks going `..` | the table lock, or nothing; a leaf |
| a process's fs context | `SpinLock` | `ns`, root, working directory | a leaf; copied out before a walk, as today |
| a process's credentials | `SpinLock` | ids, `user_ns`, caps | a leaf, as today |
| a map | `Once` | the extents; written under the namespace's write mutex, read with no lock | -- |

Every allocation and charge a change makes happens before a spin lock is
taken, but for the table's own insertions (a `BTreeMap` node, which
predates N2 and allocates without sleeping), so no
spin lock is held across anything that may sleep (`crate::sync::SpinLock`
raises the preemption count; FX-0503 if a holder blocks). A copy allocates
all its `Mount`s, charged, before it takes the new table's lock, and drops
them after releasing any lock if it is refused. `pivot_root`'s moving of
every process's root takes each process's fs lock alone, after the table
lock is released.

---

## 7. Limits

* User namespace depth: 32 (`EUSERS`), Linux's.
* Mounts per namespace: 4096 (`/proc/sys/fs/mount-max`; `ENOSPC` past it,
  Linux's answer). Bounds a copy's length, `mountinfo`'s size and the
  change lock's hold. pressure-vessel's container on the host had 134.
* How many namespaces: no count of its own. Each is charged to its job's
  memory limit, which is F-37's bound; the two `max_*` files report the
  pid limit halved, as Linux's default does, as a number to print.
* Maps: 5 extents.

---

## 8. Checks

**Boot checks** (`syscall/namespace_check.rs`, under a `namespaces` line
with a cost entry), driven through the system-call layer as a program's
calls would be, each with a negative control run once and quoted in the
commit, never committed:

* flags (built in N1 as the `mounts` boot line, FX-0885,
  `fs/mount_check.rs`, on a tmpfs mount; N2 repeats them on a bind): a read-only bind refuses `open(O_WRONLY)`, `mkdir`, `unlink`,
  `rename`, `chmod`, `truncate` with `EROFS`; `nodev` refuses a device;
  `noexec` refuses `execve` and `mmap(PROT_EXEC)`; `nosuid` runs a set-uid
  file as the caller;
* bind of a directory, a subdirectory, a file and a socket (and `connect`
  through it); `MS_REC` copies a submount; `MNT_DETACH` takes a subtree;
  `mountinfo` names each as `readlink` of an `O_PATH` descriptor does;
* bwrap's sequence, as root and as uid 1000, in a child: every call of
  §1.2, `pivot_root(".", ".")` included, ending with `/` the new tree and the
  old one unreachable by any path;
* the namespace is private: a mount in the child is absent from the
  parent's `mountinfo`, and the reverse;
* each rule of §4 attempted and refused: U1 (a child's inside-root calling
  `sethostname`, `mknod`, `setuid(0)` outside its map), U2/U3 (a map with a
  count of 2, a second write, a writer that is not the opener), U4, U5, U6,
  U7 (a set-uid-root file run from a child namespace), U8, U9, M1 to M5;
* ids: every site of §2.2's list reads 65534 for an unmapped id and the
  inside id for a mapped one;
* the F-37 fills of §5, in `fs/kmem_check.rs`;
* §2.5: a native child made inside a pivoted namespace stays in it (N3);
  an audited call from inside-root of a child namespace records kernel uid
  1000 or `NO_UID`, never 0 (N4);
* M8: `/proc/<pid>/root`, `cwd`, `exe`, `fd/*` of a non-dumpable same-uid
  target, of a set-id-exec'd target, and of another uid's process from
  inside-root of a child namespace, each refused `EACCES`; a dumpable
  same-uid target allowed (NP).

**xtask gates:**

* `cargo xtask test-bwrap` (new, x86_64; AArch64 and ARMv7-A if the same
  debs exist): Debian's `bubblewrap` with its libraries, fetched and pinned
  by `tools/common/fetch/fetch-bwrap.sh`, run on glibc's own loader as the
  dynamic busybox is: the requirements check's four argument lists, then a
  list shaped like pressure-vessel's (directory, file and socket binds,
  `--ro-bind-data`, `--tmpfs`, `--proc`, `--new-session`), each as root and
  as uid 1000, running `cat /proc/self/uid_map /proc/self/mountinfo` and
  `id`, and comparing with what the host printed.
* `cargo xtask test-steam-bootstrap --arch x86_64` with the set-aside gone:
  the requirements check must exit 0 (§9, M6).

---

## 9. The landings

In points; each landed on its own, with the image row of
`docs/BACKLOG.md` (check, release build of all three, four boots with
armv7a at `--smp 2`, x86_64 under KVM, `test-shell` with its busyboxes and
`test-vfs`, since each is stage 7 or 8), plus `test-init --arch all`, the
new tests, and `carry-coverage` with `gen-coverage-justification --check`
after the final rebase; the consultant's OK on the diff before each
`land.sh take`.

| | Landing | Gives | Gate beyond the row | Points |
|---|---|---|---|---|
| N0 | This document | the design | `cargo xtask check` | -- |
| N1 | Per-mount flags enforced (§2.3); `MS_REMOUNT`, `MS_REMOUNT|MS_BIND`; `MS_RDONLY` on the memory filesystems; `/proc/<pid>/mountinfo`; `/proc/mounts` with flags | M6 | boot checks: flags, `mountinfo` | 5 |
| N2 | Binds: directory, subtree, file, socket; `MS_REC`; `MNT_DETACH` of a subtree, detached top without a parent; propagation no-ops; the kernel-wide dentry cache, rename lock and mount ids; mutable parents under the table lock | the tree bwrap builds, in the first namespace | boot checks: binds, detach, `readlink` = `mountinfo` | 6 |
| N3 | Mount namespaces: `ns` in the fs context, per-mount namespace, crossing by the mount's table, copy on `clone`/`unshare(CLONE_NEWNS)` from root, teardown, `pivot_root` (with `"."`, `"."`), `chroot_fs_refs`, the recorded root and `root_disk`'s line, `/proc/<pid>/ns/mnt`, `mount-max`, F-37's mount-namespace fill; native children inherit the creator's namespace, root and cwd (§2.5) | **root bwrap works: `test-steam-bootstrap`'s check passes as root** | boot checks: privacy, bwrap-as-root sequence; `test-bwrap` as root | 8 |
| N4 | User namespaces: `UserNamespace`, `CLONE_NEWUSER` and its refusals, `privileged()` in the first namespace only, `ns_capable`, cap sets, `capget`/`capset`/`PR_CAPBSET_*` in a child, `execve`'s recomputation and U7, the map files and `setgroups`, `/proc/<pid>/ns/user`, the sysctls, ids at the boundary, `status`'s `Cap*`; F-37's fill; audit's uid is the kernel id (§2.5) | U1 to U9 | boot checks: maps, the U rules, id sites, the audit record | 8 |
| NP | procfs by `ptrace_may_access` (M8): `root`, `cwd`, `exe`, `fd/*` and `fd`'s listing, `maps`, `mountinfo`, `ns/*`; dumpable cleared by a set-id `execve`; closes the `proc_fd_link` row of `docs/BACKLOG.md` and the "procfs honouring `PR_SET_DUMPABLE`" part of `docs/AUTH.md`'s phase 2 | M8 | boot checks of §8's M8 line | 3 |
| N5 | Unprivileged mounting: `may_mount` by `ns_capable`, M2's types and forced flags, M3's locking on copy, M4, M5, the lock's move on `pivot_root` | **bwrap as uid 1000 works** | boot checks: M1 to M5; `test-bwrap` as uid 1000 | 6 |
| N6 | `test-steam-bootstrap` as uid 1000 (§11) without the set-aside: the image gets what bwrap's sandbox execs (`/usr/bin/true`, a static busybox under `/usr`, since the volume's libraries are links into `/data` that dangle inside the sandbox); the checker must exit 0 | the customer's end point | `test-steam-bootstrap --arch x86_64` | 3 |
| N7 | pressure-vessel on Ferrix: `_v2-entry-point` running the §1 probe in the steamrt64 container, as uid 1000; whatever it needs beyond namespaces is filed as rows (hard links on btrfs, `F_OFD_SETLK`, the host `/usr` layout it copies graphics drivers from) | the container `steamwebhelper` runs in | a probe script in `test-steam-bootstrap` | 3, plus what it finds |

**39 points** for N1 to N6 with NP, 42 with N7's probe. Order as listed: each
builds on the one before, and N3 already turns the requirements check green
for the root run Steam uses today, so the customer's end point is reachable
before the unprivileged half lands.

---

## 10. Risks

1. **The walk is on every path.** N2 and N3 change the VFS every program
   walks through. `src/lib/fs/vfs`'s host tests (3,770 lines) and the
   `vfs_ops` fuzzer (`src/tests/fuzz`) run on every slice, and the fuzzer gains
   bind, remount, detach, copy and `pivot_root` operations with one more
   property: every mount's parent chain ends.
2. **`test-steam-bootstrap` is an hour on the internet.** N6 is gated by
   one run; N3 and N5 are gated by `test-bwrap`, which needs no network.
3. **Other streams in the same files.** `steam-sysv-sem` touches
   `fs/kmem_check.rs`, `panic/catalog.rs`, `stages_check.rs`, `main.rs`:
   rebase conflicts in rows, handled by CONVENTIONS rule 4.
4. **pressure-vessel's needs past namespaces** are unknown until N7; the
   `steam-e2e` stream runs `steamwebhelper` without pressure-vessel meanwhile.
5. **`privileged()` changing** could refuse something a root process in a
   root-made user namespace did before; there is no such process today,
   since namespaces did not exist.

## 11. What the customer decided (2026-09-28)

1. **`test-steam-bootstrap` runs Steam as uid 1000 after N5**, as the
   desktop runs it. The System V semaphore work found the client must be
   uid 1000 throughout anyway: run as root, it moves its effective uid to
   1000 partway and is then refused the 0600 semaphore sets it made as
   root (`docs/I386.md`). So N6 runs the gate as uid 1000, which exercises
   N4 and N5 in the real run; the root run of N3 is the intermediate
   milestone, not the end point.
2. **U7 and U8, stricter than Linux, are accepted** from the product side,
   subject to the consultant's view, and recorded as deliberate in §4 with
   how each would be relaxed.

## 12. Where it stands (2026-09-28)

Design written after tracing the requirements check and a pressure-vessel
launch on the host. **Reviewed by the certification consultant
(2026-09-28): OK to build**, with four changes made here -- the native
`process_create` and audit contact points (§2.5), M8 and its landing NP,
and the records (`docs/AUTH.md`'s OE.AUTH note,
`docs/certification/VULNERABILITY-ANALYSIS.md`'s entry). The depth, mount
and F-37 limits and the lock order were accepted as written; U7 and U8
were accepted, with their cost named (§4). No finding id was opened for
the design. The customer's decisions are in §11. **N1 landed (2026-09-28)**:
per-mount flags enforced, `MS_REMOUNT`, `mountinfo`, closing F-53; N2 is
next, on branch `steam-userns`. Each landing's diff goes to the consultant
before `land.sh take`. **N2 landed (2026-09-30)**: binds, `MS_REC`,
`MNT_DETACH` of a subtree, the propagation no-ops, the kernel-wide ids,
cache and rename lock, and a superblock per filesystem so that a plain
remount reaches every bind of it (the note below); N3 is next.

**For N2 (interim reviewer, N1's review, 2026-09-29):** once binds exist,
a plain `MS_REMOUNT` read-only must reach the whole filesystem -- every
bind of it -- and only `MS_REMOUNT | MS_BIND` the one mount, as on Linux;
or the difference is written down here. Either way a boot check covers
it. M3's flag locking (CVE-2014-5206, -5207) stays N5's.

**N2's review (certification consultant, 2026-09-30): OK.** The interim
reviewer's note is met: a plain `MS_REMOUNT` read-only sets the
filesystem's superblock and reaches every bind, `MS_REMOUNT | MS_BIND` one
mount, and the `binds` line (FX-0886) checks both with a negative control.
The item is touched only by data and a check: FX-0886's entry in
`panic/catalog.rs` and `check_binds` in `stages_check.rs`; no `unsafe`
added. `docs/certification/VULNERABILITY-ANALYSIS.md` gains the rows for
the superblock, for detach and for the plain remount from a child
namespace. For the landings after it:

* **N3.** §6 says every allocation happens before a spin lock is taken;
  `Namespace::attach` collects the parents into a `Vec` under the table's
  lock, and a table insert may allocate there too. Copying a whole tree on
  `clone` goes through the same place, so either hoist those allocations
  or restate §6 as what holds (no allocation that can sleep). The shared
  superblock crosses namespaces, as on Linux; §4's M7 says so when N3
  lands.
* **N5.** A plain `MS_REMOUNT` from a child namespace needs privilege over
  the namespace that made the filesystem (Linux's `do_remount`, over
  `s_user_ns`), not only over the caller's mount namespace: otherwise fake
  root turns the host's `/` read-only, or back. And §8's line "N2 repeats
  them on a bind" is met only for `ro` through `open`: N5's check, where a
  bind is the boundary, shows a bind of a `nosuid`, `nodev`, `noexec` and
  read-only mount inheriting and enforcing each.
