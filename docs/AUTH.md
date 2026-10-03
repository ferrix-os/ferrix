# Authentication: who a person is, proven to Ferrix

> **Approved by the customer, 2026-09-26: decisions 1-11 as recommended.**
> It was written the same day by the `auth-design` stream, which is now the
> `auth` stream. The customer had asked for "a true authentication mechanism
> for Ferrix that is both secure and somewhat still versatile, fitting to
> the current Ferrix design with a semi-microkernel layout". §9 is kept as
> the record of what was decided and why. Phase 1 (§7) is being built; P0
> belongs to ferrix-15, who works from §7's P0 row.
>
> **Paths.** The document was written before the repository relayout
> (`fddfc32d`, `docs/LAYOUT.md`) and its paths have since been rewritten to
> the new layout. Line numbers were read at `1a8bea54`, before the move,
> which moved no line inside a file.

## 0. The proposal in one page

One ring-3 service, **`authd`**, holds every credential on the machine and
is the only program that ever reads one. Everything that needs to know
whether a person is who they say they are asks `authd` and gets a verdict
back. That covers the lock screen, the console's login, `su`, `passwd`, a
privilege prompt, and later ssh. None of these programs reads a hash, links
a hash function, or keeps a failure count of its own.

```
  keyboard ─> input driver (ring 3) ─> kernel evdev ─> hyprix ─> hyprlock ┐
                                                                          │ AF_UNIX seqpacket
  console  ─> kernel tty ─> getty ─> login ───────────────────────────────┤ /run/ferrix/auth
  su, passwd, a PAM program (later) ──────────────────────────────────────┤ SO_PEERCRED says who
                                                                          v
                                  ┌──────────────────────────────────────────────┐
                                  │ authd   (uid `auth`, its own cgroup)         │
                                  │  policy per service   /etc/ferrix/auth/...   │
                                  │  methods: password (argon2id); later TOTP,   │
                                  │           FIDO2, fingerprint                 │
                                  │  throttle per account, audit log             │
                                  │  store /var/lib/ferrix/auth  (0700 auth)     │
                                  └───────────────┬──────────────────────────────┘
                                                  │ native channel, routed by init:
                                                  │ `ferrix.auth.seat` (phase 2)
                                                  v
                                  hyprix: unlocks the screen only on authd's GRANT
```

What makes this fit Ferrix rather than any Unix:

* **It is a service like a driver is.** The design line of
  `docs/ARCHITECTURE.md` §1 puts code that needs no privilege in ring 3 in a
  process of its own. `authd` runs under init in its own cgroup, as its own
  user, with no root. A fault in it costs a verdict, not the kernel.
* **The kernel does not grow an authentication function.** The certified
  item claims no identification or authentication (FIA) and no audit (FAU),
  on purpose (`docs/certification/SECURITY-TARGET.md` §9.1, lines 303-311).
  This design keeps it so. Authentication lives in the operational
  environment, and relies on the item only for isolation (O.ISOLATE),
  handles (O.CAPABILITY) and scrubbed frames (O.SCRUB). §8.1 is the
  amendment to the Security Target that says so.
* **Identity travels in the kernel's words.** Linux programs are identified
  by `SO_PEERCRED`, which Ferrix answers today
  (`src/kernel/src/fs/socket.rs:394-400`). Native programs and services are
  identified by the unit init routed them from, through the directory
  (`docs/INIT.md` §6), whose CONNECT names the client unit.
* **Policy is files in init's syntax.** A service's rules are an INI file in
  the same three layered directories as units, read by `src/lib/init/svc`'s parser
  (`src/lib/init/svc/src/ini.rs`). Adding a service is like adding a unit.

Phase 1 (§7) builds `authd` with passwords, `passwd`, the store and a gate,
and gives hyprlock a real backend. It is about 27 points and needs nothing
from the rest of the plan. Phase 2 moves the desktop off root and makes the
compositor, not the lock client, the judge of an unlock.

---

## 1. What Ferrix does today

Each fact below was read from the tree at `1a8bea54`.

| Area | What is there | Where |
|---|---|---|
| Ids | Four user ids, four group ids and supplementary groups per process. `fork` copies them, `execve` keeps them, the `set*id` calls follow Linux's rules. | `src/kernel/src/syscall/credentials.rs:1-28`, `:148-155`; `src/kernel/src/syscall/process.rs:433` |
| Privilege | An effective uid of 0 is all privilege. There are no capability sets: `capget` reports all or nothing, and `capset` narrows nothing. | `credentials.rs:15-28`, `:177-181`, `:485-556` |
| Set-id programs | A file with mode `04000` runs as its owner, and `02010` as its group, unless `PR_SET_NO_NEW_PRIVS` is set, which `fork` passes on and `execve` keeps. `AT_SECURE` is set when the ids differ. | `src/kernel/src/fs/mod.rs:289-298`; `src/kernel/src/syscall/exec.rs:335-345`, `:721-725` |
| `nosuid` | Enforced since `docs/NAMESPACES.md`'s N1 (2026-09-29; this row re-read then): a set-id bit on a `nosuid` mount is ignored by `fs::set_ids_on` on both exec paths, by path (`fs::open_program`) and by descriptor (`execveat`); the `mounts` boot line (FX-0885) checks it. | `src/kernel/src/fs/mod.rs:328`, `:366-376`; `src/kernel/src/syscall/exec.rs:673`; `src/kernel/src/fs/mount_check.rs` |
| File permissions | Enforced, from `src/lib/fs/vfs`'s `access` functions, against the filesystem ids. | `docs/ROADMAP.md:1424-1440` |
| Kernel-made processes | Every process the kernel starts is root's. That includes a **native process made with `process_create`, whoever made it**: the loader builds it with `Process::new`, which starts from `Credentials::root()`, and `process_create` uses its caller only for the job and image handles. | `src/kernel/src/syscall/exec.rs:181-195`; `src/kernel/src/syscall/process.rs:308-314`, `:369-371`; `src/kernel/src/syscall/native.rs:1290-1325` |
| Peer identity, Linux | `SO_PEERCRED` answers pid and **effective** uid and gid. When this was written they were the ids of whoever *made* the connecting socket; since K-E they are the connector's at `connect` and the listener's at `listen`, as on Linux (§8.3, E-01). `SCM_CREDENTIALS` is stamped only when asked for, and root may name any ids in it. | `src/kernel/src/fs/socket.rs` (`listen`, `connect_stream`); `src/kernel/src/syscall/sockets.rs:1053-1056` |
| Peer identity, native | A channel carries bytes and handles, and nothing about who wrote them. The directory's CONNECT carries the client *unit's* name, which init fills in. | `src/lib/proto/native-abi/src/rights.rs:1-9`; `docs/INIT.md:1395-1405` |
| Jobs from paths | `job_for_cgroup` gives `MANAGE` to a caller who may write that cgroup's `cgroup.procs`, and delegation chowns that file to a user. | `src/lib/proto/native-abi/src/nr.rs:201-211`; `docs/CGROUPS.md` §3.1, §5 |
| `/proc/<pid>` | Owned by the process's effective ids, always. `PR_SET_DUMPABLE` is kept as Linux keeps it -- inherited across `fork`, cleared by a set-id `execve` or a change of effective or filesystem ids, set again by any other `execve` -- and changes nothing here. `fd` entries are plain symbolic links, not Linux's magic links. | `src/kernel/src/fs/procfs.rs:797-808`, `:327-331`, `:980`; `src/kernel/src/syscall/attributes.rs:88-89`, `:308-314` |
| `ptrace` | Not in the call tables: no process can read another's memory, except through a VMO both hold. | `src/lib/proto/linux-abi` has no `Ptrace`; `SECURITY-TARGET.md` FDP_IFC.1 |
| Memory | No swap and no core dumps: a secret's page never leaves RAM. Frames are zeroed when handed to a new owner, not when freed. The kernel heap is not zeroed. `mlock` is `ENOSYS`. | `src/kernel/src/syscall/memory.rs:383-390`; `SECURITY-TARGET.md:144`, `:261`; `docs/BACKLOG.md` |
| Randomness | ChaCha20 seeded from firmware, the CPU's instruction and jitter. A machine with neither of the first two says at boot that it is not seeded. | `src/kernel/src/random.rs:1-33` |
| Device nodes | `/dev/console` is `0600` root. Cards and `event*` are `0660` root:root, and there is no `input` or `video` group. | `src/kernel/src/fs/devfs.rs:229`; `src/kernel/src/interfaces/display/mod.rs:447-450`; `src/kernel/src/interfaces/input/evdev.rs:237-241` |
| DMA | Contained by an IOMMU on x86-64 and AArch64. On ARMv7-A `virt` and the DK1, any ring-3 driver can read all memory, and the boot says so. | `docs/ARCHITECTURE.md:326-357` |
| Accounts | Busybox images carry `root:x:0:0` and `ferrix:x:1000:1000` with a home. `test-init` carries the same two. The foot compositor image carries only root. No image has `/etc/shadow`. | `tools/common/xtask/src/initramfs.rs:536-546`; `tools/common/xtask/src/init.rs:311-314`; `tools/common/xtask/src/compositor.rs:4360` |
| The btrfs root | The first boot unpacks the initramfs onto the volume. Later boots re-unpack only a changed archive. **A file the archive carries is replaced; a file it does not carry is kept.** | `src/kernel/src/fs/root_disk.rs:25-31` |
| getty | `setsid`, takes the terminal, then execs `$SHELL` as a login shell. "There is no `login` yet." | `src/user/system/linux/init/getty/src/main.rs:7-11`, `:78-84`; `src/user/system/linux/init/units/getty@.service` |
| init | Reads `User=` from `/etc/passwd`. Its control socket is `0666`, and `SO_PEERCRED` decides who may change state. A user may make a scope only under their own `user-<uid>.slice`. | `src/user/system/linux/init/init/src/spawn.rs:446-487`; `src/user/system/linux/init/init/src/control.rs:5-6`, `:67`; `docs/INIT.md` §10 |
| The desktop | hyprix is linked into the kernel as its init, so it and every client run as uid 0. Moving it under init is `docs/INIT.md` L10, not started. | `tools/common/xtask/src/compositor.rs:1237-1239`; `docs/INIT.md:810`, `:887` |
| `/dev/tty` | Mode `0666`, and the console to whoever opens it, whatever their controlling terminal or none (`docs/BACKLOG.md`, load ring). A uid-1000 program can write to the console and read what is typed there, so it can show a fake prompt and race the console's `login` for a password. `test-init`'s probe records it at every boot until the fix. `su` reads only a terminal on its standard input. | `src/kernel/src/fs/devfs.rs` (`tty`, 5:0, `Behaviour::Console`) |
| The lock | `ext-session-lock-v1`. Any client may take the lock. Only the client that holds it may unlock, on the lock object it was given. **Until 2026-10-03 that was not so**: hyprix held the lock by the holder's place in its list and kept that place when the holder died, and the server passed `unlock_and_destroy` on any lock object, so a client that came to sit at a dead locker's place could take a refused lock of its own and unlock the session with it. Fixed by the lock-holder commit of that day (the certification consultant's OK IF, ledger line 296): a lock whose holder went is held by nobody, and a refused lock unlocks nothing. A lock whose client died stays locked, and **a second client is refused even then**, so a crashed locker needs a reboot. | `src/user/system/linux/compositor/server/src/client/lock.rs:29-46`, `:113-121`, `:176-186`; `src/user/system/linux/compositor/hyprix/src/state.rs:3429-3447` (`Lock::held_by`, `Lock::renumber`), `lock_changed` |
| hyprlock | Authentication through `auth::Backend` (`ready`, `begin`, `respond`). On every desktop its backend is `Service`, `authd`'s client; it refuses to lock an account with no credential and unlocks only on `authd`'s ACCEPTED (P1.5, landed 2026-10-03, the certification consultant's OK IF of ledger line 295). `SIGUSR1` does not unlock. | `src/user/system/linux/compositor/hyprlock/src/auth.rs`; `docs/DESKTOP-CLIENTS.md` §5.2 |
| ferrousli | `getspnam_r` reads `/etc/tcb/<name>/shadow` or `/etc/shadow`, as musl does. `crypt` does DES, MD5, `$5$` and `$6$`. Blowfish gives `"*"`. There is no yescrypt and no Argon2. | `src/user/system/linux/ferrousli/src/shadow.rs:1-12`; `src/user/system/linux/ferrousli/src/crypt.rs:1-27` |
| Busybox | Built without PAM, with shadow passwords and libc's `crypt`, sha512 by default. `login`, `su`, `passwd`, `chpasswd`, `vlock` and `adduser` are built, and all are linked in `/bin`. | `~/.local/share/ferrix/busybox/ferrousli/src/busyboxconfig`; `tools/common/xtask/src/initramfs.rs:237`, `:243`, `:253`, `:260` |
| ssh | `sshdt`, key-only. "`sshdt` given no key and no password accepts anyone", which is why every boot authorizes a key. | `tools/common/xtask/src/ssh.rs:10-31`; `docs/ROADMAP.md:3491-3526` |

Two rows need action whatever the customer decides about the rest.

**`process_create` makes root processes for anyone.** Read together, the
rows on kernel-made processes and on jobs from paths say this. A uid-1000
program in a `Delegate=yes` service can get `MANAGE` on its own job and
make a VMO. `process_create` in that job then gives it a process running as
root. `test-init` already runs such a service (`docs/INIT.md:1263`). I have
read this and not booted it. Slice P0 (§7) is the fix: the child takes its
creator's credentials, as a fork child does (`process.rs:433`). It comes
with a boot check that makes the escalation and requires the refusal.

**An empty second field in `/etc/passwd` means no password** to busybox's
`login` and `su`. Every `/etc/passwd` Ferrix writes must keep `x` there, and
the store (§5) must never write a hash that matches an empty string.

---

## 2. Threat model

### 2.1 What is protected

| Id | Asset | Where it lives |
|---|---|---|
| AA.STORE | Credentials at rest: password hashes, and later TOTP seeds and FIDO2 public keys | `/var/lib/ferrix/auth`, on the btrfs root |
| AA.TRANSIT | A secret in transit between processes: keyboard to compositor to lock screen to `authd`, console to `login` to `authd` | kernel evdev and tty buffers, the Wayland socket, the auth socket |
| AA.MEMORY | A secret in the memory of `authd`, `login`, `hyprlock` and `passwd` while it is checked | their address spaces |
| AA.SESSION | A locked session: what is on the screen and what the keyboard reaches | hyprix |
| AA.VERDICT | The link between "authd accepted" and what is then allowed: an unlock, a uid change | the auth socket, and the seat channel (phase 2) |

These are the environment's assets, not the certified item's. The
Security Target's assets (`SECURITY-TARGET.md` §3.1) are memory, the
kernel, handles, devices and processor time, and this design relies on all
five.

### 2.2 Who attacks, and what stops them

The Security Target's threat agent is "unprivileged code running on the
TOE" (§3.2, lines 105-107). This design adds people, and it splits code by
the uid it runs as. The phase 1 and phase 2 columns differ because phase 1
leaves the desktop running as root.

| Agent | Wants | Phase 1 (desktop is root) | Phase 2 (desktop is a user) |
|---|---|---|---|
| **TA.WALKUP** A person at a *locked* screen, with the keyboard, the pointer and a USB port | In | The lock takes the keyboard (hyprix, stage 18). Only `authd`'s verdict opens it. The throttle (§3.5) makes guessing slow. A USB keyboard that types guesses gets the same throttle. | Same, and the compositor unlocks only on `authd`'s grant for the lock that is up (§3.7). Crashing the locker leaves the screen locked, and a new locker, started by a `bindl` key, may take over; it too needs the password. |
| **TA.WALKUP'** The same person at an *unlocked*, unattended screen | Keep access later | `passwd` asks for the old password first, so they cannot change it. They can do anything else root can: phase 1 does not defend this. | `passwd` and becoming root both ask for a password. They can run anything as the user, and that is out of scope (§2.3). |
| **TA.CLIENT** A compromised desktop client | The password, or an unlock | It is root and can read the store. Phase 1 does not defend this, and says so. | Same uid as the session. It cannot read the store (`0700 auth`) or `authd`'s memory (no `ptrace`, §1). It cannot forge the grant: the seat channel is init-routed to `sessiond`, which relays over the session's own socket pair and takes nothing grant-shaped from hyprix (§3.7). It cannot reopen that socket through `/proc`. It can guess only at the throttle's rate. Killing hyprlock and taking its lock over unlocks nothing (§3.7, `--boot hyprlock-session`). **Killing hyprix ends the session at the console's login and nothing of the user's starts it again (§6.4, P2.7)**. What it can keep: a program of its own moved into a scope of the user's outlives the session, with the user's files and network and no device, lock channel or later session's socket; closing that is the customer's decision (§6.4). **It can draw a fake lock screen and phish**, which no design on a same-uid desktop prevents (§2.3). **Since P2.6 that password is also root's** on an image where the user is in `wheel`, as `ferrix` is on `--everything` (decision 5): a phished password, or a guess at the throttle's rate, now reaches root through `su`. **Until `/dev/tty` is the caller's own terminal** (§1, `docs/BACKLOG.md`), any program of the user's can also write to the console and read from it, so it can show a fake prompt there and race the console's `login` for what is typed. |
| **TA.NET** A network attacker, once ssh or a network login exists | A shell | `sshdt` stays key-only (`tools/common/xtask/src/ssh.rs:29-31`). `authd` listens on no network socket. | Password or keyboard-interactive ssh goes through `authd` (phase 3), with the same throttle and audit. Until then it stays off. |
| **TA.ROOT** A Linux-ABI program running as root | Everything | Out of reach by design. Root reads any file and can replace `authd`. What still holds: the hashes are Argon2id, so a stolen store costs a lot of work per guess (§5.1). | Same. Phase 2 makes root rarer: no desktop client runs as root. |
| **TA.OFFLINE** Someone with a copy of the disk (`build/root.img`, the DK1's SD card) | Passwords, which people reuse | Argon2id with a per-user salt. Nothing else: there is no disk encryption. | Same. |
| **TA.DRIVER** A compromised ring-3 driver | Keystrokes, secrets in memory | x86-64 and AArch64: its IOMMU domain confines it (`ARCHITECTURE.md` §7). ARMv7-A and the DK1: it can read all memory, including `authd`'s, and the boot says so (`ARCHITECTURE.md:340-357`). The input driver sees every keystroke by its nature: it is in the trusted base of any password typed. | Same. |

### 2.3 Out of scope, said so

* **Phishing by code that runs as the user.** A program that runs as you
  can draw a window that looks like the lock and ask for your password.
  Only a trusted path, a key combination the compositor alone answers, can
  fix that, and even that only helps people who use it. §6.5 leaves room for
  one.
* **Root.** Nothing on a Unix defends against root. This design keeps root
  rare, and its audit log is only as trustworthy as root is.
* **Disk encryption, secure boot and measured boot.** `A.FIRMWARE` and
  `A.PHYSICAL` (`SECURITY-TARGET.md` §3.3) carry these, as they do today. A
  credential-sealed disk key is a later design, and §5.1's store format
  leaves room for it.
* **Side channels between processes on one core** (`SECURITY-TARGET.md`
  §9.5, V-06). Argon2id's first pass does not depend on the data, which is
  the reason to prefer `id` over `d`. Nothing more is claimed.
* **Denial of service by locking.** Anyone in a session may lock it. Locking
  is harmless: the owner types their password.

---

## 3. Architecture

### 3.1 Three ways to do it, weighed

| | **A. PAM-style modules in each program** | **B. A pam_unix-compatible `/etc/shadow`** | **C. A dedicated service (recommended)** |
|---|---|---|---|
| Who reads hashes | Every program that authenticates, or a set-uid helper it runs (`unix_chkpwd`) | Every program that authenticates | `authd` alone |
| Ported programs | Need a PAM library and its modules. Ferrix's programs are static, so the modules would be linked in, not loaded. | busybox `login`, `su` and `passwd` work unchanged, and so does any `getspnam` program | Ferrix's own `login`, `su` and `passwd`. Ported PAM programs through a shim (§4.3). |
| Hash | Any the module links | Only what the C library's `crypt` reads: `$6$` (sha512crypt) on musl and ferrousli (`crypt.rs:6-14`). That is not memory-hard. | Argon2id, memory-hard (§5.1) |
| Throttle and lockout | Each program's own. `pam_faillock` needs a shared, writable tally file. | None | One, per account, across every service |
| Audit | Each program's own log lines | None | One log, one format |
| A lock screen running as the user | Needs a set-uid helper, because the user cannot read the hashes | Same | Asks over a socket. Needs no privilege. |
| A new method (TOTP, FIDO2) | A module in every program, and its secrets readable by all of them | Not possible | One method in `authd`. Clients see only prompts. |
| Fits Ferrix's layout | No: it spreads the most sensitive code into every program | No: it is Linux's layout, not Ferrix's | Yes: a ring-3 service in its own cgroup, identified by the kernel, reached like any other |
| Cost | Largest: a PAM ABI, modules, and policy files that most people get wrong | Smallest: about a day | About 27 points for phase 1 (§7) |

B is what hyprlock's interim `Shadow` backend already does, and the
customer declined it as a policy. It is also a dead end as a mechanism. It
fixes the hash to the one the C library knows, and it needs every checker to
be root. It has nowhere to put a throttle. And it cannot grow a second
factor.

A is Linux's answer, and its reason for existing does not apply here. PAM
lets one binary load another vendor's module at run time, and every Ferrix
program is built from this tree. Its costs do apply: secrets in every
process's memory, a set-uid helper, and a per-program throttle.

C costs a daemon, a protocol and a store format. In return, the secret goes
to one process and the verdict comes back. The hash never leaves `authd`, a
new method is one change, and the lock screen needs no privilege. It is how
macOS (`opendirectoryd`), Windows (LSASS) and systemd-homed arrange the same
thing, and it is the shape `docs/ARCHITECTURE.md` gives any function that
needs no ring 0. **Recommended.**

### 3.2 `authd`

| | |
|---|---|
| Program | `/sbin/authd`, std Rust on `*-linux-musl`, like init (`docs/INIT.md` §2) |
| Crate | `src/user/system/linux/auth/authd`, in a workspace `src/user/system/linux/auth/` beside `src/user/system/linux/init/` |
| Runs as | its own user, `auth`, with a fixed system uid (§9, decision 10). It needs no root: it reads its own store, and the programs that change uid (`login`, `su`) are the ones that are root. |
| Unit | `auth.service` (`Type=simple`, started by its socket; no `User=`, since `authd` drops to `auth` itself after preparing the store; `MemoryMax=320M`, enough for one hash at the ceiling and a little more; `NoNewPrivileges=yes` once L13 lands) and `auth.socket` (`ListenSequentialPacket=/run/ferrix/auth`, `SocketMode=0666`), which init already supports (`src/lib/init/svc/src/kind/socket.rs:19-24`, `:83`) |
| Without init | Under the phase 1 desktop, where hyprix is pid 1, `exec-once = /sbin/authd` starts it as root. It binds the socket and then drops to `auth` with `setresuid`. It is the same binary and the same socket, so no client can tell the difference. |
| Offers | `ferrix.auth.seat` in the directory (phase 2, §3.7) |

One connection carries one conversation, as one PAM handle does. A client
that wants two, such as fingerprint and password at once (hyprlock's
upstream does this), opens two connections.

### 3.3 The protocol

Records on a `SOCK_SEQPACKET` socket, one per packet, at most 4 KiB. They
are fixed little-endian layouts in `src/lib/proto/auth-proto`, which allocates
nothing, as the directory's records do (`src/lib/proto/native-abi/src/directory.rs`),
so a native client could use it later. The conversation is PAM's own, so
the PAM shim of §4.3 is a direct translation:

```
client -> authd
  HELLO     version
  BEGIN     service, account (empty: the peer's own), method hint (empty: policy's)
  RESPOND   bytes                       an answer to the last PROMPT
  CANCEL

authd -> client
  PROMPT    secret | visible, text      PAM_PROMPT_ECHO_OFF / _ON
  INFO      text                        PAM_TEXT_INFO   ("2 attempts left before a 30 s wait")
  ERROR     text                        PAM_ERROR_MSG
  ACCEPTED  account, uid                the conversation is over
  FAILED    text, retry_after_ms        the conversation is over; ask again after the delay
  UNAVAILABLE text                      no verdict could be had: no such service, no credential set,
                                        the store unreadable. Never a silent yes.
```

`passwd` is a service like the others. Its conversation asks for the old
secret, the new one, and the new one again, and ends ACCEPTED once the store
is written. `authctl status <account>` is a one-record request, answered
only to root and to the account itself. It says whether a credential is set,
which methods exist, and until when the account is throttled. It never
reveals a hash.

What a verdict does not carry: a token for the client to hand on. `login`
and `su` are root, and they act on ACCEPTED themselves. The lock is the one
place where the verdict must reach a third party, and there `authd` tells
that party directly (§3.7) rather than trusting the client to carry it.

### 3.4 How a client proves who it is

**Over the socket: `SO_PEERCRED`.** `authd` reads the peer's pid, effective
uid and effective gid once, at accept. The uid decides what the peer may
ask. The pid is used only for the audit line and to read
`/proc/<pid>/cgroup` for it, because a pid can be reused and is never
evidence. The rules:

* **Any peer** may authenticate *as its own uid* for a service whose policy
  says `Account=self`: the lock screen, `passwd` on one's own password.
* **Only a root peer** may name another account. That covers `login` (root,
  from getty) and `su` (set-uid root, so its effective uid is 0).
* **Only a root peer or `auth` itself** may set another account's
  credential, reset a throttle, or read another account's status.

Until K-E, Ferrix gave a connection's peer the ids its *socket was made
with*, not the ones it had at `connect`. A program that made a socket as
root and then dropped to uid 1000 before connecting was still root to
`authd`. Since K-E (§8.3, E-01, closed) the kernel takes them as Linux
does. The accepted end names who called `connect`, as they were at that
call. The connecting end names who called `listen`, as they were then.
Handing the descriptor on changes neither. The rule above was written for
the old semantics, too: being root lets a peer *ask* and never lets it
*skip* a check, since a root peer still types the password.

**Over a native channel: the unit.** A channel says nothing about its
writer, so a native client gets no uid. What it gets is better for a
service: init routes an OPEN only from a unit whose file lists the name in
`Uses=`, and its CONNECT names that unit (`docs/INIT.md` §6, `:1395-1405`).
`authd` uses this for exactly one thing, the seat channel (§3.7), which
only `hyprix.service` may open.

### 3.5 Throttling, not lockout

Every failure is counted per **account**, not per peer, so a thousand
connections guess no faster than one:

* Each failed attempt costs a fixed `FailDelaySec=` (2 s, pam_unix's) before
  FAILED is sent. hyprlock already waits for it, and will use
  `retry_after_ms` instead of its own fixed delay. Until that FAILED has gone
  out, no attempt on the account is looked at, from any connection.
* From the fourth consecutive failure, the next attempt is refused before it
  is checked until `2^(n-3)` seconds have passed, capped at 300 s. INFO says
  so in words ("wait 16 s"), and hyprlock shows it as `$PAMFAIL`, as it
  shows `pam_faillock`'s text today.
* A success resets the count. The count is kept in the store (§5.2) with
  `fsync`, so restarting `authd` or rebooting does not reset it.
* An unknown account is checked against a dummy hash with the same
  parameters, and fails with the same text in the same time. It is also
  counted and throttled as an account is, in a bounded table in memory keyed
  by a keyed hash of the name, so that from the fourth failure on it answers
  "wait" just as an account does (ferrix-55's review). A caller learns nothing
  from the answers about which accounts exist, and naming accounts makes no
  file. A restart of `authd` forgets those tallies and not an account's, and
  only root can restart it.
* Hashing is serialized: one Argon2id at a time for the whole machine. That
  bounds `authd`'s memory to one hash's (§5.1), and makes guessing through
  many services no faster.

**No permanent lockout by default.** On a desktop with one person, a lockout
is a denial of service against that person, and anyone who can reach the
lock screen can trigger it. The delay cap achieves what a lockout is for: at
one attempt per 300 s, a 10,000-word guess list takes a month. `authctl
reset <account>` (root) clears a throttle. A policy may still ask for a hard
lockout per service with `LockoutAfter=` (decision 6).

### 3.6 Audit

One line per conversation's end, and one per credential change:

```
auth: service=hyprlock account=ferrix peer=pid:812,uid:1000 unit=session-1.scope method=password result=failed failures=4 wait=2s
auth: service=passwd account=ferrix peer=pid:903,uid:1000 unit=session-1.scope result=changed
```

The line never holds the secret, its length or its hash. It goes to
`authd`'s standard output, which init keeps in its per-unit log
(`svc log auth`, `docs/INIT.md` §10). It is also appended, with `fsync`, to
`/var/log/ferrix/auth.log` (`0600 auth`), capped at 1 MiB and rotated once.
Root can edit both, which §2.3 already says. This is FAU outside the item,
as the Security Target's §9.1 expects.

### 3.7 Who may unlock: the compositor decides, on `authd`'s word

**Built 2026-10-03 (P2.5)** for a session that runs as its user, which is
`run-compositor --everything`'s. The design below is the one the
certification consultant reviewed (OK IF, ledger line 299, conditions G1 to
G9), with what building it changed said where it changed.

The problem: the client that holds the lock decided alone. Once the desktop
is a user's, every client is that user's, and the lock holder is the one
process between a same-uid program and the session. So a session's lock
goes only on `authd`'s word that the session's user typed their password
while that lock was up.

**The channels.** `auth.service` says `Offers=ferrix.auth.seat` and the
session's `hyprix.service` says `Uses=ferrix.auth.seat`, so init routes
the name to that unit alone (`docs/INIT.md` §6). Init gives a service's
bootstrap channel to the process it spawned, and that slot is sealed at
`execve`; in a session that process is `sessiond` (root, the seat's owner),
and hyprix is its fork-and-exec child as the user. A native handle cannot
pass through `SCM_RIGHTS`, and hyprix has no native channel to take one on,
so `sessiond` cannot hand the endpoint on: it **relays**. It already is the
one process hyprix trusts, for the devices.

```
authd ==ferrix.auth.seat== sessiond ==fd 4 socketpair== hyprix (uid 1000)
       ARM, DISARM ->                 <- locked N, unlocked N
       <- SEAT_READY, GRANT           -> grants on|off, grant N
```

The lock's lines go on a socket pair of their own, descriptor 4
(`compositor_seat::lock`), not on the devices' descriptor 3: that one is
request and answer, and a grant arriving on its own could be read as a
device's answer. Like descriptor 3 it has no path, hyprix marks it
close-on-exec at once, and a uid-1000 program cannot reopen it through
`/proc/<hyprix>/fd/4` (a socket reopens as `ENXIO`, as on Linux) or
`ptrace` hyprix (Ferrix has no `ptrace`).

**The rules.**

1. hyprix numbers its locks, an epoch that only grows, and says `locked
   N`. `sessiond` sends `ARM {uid, N}` to `authd` for the session's uid; one
   epoch is armed at a time, and a new one replaces it. An epoch that does
   not grow, or any line from hyprix that is not `locked` or `unlocked`, ends
   the session: nothing grant-shaped is taken from hyprix's side.
2. When a conversation of a `Grant=seat` service (hyprlock's alone) ends
   ACCEPTED for the armed uid, `authd` sends `GRANT {uid, N}` and disarms
   it, and its audit line says `result=granted why=seat-epoch=N`. ARM,
   DISARM and GRANT exist only on the seat channel; from the socket they are
   records out of turn. `authctl unlock-seat` (uid 0 by `SO_PEERCRED`)
   grants the armed epoch, audited with who asked.
3. `sessiond` passes `grant N` to hyprix only for the session's uid and the
   epoch it armed, once.
4. hyprix keeps a grant with its epoch and spends it on that lock's unlock,
   which must still be the holder's own `unlock_and_destroy` on the lock it
   was given. An unlock that comes before its grant waits
   `grants::UNLOCK_WAIT`, 2 s (the grant and the answer travel different
   ways); with no grant by then it is refused, and the lock is held by
   nobody. A waiting unlock outlives its holder: hyprlock exits the moment
   it has asked, and the person who typed the right password is not left at
   an orphaned lock because the grant came a moment later (the consultant's
   S2). A grant for another epoch, a late one, or one for a lock held by
   nobody and not waiting, is nothing.
5. **Takeover.** A lock held by nobody -- its holder died, or its unlock was
   refused -- may be taken over by a new locker: a new epoch, `locked`
   told afresh, the windows hidden throughout, and nothing about it an
   unlock. A lock whose holder lives still refuses every second locker.
   This is `misc:allow_session_lock_restore`, made safe; in a session it is
   always so, and outside one never. **Who starts the new locker** (G7):
   the user's `bindl` (a bind that works while locked, as Hyprland's does;
   with a modifier, `bindl = SUPER, L, exec, hyprlock`, so that typing at
   the lock does not start one) or hypridle's `lock_cmd`; a person at the locked screen presses the
   key, types the password, and the new lock goes on its own grant. That
   path never unlocks by itself.
6. **While no grant can come** -- `sessiond` has no seat channel, or it
   went -- hyprix takes no new lock in a session, and says so: a lock nothing
   can open would lock the person out, and one opened without a grant would
   be no lock. A held lock stays locked until a grant can come again. A
   compositor whose lock channel `sessiond` named but which could not take
   it is in this state for good (the consultant's S1); `authd` runs as uid
   90, so no program of the user's can make the window by killing it.

A desktop with no session user has no descriptor 4. There hyprix keeps the
holder's word, as in phase 1, since every client there is root.

**`misc:lock_grace`** is hyprix's (seconds, default 0, capped at
`grants::LOCK_GRACE_CAP`, 10 s, whatever the user's file says). Within it,
measured on hyprix's monotonic clock from when it took the lock, a fresh
lock's holder unlocks with no grant: that is hyprlock's `--grace`, which
hyprlock cannot extend. **A takeover gets no grace** (G1): otherwise killing
hyprlock and taking over its lock would buy one. What is left, and accepted
with §2.3's fake lock screen: a program that takes the lock *before*
hyprlock does can unlock within the grace it was given.

**`SIGUSR1`** does not unlock; `authctl unlock-seat` is root's audited way.

**Tested by** `cargo xtask test-compositor --boot hyprlock-session`, the
desktop as `ferrix`: a lock goes on `authd`'s grant; a program of
`ferrix`'s kills hyprlock, takes its lock over and asks to unlock at once,
and the screen stays locked; the same program cannot open descriptor 4
through `/proc`; and a new hyprlock, started by a `bindl`, takes the lock
over and the password lets it go. Its negative controls, through
`gate.sh control`, make a takeover granted, make hyprix ignore grants, and
make `authd` grant nothing; each must fail the boot. Host tests cover every
rule above in `hyprix/src/state/grant_tests.rs`, `sessiond/src/relay.rs`,
`authd/src/tests.rs` and `compositor_seat::lock`.

**Closed since, by P2.7** (§6.4): killing hyprix no longer brings back a
fresh, unlocked desktop; the session ends at the console's login.

### 3.8 Secrets in memory

* **One secret type.** `src/lib/proto/auth-proto` gives `Secret`: a
  fixed-capacity buffer (256 bytes, the most any method needs) that is never
  `Clone` or `Debug`. It is zeroed with volatile writes and a compiler fence
  on drop. `authd`, `login`, `passwd`, `su` and hyprlock hold typed secrets
  only in it. A fixed buffer is not moved by a `Vec` growing, which is how
  copies of a secret usually survive in Rust.
* **Argon2id's working memory** is zeroed after each hash, before it is
  freed. It is the largest copy of anything derived from the password.
* **No swap, no core dumps** (`src/kernel/src/syscall/memory.rs:387`), so
  nothing writes a secret's page to disk. `mlock` is `ENOSYS`
  (`docs/BACKLOG.md`). `authd` calls it anyway and ignores the error,
  so it is right the day the kernel grows swap. Accepting it as a no-op is
  slice K-D.
* **The kernel's copies.** A password passes through the tty's line buffer
  or evdev's queue, then the Wayland socket's buffer, then the auth
  socket's. A frame is zeroed when it is handed out again (O.SCRUB). A
  kernel heap allocation is not, so a freed socket buffer holds its bytes
  until reused. Nothing in user space can read it back without a kernel
  bug. Zeroing the socket, pipe and tty buffers when they are freed is slice
  K-C, as defence in depth.
* **`/proc/<pid>`** of hyprlock, `login` and `authd` should not be readable
  by the same uid. `PR_SET_DUMPABLE 0` is how Linux programs ask for that,
  and Ferrix keeps it and ignores it (§1). A set-id `execve` or an id change
  clears it, as on Linux (since 2026-09-30); slice K-B makes procfs honour it.
  It matters little today, because `fd` entries are plain links (§1). It
  matters as soon as anything like `/proc/<pid>/mem`, `environ` or Linux's
  magic `fd` links arrives, and the rule is cheaper before them than after.

---

## 4. Versatility

### 4.1 Methods

A method is one Rust trait in `authd`: given the account's stored
parameters, run a sub-conversation (prompts in, responses out) and say
accepted or not. The store holds a line per method an account has (§5.2),
and the service policy says which are needed.

| Method | Stored | Phase | Needs |
|---|---|---|---|
| `password` | Argon2id PHC string | 1 | nothing |
| `password` legacy import | `$6$` / `$5$` string | 1 | the SHA-2 crypt code hyprlock already tested against Drepper's vectors (branch `hyprlock`, `src/user/system/linux/compositor/hyprlock/src/crypt.rs`), moved into `authd`. It verifies an imported hash, then rewrites it as Argon2id on the first success. |
| `totp` | RFC 6238 seed, digits, period | 3 | HMAC-SHA-1 and a clock that is right, which on a board without a battery means NTP first (`ntpd` is in busybox) |
| `fido2` | credential id and COSE public key, per key | 3 | CTAP2 over USB HID. `src/user/system/native/drivers/usb/usbhid` exists, and the DK1 has USB host (`src/kernel/src/platform/st/stm32mp1/usb.rs`). A hidraw-style path from that driver to `authd` is the unsized part. |
| `fingerprint` | a reader's template handle | later | a reader driver. There is none. |
| `sshkey` | nothing: ssh keys stay in `~/.ssh/authorized_keys` | 3 | `authd` only records and throttles an ssh login's verdict, which `sshdt` makes itself. |

### 4.2 Policy per service

A service is a file named after it, in the three layers units use:
`/lib/ferrix/auth/services/` from the image, `/etc/ferrix/auth/services/`
from the admin, and `/run/ferrix/auth/services/` at run time. A file in a
higher layer replaces one of the same name, and a `<name>.d/*.conf` drop-in
changes keys in it (`docs/INIT.md` §4.1). The parser is `src/lib/init/svc`'s
(`src/lib/init/svc/src/ini.rs`), so a policy with a mistake is a warning in the same
words as a unit with one.

```ini
# /lib/ferrix/auth/services/hyprlock
[Service]
Description=Unlock the screen
Account=self          # only the peer's own uid
Methods=password      # later: "password totp" (both), "fido2|password" (either)
Grant=seat            # tell the seat owner (§3.7)
FailDelaySec=2

# /lib/ferrix/auth/services/login
[Service]
Description=Log in on a terminal
Account=any
Callers=root          # getty's login is root; nobody else may name an account
Methods=password
FirstPassword=local   # an account with no credential may set one here, on a local console only (§5.4)

# /lib/ferrix/auth/services/su  (as built, P2.6)
[Service]
Description=Become root
Account=caller        # authenticate the person asking, not the target (decision 5)
Callers=any           # su connects with the person's own uid; see below
Methods=password
TargetGroup=wheel     # the caller must be in wheel; asked before any password
FailDelaySec=2

# /lib/ferrix/auth/services/passwd
[Service]
Account=self
Methods=password      # the old one first, then the new one twice
```

| Service | Who asks | Account | Methods | Phase |
|---|---|---|---|---|
| `hyprlock` (or `auth:pam:module`) | the lock client | self | password; later + fingerprint or FIDO2 | 1 |
| `passwd` | Ferrix's `passwd` | self; root for anyone | password | 1 |
| `login` | Ferrix's `login` on a getty | any, from a root caller | password | 2 |
| `su` | Ferrix's `su` | the caller, for a wheel member | password | 2 |
| `greeter` | a graphical login | any, from `sessiond` | password | 3 |
| `polkit`-style prompts | a privileged service asks for the session user's consent (§4.5) | the session user | password | 3 |
| `sshd` | `sshdt`, keyboard-interactive | any, from a root caller | password + TOTP | 3 |

An unknown service name is UNAVAILABLE, never a default policy, so a typo
cannot open a door.

### 4.3 Programs that expect PAM or `/etc/shadow`

Three kinds of program, and a different answer for each:

1. **Programs this tree writes.** Ferrix's own `login`, `su` and `passwd`
   (§7) speak the protocol. The busybox applet links of those three names in
   `/bin` (`tools/common/xtask/src/initramfs.rs:237`, `:243`, `:253`) are replaced by
   Ferrix's programs, as uutils and zinc already replace busybox applets
   (`docs/UUTILS.md`). The same goes for `chpasswd`, `cryptpw`, `mkpasswd`,
   `adduser` and `vlock`, which would otherwise write or read hashes
   themselves: they are unlinked, or, for `vlock`, replaced with a small
   client.
2. **Ported programs that use PAM**: `sudo`, OpenSSH, `swaylock`, upstream
   hyprlock's C++. A **PAM shim in ferrousli** gives them `pam_start`,
   `pam_authenticate`, `pam_acct_mgmt`, `pam_chauthtok`,
   `pam_open_session`, `pam_close_session`, `pam_end`, `pam_get_item`,
   `pam_set_item` and `pam_strerror`, with Linux-PAM's ABI. Its one "module"
   is the protocol: `pam_start(service, user, conv)` is BEGIN, and each
   PROMPT, INFO and ERROR is one message to the program's `conv` callback.
   It loads no modules and reads no `/etc/pam.d`, because `authd`'s policy
   is the policy. The work is about 5 points, in phase 3 (ferrousli stream).
3. **Ported programs that read `/etc/shadow` directly**, through
   `getspnam` and `crypt`, and static musl programs (Alpine's busybox)
   whatever they call. **Refused, safely**: there is no `/etc/shadow`, so
   `getspnam` finds nothing and the check fails. No empty password is ever
   the result, because `/etc/passwd` says `x` (§1). A shim that answers
   `getspnam` with a marker hash, which ferrousli's `crypt` then checks with
   `authd`, would make busybox's `login` and `su` work over the service
   (busybox is built against ferrousli, `docs/BACKLOG.md` "The busyboxes").
   It is possible, and about 2 points. I recommend against it (decision 7):
   it keeps a second, quieter path into authentication, and Ferrix's own
   programs cover the three that matter.

A native program (no libc) links `src/lib/proto/auth-proto` and speaks the same
records over the socket, or over a directory channel if its unit is given
`Uses=ferrix.auth`. That name is reserved and not needed yet.

### 4.4 hyprlock's configuration, mapped

The hyprlock stream answered the questions this section depends on
(2026-09-26). Its `check()` already runs on its own thread, and its
`$PAMPROMPT` is hard-coded today. It asks for this interface:

```rust
pub trait Backend: Send + Sync {
    /// Start a conversation. The first prompt's text is known before the
    /// person types, which is what `$PAMPROMPT` shows at lock time.
    fn begin(&self) -> Result<Prompt, Verdict>;
    /// Answer the last prompt.
    fn respond(&self, secret: &Secret) -> Next;   // Prompt | Accepted | Failed | Unavailable
}
```

| hyprlock.conf | On Ferrix |
|---|---|
| `auth { pam { enabled = true } }` | the `Service` backend: a connection to `/run/ferrix/auth`, BEGIN with the service name below and the peer's own account |
| `auth:pam:module = hyprlock` | the service name, so the policy is `/etc/ferrix/auth/services/hyprlock`. A module with no policy file is UNAVAILABLE, and hyprlock says so. |
| `$PAMPROMPT` | the first PROMPT's text, fetched at lock time by `begin()`. hyprlock re-prompts only when the text changes, as upstream's `Pam.cpp` conversation does. |
| `$PAMFAIL`, `$FAIL` | FAILED's text, or an INFO received since the last prompt. "wait 16 s" is shown the way `pam_faillock`'s "left to unlock" is. |
| the 2 s refusal hold | `retry_after_ms` from FAILED, in place of hyprlock's fixed delay |
| `$ATTEMPTS` | hyprlock's own count, unchanged |
| `auth { fingerprint { enabled = true } }` | a second connection, BEGIN with method hint `fingerprint`, run beside the password conversation. Whichever is ACCEPTED first unlocks, as upstream does. Until a reader exists it is UNAVAILABLE and hyprlock logs it and turns it off, as it already does. |
| `fingerprint:ready_message`, `present_message` | INFO texts the fingerprint method sends |

Phase 1 replaces `Missing` with `Service`. `Missing` stays only as the
verdict when the socket is absent: UNAVAILABLE, "no authentication service
is running". `Shadow` leaves the binary, and its tested rules move into
`authd`'s legacy import. The gate's `Hashed` and `/etc/hyprlock/gate.hash`
are replaced by a gate image that seeds a real store entry (§5.3), so the
gate tests the path people use.

### 4.5 Privilege prompts (phase 3)

A privileged service sometimes needs the session user's consent: changing
the network, mounting a disk, or a non-root `svc stop`, which init refuses
today (`docs/INIT.md` §10). polkit's shape fits Ferrix's directory well:

1. The service sends `authd` `ASK { account: the session user, action,
   reason }` over its own directory channel, `Uses=ferrix.auth.ask`.
2. `authd` sends the prompt to the session's *agent*, a small client in the
   session that registered with `Offers=`-like consent. It is drawn by
   hyprix's session, like hyprlock is. The agent runs a normal conversation
   (§3.3) for service `polkit`.
3. `authd` answers the service yes or no. The service never sees the
   password, and the agent never gets the privilege.

The actions and who may consent to them are policy files, like services.

---

## 5. Credentials at rest

### 5.1 The KDF: Argon2id

| | Argon2id | yescrypt (`$y$`) | sha512crypt (`$6$`) |
|---|---|---|---|
| Memory-hard | yes | yes | no: a GPU runs many at once |
| Specified by | RFC 9106 (2021), with test vectors | a reference implementation and a draft | Drepper's 2008 text |
| On Ferrix today | nothing | nothing: ferrousli's `crypt` gives `*` for it | ferrousli's `crypt` (`crypt.rs:11-12`); hyprlock's Rust copy |
| Readable by `crypt(3)` programs | no | glibc's libxcrypt only | yes |
| Data-independent first pass (§2.3) | yes (the `id` part) | no | not applicable |

Being readable by `crypt(3)` is sha512crypt's only advantage, and §4.3 says
no program reads the hash. Of the two memory-hard hashes, Argon2id has the
RFC, the test vectors, and the side-channel property. **Argon2id**, written
as a PHC string (`$argon2id$v=19$m=…,t=…,p=…$salt$hash`), with a 16-byte salt
from `getrandom` and a 32-byte output.

**Written here, in `src/lib/crypto/argon2`**: BLAKE2b and Argon2id, `no_std`,
no `unsafe`, host-tested against RFC 9106 §5's vectors and BLAKE2's, under
Miri, with a fuzzer on the PHC-string parser. That is `docs/ARCHITECTURE.md`
§9's rule (byte logic in `src/lib/`, where every tool reaches it), and it is
what hyprlock already did for SHA-2. The alternative is RustCrypto's
`argon2` crate, which userland is free to use (the compositor workspace
already pulls 42 external crates), and is decision 2.

**Parameters, chosen when a password is set, and stored with it.**
`authd` measures the machine it runs on and picks the memory cost that takes
about the target time, between a floor and a ceiling:

| | x86-64, AArch64 | ARMv7-A (DK1: 2 × Cortex-A7, 512 MiB) |
|---|---|---|
| Target time for one check | 0.5 s | 1 s |
| Floor (OWASP's minimum) | m = 19 MiB, t = 2, p = 1 | the same |
| Expected choice | m = 64 MiB, t = 3, p = 1 (RFC 9106's second recommendation, with p = 1) | about m = 19-32 MiB, t = 2 |
| Ceiling | m = 256 MiB | m = 64 MiB |

**What one check took**, measured with `src/lib/crypto/argon2`'s
`examples/timing` (the fastest of three runs):

| Where | Floor (19 MiB, t = 2) | 64 MiB, t = 3 |
|---|---|---|
| The build host, natively (Ryzen 9 9900X), which a KVM guest runs at | 38 ms | 213 ms |
| `qemu-aarch64` as a Cortex-A72, under TCG | 146 ms | 734 ms |
| `qemu-arm` as a Cortex-A7, under TCG | 201 ms | 1064 ms |
| The DK1 (2 × Cortex-A7 at 800 MHz) | **estimate:** 0.4 to 0.6 s | **estimate:** 2 to 3 s |

Read on 2026-09-26, with the host's load between 30 and 40 from other
sessions, so the emulated rows are upper bounds. The emulator rows are
the emulator's speed, not a Cortex-A7's: TCG on a fast host runs 32-bit
Arm code far quicker than an 800 MHz core does. The DK1 row is worked out,
not measured, and stays an estimate until the board is free to time. One
block is about 6,000 instructions of 64-bit arithmetic done in 32-bit
halves, at about one instruction a cycle, and the floor is 38,912 blocks.
So an x86-64 or AArch64 machine lands well above the floor at 0.5 s, and
the DK1 at about the floor for its 1 s. That was the table's prediction,
and the floor holds on every target. Each `authd` also says what the floor
took when it first sets a password (`authd: argon2id at the floor ...`), and
`cargo xtask test-auth` prints that line for every architecture it boots.

**In the guest**, as `cargo xtask test-auth --arch all` read it on
2026-09-27 (QEMU under TCG, the build host's load about 25 to 30). Each boot
times the floor twice, once for the seed's first use and once for `passwd`,
and chooses from what it read:

| Guest | Floor (19 MiB, t = 2) | What `authd` then chose |
|---|---|---|
| x86-64 | 352 ms, 362 ms | m = 26 MiB, t = 2 |
| AArch64 | 620 ms, 307 ms | m = 19 MiB, then 30 MiB, t = 2 |
| ARMv7-A | 502 ms, 476 ms | m = 37 MiB, then 39 MiB, t = 2 (its target is 1 s) |

These are emulator speeds, and the two AArch64 readings show how much the
host's load moves them; a guest under KVM reads the build host's own figure
above. The floor held on every guest, as it must.

`p = 1` because `authd` checks one password at a time (§3.5), so a second
lane buys nothing. Because each hash carries its own parameters, a store
copied from x86-64 to the DK1 still verifies there, only more slowly. When
the floor rises, `authd` rehashes on the next success, as it does for a
`$6$` import.

**Emulation.** Gates run under TCG, which can be tens of times slower. A
gate image's seeds (§5.3) use the floor parameters, so a check under
emulation takes seconds, not minutes, and only those seeds use them.

### 5.2 The store

```
/var/lib/ferrix/auth/            0700 auth:auth
    users/<name>                 0600 auth:auth   the credential record
    state/<name>                 0600 auth:auth   failure count, throttle deadline, last success
/var/log/ferrix/auth.log         0600 auth:auth   §3.6
```

**On the root volume, never in the initramfs.** The btrfs root replaces
every file the archive carries whenever the archive changes
(`src/kernel/src/fs/root_disk.rs:25-31`). A password set with `passwd` would be
lost at the next rebuild if the archive carried the store. Because nothing in
the archive lives under `/var/lib/ferrix/auth`, the volume's copy survives
every rebuild, as a user's other files do. On a tmpfs root (test boots,
`--tmpfs-root`) the store starts empty every boot, which is right for a
machine that forgets everything.

**The record, a line per fact**, written only by `authd`:

```
format 1
account ferrix 1000
password $argon2id$v=19$m=65536,t=3,p=1$c2FsdHNhbHRzYWx0c2FsdA$…
changed 2026-09-26T17:40:00Z
```

Later methods add lines: `totp <label> <seed> <digits> <period>`,
`fido2 <label> <credential-id> <cose-key>`. An account may be `locked` (a
line `locked <why>`), in which case no method opens it. One whose record is
absent has **no credential**, which is not the same as an empty one and
never opens anything (§5.4).

**Checked against `/etc/passwd` at every use.** The record names the account
and its uid. If `/etc/passwd` gives that name another uid, `authd` refuses
with UNAVAILABLE and logs it. A uid given to a new account never inherits an
old account's password.

**Written atomically**: a new file beside the old one, `fsync`, `rename`,
`fsync` of the directory. Failure counts go in `state/`, not `users/`, so a
wrong guess never rewrites the credential file. One file per account, as
tcb's `/etc/tcb/<name>/shadow` (`src/user/system/linux/ferrousli/src/shadow.rs:4-5`), so one
account's change cannot damage another's.

### 5.3 Provisioning

| Moment | How a credential comes to exist |
|---|---|
| **Building an image** | `cargo xtask run … --auth-seed <account>` prompts on the host, hashes the password there with the same `src/lib/crypto/argon2` at the target's parameters, and places the PHC string in the archive at `/lib/ferrix/auth/seed/<account>` (`0600 root`). `--auth-seed-file <account>=<file>` does the same without a prompt, for CI. |
| **First boot** | `authd` imports each seed whose account has no record yet, and never one that has a record. So a seed sets the first password, and a later `passwd` change survives the next build even though the archive still carries the seed. The import is audited. |
| **A running system** | `passwd` for your own account (the old password, then the new twice). As root, `passwd <account>` for anyone, with no old password. |
| **A fresh image with no seed** | §5.4 |
| **Gates** | A gate image seeds a known test password at the floor parameters. Only gate images carry it, as only the gate image carries `hyprlock-gate` today. |

The seed is a hash and not a password, but a hash is still worth guessing
at. It is `0600 root` in the archive, and it is on disk only where the
image is.

### 5.4 Before any password exists

**No password is never "any password".** An account without a record cannot
be opened by any method.

* **The lock screen.** hyprlock asks `authd` for its own account's status
  (the record `authctl status` uses) at start. With no credential set it does not take the lock, and says why on
  screen and on standard error: "no password is set for ferrix: run
  `passwd` first". The customer's `SUPER+L` then does nothing visible
  instead of locking them out (decision 4).
* **The console.** `login` finds the account has no credential. **Built
  2026-10-03 (P2.3; the certification consultant's OK IF, ledger line 303).**
  `authd`, not `login`, decides whether the caller is at a **local
  console**: under a `FirstPassword=local` policy (`login`'s), asked by
  root (`Callers=root`), it reads the caller's `/proc/<pid>/stat` and
  takes it as local only if the controlling terminal's `tty_nr` is the
  console's, **5:1, which Ferrix encodes as 1281** (`test-init` reads it
  back on the target), and only if the same pid's real and effective uid
  are still 0, read from `/proc/<pid>/status` around it. A pty's session
  reads 0, as no terminal does; an unreadable file or a pid that is gone is
  not local either (`authd/src/local.rs`). Then it says "ferrix has no
  password. Choose one now:", asks for it twice, refuses an empty one or
  two that differ, stores it, audits `result=first-password` with the
  `tty_nr` it saw, and answers ACCEPTED: the person is in with the password
  they just chose. **Only a person's account** is offered one: a uid from
  `PERSON_UID_FIRST` (1000) to below `PERSON_UID_END` (60001) and a shell
  that is not `nologin` or `false` (`authd/src/accounts.rs`); root, `auth`
  and every system account get "no password is set", audited
  `no-credential,not-a-persons-account`. **What 5:1 is on each target**:
  in QEMU, the serial port, which is whoever holds QEMU's standard input or
  its socket; on the DK1, its UART over the ST-LINK's USB; on the Pixel 7,
  the CDC-ACM console over USB. A serial console that is reachable over a
  network counts as local under this rule. That is acceptable only because
  of the physical-presence assumption `A.PHYSICAL` already makes, which
  holds the machine's console to be in the room with its owner.
* **ssh** never offers a first password. Until a credential exists it is key
  only, as today.
* **root** has no record on a fresh image, so it is locked: nobody logs in
  as root by password. Phase 1's desktop is the exception (decision 3).

---

## 6. The session as a user, not root

### 6.1 Where it stands

**2026-10-03:** the customer decided that `run-compositor --everything`
runs as `ferrix`, and P2.4 and the device half of P2.5 are built for it
(`src/user/system/linux/compositor/sessiond`, `.../seat`,
`tools/common/xtask/src/session.rs`). `hyprix.service` runs `sessiond
--user ferrix -- /bin/hyprix ...`; `sessiond` stays root, makes
`/run/user/1000`, seeds `/home/ferrix` from `/etc/skel` once, and starts
hyprix as `ferrix` with one end of a socket pair as descriptor 3. hyprix
opens every card, render node and `event*` node through it, and `sessiond`
opens only those, by name, and hands the descriptor over with
`SCM_RIGHTS`; the nodes stay `0660 root`, and no path reaches the
channel. Every client is `ferrix`'s; `sshdt` and `udhcpc`, which must stay
root, are units of `graphical.target` instead of `exec-once` lines. The
kernel gives a pseudoterminal's slave to the process that opened
`/dev/ptmx`, as devpts does, so a terminal works as a user. Not built
yet: the graphical session's own scope (`login` gives a console session
one, P2.3), and
the other desktops, which still run as root. The grant hyprix unlocks on
(P2.5, §3.7) was built the same evening. **P1.5 landed the same day**
(the certification consultant's OK IF, ledger line 295): every desktop
image carries `authd` and `/bin/hyprlock`, which asks `authd` for the
session's own account's password, so on `--everything` that is
`ferrix`'s. A password is seeded only where `--auth-seed` or
`--auth-seed-file` asks; without one hyprlock refuses to lock and says
why, and `passwd` on the desktop sets one. The text below is the plan
as it was written before that.

hyprix is linked into the kernel as its init (`tools/common/xtask/src/compositor.rs:1239`),
so it and every program it starts are uid 0. It opens the card and the
`event*` nodes itself, and it can because they are `0660 root`
(`src/kernel/src/interfaces/display/mod.rs:447-450`, `src/kernel/src/interfaces/input/evdev.rs:237-241`).
`docs/INIT.md` L10 (6 points, not started) moves hyprix under init as
`hyprix.service`, with a scope per client (`docs/INIT.md` §5.6). L10 is
necessary. It is not enough on its own, because hyprix under init still runs
as root unless something gives it the devices.

### 6.2 What it takes

1. **`login` on the console** (P2.3, **built 2026-10-03**). `getty
   --login` execs `/bin/login` (`src/user/system/linux/auth/login`, Ferrix's own,
   in busybox's place) instead of root's shell; an image chooses it, and
   the gate images keep the root shell as their automatic login (decision
   8). `login` asks for a name and runs the `login` conversation, so
   `authd` alone decides, and offers a first password (§5.4). On ACCEPTED,
   still root, it asks init for `session-N.scope` under `user-<uid>.slice`
   (`N` counted from 1 at every boot under `flock` in `/run/ferrix/login`).
   An init that refuses is said on the console and in `login`'s line, and
   the login goes on without a scope, so **nothing may rely on the scope
   yet**: P2.7's ending of a session must not, until a refused scope stops
   the login (the consultant's F6). Then `setgroups` from `/etc/group`,
   `setresgid`, `setresuid`, each checked, and every id and the groups read
   back with `getresuid`, `getresgid` and `getgroups`; a mismatch execs
   nothing (F3). A `nologin` or `false` shell is refused. The environment is
   cleared to `HOME`, `USER`, `LOGNAME`, `SHELL`, `PATH`, and getty's `TERM`
   if it is a plain terminfo name (F7), and the shell runs as a login shell
   in the account's home. Three wrong passwords end `login`, and getty
   starts it again.
2. **A session manager, `sessiond`** (P2.4): root, a unit, the one owner of
   **seat0** (the machine's screens, keyboard, pointer and sound). It is
   logind's device half and seatd's whole job, and no more:
   * It opens the card, the `event*` nodes and `/dev/snd/*`, and passes the
     descriptors to the session's compositor with `SCM_RIGHTS`, which works
     today (`src/kernel/src/fs/socket.rs:34-40`). The nodes stay `0660 root`.
     **No user or group is given the input nodes**: with read access to
     `event*`, any program in the session could read the lock screen's
     keystrokes. Ferrix has no `input` group today (`evdev.rs:237`), and
     this design keeps it that way.
   * It starts the graphical session: `hyprix` as the account's uid, in
     `user-<uid>.slice/session-N.scope`, after the `login` or `greeter`
     conversation, or at once for an image configured to log a named user in
     automatically (decision 8).
   * It knows which session is active on the seat, which is what a second
     session and user switching would need later.
3. **hyprix takes its devices from `sessiond`** instead of opening
   `/dev/dri/card0` and `/dev/input/event*`, and uses the seat channel of
   §3.7 (P2.5).
4. **Kernel prerequisites**: P0 (native processes take their creator's
   credentials) before any non-root desktop, and K-B (dumpable) with it.
5. **`su`** (P2.6, **built 2026-10-03**; the certification consultant's OK
   IF, ledger line 309): Ferrix's own, `/bin/su`, set-uid root (mode 4755,
   in busybox's place on every image with `authd`). Run by root it asks
   nothing, as every `su`. Run by anyone else, the target must be root, and
   the person shows their *own* password (decision 5):
   * **The kernel says who asks.** `su` sets its effective uid back to the
     person's for the `connect` alone and takes root back from its saved
     uid at once: `SO_PEERCRED` is fixed at connect, so `authd` sees the
     person's uid, not root's. That is why the policy reads `Callers=any`
     where the first sketch said `Callers=root`: with `su` set-uid, root as
     the caller would have meant authenticating root, who has no password.
     An ACCEPTED of the `su` service grants nothing by itself -- it is the
     person's own password, which any `Account=caller` service already
     checks -- and only the set-uid program acts on it.
   * **`TargetGroup=wheel`**, checked by `authd` at BEGIN from
     `/etc/group` (primary group or listed): a non-member is refused before
     any password is asked, audited `not-in-group`.
   * `su` talks only to the compiled-in socket and only to a listener whose
     uid is 0 or 90 (U1), clears its environment before anything reads it
     (U2), and keeps root only on an ACCEPTED for the caller's own uid and
     account (U3); every id and group is then read back, with `login`'s
     shared code (`src/user/system/linux/auth/account`).
   * **While its euid is the person's** (U4) -- the `connect`, nothing
     else -- its `/proc/<pid>` entries are theirs, and Ferrix ignores
     `PR_SET_DUMPABLE` (§1). Nothing there exposes a secret: no secret has
     been typed yet; and in any case Ferrix has no `/proc/<pid>/mem`, no
     `process_vm_readv` and no `ptrace`, its `fd` entries are plain links,
     and a socket refuses to be reopened.
   * **The password comes from standard input when that is a terminal, and
     from nowhere else** (U5): with none, `su` refuses before connecting, so
     a password is never piped into it. It does not open `/dev/tty`, which on
     Ferrix is the console whatever the caller's controlling terminal is
     (`docs/BACKLOG.md`): a `su` in a pty would otherwise ask on the wrong
     screen.
   `test-init --arch all`'s `su` stage shows each of these.

### 6.3 Seats, and who may lock and unlock

* **One seat, one active graphical session**, in phase 2. More than one is
  later, and nothing here assumes it cannot happen.
* **Anyone in the session may lock it.** It is harmless (§2.3).
* **Only the session's own account unlocks it**, through `authd`'s grant.
  An administrator does not unlock another user's session by typing *their
  own* password. `authctl unlock-seat` (root, audited) is the one override
  (§3.7).

### 6.4 When the compositor dies

Under init with `Restart=`, a hyprix that is killed comes back as a *fresh,
unlocked* desktop running as the same user. Killing it would then be the
way past a locked screen, and any same-uid client can send that signal
(`src/kernel/src/syscall/credentials.rs:70-82`). So `hyprix.service` does not
restart into the same session. **The session ends with its compositor**:
`sessiond` stops the scope (`cgroup.kill`) and the seat goes back to login.
This is what GNOME does on Wayland, and it is the only safe answer.

**Built 2026-10-03 (P2.7; the certification consultant's OK IF, ledger line
306).**

* A session's `hyprix.service` has no `Restart=` (a root desktop's keeps
  `Restart=on-failure`, since every client there is root).
* `sessiond` puts the compositor in `user-<uid>.slice/session-<n>.scope`
  **before it execs** (E1): between fork and exec the child writes its pid
  down a pipe and waits; a thread of `sessiond`'s asks the init for the
  scope and answers, and anything but yes stops the exec, so nothing the
  compositor starts is ever outside the scope. `n` is `login`'s count, so
  a boot's sessions are numbered once. **A scope the init refuses is not a
  session**: none is started, so the session's end may rely on the scope
  (F6 for this path; host-tested in `sessiond/src/gate.rs` and `scope.rs`).
* When the compositor exits, `sessiond` asks the init to stop the scope,
  which signals and then writes `cgroup.kill`: every program of the session
  ends. Every way `sessiond` ends after the scope was made goes through
  this, a failure of its own included, with the compositor killed first
  (Q1). Should the init not stop the scope, `sessiond`, root, writes the
  scope's own `cgroup.kill`; should that fail too, it exits non-zero and
  says that the session's processes may live on (Q2). Then `sessiond` exits, and the init stops `hyprix.service` the same
  way: a service is stopped when its cgroup is empty, not when its main
  process exits, and a stop kills by `KillMode=` (control-group by
  default) and then writes `cgroup.kill` (`src/lib/init/svc/src/manager/service.rs`).
* The console's getty runs `--login` on a session's image, so the seat a
  session that ended goes back to is a login.
* **Nothing of the user's starts it again** (E2): `svc start` and `svc
  restart` of a system unit are root's alone ("only root may change the
  system"); a user may make scopes under `user-<uid>.slice` and nothing
  else; init starts a unit for a user's process only by socket or
  directory activation, and `hyprix.service` has neither.
* **No device outlives the session** (E5), from the code: `sessiond` opens
  each card, render node and `event*` node `O_CLOEXEC` (`seat/src/lib.rs`)
  and keeps no copy; hyprix holds them close-on-exec, so no program it
  starts inherits one; and the protocol server sends a client only the
  keymap's memfd and the clipboard's pipes, never a device. The devices
  close with hyprix, so a lingering program (below) cannot read the next
  login's keystrokes.

**What it costs** (E7): on a session's desktop -- `run-compositor
--everything` -- a compositor that crashes no longer comes back: the
session ends at the console's login, where the driver-stall recovery once
relied on `Restart=`. To have the desktop again: reboot, or root runs `svc
start hyprix.service`. A greeter, or a session started from a console
login, is §6.5's later.

**What it leaves** (E6): a program of the user's may move itself into a
scope of its own under `user-<uid>.slice` before the session ends, and so
outlive it. It keeps the user's files and network and nothing of the seat:
no device, no lock channel, and no Wayland socket of a later session unless
root starts one for that user. Stopping `user-<uid>.slice` when the user's
last session ends would close it (systemd-logind's `KillUserProcesses=`);
that is the customer's decision, filed in `docs/BACKLOG.md`.

**Tested by** `cargo xtask test-compositor --boot session-end`, the desktop
as `ferrix` with a compromised client of the session: a lock it is refused
while hyprlock holds the screen unlocks nothing; `authd`'s store is
`Permission denied` on ferrix's real record; it `kill -9`s hyprix, and the
session ends -- its heartbeat stops, no second session starts, the unit is
not active again -- and a process of ferrix's that had moved into a scope
of its own is refused `svc start` and `svc restart`; the console is a
login.

### 6.5 Now, and later

| Now (phase 2) | Later |
|---|---|
| console `login`, and a desktop started by `sessiond` for one account | a graphical greeter (hyprlock's widgets and layout, speaking the `greeter` service) |
| one seat | several sessions, and switching between them, with device revocation (`EVIOCREVOKE`, dropping DRM master) in the kernel |
| hyprix is the only client of the seat channel | a trusted path: a key combination only hyprix answers, which always shows the real lock or greeter |

---

## 7. Phased plan

In story points, each slice landed and gated on its own, owners by stream.
The gates are the ones `docs/BACKLOG.md`'s "What a landing runs" names for
the area touched, plus the boot named here. Every new boot stage has a
negative control that must be seen to fire, per the repository's rule.

### P0: the kernel hole (2 points, first, independent)

| | Slice | Owner | Gate | Points |
|---|---|---|---|---|
| P0 | `process_create` gives the child its creator's credentials, as `fork` does. A boot check makes a native process as uid 1000 in a delegated job and requires `getuid` in it to be 1000. Its negative control is the old `Credentials::root()`, which must fail that line. | ferrix-15 (was: kernel, native ABI) | the kernel row of the gate table; `test-init --arch all` | 2 |
| P0a | Init refuses `User=` and `Group=` on a `Type=native` unit, failing closed. Init makes a native service's process itself, so such a unit ran as root and the keys were silently ignored (found by ferrix-15 beside P0). **Done 2026-09-26**: the unit, `SupplementaryGroups=` too, loaded as `bad-setting`; P0b replaced the refusal the same day. | ferrix-15 | `test-init --arch all` | with P0 |
| P0b | With init's L11, a native service's process is made by a forked child that has already become the unit's user, so `process_create` (after P0) gives it that user's credentials. **Done 2026-09-26**: `test-init` runs `pong-as-user.service` (`User=ferrix`) and reads uid and gid 1000 in every role from its `/proc/<pid>/status`; the helper left root reads 0. | ferrix-15 | `test-init --arch all` | with L11 |
| P0c | A `Delegate=yes` unit with `User=1000` gets `MANAGE` on its own job through `job_for_cgroup` (`src/kernel/src/fs/cgroupfs.rs:356-357`), and native `job_set_limit` (`src/kernel/src/syscall/native.rs:1244`) asks for `MANAGE` alone, so the unit can lift its own `MemoryMax=` or `TasksMax=` to unlimited. Ancestor slices still bound it. The fix: a `SET_LIMIT` job right, granted only to a caller that may write `memory.max`. Found by ferrix-15 and confirmed by ferrix-2c in the code; after ferrix-55's OK. **Done 2026-09-26** (`54cba422`, F-40 closed): the `limits` boot line. | ferrix-15 | `test-init --arch all` | 2 |

P0 and P0c have one shape: `MANAGE` on a job is too coarse a right. A
delegated user holds it for its own subtree, as it must to move its own
processes, and it then reached everything `MANAGE` guards: making a root
process (P0) and lifting its own limits (P0c). Any new call that takes a
job should ask for the narrowest right it needs, not for `MANAGE`.

### Phase 1: `authd`, passwords, and a real hyprlock (27 points)

`authd` does not wait for P0b. It is a Linux-ABI program (std on musl),
started by init's Linux spawn path, which already sets `User=` before
`execve` (`docs/INIT.md` §16, "Spawn"). So `auth.service` runs as `auth`
under init from its first boot. Under today's desktop, where hyprix is pid 1
and there is no init, `authd` starts as root from `exec-once`, binds its
socket, and drops to `auth` itself with `setgroups`, `setresgid` and
`setresuid` before it reads the store (§3.2).

Phase 1 gives hyprlock the path it keeps. The socket, the protocol and the
store are the ones phase 2 uses, and phase 2 changes who runs hyprlock, not
what hyprlock does.

| | Slice | Owner | Gate | Points |
|---|---|---|---|---|
| P1.1 | `src/lib/crypto/argon2`: BLAKE2b, Argon2id, PHC strings; RFC 9106 and BLAKE2 vectors; Miri; a fuzzer on the parser; the timing table of §5.1 measured on x86-64 KVM, AArch64 and the DK1, and written in | auth | `cargo xtask check`, Miri, fuzz | 5 |
| P1.2 | `src/lib/proto/auth-proto`: the records of §3.3, their framing, `Secret`; host tests; a fuzzer on the decoder | auth | `cargo xtask check`, Miri, fuzz | 3 |
| P1.3 | `authd`: the socket, `SO_PEERCRED` rules (§3.4), policy files over `src/lib/init/svc`'s parser (§4.2), the `password` method with Argon2id and `$6$`/`$5$` import-and-rehash, the store (§5.2), seeds (§5.3), throttle (§3.5), audit (§3.6), zeroing (§3.8), started by `auth.socket`. Host tests run a real `authd` over a temporary root, as hyprlock's `Store::at` does. | auth | `cargo xtask check` | 8 |
| P1.4 | `passwd` and `authctl` (status, reset, unlock-seat, which is inert until phase 2). They replace busybox's `passwd`, `chpasswd`, `cryptpw` and `mkpasswd` links (§4.3). | auth | `cargo xtask check` | 3 |
| P1.5 | hyprlock's `Service` backend and the conversational trait (§4.4). `Shadow` and `Hashed` go. hyprlock refuses to lock an account with no credential (§5.4). | hyprlock | the hyprlock stream's gate | 2 |
| P1.6 | Images and the gate. `--auth-seed` and `--auth-seed-file`. `auth.service` and `auth.socket` in images with init, `exec-once = /sbin/authd` in the desktop's. **`cargo xtask test-auth --arch all`**: a seeded account, a wrong password refused after the delay, the fourth failure throttled, the right one accepted, `passwd` changing it, and the change surviving a reboot on the btrfs root (x86-64, which attaches one). Also: the audit lines are there, no line contains the password, an unknown account fails like a wrong password, and a uid-1000 peer naming another account is refused. **`test-hyprlock`** moves onto the real `authd`. | auth, with hyprlock | `test-auth`, `test-hyprlock` | 5 |
| P1.7 | Documents: the Security Target amendment (§8.1), roadmap and backlog rows, `docs/sysml/` | auth | `cargo xtask check` | 1 |

**What phase 1 gives the customer.** `SUPER+L` locks the desktop, and the
screen opens only for the account's password, checked by Argon2id, throttled
and audited. The password is set at build time or with `passwd`, and survives
rebuilds. What it does not give yet: while hyprix is init, the locked account
is root (hyprlock asks for its own uid's password, `getuid() == 0`), and a
compromised client is root. Decision 3 is whether that is acceptable for
phase 1. hyprlock does not change when phase 2 moves the session to
`ferrix`: its `getuid()` does.

### Phase 2: the desktop as a user (31 points, plus L10's 6)

| | Slice | Owner | Needs | Gate | Points |
|---|---|---|---|---|---|
| L10 | hyprix under init (`docs/INIT.md` §13, already planned) | init | | `test-compositor` under init | (6) |
| P2.1 | K-B: procfs honours `PR_SET_DUMPABLE` (set-id `execve` and id changes clear it since 2026-09-30) | kernel | | kernel gate | 2 |
| P2.2 | K-C: zero socket, pipe and tty buffers when freed | kernel | | kernel gate | 1 |
| P2.3 | `login`, and getty execs it. First password on a local console. `test-init` gains a stage: log in as `ferrix`, a wrong password refused, `id` says 1000, the session's scope is `user-1000.slice/session-1.scope`. **Built 2026-10-03** (§6.2; `getty --login` is per image) | auth, init | P1 | `test-init --arch all` | 5 |
| P2.4 | `sessiond`: seat0, device descriptors by `SCM_RIGHTS`, starts hyprix as the account in its scope, ends the session with its compositor. **Built 2026-10-03 for `--everything`** (§6.1), all but the scope: the session stays in `hyprix.service`'s cgroup; `test-compositor --boot everything-desktop` runs it as uid 1000 | session (new) | L10, P0 | `test-compositor` as uid 1000 | 10 |
| P2.5 | hyprix: devices from `sessiond`, the seat channel and grants (§3.7), a new locker taking over a dead lock, `misc:lock_grace`. **Built 2026-10-03**: the devices, then the grants, the takeover and `lock_grace` (§3.7; the consultant's OK IF, ledger line 299); `test-compositor --boot hyprlock-session` | compositor | P2.4, P1.3 | `test-compositor`, `test-hyprlock` | 6 |
| P2.6 | `su`, set-uid root, the wheel rule. **Built 2026-10-03** (§6.2): the gate is `test-init`'s `su` stage, since `test-vfs` boots no `authd` | auth | P1 | `test-init --arch all` | 3 |
| P2.7 | Adversary controls in the gates. A client that calls `unlock_and_destroy` with no grant leaves the screen locked. So does a client at a dead locker's place, and one holding a lock it was refused, each sending `unlock_and_destroy`; putting back the old place-only check makes that boot fail (the consultant's condition, ledger line 296; host tests in `hyprix/src/state/tests.rs` already). A client that kills hyprix lands at `login`, not on a desktop. A uid-1000 program cannot read `/var/lib/ferrix/auth`. Each has a sabotage that must make it fail. **Built 2026-10-03** (§6.4; `--boot session-end` and `--boot hyprlock-session`) | auth, compositor | P2.4, P2.5 | `test-compositor`, `test-auth` | 4 |

### Phase 3: versatility (about 32 points sized, plus unsized items)

| | Slice | Owner | Points |
|---|---|---|---|
| P3.1 | PAM shim in ferrousli (§4.3) | ferrousli | 5 |
| P3.2 | TOTP method, and `authctl totp enrol` with a QR code (`src/lib/kernel/qr` exists) | auth | 3 |
| P3.3 | `sshdt` keyboard-interactive and password through `authd`, the `sshd` service | auth, net | 4 |
| P3.4 | Privilege prompts: `ferrix.auth.ask`, an agent in the session (§4.5), and the first user, a non-root `svc stop` | auth, init | 6 |
| P3.5 | Accounts: `useradd`/`userdel`, with `/etc/passwd` generated from `/lib/ferrix/sysusers` plus the store's accounts. The archive stops carrying `/etc/passwd` (§5.2 says why it must). | auth | 4 |
| P3.6 | A graphical greeter on hyprlock's widgets | desktop clients | 8 |
| P3.7 | K-E: `SO_PEERCRED` taken at `connect` and `listen`, as Linux does. **Moved into phase 1 and done** (§8.3) | auth | 3 |
| P3.8 | K-D: `mlock` accepted as a no-op within `RLIMIT_MEMLOCK` (`docs/BACKLOG.md`) | kernel | 1 |
| — | FIDO2 over USB HID (needs a hidraw path from `src/user/system/native/drivers/usb/usbhid`), fingerprint (needs a reader), a trusted path, several seats | | unsized |

### Order

```
P0 ──────────────────────────────┐
P1.1 ─┐                          │
P1.2 ─┼─> P1.3 ─> P1.4 ─> P1.6 ──┼─> P2.3 ─> P2.6
      │       └──> P1.5 ─┘       │
L10 ──┴──────────────────────────┴─> P2.4 ─> P2.5 ─> P2.7 ─> phase 3
P2.1, P2.2: any time before P2.4
```

`docs/CONVENTIONS.md`'s first splitting rule is to start the critical path
first. Here that is P1.3, which can be written against P1.1's and P1.2's
interfaces while they are being built, and L10, which the init stream owns
already.

---

## 8. What changes elsewhere, once approved

### 8.1 The Security Target

The item's boundary does not move. `docs/certification/SECURITY-TARGET.md`
gains, in the words it already uses:

* **§4.2**, an objective for the environment: **OE.AUTH**, "people are
  identified and authenticated by the ring-3 authentication service of
  `docs/AUTH.md`, which alone holds credentials. It relies on the TOE for
  O.ISOLATE, O.CAPABILITY and O.SCRUB, and on the Linux personality's
  credentials for the uid a process runs as." A matching assumption
  **A.AUTH**: the authentication service and the programs that act on its
  verdict (`login`, `su`, `sessiond`, hyprix) are competently built,
  which is `A.ADMIN`'s shape. It also relies on the Linux personality's uid
  model and on its `SO_PEERCRED`, both in the uncertified load ring. No
  organisational security policy is needed for this (ferrix-55's review,
  2026-09-26; the TOE claims no FIA or FAU, F-21b).

  **User namespaces (`docs/NAMESPACES.md`, designed 2026-09-28).** Once they
  exist, "the uid a process runs as" is a *kernel* uid, which a user
  namespace never changes, and OE.AUTH relies on that design's rule U1:
  `privileged()`, the check behind every root-only call of the personality,
  is an effective kernel uid of 0 *in the first user namespace*. A process
  that is root inside a namespace it made is not root to `authd`, to a
  file's permissions, or to any root-only call, and `SO_PEERCRED` answers
  kernel ids translated for the reader, never a namespace's inside id taken
  for a real one. U2 to U9 and M1 to M8 there are the rest of the argument.
* **§9.1**, a sentence: FIA and FAU are still absent from the TOE. Their
  environment counterparts are `authd` and its audit log, and on ARMv7-A and
  the DK1 a ring-3 driver can read that service's memory
  (`ARCHITECTURE.md` §7).
* **§2.3** stays as it is. An OS Protection Profile still cannot be
  claimed, because its FIA would be the environment's, not the TOE's.

### 8.2 The other documents

* `docs/INIT.md` §6: `ferrix.auth.seat` and `ferrix.auth.ask` as
  directory names; §4.4: `auth.service` and `auth.socket` among the shipped
  units; L10's gate names the session ending with its compositor (§6.4).
* `docs/ROADMAP.md`: authentication as a section of stage 15 ("a real
  userland", whose exit is a shell a person can use), with phase 3 as its
  own row (decision 9).
* `docs/BACKLOG.md`: a row per phase with its owner, and P0 as a row of its
  own today, since it is a hole whatever else is decided.
* `docs/sysml/`: the service, its store and its channels, in the model's
  maturity terms.

### 8.3 Known weaknesses of the environment

Listed here and in `docs/certification/VULNERABILITY-ANALYSIS.md`'s
"What this analysis does not cover", until each is fixed:

* **E-01, `SO_PEERCRED` named who made a socket, not who connected it.
  Closed by K-E.** A root-made socket used by a process that had dropped to
  another uid read as root. The kernel now takes the ids as Linux's
  `unix_listen` and `unix_stream_connect` do: each `listen` records the
  caller's effective ids and pid (`Socket::listen`), and `connect_stream`
  gives the accepted end the connecting process's ids at the call and the
  connecting end the listener's. A connection to a socket with no
  listen-time ids is refused (`ECONNREFUSED`), so the creation-time ids can
  never come back silently. A socket pair keeps its creator's, as on Linux. The boot check `check_peer_credentials_are_the_callers`
  (`src/kernel/src/syscall/check.rs`) makes both sockets as root, listens as uid
  4242, connects as uid 1000, then, as uid 2000, sends the accepted
  descriptor over a pair with `SCM_RIGHTS` and closes the original. It
  requires the descriptor that arrived to name 1000, and the connecting end
  4242. Its negative control, the
  creation-time ids put back, names root and fails that line.
  ferrix-55 agreed the design (advisory: `fs/socket.rs` is in the load
  ring).

---

## 9. Decisions for the customer

All eleven were taken as recommended on 2026-09-26. The text below is as
it was put to the customer.

1. **A dedicated service (`authd`) rather than a shadow file or PAM
   modules.** *Recommended: yes* (§3.1). It is the only one of the three
   that keeps hashes out of every client, lets the lock screen need no
   privilege, and can grow a second factor.
2. **Argon2id, written in `src/lib/crypto/argon2`, or RustCrypto's `argon2`
   crate.** *Recommended: Argon2id, written here* (§5.1). It is about 5
   points, the vectors are public, and it keeps the most sensitive
   arithmetic in code the tree has read. RustCrypto's crate can check it in
   a host test.
3. **Phase 1 locks the desktop with root's password**, because the desktop
   is root until phase 2. *Recommended: accept it for phase 1.* hyprlock
   asks for its own uid's password and does not change when phase 2 makes
   that uid `ferrix`'s. The alternative is to wait for phase 2 (about 37
   points more) before the lock screen can be used.
4. **What a lock screen does when no password is set.** *Recommended:
   refuse to lock, and say why* (§5.4). The alternatives are to lock anyway,
   which locks the person out, or to let an empty password through, which
   the customer ruled out.
5. **How someone becomes root.** *Recommended: root stays locked, and a
   member of `wheel` becomes root with their own password* (sudo's rule,
   §4.2 `su`). Classic `su` asks for root's password, which means root must
   have one.
6. **Throttle or lockout.** *Recommended: a growing delay capped at 5
   minutes, no permanent lockout, and `LockoutAfter=` for a service that
   wants one* (§3.5).
7. **Programs that read `/etc/shadow`.** *Recommended: refuse them safely,
   and give PAM programs a shim in phase 3* (§4.3). No `getspnam` shim.
8. **Starting the desktop in phase 2**: log in at the console and start the
   desktop from there, or have `sessiond` start it for one named account at
   boot. *Recommended: log in first, with an automatic login configurable
   per image* (for the gate images and for the customer's own machine if
   wanted). An automatically logged-in desktop still asks for the
   password at the lock, so its account must have one (decision 4).
9. **Where it goes on the roadmap.** *Recommended: stage 15, "a real
   userland"*, with phases 1 and 2 as its rows and phase 3 after the stage.
   P0 goes into the backlog now.
10. **Names and ids**: `authd`, `/run/ferrix/auth`, `/var/lib/ferrix/auth`,
    `/lib/ferrix/auth/services`, the `auth` system user with a fixed uid
    below 100 (proposed 90), `sessiond`, `authctl`. *Recommended: as
    written.*
11. **The unlock override.** *Recommended:* hyprlock's `SIGUSR1` becomes
    root's audited `authctl unlock-seat`, and `--grace` is capped by
    hyprix's `misc:lock_grace`, default 0 (§3.7).

---

## 10. Where it stands (2026-09-27)

| Slice | State |
|---|---|
| P0 | on `main` as `f84a8d3c` (ferrix-15): a native process runs as the process that made it, checked in every `test-init` boot |
| P0a, P0b, P0c | done 2026-09-26 (ferrix-15): P0a refused `User=` on a native unit until P0b made it run as that user; P0c is `54cba422`, the `SET_LIMIT` right |
| P1.1 `src/lib/crypto/argon2` | on `main` with this section: RFC 9106's vector and five RustCrypto ones, Miri in CI, the `argon2_phc` fuzz target, the timings of §5.1 |
| P1.2 `src/lib/proto/auth-proto` | on `main` with this section: the records of §3.3, `Secret`, the `auth_proto` fuzz target |
| P1.3 `authd`, P1.4 `passwd` and `authctl` | on `main` with this row (`src/user/system/linux/auth/`): 28 host tests, one of them over a real socket, and `test-auth` on all three architectures |
| P1.6 `cargo xtask test-auth` | on `main` with this row (`tools/common/xtask/src/auth.rs`): passes on x86-64, AArch64 and ARMv7-A, and each of `--sabotage accept-any`, `tell-unknown`, `let-anyone-name` and `no-throttle` fails on its own line |
| P2.6 `su` | 2026-10-03: `/bin/su` set-uid root; `authd`'s `su` service, `Callers=any` with `TargetGroup=wheel` enforced at BEGIN; `ferrix` in `wheel` (gid 10) wherever `authd` is carried (§6.2; the consultant's OK IF, ledger line 309). `test-init --arch all`'s `su` stage |
| P2.3 `login` | 2026-10-03: `/bin/login`, `getty --login`, `FirstPassword=local` decided by `authd` from the caller's `tty_nr` (1281, the console's) for a person's account only (§5.4, §6.2; the consultant's OK IF, ledger line 303). `test-init --arch all`'s `login` stage: no first password with no controlling terminal, the first password at the console, every id 1000 in `user-1000.slice/session-1.scope`, a wrong password refused, `session-2.scope` |
| P2.5 the seat's grant | 2026-10-03: `authd` offers `ferrix.auth.seat`, `sessiond` arms each lock and relays its grant on descriptor 4, hyprix lets a session's lock go only on it, a dead lock may be taken over, `misc:lock_grace` capped (§3.7, ledger line 299). `--boot hyprlock-session` and three negative controls. Not closed: killing hyprix restarts a fresh desktop until `login` (P2.3, P2.7) |
| hyprix's lock holder | 2026-10-03: only the holder unlocks, on its own lock object, and nobody once it has died (§1's lock row; the consultant's OK IF, ledger line 296); seven host tests, and negative controls in `~/.local/share/ferrix/logs/lock-orphan/` |
| P1.5 hyprlock's backend | on `main` with this row (2026-10-03): `/bin/hyprlock` talks to `authd` through `src/user/system/linux/auth/client` (`hyprlock/src/auth.rs`), and every desktop image carries `authd` beside hyprlock (`tools/common/xtask/src/compositor/desktop.rs`), with a seed only where `--auth-seed` asks (the certification consultant's OK IF, ledger line 295). `test-compositor --boot hyprlock` locks, refuses a wrong password and takes the right one through `authd`; `--boot hyprlock-unset` refuses to lock an account with no password (decision 4) |
| K-E `SO_PEERCRED` at `connect` and `listen` | on `main` with its boot check (E-01, closed) |
| P1.7 the Security Target's OE.AUTH | on `main` with this row: OE.AUTH, A.AUTH and §9.1's note |

**One rule found while building it**, and now written into §3.5: after any
failure, no attempt on that account is looked at, from any connection,
until its FAILED has gone out. A guesser with a hundred connections gets
one guess per delay, like one with a single connection.

**Left for the desktop.** Since init's L10 (`e00def08`) the desktop's image
boots `/sbin/init` too, and hyprix is `hyprix.service`, so `authd` reaches it
the same way as any image: `auth::carried` (`tools/common/xtask/src/auth.rs`) with the
image's init files, and the `auth` account (`auth::PASSWD_LINE`) in its
`/etc/passwd`. It goes in with hyprlock's P1.5, whose lock screen is the
first desktop client to use it, so that `test-compositor`'s boots grow the
auth build only when something on them needs it. authd reads no `HOME` and
no other environment than init's `LISTEN_FDS` and `LISTEN_PID`, so
5fa34300's `HOME=/` for hyprix.service does not concern it.
