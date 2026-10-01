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
* **A namespace stands on an empty bottom mount.** Its root mount is a
  read-only filesystem with one empty directory, and `/` is the filesystem
  the namespace was made with, mounted on it. That is the shape a booted
  Linux machine has -- on a Linux 7.0 host `/` is mount 34 whose parent,
  2, `mountinfo` does not show, because the reader's root cannot reach it
  -- and it is what `pivot_root` needs: Linux refuses a root mount with no
  parent, which is why it cannot pivot away from an initramfs. Here every
  `/` has one, in memory or on a disk. The bottom mount is hidden from
  `mountinfo` and `/proc/mounts`, which leave out what the reader's root
  cannot reach; `/` itself cannot be unmounted (`EBUSY`), since the kernel's
  walks and every new process start there, and `pivot_root` is the way to
  move it; after a pivot the namespace's `/` for a new process is the new
  root.
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
  read-only writes the filesystem out first, as `umount2` does, so a btrfs `/`, `/data` or `/home` remounted read-only
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

  **U8's honoured set grows by two for network namespaces (2026-10-01):**
  `CAP_NET_ADMIN` and `CAP_NET_RAW` join `userns::HONOURED`, and no site asks
  `holds()` for either. Every privileged network site asks `capable_over`
  against the user namespace that owns the network namespace the action is
  on: `link::net_admin` for the `ifreq` setters, the rtnetlink writers and
  the uevent send, and `syscall/sockets.rs` for raw and packet sockets. So a
  process in a child user namespace is privileged over the network
  namespaces it owns and over no other: fake root left in the host's network
  namespace is refused raw sockets, interface, address and route changes
  (the `netns` line, NN5); moving an interface asks `CAP_NET_ADMIN` over the
  owner of the namespace the message is sent in (the socket's) and over the
  one it names (`IFLA_NET_NS_PID` or `_FD`), NN10. No `/proc/sys/net` file
  exists to write. `capable_over` is unchanged, so neither capability reaches
  a file, a process or the first namespace's tables.

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
  that tree is a device node or set-id file the child made. What the
  namespaces do share is each filesystem's superblock, as on Linux: a
  plain `MS_REMOUNT` read-only in one reaches the filesystem's mounts in
  every other (N2's superblock). That is a change to the filesystem, not
  to a tree, and from a child namespace it is refused unless the child
  made the filesystem (N5, the vulnerability analysis's row).
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

**What stays as it is:** pid and network namespaces (`EINVAL`), and `setns`
into either, which no descriptor can name. The UTS, IPC and cgroup
namespaces and `setns` by descriptor are built (§12, "The small
namespaces, built"), and the rules for joining are in the next list.

**Joining and naming (S1 to S6, the small namespaces)**

* **S1. Making a UTS, IPC or cgroup namespace needs `CAP_SYS_ADMIN` in the
  user namespace the caller will be in**, and its owner is that user
  namespace; a caller that asks for a user namespace with them holds it.
* **S2. The names are the owner's.** `sethostname`, `setdomainname` ask
  `CAP_SYS_ADMIN` over the UTS namespace's owner, not `privileged()`: fake
  root names the namespace it made, nothing names the first's from a child.
* **S3. `setns` needs `CAP_SYS_ADMIN` over the target's owner and in the
  caller's own user namespace** (UTS, IPC, cgroup; a mount namespace also
  `CAP_SYS_CHROOT` in the caller's own), so a process in a child user
  namespace cannot step back into the namespaces of the one that made it.
* **S4. A user namespace is joined only by a holder of `CAP_SYS_ADMIN` in
  it**, never one's own or an ancestor, and never from a multithreaded
  process or a shared fs context; it then holds every capability there.
* **S5. A namespace file opens only for the same person or root** (Linux
  asks `ptrace_may_access`), and `NS_GET_USERNS` and `NS_GET_PARENT` show a
  caller only user namespaces it is inside of or above.
* **S6. A cgroup namespace is a boundary for moves**: a writer of
  `cgroup.procs` there names only cgroups under its root (`ENOENT`), and
  reads every path from that root.

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
| UTS, IPC and cgroup namespace | `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWCGROUP` | each at creation; an IPC namespace's sets as they are made, to the job that makes them |
| a namespace file | `open` of a `/proc/<pid>/ns` link, `NS_GET_USERNS` | the inode, the detached mount and the open file, to the job that opens; the namespace it holds stays the maker's |

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
| NP | procfs by `ptrace_may_access` (M8): `root`, `cwd`, `exe`, `fd/*` and `fd`'s listing, `maps`, `mountinfo`, `ns/*` (dumpable is cleared by a set-id `execve` since 2026-09-30); closes the `proc_fd_link` row of `docs/BACKLOG.md` and the "procfs honouring `PR_SET_DUMPABLE`" part of `docs/AUTH.md`'s phase 2 | M8 | boot checks of §8's M8 line | 3 |
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
remount reaches every bind of it (the note below). **N3 landed
(2026-09-30, 1384e6e6)**: a mount namespace per process, `pivot_root`,
`openat2`, and bubblewrap as root (below), reviewed with conditions for N4
and N5; N4 is next.

**For N2 (interim reviewer, N1's review, 2026-09-29):** once binds exist,
a plain `MS_REMOUNT` read-only must reach the whole filesystem -- every
bind of it -- and only `MS_REMOUNT | MS_BIND` the one mount, as on Linux;
or the difference is written down here. Either way a boot check covers
it. M3's flag locking (CVE-2014-5206, -5207) stays N5's.

**N2's review (certification consultant, 2026-09-30): OK**, and again on
the amended commit after the code reviewer's two blockers were fixed: B1,
a mount point removed or renamed through another bind of its filesystem
(now `EBUSY`, in FX-0886), and B2, `MNT_DETACH` writing out only the
target's filesystem (now every one inside). The interim
reviewer's note is met: a plain `MS_REMOUNT` read-only sets the
filesystem's superblock and reaches every bind, `MS_REMOUNT | MS_BIND` one
mount, and the `binds` line (FX-0886) checks both with a negative control.
The item is touched only by data and a check: FX-0886's entry in
`panic/catalog.rs` and `check_binds` in `stages_check.rs`; no `unsafe`
added. `docs/certification/VULNERABILITY-ANALYSIS.md` gains the rows for
the superblock, for detach and for the plain remount from a child
namespace. For the landings after it:

* **N3.** Met before landing: `attach` allocates nothing under the table's
  lock, and §6 names the table's own insertions; §4's M7 names the shared
  superblock. What stays: no check sees `MNT_DETACH` of a subtree write out
  a filesystem inside it other than the target's (the code reviewer's B2,
  fixed in N2, of F-53's data-loss class; a tmpfs has nothing to write).
  N3, whose `pivot_root` and detach of the old root put `/data`'s btrfs
  inside a detached subtree, carries a check that data written inside the
  subtree just before the detach is committed after it, or the argument
  why none can be built.
* **N5.** A plain `MS_REMOUNT` from a child namespace needs privilege over
  the namespace that made the filesystem (Linux's `do_remount`, over
  `s_user_ns`), not only over the caller's mount namespace: otherwise fake
  root turns the host's `/` read-only, or back. And §8's line "N2 repeats
  them on a bind" is met only for `ro` through `open`: N5's check, where a
  bind is the boundary, shows a bind of a `nosuid`, `nodev`, `noexec` and
  read-only mount inheriting and enforcing each.

**N3 built (2026-09-30, os-98), for the consultant's review before it
lands.** A process's fs context names its mount namespace;
`unshare(CLONE_NEWNS)` and `clone(CLONE_NEWNS)` copy it for a privileged
caller (`EPERM` otherwise, `EINVAL` with `CLONE_FS` or a shared context),
every mount new and charged, the caller's root and working directory
moved to the copies. A mount records its namespace's table, and a walk
crosses a mount point in that table, so a kept descriptor stays in its
own tree. `pivot_root` is Linux's, checks and order, with its parent
pointers written new root first so the parents stay acyclic, then
`chroot_fs_refs` over the registry; `umount2` acts on the mount on top of
the place it names, which is what takes the old root after
`pivot_root(".", ".")`. A namespace ends with the last context naming it,
its mounts disconnected. `/proc/<pid>/ns/mnt` reads `mnt:[N]`,
`/proc/sys/fs/mount-max` is 4096 (`ENOSPC` past it), `/proc/<pid>/mounts`
and `mountinfo` are the process's namespace's, and a native child starts
in its creator's namespace, root and working directory (§2.5).

Evidence: the `mntns` boot line (FX-0887, `fs/namespace_check.rs`) on all
three architectures -- privacy both ways, bubblewrap's calls as root to the
last `pivot_root(".", ".")`, the native child, and the write-out owed
above: a file written into the stage 12 btrfs through the old root just
before its detach, read back by a second read-only mount of the disk. The
`kmem` line's mount-namespace fill (F-37, §5). Eleven host tests of the
VFS (eight of namespaces, three of `openat2`'s resolve flags), and the
`vfs_ops` fuzzer's copy, `chroot` and `pivot_root` operations. `cargo xtask
test-bwrap` runs Debian's bubblewrap 0.12 as root on a fresh btrfs root:
the requirements check's four argument lists and a container shaped like
pressure-vessel's. Five negative controls, each stopping the boot with its
own message: the native child in the first namespace's root, the detach's
write-out narrowed to the target, `pivot_root` not moving the caller's
root, `RESOLVE_IN_ROOT` not rooting the walk, a copy's mounts uncharged.

What the design did not foresee, and what N3 did about it:

* **`openat2`.** bubblewrap from 0.12 opens every place it binds from and
  onto with `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)` (its fix for
  GHSA-pxhw-h44j-8pfx), and Debian builds it with no fallback, so it made no
  sandbox at all. N3 answers `openat2` with all of Linux's resolve flags
  and `struct open_how`'s size rules, in the `mntns` line and host tests.
* **procfs's fixed names are remembered**, so that bubblewrap as root can
  bind `/proc/sys` and `/proc/sysrq-trigger` onto themselves and remount
  them read-only (§1.4): a walk crosses only a mount on a dentry it finds
  again. `/proc/sys/kernel/overflowuid` and `overflowgid` read 65534.
* **Every namespace stands on an empty bottom mount** (landed after N3,
  2026-09-30; §2.1). `pivot_root` refuses a root mount with no parent, as
  Linux refuses it on an initramfs, and as N3 landed the kernel's tmpfs
  was the first namespace's root mount itself, so bubblewrap worked only
  on a disk root and `test-steam-bootstrap`, which boots `/` in memory,
  could not pass its requirements check. Now `/` is always a mount on the
  bottom one, in memory or on a disk: the requirements check exits 0 in
  `test-steam-bootstrap` as root, and `test-bwrap` and the `mntns` line run
  from the in-memory `/`. §9's promise for N3 is met.
* **Deferred to N4:** the first namespace's root recorded as a `Location`
  (§2.1). Only U6, "a chrooted process may not make a user namespace",
  reads it, and U6 is N4's; `mountinfo` prints from the reader's root, as
  it did.
* **A mount point counts in every namespace**: a directory a mount of any
  namespace covers is `EBUSY` to `rmdir`, `unlink` and `rename`, as on
  Linux before 3.18, where since then only the caller's namespace counts.
  Stricter, and simpler to argue.
* `sync` writes out the caller's namespace's filesystems and the first's,
  once each, where Linux writes out every superblock: a filesystem mounted
  in a third namespace alone is written by that namespace's own `sync` or
  unmount.

**N3's review (certification consultant, 2026-09-30): OK**, on 992fc6e6,
with one change made before landing: `test-boot` fails a boot whose stage
12 check wrote `vdc` if its `mntns` line is missing or lacks "on the disk
after the detach", so the write-out's evidence cannot drop out unnoticed
(`namespace_problem` in `tools/common/xtask/src/qemu.rs`). The landed
1384e6e6 differs from what was reviewed by that and by refactors that do
not change behaviour. The item is touched only by a check and data:
`check_namespaces`'s two calls in `main.rs`, its line in
`stages_check.rs`, and FX-0887 in `panic/catalog.rs`; no `unsafe` added;
the boundary gate reads no upward reference. What N2's review asked of N3
is met: the write-out is read back by a fresh read-only btrfs mount of
the disk, which sees only what was committed; the native child is in its
creator's tree; `Namespace::copy` makes every mount and map entry under the
change lock and moves the map in under the spin lock (§6). `openat2`
answers Linux's size and flag rules, and the procfs names and overflow ids
are accepted as built. The bottom mount (5f640790) was reviewed on
33489ca6 and landed with the same patch: OK. The bottom is an empty,
read-only filesystem no path reaches; `Namespace::root` descends from it
to the top, as Linux's `current_chrooted` does, so the kernel's walks and
new processes start where they did; `umount2("/")` is `EBUSY`, stricter
than Linux's remount or lazy detach, and init remounts explicitly (F-53);
`/proc/mounts` hides what the reader's root cannot reach, as `mountinfo`
does. No item file was touched, and `test-init --arch all`, which runs the
root switch and F-53's shutdown remount, passed. For the landings after
it:

* **N4.** A write through a read-only bind of `/proc/sys`, as bubblewrap
  as root makes one, is refused `EROFS`, by a check with a negative
  control: bubblewrap's read-only `/proc/sys` is what keeps a container's
  root from the host's sysctls, and today only `test-bwrap` runs it,
  without looking. The first namespace's root recorded as a `Location`
  lands with U6, which reads it, and U6 compares a process's root with
  `Namespace::root`, the top, not the bottom mount, or every process reads
  as chrooted.
* **NP.** `get_robust_list` of another process's thread answers its
  robust-list head, an address in that process, with no permission check;
  Linux asks `PTRACE_MODE_READ_REALCREDS`. This predates the namespaces:
  f16ab27a moved the head per thread and kept the lookup as it was. NP's
  `ptrace_may_access` gates it along with M8's procfs links, with a check
  that another uid's thread is refused (the certification consultant's
  review of f16ab27a, 2026-09-30). NP must also not see a new process as
  dumpable before it inherits: between its publication (`publish_forked`,
  or `register` for a native child) and `attributes::inherit`, a child is
  findable with default attributes. Either create the attributes with the
  entry, or show that no `ptrace_may_access` can run in that window
  (the review of 5df02c3b; a BACKLOG row).
* **Before N5.** A namespace's end writes out each filesystem whose last
  mount it drops, as a final unmount does on Linux. `Namespace`'s `Drop`
  disconnects its mounts and writes nothing, and btrfs commits on its own
  only `/`, `/data` and `/home`, so a disk mounted inside a namespace alone loses
  what it wrote since its last commit when the namespace ends (F-53's
  class, as N2's B2 was). Not reachable today: only root mounts a disk, and
  every path that does -- bubblewrap's binds, the stage 12 disk -- shares
  its superblock with the first namespace, which `sync` and the periodic
  commit reach. The check: `vdc` mounted inside a namespace alone, written,
  the namespace ended, and a read-only mount reading it back. It also
  closes the gap `sync` leaves above.
* **N5.** "A mount point counts in every namespace" is stricter for the
  namespace holding the mount and looser for everyone else. Once uid 1000
  can mount a `tmpfs` in its own namespace, it can pin any directory it can
  see against its owner's `rmdir` and `rename`, and the `EBUSY` tells the
  owner a mount is there -- why Linux changed this in 3.18. N5 either takes
  Linux's rule, removing a name detaching the other namespaces' mounts on
  it, or confines an unprivileged mount point to where pinning it harms no
  one else, with a check either way.

**N4 built (2026-09-30, os-7c, branch `stage13-n4-userns`), for the
consultant's review before it lands.** A `UserNamespace` (`syscall/userns.rs`)
with a level, an owner, two maps written once and the `setgroups` switch;
`Credentials` hold `user_ns` and four capability sets, all ids staying kernel
ids. `privileged()` is an effective uid of 0 in the first namespace only
(U1); `holds()` honours `CAP_SETUID`, `CAP_SETGID`, `CAP_SETPCAP`,
`CAP_SYS_CHROOT` and `CAP_SYS_ADMIN` in a child and nothing else (U8);
`capable_over` is `cap_capable`. `CLONE_NEWUSER` through `clone`, `clone3`
and `unshare`, alone or with `CLONE_NEWNS`, refused with `CLONE_FS` or
`CLONE_THREAD` (U5), from a process whose root is not its namespace's top
(U6), past 32 levels (`EUSERS`), for a creator whose ids are unmapped, and
for a thread of several. `/proc/<pid>/uid_map`, `gid_map`, `setgroups` and
`ns/user`: a map is written once, at offset 0 (`EINVAL` otherwise), its
opener's credentials captured at open and its writer's read at write, both
judged; a run must lie inside one of the parent's extents; `setgroups`
follows `setgroups_write`. `execve` in a child namespace ignores set-id bits
(U7) and gives the bounding set to the namespace's root alone. `capget`,
`capset` (Linux's subset rules), `PR_CAPBSET_READ` and `PR_CAPBSET_DROP`
act on the real sets in a child; `status` has `CapInh` to `CapAmb`.

Ids are told as the reader's namespace names them or 65534, and taken as
the caller's namespace maps them or `EINVAL` (U9), at `getuid` and its
kin, `getres*`, `getgroups`, `set*id`, `setgroups`, `stat` and `statx`,
`chown`, `status`, `SO_PEERCRED`, `SCM_CREDENTIALS` (both ways), System V
semaphores' `ipc_perm` (`IPC_STAT`, `IPC_SET`), `PRIO_USER`, and `si_uid`
of `kill`, `tkill`, `tgkill` and `SIGCHLD` (signalfd's `ssi_uid` too). The
last was never filled: every signal read `si_uid` 0, in the first
namespace as well. `chroot` needs `CAP_SYS_CHROOT` in the caller's
namespace. `mount`, `umount2`, `pivot_root` and the rest of `fsctl` still
need the first namespace's root, so a child namespace cannot unmount an
over-mount before N5's M3 and M4 locks exist.

Evidence: the `userns` boot line (FX-0888, `fs/userns_check.rs`) on x86_64:
36 calls, 14 refusals -- a namespace named apart, ids 65534 until mapped, a
`gid_map` before `deny` (U4), kernel root, two ids and a malformed map
(U2), a second write (U3), the owner's id mapped and read back as written
from inside and from the parent, fake root refused `sethostname`, `mount`,
`setuid` to an unmapped id and `setgroups`, every capability in its own
namespace and a bounding-set drop, kernel root in a namespace it made
refused the same (U1), a chrooted process refused (U6), a map opened by
root and written by an unprivileged holder refused (U3), a run spanning
two of the parent's extents and the gap between them refused, a set-id bit
ignored and the sets given to the namespace's root alone (U7), and a write
through a read-only bind of `/proc/sys` refused `EROFS` (the N3 review's
condition). Negative controls, each stopping the boot with its own message:
the range test narrowed to the first id ("a run spanning two of the
parent's extents ... was accepted"), the writer's check dropped ("a map
opened by root was written wider by an unprivileged holder"), the chroot
test off ("a chrooted process made a user namespace"), set-id bits
honoured ("execve of a set-id file changed an id in a child namespace"),
and `privileged()` ignoring the namespace ("kernel root inside a child
namespace was privileged ..."). The first version of that last control did
not fire, because fake root is kernel uid 1000 and unprivileged either way;
`root_made` is the case that matters.

How it differs from the design, and what is open:

* **U6 needs no recorded `Location`.** It compares the process's root with
  `Namespace::root()`, the top, as the N3 review asked; the first
  namespace's root is not stored anywhere.
* **A boot check has no current process.** `userns::acting_as` names the
  process a check drives, for the map files' opener and writer and the
  reader of `status`; it is `None` outside the check. The consultant should
  look at it: it is a static in a load file read by `userns::acting`.
* **`CLONE_NEWNS` by a holder of `CAP_SYS_ADMIN` in a child namespace is
  allowed.** It copies; nothing in the copy can be unmounted or remounted
  before N5.
* **Built after the consultant's first review of the core (2026-09-30):**
  U8's check (root inside a namespace refused a 0600 file, `chmod`,
  `chown`, `mknod` and `kill` for an id it does not map); U5's flags on
  `family::namespaces_asked`, which `clone` and `clone3` both ask first; F-37's
  fill for user namespaces in `kmem_check` (128 made to the limit, then
  `ENOMEM`); `si_uid`'s check for `kill` and `SIGCHLD`; the audit subject's
  check; and `userns::acting_as`, which now refuses being nested. Negative
  controls, each stopping the boot with its own message: `privileged()`
  answering by `holds(CAP_SYS_ADMIN)` ("root inside a namespace made a device
  node"), the `CLONE_FS` test dropped, the charge dropped ("a job made more
  than its limit could hold"), `encode` not translating ("si_uid did not read
  65534 ..."), and `Subject::of` recording uid 0 ("an audit record of root
  inside a namespace recorded an id that is not the kernel's").
* **The audit record's check is a check of the subject, not of a record.**
  No personality supplies a uid to the audit trail yet: `Subject::of` always
  answers `NO_UID`. The check requires that, or the kernel's uid, and that the
  pid and job are the caller's; the day a personality supplies one, this is
  where it would have to be the kernel id, and the check fails on uid 0.
  There is no way to sabotage a supplier that does not exist, so its control
  is `Subject::of` recording 0.
* **Not built:** a boot check of `unshare(CLONE_NEWUSER)` with a shared fs
  context (the harness has no process that shares its context; the code tests
  the reference count, and the flags half is checked); a real `execve` of a
  set-id file under a namespace (`Credentials::exec` is the decision point and
  is checked); the `max_user_namespaces` sysctl (N5 adds it, with the
  enforcement). `verify_root_map` needs file capabilities, which Ferrix has
  none of; add it if it ever does.
* **Gated:** `cargo xtask check`; the boot on x86_64, aarch64 and armv7a at
  `--smp 2`; `test-shell`, `test-vfs` and `test-init` on x86_64; and, in the
  landing's final run, `test-shell`, `test-vfs` on both Arm targets and
  `test-init --arch all`. `carry-coverage` and `gen-coverage-justification
  --check` run on the final rebase. *(Corrected by the review below: the
  landed tree's run did not include the `test-shell` and `test-vfs` rows.)*

**N4's review (certification consultant os-ad, 2026-10-01, after the fact:
d171ffe5 landed without the OK this section asked for): OK with
conditions.** Nothing found would have blocked it.

* **The item.** Touched only by data and a check: FX-0888 in
  `panic/catalog.rs`, `check_user_namespaces` in `stages_check.rs`, and
  `pub(crate) mod userns` in `syscall/mod.rs`; the manifest adds
  `syscall/userns.rs` to the load. No `unsafe` added, no upward reference.
* **U1, U6, U8's call sites.** `privileged()` is an effective uid of 0 in the
  first namespace; every `holds()` site asks one of the five honoured
  capabilities; mount, `sethostname`, `mknod`, raw sockets, limits, time and
  `kill` still need `privileged()`; U6 compares with `Namespace::root()`, the
  top, as N3's review asked. The map files follow `map_write` and
  `new_idmap_permitted`, stricter in two places.
* **The gate, as run on the landed tree** (ed8188a2, whose tree the gate's
  worktree held): `cargo xtask check`, the boots on x86_64 and aarch64 at
  four processors and armv7a at `--smp 2`, each with the `userns` line (118
  calls, 22 refusals), and `test-init --arch all`, by os-7c; and
  `test-shell` and `test-vfs` on x86_64 with ferrousli's busybox and on
  aarch64 and armv7a at `--smp 2` with musl's, by the review, all six passing
  (logs `osad-n4-*` in the gate host's queue logs). No armv7a boot at four
  processors and no release build ran on any N4 tip (the verification
  auditor, os-db).
* **The negative controls.** The commit claims ten. Five have logs, on the
  earlier tip 5bc2affc (`control-n4-{u8-caps,u5-fs,kmem-charge,si-uid,audit-uid}`);
  the range test, the writer's check, the chroot test, set-id bits honoured
  and `privileged()` ignoring the namespace have none anywhere (os-db).
  Until they are re-run on `main` with their logs kept, the checks they
  stand for are evidenced by the check alone.

What is owed, in `docs/BACKLOG.md` P2, with the next landing in
`userns.rs`, `credentials.rs` or `fs/userns_check.rs` (N5 or NP at the
latest):

* **The `EROFS` check's negative control.** N3's review asked that the write
  through a read-only bind of `/proc/sys` be refused by a check with a
  negative control. The check exists (`fs/userns_check.rs`); none of the ten
  controls is its.
* **U8's claim.** `VULNERABILITY-ANALYSIS.md` says no `DAC_OVERRIDE` in a
  child namespace "at all", but `vfs::Access::privileged` is a filesystem uid
  of 0 with no namespace, as §2.2 says: a namespace kernel root made keeps
  root's file override, where Linux refuses it over a file whose owner is not
  mapped. No escalation, since the process was root. Restate the row and add
  a 0600 read to `root_made`'s check, or make `Access` namespace-aware.
* **`userns::ACTING`** answers for any task while a check runs, and the
  check runs after the secondaries and devmgr have started. No other path
  reaches it today; answer only when the current task is the check's.
* **A quoted message.** The control "`privileged()` ignoring the namespace"
  would stop the boot at `userns_check.rs:326` (the host name), before the
  message its row quotes; it has no log. Run it and quote the message that
  fires.

*Settled by the small namespaces' landing (2026-10-01).* The `EROFS`
check has its control (the remount without `MS_RDONLY`: "a write through a
read-only bind of /proc/sys was not refused EROFS"). U8's row is restated for
a process whose kernel uid is not 0, `kernel_root_keeps_override` records the
difference as a check and a BACKLOG row holds it. `userns::ACTING` stores the
id of the task that set it and answers only for that task; it has no control,
because it is a check-harness hook and a race on it cannot be staged from a
boot check. The `privileged()` controls were run on this tree and their first
message depends on the tree: "`privileged()` ignoring the namespace" stops at
"kernel root inside a child namespace was privileged in the whole system's
sense" (`userns_check.rs`'s `root_made`, after the host name), because
`sethostname` here asks `capable_over` the UTS namespace's owner, as Linux's
`ns_capable(uts_ns->user_ns)` does, and no longer `privileged()`; on the tree
the review ran, before the UTS namespace, it stopped at the host name
(`userns_check.rs:326`). "`privileged()` as `holds(CAP_SYS_ADMIN)`" stops at
"root inside a namespace made a device node (CAP_MKNOD)".

**The small namespaces, designed (2026-09-30, os-smallns).** UTS, IPC and
cgroup namespaces, `setns(2)` by namespace descriptor, and the `unshare` and
`clone` flags for all five that exist. Pid namespaces (`docs/PIDNS.md`) and network ones are other
landings; the network flag stays `EINVAL`. Stage 13's roadmap file has the rest.

*The data.* A mount namespace stays in the fs context and a user namespace in
the credentials. The other three are named together in a `NsProxy`
(`syscall/nsproxy.rs`), one per process, behind a `SpinLock` in `Process`
beside `credentials`:

```
NsProxy { uts: Arc<UtsNamespace>, ipc: Arc<IpcNamespace>, cgroup: Arc<CgroupNamespace> }
UtsNamespace    { id, owner: Arc<UserNamespace>, names: SpinLock<(nodename, domainname)>, charge }
IpcNamespace    { id, owner: Arc<UserNamespace>, table: SpinLock<sem::Table>, charge }
CgroupNamespace { id, owner: Arc<UserNamespace>, root: Arc<Job>, charge }
```

The first of each kind is a static made on first use, with Linux's own
inode numbers (`UTS_NS_INIT_INO` 0xEFFFFFFE, `IPC_NS_INIT_INO` 0xEFFFFFFF,
`CGROUP_NS_INIT_INO` 0xEFFFFFFB) and the first user namespace as owner; the
ones made after it count up from 0xF9000000, and user namespaces from
0xF8000000, so that no two kinds share a number (the mount namespaces count
from 0xF0000000). `fork` copies the proxy; `execve` keeps it. A mount
namespace gains an `owner` the VFS does not interpret (an `Arc<dyn Any>` set
once by the kernel after `Namespace::copy`), so that `setns` can ask
`CAP_SYS_ADMIN` over it.

*Where it differs from Linux.* Linux keeps the proxy per task; here it is
per process, so a thread cannot leave its group's namespaces, and `clone`
with `CLONE_THREAD` and any `CLONE_NEW*` is `EINVAL` (as `unshare` from a
multithreaded process is for `CLONE_NEWUSER` already). `CLONE_NEWIPC` with
`CLONE_SYSVSEM` is `EINVAL`, Linux's.

*Making one.* `CLONE_NEWUTS`, `CLONE_NEWIPC`, `CLONE_NEWCGROUP` from `clone`,
`clone3` and `unshare`: `CAP_SYS_ADMIN` in the user namespace the caller will
be in (the new one if `CLONE_NEWUSER` is asked with them), `EPERM` without;
the new namespace's owner is that user namespace. A UTS namespace starts with
a copy of its creator's names. An IPC namespace starts empty. A cgroup
namespace's root is the creator's own cgroup (the parent's, for `clone`,
whatever `CLONE_INTO_CGROUP` says).

*Who may set the names.* `sethostname`, `setdomainname` and the two sysctl
files write the names of the caller's UTS namespace. The system calls need
`CAP_SYS_ADMIN` over the namespace's owner (`capable_over`), not "kernel
root": fake root may name the namespace it made and never the first's.

*The descriptors.* `/proc/<pid>/ns/{mnt,user,uts,ipc,cgroup}` stay magic
links whose text is `type:[N]`, and following one (`open`) now leads to an
inode of a new filesystem, `nsfs` (`fs/nsfs.rs`), one inode per namespace and
kind, numbered by the namespace's id: two opens are one `st_ino`. The inode
holds the namespace strongly, so a descriptor keeps it alive; the open file is
charged as every open file is, the namespace at its creation (F-37). The
file's `ioctl`s are `NS_GET_USERNS` (0xb701), `NS_GET_PARENT` (0xb702, user
namespaces only, `EPERM` for one the caller cannot see, `EINVAL` for another
kind), `NS_GET_NSTYPE` (0xb703) and `NS_GET_OWNER_UID` (0xb704, user
namespaces only); verified against `linux/nsfs.h` on the build host.

*`setns(fd, nstype)`* in Linux's order: `EBADF`; `EINVAL` for a file that is
not nsfs or a type that is neither 0 nor the file's; then by kind, `EPERM`
unless `CAP_SYS_ADMIN` over the target's owner **and** in the caller's own user
namespace (uts, ipc, cgroup); for a mount namespace also `CAP_SYS_CHROOT` in
the caller's own, `EINVAL` for a shared fs context, and the caller's root
and working directory become the target's root; for a user namespace `EINVAL`
from a multithreaded process, with a shared fs context, or into the caller's
own, `EPERM` without `CAP_SYS_ADMIN` in the target (which refuses every
ancestor), then all capabilities there. No pidfd form: a pidfd is `EINVAL`.

*Locks and order.* The proxy lock is a leaf, cloned out before use as the fs
context is. A UTS namespace's name lock is a leaf. An IPC table's lock takes
the place the one global table's took, before a set's state lock. Every
namespace is allocated and charged before any spin lock is taken; a failed
`clone` drops what it made with the child.

*The sites:* `clone`/`clone3`/`unshare` flags (`family.rs`, `namespace.rs`);
`sys_uname`, `sethostname`, `setdomainname`, the two sysctls (`system.rs`,
`procfs/render.rs`); every `TABLE` use in `sem.rs` and the undo list, which
now names its set's namespace; `/proc/<pid>/cgroup`, the `cgroup2` mount
root and the `cgroup.procs` move rule (`cgroupfs.rs`, `fsctl.rs`); procfs's
`ns` directory and `link_location`; `ioctl` (`fd.rs`); `setns`
(`namespace.rs`).

*Evidence planned:* the `smallns` boot line (FX-0892, `fs/smallns_check.rs`),
a check and a negative control for every rule above, and `kmem_check` fills
for the three new kinds.

**The small namespaces, built (2026-09-30, os-smallns; landed 2026-10-01).** UTS, IPC and cgroup
namespaces; `CLONE_NEWUTS`, `CLONE_NEWIPC` and `CLONE_NEWCGROUP` through
`clone`, `clone3` and `unshare`, alone or with `CLONE_NEWUSER` and
`CLONE_NEWNS`; namespace files; `setns` by descriptor and by pidfd. The
design above was built as written, with the differences listed below.

What a program sees:

* **UTS.** `sethostname`, `setdomainname`, `uname`'s `nodename` and
  `domainname` and `/proc/sys/kernel/{hostname,domainname}` are the caller's
  namespace's; a new one starts from its creator's names. The right to name
  is `CAP_SYS_ADMIN` over the namespace's owner (S2).
* **IPC.** The semaphore table is the namespace's, with its keys, ids,
  `IPC_INFO`, `SEM_INFO` and undo records; the sets go with the namespace.
  There is no `/proc/sysvipc/sem` in Ferrix to make per namespace.
* **cgroup.** The creator's cgroup is the root. `/proc/<pid>/cgroup` is told
  from the reader's root with `..` for what lies outside it; a `cgroup2`
  mounted by a process in the namespace has the root as its `/`; a writer of
  `cgroup.procs` there moves processes only between cgroups beneath it
  (`ENOENT` otherwise, Linux's `cgroup_procs_write_permission`).
* **Files.** `open` of `/proc/<pid>/ns/{mnt,user,uts,ipc,cgroup}` is a
  descriptor on nsfs (`fs/nsfs.rs`): one inode per namespace, `readlink` of
  `/proc/self/fd/N` reads `uts:[N]`, the four requests of `linux/nsfs.h`
  (`0xb701` to `0xb704`, read from the header on the build host) answer.
* **`setns(fd, nstype)`** with a namespace file or a pidfd, by the rules of
  S3 and S4. With a pidfd the flags are a mask of the five kinds and every
  namespace is judged before the first is joined.

How it differs from Linux, and from the design above:

* **Per process, not per thread.** A `NsProxy` belongs to the process.
  `clone(CLONE_THREAD)` with any of the three flags, and `unshare` or `setns`
  into one from a process of several threads, are `EINVAL` (Linux allows
  them). `unshare(CLONE_NEWUSER)` already was.
* **A shared fs context cannot be swapped**, so `setns` into a mount
  namespace and into a user namespace from a process that shares its fs
  context (`CLONE_FS`) is `EINVAL`; Linux copies the struct and succeeds for
  a mount namespace.
* **`/proc/<pid>/ns/*` opens by the same person or root** (S5); Linux asks
  `ptrace_may_access`, including dumpability, which Ferrix does not yet have
  for these links (the M8 landing, NP, adds it for `root`, `cwd`, `exe`, `fd`).
* **Native processes** (`process_create`) start in their creator's UTS, IPC
  and cgroup namespaces, as in its mount namespace since N3
  (`launch::load_native` carries the proxy). The first landing left them in
  the first namespaces, which the consultant found reachable: a process with a
  MANAGE job from `job_for_cgroup` made a child in the host's IPC namespace
  (B1). `namespace_check.rs`'s native-child check compares `ns/uts`,
  `ns/ipc` and `ns/cgroup` of creator and child.
* **A cgroup move is judged in the namespace of the descriptor's holder, in
  all three ways in.** A `cgroup.procs` write by the namespace its *opener*
  was in, recorded at open (Linux's CVE-2021-4197 fix); `CLONE_INTO_CGROUP`
  (`clone_target`) and native `job_for_cgroup` (no MANAGE over a cgroup
  outside the caller's root) by the caller's. Each has a check in
  `smallns_check.rs` and a control below (C1).
* **Conditions for later landings (consultant, 2026-10-01).** (C2) nsfs's
  `may_open` compares against the target's effective ids only; Linux's rule
  compares the caller's fsuid with the target's uid, euid and suid, and
  `readlink` of `ns/*` has no gate at all; NP replaces both by
  `ptrace_may_access` with dumpability. (C3) NP's scope includes `ns/*`, as
  §9's row says, and not only `root`, `cwd`, `exe` and `fd`. (C4) An nsfs
  descriptor keeps a mount namespace alive past its last process, and `setns`
  can drop the last reference: the write-out check owed before N5 must cover a
  namespace ended by closing an nsfs descriptor, and N5's pinning rule must
  say that a descriptor holds a pin.
* **Kernel root keeps its file override in a namespace it made.** The file
  system's `Access::privileged` is `uid == 0` and has no namespace, so kernel
  root (which is root of the first namespace as well) reads a 0600 file of
  an id the namespace does not map; Linux refuses it. U8's guarantee is for a
  process whose kernel uid is not 0, and `userns_check.rs`'s
  `kernel_root_keeps_override` records the difference.
* **A pidfd of an exited process may still be joinable** where Linux answers
  `ESRCH` (BACKLOG).
* **Ids of namespaces** count from ranges of their own so that no two kinds
  share an inode number; Linux's dynamic inode numbers are one range.
  Mount namespaces still print `mnt:[4026531840]`-style numbers from
  0xF0000000, user namespaces now from 0xF8000000.
* **`sethostname` by sysctl write** is judged by the file's mode only (root's
  0644), as before; fake root writes the file only through `sethostname`.
* No `/proc/sys/kernel/sem` or other per-namespace IPC sysctls, and no
  shared memory or message queues, which Ferrix does not have.
* `NS_GET_MNTNS_ID`, `NS_GET_ID`, `NS_MNT_GET_*` and the pid requests of
  a newer `nsfs.h` are `ENOTTY`.

Evidence. The `smallns` boot line (FX-0892, `fs/smallns_check.rs`) drives
all of it with check processes through the system-call layer: on x86_64,
aarch64 and armv7a at `--smp 2`; `test-shell` and `test-vfs` with the static
busybox as init on x86_64 pass with the line in their boots. The `kmem` line
gains UTS, IPC and cgroup namespaces and namespace files (F-37, §5).

Negative controls, each a one-line change run once in the queue and undone,
each stopping the boot with `smallns self-check failed: <message>`
(`kmem` ones with `kmem: a job made more than its limit could hold`):

| Rule | The one-line change | The message |
|---|---|---|
| a UTS namespace is a copy, then private | `nsproxy::make` keeps the creator's namespace | a name set in a child UTS namespace reached its creator's |
| `uname` tells the caller's names | `sys_uname` reads the first namespace's | uname did not tell a UTS namespace's own names |
| the sysctl tells the caller's name | `hostname` renders the first's | /proc/sys/kernel/hostname did not tell the reader's own UTS namespace |
| `unshare` gives the copy | the `unshare` branch for the three flags off | unshare(CLONE_NEWUTS) did not give a copy in a namespace of its own |
| a namespace is named apart | a copy takes the first's number (UTS, IPC, cgroup: three changes) | a new UTS namespace was not named apart by /proc/<pid>/ns/uts; ... IPC ... ns/ipc; a new cgroup namespace was not named apart by /proc/<pid>/ns/cgroup |
| S1 needs `CAP_SYS_ADMIN` | `if false && !allowed` in `nsproxy::make` | unshare(CLONE_NEWUTS) by uid 1000 was not refused EPERM |
| S2 the owner's right, not `privileged()` | `nameable` tests `held.privileged()` | fake root could not set the UTS namespace it made |
| S2 uid 1000 cannot name the first's | `nameable` lets everyone | *the `userns` line's* "fake root in a user namespace set the host name" first (the rule has two checks; the earlier one stops the boot) |
| S3 a child cannot join the first UTS namespace | the `may_join` test off in `join_small` | a process in a child user namespace joined the first UTS namespace |
| S3 nor the first mount namespace | the owner test off in `join_mount` | fake root joined the first mount namespace |
| a mount namespace's owner is recorded | `copy_namespace` does not record it | a mount namespace made in a user namespace could not be joined from inside it |
| S4 no ancestor user namespace | `check_user`'s capability test is `true` | a process joined an ancestor user namespace |
| S4 only the owner | the owner's-uid test of `capable_over` is `true` | a user namespace was joined by someone who did not own it |
| S4 not one's own | the own-namespace test off | setns into the user namespace the caller is in was not EINVAL |
| S4 every capability after | `join_user` does not enter | joining a user namespace did not give every capability there |
| `setns` of a non-namespace is `EINVAL` | `EPERM` in its place | setns of a file that is not a namespace was not EINVAL |
| `setns` type must match | the type test off | setns with a type that is not the file's was not EINVAL |
| `setns` changes the names | the UTS arm of `put_in_proxy` does nothing | setns into a UTS namespace did not change the names the caller tells |
| `setns` into a mount namespace resets the working directory | the working directory kept | setns into a mount namespace left the working directory behind |
| pidfd: no flag is `EINVAL` | the `flags == 0` test off | setns on a pidfd with no namespace flag was not EINVAL |
| pidfd: only namespaces that exist | `CLONE_NEWPID` in the mask | setns on a pidfd with CLONE_NEWPID, which does not exist, was not EINVAL |
| pidfd: every kind asked for | the IPC kind joins the cgroup instead | setns on a pidfd did not join every namespace asked for |
| pidfd: capabilities after the user join | the own-capability test always runs | an owner could not join a user namespace and its UTS namespace together |
| pidfd: and not before | the own-capability test never runs | a person with no capability joined a UTS namespace alone through a pidfd |
| pidfd: `CLONE_NEWUSER` is joined | the user arm off | setns on a pidfd left the caller outside what it joined |
| IPC keys are the namespace's | a new IPC namespace is the first's | a new IPC namespace saw the first's semaphore keys |
| `SEM_INFO` counts the namespace's sets | `info` reads the first's table | SEM_INFO in a new IPC namespace counted the first's sets |
| an IPC namespace ends with its last holder | a new one is leaked | an IPC namespace outlived its last holder |
| `CLONE_NEWIPC` excludes `CLONE_SYSVSEM` | the test off | CLONE_NEWIPC with CLONE_SYSVSEM was not refused EINVAL |
| no new namespace for a thread | the test off | CLONE_NEWUTS with CLONE_THREAD was not refused EINVAL |
| pid stays `EINVAL` | `CLONE_NEWPID` allowed in `namespaces_asked` | clone with CLONE_NEWPID was not refused |
| B1 a native child takes its creator's UTS, IPC, cgroup and network namespaces | `load_native` does not copy the proxy | `mount namespace self-check failed: a native child's UTS, IPC, cgroup or network namespace was not its creator's` |
| C1 a `cgroup.procs` write is judged in the opener's namespace | `write_to` asks the writer's (`acting()`) | a descriptor opened inside a cgroup namespace moved a process out of it after its writer left |
| C1 `CLONE_INTO_CGROUP` stays inside the creator's root | the namespace test in `clone_target` off | CLONE_INTO_CGROUP started a child outside its creator's cgroup namespace |
| C1 `job_for_cgroup` gives no MANAGE outside the root | the test answers `true` | a MANAGE handle was given for a cgroup outside the caller's cgroup namespace |
| N3 condition: a read-only bind of `/proc/sys` refuses writes | the remount drops `MS_RDONLY` | `user namespace self-check failed: a write through a read-only bind of /proc/sys was not refused EROFS` |
| N4 U1 `privileged()` is the first namespace's only | `privileged()` is `holds(CAP_SYS_ADMIN)` | root inside a namespace made a device node (CAP_MKNOD) |
| N4 U1 again | `privileged()` ignores the namespace | kernel root inside a child namespace was privileged in the whole system's sense |
| cgroup paths are the reader's | the common prefix is not counted | /proc/<pid>/cgroup did not read / at the namespace's root |
| ... with `..` outside | `..` written as `x` | a cgroup outside the namespace's root was not shown with /.. |
| a mount is rooted in the namespace | `cgroup2` mounts the whole tree | a cgroupfs mounted in a cgroup namespace did not have its root as / |
| S6 a move stays inside | the rule off | a process in a cgroup namespace moved itself outside its root |
| a clone's root is its creator's cgroup | rooted at the tree's root | a cloned cgroup namespace was not rooted at its creator's cgroup |
| one inode per namespace | each open takes a new number | two opens of one namespace were two inodes |
| the descriptor reads its name | the label of a UTS namespace is `ipc` | readlink of a namespace descriptor did not read its namespace's name |
| `NS_GET_NSTYPE` | answers 0 | NS_GET_NSTYPE of a UTS namespace did not answer CLONE_NEWUTS |
| `NS_GET_USERNS` shows only what the caller is inside | the test answers yes | NS_GET_USERNS showed a process a user namespace it is not inside of |
| `NS_GET_PARENT` only for user namespaces | a UTS namespace answers | NS_GET_PARENT of a UTS namespace was not EINVAL |
| `NS_GET_OWNER_UID` | answers owner + 1 | NS_GET_OWNER_UID did not tell the owner's id |
| S5 another person's link | `may_open` always true | uid 1000 opened another person's namespace |
| F-37: UTS, IPC, cgroup namespaces charged | `Charge::bytes(0)` for the namespace | kmem: a job made more than its limit could hold (the line before names which kind) |

The namespace files' fill in `kmem_check` has no control: the detached mount
the VFS charges bounds it, and the inode's few bytes do not change the count
a job makes. That is a gap in the evidence, not in the code's charge.

**Pid namespaces (2026-09-30, branch `stage13-pidns`, built on
`stage13-n4-userns`, not landed; `docs/PIDNS.md` is the design).** This is
the pid half that the first page of this document left out. `CLONE_NEWPID`
through `clone`, `clone3` and `unshare`, alone or with `CLONE_NEWUSER`;
a task in a namespace below the first holds its numbers in a `Numbers`
record (`syscall/pidns.rs`), and the machine-wide number stays the key of
the registry, the jobs, groups, sessions and the terminal. Every call that
names or reports a pid speaks the caller's namespace (PIDNS §4 lists the
sites, found by grep); the init of a namespace gets its orphans, takes the
namespace with it when it ends, and ignores what it has no handler for;
procfs is per namespace (`status` has `NStgid`/`NSpid`/`NSpgid`/`NSsid`;
`ns/pid`, `ns/pid_for_children`). The exit criterion's pid-1 part is here.
Evidence and differences are PIDNS §8 and §9. Two things in this document
change with it: §1.5's row for `CLONE_NEWPID` is no longer `EINVAL`, and
the N4 "gated so far" list above is unchanged by it.

**Network namespaces (2026-09-30, branch `stage13-netns`; landed 2026-10-01).**
`docs/NETNS.md` is the design and its §11 says what was built and how it
differs. What touches this document's rules: U8's honoured set gains
`CAP_NET_ADMIN` and `CAP_NET_RAW`, which a child user namespace holds over the
network namespaces it owns and over nothing else (`capable_over` is unchanged,
so neither reaches the first namespace's, a file or a process), each with a
boot check and a control in the `netns` line (FX-0893); `CLONE_NEWNET` is no
longer `EINVAL` through `clone`, `clone3` and `unshare`; and a network namespace
is created, configured and ended under the owner rule of §2.2, charged to the
creating job as §5 asks (three more fills in `kmem_check`).
