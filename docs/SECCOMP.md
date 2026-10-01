# seccomp-bpf: the second half of Chromium's sandbox

Steam's browser helper, `steamwebhelper` (Chromium, as CEF), runs on Ferrix
with `-no-cef-sandbox` (`docs/STEAM.md` §3). Chromium's Linux sandbox has
two layers: a namespace layer (user and pid namespaces, with network ones
if it can get them, and a `chroot` into a directory that no longer exists),
and a seccomp-bpf filter in every sandboxed child. `docs/NAMESPACES.md`
builds user and mount namespaces for Steam's container, and says seccomp
and pid namespaces are out of it. This document designs the seccomp half.
It is stage 13's third part (`docs/roadmap/stage-13-namespaces-cgroups-v2-seccomp.md`),
whose exit asks for "a seccomp filter that blocks a syscall", and it is what
init's L13 `SystemCallFilter=` waits on (`docs/INIT.md`).

Status: **design, for review** by the certification consultant (os-9f) and
the namespaces stream (os-98). Written 2026-09-30 on `main` at 813d1ea8. No
code yet.

**In one paragraph.** Six landings, S1 to S6, **20 points**, give Ferrix
Linux's seccomp-bpf:
* a classic-BPF verifier and interpreter in `src/lib/`;
* a filter check registered into the core's system call entry, run
  before anything answers a call;
* filter chains per thread, inherited everywhere;
* `ERRNO`, `KILL`, `LOG`, `TRAP` and `TSYNC`;
* a gate that runs Linux's own seccomp selftest.

That meets stage 13's exit clause at S3 (13 points), and init's
`SystemCallFilter=`. It does **not** by itself retire `-no-cef-sandbox`.
Measured on the host (§1.2): Chrome with user namespaces and seccomp, but
with `CLONE_NEWPID` refused, does not start at all. So S7 and S8 (5 more
points) also need N4 and pid namespaces, which no design owns yet (§11,
R1).

---

## 1. What the sandboxes ask for, measured

### 1.1 Chromium, traced on the host

On 2026-09-30, `strace -f -v` of Google Chrome 151.0.7922.137,
`--headless=new --disable-gpu --dump-dom about:blank`, on nazuna (Linux
7.0, `CONFIG_SECCOMP_FILTER=y`). The traces are
`~/.local/share/ferrix/seccomp-design-ref/chrome.strace` and
`chrome-v.strace`. Nine processes sandboxed themselves: two of type
`zygote` and seven forked from them. Each did the same thing, in the
same order:

```
prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, NULL)        = -1 EFAULT   probe: seccomp-bpf exists
seccomp(SECCOMP_SET_MODE_FILTER, TSYNC, NULL)            = -1 EFAULT   probe: TSYNC exists (reported only)
rt_sigaction(SIGSYS, {handler, SA_SIGINFO|SA_NODEFER})  = 0
prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)                  = 0
seccomp(SECCOMP_SET_MODE_FILTER, SPEC_ALLOW, NULL)       = -1 EFAULT   probe: SPEC_ALLOW exists
seccomp(SECCOMP_SET_MODE_FILTER, SPEC_ALLOW, {len=657})  = 0          657 to 662 instructions
```

Every process was single-threaded when it installed its filter. Each made
its threads afterwards, with `clone3`. The filter answered that with
`ENOSYS`, and glibc then fell back to `clone`. So `SECCOMP_RET_ERRNO` must
be exact: a wrong errno there, and no renderer gets a thread.

What the 657-instruction program is (decoded by `strace -v`):

```
ld  [4]                       arch
jeq #0xc000003e, +1           AUDIT_ARCH_X86_64
ret #TRAP|0xa                 wrong architecture: SIGSYS, trap 10
ld  [0]                       nr
jset #0x40000000, +0, +1      the x32 bit
ret #TRAP|9
jge ... (binary search over the number, then argument checks)
```

| Used | Count over the nine filters |
|---|---|
| `ld [k]` (`BPF_LD\|BPF_W\|BPF_ABS`) | 1,813: offsets 0 (nr), 4 (arch), 0x10 to 0x2c (the low *and high* halves of `args[0..3]`); never `instruction_pointer` |
| `jeq`, `jge`, `jset` against a constant | 1,696, 1,338, 661 |
| `ja` | 225 |
| `and #k` | 63 |
| `ret #k` | 126 |
| actions returned | `ALLOW`; `ERRNO` with `EPERM`, `EINVAL`, `ENOSYS`; `TRAP` with data 1 to 10 |
| never used | `KILL_*`, `TRACE`, `USER_NOTIF`, `LOG`, scratch memory, `X`, `ld [k]` of anything but a word |

Chromium's source agrees (paths under
`https://chromium.googlesource.com/chromium/src/+/refs/heads/main/`):

* **Detection** (`sandbox/linux/seccomp-bpf/sandbox_bpf.cc`,
  `KernelSupportsSeccompBPF`, `KernelSupportsSeccompFlags`). A `NULL`
  program must fail with `EFAULT`. For a flag it does not know, the kernel
  must answer `EINVAL` or `ENOSYS`, since anything else trips a `DCHECK`.
  So the kernel checks the flags before it reads the program, as Linux's
  `seccomp_set_mode_filter` does.
* **Installing** (`SandboxBPF::InstallFilter`). With threads it uses
  `TSYNC` and has no fallback. Without threads it uses `SPEC_ALLOW` if the
  probe passed, else `prctl(PR_SET_SECCOMP)` alone. On the `SPEC_ALLOW`
  path it first asks `PR_GET_SPECULATION_CTRL` for indirect branches,
  and gives up quietly on an error.
* **Who uses `TSYNC`** (`sandbox/policy/linux/sandbox_linux.cc`,
  `StartSeccompBPF`). Only a process started with
  `allow_threads_during_sandbox_init` and already threaded. On the desktop
  that means a GPU process that has threads at that point, and it then
  runs *unsandboxed* with a warning. Renderers never do. chrome://sandbox
  shows `TSYNC` support as a row of its own, and the verdict does not
  count it.
* **The `SIGSYS` handler** (`sandbox/linux/seccomp-bpf/trap.cc`). It is
  installed with `SA_SIGINFO|SA_NODEFER` and unblocked. It refuses a
  `SIGSYS` unless every one of these holds:
  * `si_code == SYS_SECCOMP`;
  * `1 <= si_errno <=` its trap count;
  * `si_call_addr` equals the context's instruction pointer;
  * `si_syscall` equals the context's system call register;
  * `si_arch` equals its own architecture.

  It then reads the arguments from the context and writes a result into it
  (`sandbox/linux/bpf_dsl/seccomp_macros.h`). The registers:

  | ABI | number | result | arguments |
  |---|---|---|---|
  | x86-64 | `RAX` | `RAX` | `RDI RSI RDX R10 R8 R9` |
  | i386 | `EAX` | `EAX` | `EBX ECX EDX ESI EDI EBP` |
  | ARM | `r7` | `r0` | `r0`-`r5` |
  | AArch64 | `x8` | `x0` | `x0`-`x5` |

  So the frame a trap is delivered on must hold the registers *as the
  program made the call*: the number still in `RAX`/`EAX`, and the first
  argument still in `x0`/`r0`. That is Linux's `syscall_rollback`.
* **chrome://sandbox's verdict** (`chrome/browser/ui/webui/sandbox/sandbox_internals_ui.cc`)
  is `(SUID || UserNS) && PIDNS && NetNS && SeccompBPF`.

### 1.2 What the namespace layer adds, and why this is not only seccomp

The same trace shows the namespace layer:

* `clone(CLONE_NEWUSER|CLONE_NEWPID|CLONE_NEWNET)` for the zygote.
* `unshare(CLONE_NEWUSER)` twice. One is the capability probe, and the
  other is the zygote moving into a nested user namespace.
* A `CLONE_VM|CLONE_FS|CLONE_VFORK` child that does
  `chroot("/proc/self/fdinfo/")` and exits. That leaves the zygote rooted
  in a directory that no longer exists.
* `clone(CLONE_NEWPID)` for each renderer.

Chromium's source makes two of these hard requirements:

* **A pid namespace is mandatory for the namespace sandbox.**
  `SandboxLinux::EngageNamespaceSandboxInternal`
  (`sandbox/policy/linux/sandbox_linux.cc`) does
  `CHECK(NamespaceSandbox::InNewPidNamespace())` and
  `CHECK_EQ(1, getpid())` for the zygote.
  `NamespaceSandbox::LaunchProcess` drops `CLONE_NEWPID` and
  `CLONE_NEWNET` silently when `/proc/self/ns/pid` or `/proc/self/ns/net`
  is missing (`sandbox/linux/services/namespace_sandbox.cc`,
  `namespace_utils.cc`). But the zygote then fails that `CHECK`.
  Without a namespace layer the only other layer is the set-uid
  `chrome-sandbox`. Without that, `zygote_host_impl_linux.cc` says
  `No usable sandbox!`, which is exactly what the Steam spike saw
  (`~/.local/share/ferrix/steam-ref/SPIKE.md`).
* **The browser checks the zygote's pid is above 1**, as it arrives in
  `SCM_CREDENTIALS`. So credentials passing between pid namespaces must
  translate the pid.

A network namespace is optional for running: Chromium drops the flag.
It is required for chrome://sandbox to say "adequately sandboxed".

**Measured: what Chrome does on a kernel that refuses them**
(2026-09-30, nazuna, Chrome 151). This is what Ferrix does today and
after NAMESPACES' N4:
* `clone` and `unshare` with `CLONE_NEWPID` or `CLONE_NEWNET` are
  refused `EINVAL`, as `docs/NAMESPACES.md` §1.5 keeps them.
* `/proc/self/ns/` holds only `mnt` and `user` (NAMESPACES §2.4).

**How the kernel was made to refuse them.**
* A launcher, `nsdeny`, sets no-new-privs and installs a filter before
  it `exec`s Chrome. The filter:
  * answers `clone` and `unshare` `EINVAL` when a chosen flag is set;
  * answers `clone3` `ENOSYS`, so glibc falls back to `clone`, where the
    flags are visible;
  * allows everything else, `CLONE_NEWUSER` included.
* An `LD_PRELOAD` shim, `nshide.so`, makes `access` and `stat` of chosen
  `/proc/self/ns/<name>` answer `ENOENT`. Chromium's
  `KernelSupportsUnprivilegedNamespace` asks exactly that
  (`base::PathExists`).

**The runs.** Each is `--headless=new --disable-gpu` under
`strace -f -e trace=clone,clone3,unshare,seccomp,chroot`. The files are in
`~/.local/share/ferrix/seccomp-design-ref/nsx/` (`nsdeny.c`, `nshide.c`,
`seccomp-design-nsx.sh`, one `.strace` and `.err` per run).

| Run | Refused `EINVAL` | Hidden in `/proc/self/ns` | What Chrome did | Sandboxed? |
|---|---|---|---|---|
| v0, control | nothing | nothing | zygote `clone(NEWUSER\|NEWPID\|NEWNET)`, 8 renderers `clone(NEWPID)`, `chroot`, 8 filters of 600+ instructions | yes |
| v1 | `NEWPID`, `NEWNET` | nothing | the zygote's `clone(NEWUSER\|NEWPID\|NEWNET)` = `EINVAL`; the browser dies at once of `SIGTRAP` (Chromium's immediate-crash `CHECK`), exit 133 | no: dead |
| v2 (Ferrix today, and after N4) | `NEWPID`, `NEWNET` | `pid`, `net` | flags dropped: the zygote started with `clone(NEWUSER)` alone; the browser stops at `FATAL:zygote_host_impl_linux.cc:221 Check failed`, waiting for the zygote's boot message, exit 134. No filter was installed and there was no `chroot`. | no: dead |
| v4 | `NEWPID` | `pid` | the same as v2, with `clone(NEWUSER\|NEWNET)` | no: dead |
| v3 | `NEWNET` | `net` | flags dropped: zygote `clone(NEWUSER\|NEWPID)`, renderers `clone(NEWPID)`, `chroot`, 8 filters, the page loaded, exit 0 | **yes**, without a network namespace |

**Neither run fell back to the set-uid sandbox.** That fallback is taken
only when the user namespace probe fails, and here it passed. It never
said "No usable sandbox" either: it chose the namespace layer and died in
it. On Ferrix after N4 the probe passes too, so the set-uid path is not a
way out. Before N4 the probe fails, and the set-uid path would need a
root-owned set-uid `chrome-sandbox` beside the browser. The Steam spike
found none, and got "No usable sandbox!".

**So, answered by measurement: seccomp and N4 do not retire
`-no-cef-sandbox` on their own. Pid namespaces are needed too.** With
user namespaces and seccomp but no pid namespace, Chrome with its sandbox
on does not start at all (v2). It is not merely less sandboxed.

A network namespace is not needed to run sandboxed (v3). It is needed
only for chrome://sandbox's "adequately sandboxed" verdict. Headless
Chrome turns chrome://sandbox into a new tab, so the verdict row was not
read. The verdict's formula is from the source (§1.1).

**Retiring `-no-cef-sandbox` therefore needs:**
* this document's seccomp;
* `docs/NAMESPACES.md`'s user namespaces: N4, with nesting at least two
  deep, since the zygote nests one inside the probe's, and Steam's
  pressure-vessel adds one below both;
* **pid namespaces, which no design owns yet**: unsized, the namespaces
  stream's (§9's NP-pid, §11 R1);
* network namespaces **only if** the customer's end point includes
  chrome://sandbox's verdict (§11 R2).

Seccomp alone turns chrome://sandbox's "Seccomp-BPF sandbox" row green and
nothing else.

### 1.3 The other users

| User | What it needs | Source |
|---|---|---|
| bubblewrap `--seccomp FD`, `--add-seccomp-fd FD` | `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)` per program. No `seccomp(2)`, no flags. `PR_SET_NO_NEW_PRIVS` first. Filters are the last thing before `execve`, and also in its pid-1 init with `--unshare-pid`. | `bubblewrap.c` at 0f8073dd |
| pressure-vessel | None for the helper today. Its copy of Flatpak's `setup_seccomp` sits under an `ENABLE_SECCOMP` its build never defines, and the host trace showed `Seccomp 0` inside the container (`docs/NAMESPACES.md` §1.3). | `steam-runtime-tools/pressure-vessel/flatpak-run.c` |
| Flatpak (Steam as a Flatpak) | libseccomp, default `ALLOW`, lists answered `ERRNO` (`EAFNOSUPPORT` for socket families), **one filter for x86-64 and i386 together**, branching on `arch`. Passed to bwrap with `--seccomp`. | as above |
| init's L13, `SystemCallFilter=`, `SystemCallArchitectures=` | `ERRNO`, `KILL_PROCESS`, `LOG`; filters covering two architectures; `NoNewPrivileges=` | `docs/INIT.md` §4 |
| The spike's `noipc.py` | a raw filter for x86-64 and i386 returning `ENOSYS` for System V IPC | `steam-ref/SPIKE.md` |
| libseccomp itself | probes the kernel when it starts. It checks `seccomp(2)`, then `GET_ACTION_AVAIL` for the newer actions, then the filter flags, and sets an "API level" from what it finds. Unverified from source (§11, R6). | `src/system.c` |
| Firefox's content sandbox | not researched (§11, R6) | |

### 1.4 Linux's interface

From the headers on nazuna (`/usr/include/linux/seccomp.h`, `filter.h`,
`bpf_common.h`, `audit.h`, `prctl.h`, `ptrace.h`,
`asm-generic/siginfo.h`; the call numbers from QEMU's `linux-headers/`),
and Linux's `kernel/seccomp.c` and `net/core/filter.c` at 551c722f
(`https://github.com/torvalds/linux/blob/master/`).

| Call | x86-64 | i386 | AArch64 | ARMv7-A |
|---|---:|---:|---:|---:|
| `seccomp` | 317 | 354 | 277 | 383 |
| `prctl` | 157 | 172 | 167 | 172 |
| `restart_syscall` | 219 | 0 | 128 | 0 |
| `bpf` (not built, `ENOSYS`) | 321 | 357 | 280 | 386 |

* **`seccomp(op, flags, args)` operations:** `SET_MODE_STRICT` 0,
  `SET_MODE_FILTER` 1, `GET_ACTION_AVAIL` 2, `GET_NOTIF_SIZES` 3.
* **Filter flags:** `TSYNC` 1<<0, `LOG` 1<<1, `SPEC_ALLOW` 1<<2,
  `NEW_LISTENER` 1<<3, `TSYNC_ESRCH` 1<<4, `WAIT_KILLABLE_RECV` 1<<5.
  Any other bit is `EINVAL`.
* **`prctl`:** `PR_GET_SECCOMP` 21, `PR_SET_SECCOMP` 22 (mode 1 or 2; mode
  2 takes the program in `arg3`), `PR_SET_NO_NEW_PRIVS` 38,
  `PR_GET_NO_NEW_PRIVS` 39, `PR_GET_SPECULATION_CTRL` 52,
  `PR_SET_SPECULATION_CTRL` 53 (`PR_SPEC_INDIRECT_BRANCH` 1,
  `PR_SPEC_PRCTL` 1<<0, `PR_SPEC_FORCE_DISABLE` 1<<3).
* **Actions** (upper 16 bits; the lower 16 are `SECCOMP_RET_DATA`):

  | Action | Value |
  |---|---|
  | `KILL_PROCESS` | `0x80000000` |
  | `KILL_THREAD` | `0x00000000` |
  | `TRAP` | `0x00030000` |
  | `ERRNO` | `0x00050000` |
  | `USER_NOTIF` | `0x7fc00000` |
  | `TRACE` | `0x7ff00000` |
  | `LOG` | `0x7ffc0000` |
  | `ALLOW` | `0x7fff0000` |

  Masks: `ACTION_FULL 0xffff0000`, `ACTION 0x7fff0000`, `DATA 0xffff`.
* **`struct seccomp_data`, 64 bytes:** `int nr` at 0, `__u32 arch` at 4,
  `__u64 instruction_pointer` at 8, `__u64 args[6]` at 16.
* **`struct sock_filter`:** `{u16 code; u8 jt; u8 jf; u32 k}`.
  `struct sock_fprog` is `{unsigned short len; struct sock_filter *filter}`.
  `BPF_MAXINSNS 4096`, `BPF_MEMWORDS 16`.
* **Architecture tokens** (`EM_*` | `__AUDIT_ARCH_64BIT 0x80000000` |
  `__AUDIT_ARCH_LE 0x40000000`):

  | Token | Value |
  |---|---|
  | `AUDIT_ARCH_X86_64` | `0xC000003E` (seen in Chrome's filter) |
  | `AUDIT_ARCH_I386` | `0x40000003` |
  | `AUDIT_ARCH_AARCH64` | `0xC00000B7` |
  | `AUDIT_ARCH_ARM` | `0x40000028` |

  `__X32_SYSCALL_BIT` is `0x40000000`.
* **`SIGSYS`** is 31. `SYS_SECCOMP` is 1. `siginfo._sigsys` is
  `{void *_call_addr; int _syscall; unsigned int _arch}`.
* **ptrace, not built** (§3.10): `PTRACE_EVENT_SECCOMP` 7,
  `PTRACE_O_TRACESECCOMP` 1<<7, `PTRACE_O_SUSPEND_SECCOMP` 1<<21,
  `PTRACE_SECCOMP_GET_FILTER` 0x420c.
* **`/proc/<pid>/status`** on the host: `NoNewPrivs:`, `Seccomp:` (the
  mode), `Seccomp_filters:` (the count), and `Speculation_Store_Bypass:`.

### 1.5 The minimal set

| Needed | Not needed (answered as a kernel without it answers) |
|---|---|
| `seccomp(SET_MODE_FILTER)` with `TSYNC`, `TSYNC_ESRCH`, `LOG`, `SPEC_ALLOW`; `prctl(PR_SET_SECCOMP)` | `NEW_LISTENER`, `WAIT_KILLABLE_RECV`: `EINVAL` |
| `ALLOW`, `ERRNO`, `TRAP` with the rolled-back frame, `KILL_THREAD`, `KILL_PROCESS`, `LOG` | `USER_NOTIF` returned by a filter: `ENOSYS`, as Linux answers without a listener |
| `TRACE` returned by a filter: `ENOSYS`, as Linux answers without a tracer | ptrace itself, `PTRACE_O_TRACESECCOMP`, `PTRACE_SECCOMP_GET_FILTER` |
| `GET_ACTION_AVAIL`: 0 for the seven built actions, `EOPNOTSUPP` for `USER_NOTIF` | `bpf(2)`, extended BPF, `SO_ATTACH_FILTER`: `ENOSYS` / `ENOPROTOOPT` as today |
| `SET_MODE_STRICT`, `PR_GET_SECCOMP` | Linux's per-filter action cache (§3.11) |
| `NoNewPrivs`, `Seccomp`, `Seccomp_filters` in `/proc/<pid>/status` | |
| four ABIs: x86-64, i386 on x86-64, AArch64, ARMv7-A EABI with its private range from `0xf0000` | x32: Ferrix has none, and a number with bit 30 set is `ENOSYS` |
| `PR_GET_SPECULATION_CTRL`/`PR_SET_SPECULATION_CTRL` answering "force-disabled" (§3.9) | |

---

## 2. Ferrix today

Read on 813d1ea8. Paths are under `src/kernel/src/` unless given.

**The way in.**
* **One dispatcher for every entry.** Each architecture's entry fills a
  `trap::SyscallArgs {abi, number, args: [u64; 6]}` (`trap.rs:179`) and
  calls `trap::system_call` (`trap.rs:263`). That calls the entry
  `main.rs` registered, `syscall::dispatch_with::<Linux>`
  (`syscall/mod.rs:128`).
* **`Abi` says which table the call is in** (`trap.rs:198`): `Native` or
  `Compat`. It is decided by the entry, never by the process: on x86-64 a
  `SYSCALL` is x86-64's and `int $0x80` is i386's, whatever image the
  process runs. That is the rule the architecture token must follow
  (§3.2), and it is already there.
* **`dispatch_with` does three things, in order:**
  1. It sends the native range (`0x1000..=0x1FFF`, `is_native`) from the
     `Native` entry to the native ABI (`native_call`, `mod.rs:165`).
  2. It decodes a Linux number with its Spectre clamp. A number in no
     table is `ENOSYS` there (`mod.rs:146`).
  3. It hands the decoded call to the Linux personality, `linux::dispatch`
     (`linux.rs:66`).
* **`SyscallArgs` has no instruction pointer.** It lives only in each
  architecture's frame: x86-64 `SyscallFrame.rcx` and `TrapFrame.rip`,
  AArch64 `elr`, ARMv7-A `pc`.
* **Some calls are answered before `trap::system_call`, in the
  architecture's own entry, and never reach the dispatcher:**
  * x86-64 `arch_prctl` and `rt_sigreturn` (`arch/x86_64/syscall.rs:501`,
    `:509`);
  * i386 `sigreturn` and `rt_sigreturn` (`arch/x86_64/mod.rs:1227`);
  * AArch64 `rt_sigreturn` (`arch/aarch64/trap.rs:436`);
  * ARMv7-A `set_tls` and `sigreturn`/`rt_sigreturn`
    (`arch/armv7a/trap.rs:455`, `:466`).

  **A hook in the dispatcher would not see these** (§3.3).
* **Numbers are the full register.** x86-64 reads `rax` as a `usize`
  (`syscall.rs:488`); Linux reads its low 32 bits. On Ferrix a number with
  upper bits set is in no table and is `ENOSYS`.
* **Other entries on x86-64.** A `SYSCALL` from 32-bit code is `ENOSYS`
  (`syscall.rs:323`), and `SYSENTER` faults (`IA32_SYSENTER_CS` is 0).
  ARMv7-A is EABI only.
* **The tables** are in `src/lib/proto/linux-abi/src/nr.rs`:
  `from_x86_64` :2873, `from_i386` :3908, `from_aarch64` :3224,
  `from_arm` :3538. **None has `seccomp`, `bpf` or `ptrace`**, so all
  three are `ENOSYS` today.

**State.**
* **A `Thread`** (`syscall/thread.rs:42`) holds its tid, its process, its
  robust list, its `signals` behind a `SpinLock`, and its saved `resume`
  registers.
* **A `Process`** (`syscall/process.rs:79`) holds credentials
  (`SpinLock<Credentials>`, `:217`) and the thread list
  (`threads: SpinLock<Vec<Weak<Thread>>>`, `:172`). `Process::threads`
  (`:1282`) lists the live ones.
* **Lock order** (`process.rs:1350`, `thread.rs:23`): the thread list,
  then the process's signal state, then a thread's signals.
* **Fork and clone.** `family::clone_with` (`family.rs:417`) forks through
  `Process::forked_into` (`process.rs:417`), and makes a thread with
  `Thread::sibling` and `process::start_thread` (`family.rs:612`).
* **Exec.** `exec.rs` ends the other threads (`end_other_threads`,
  `process.rs:822`) before its point of no return, as Linux's `de_thread`
  does.
* **Native children.** A native `process_create` gets its creator's
  credentials and fs context through `launch::load_native`
  (`syscall/launch.rs:108`; `docs/NAMESPACES.md` §2.5).

**`prctl` and no-new-privs.**
* **`attributes::sys_prctl`** (`syscall/attributes.rs:287`) answers
  `PDEATHSIG`, `DUMPABLE`, `NAME`, `CAPBSET_READ`, `CHILD_SUBREAPER` and
  `NO_NEW_PRIVS`. Every other option is `EINVAL`, `PR_SET_SECCOMP` and
  `PR_GET_SECCOMP` included. So Chromium's first probe reads "no
  seccomp".
* **`no_new_privs` is per process.** It is kept in `Attributes`, in a
  table keyed by pid and start time (`attributes.rs:91`, `:121`). It is
  read in one place, `exec.rs:765`, where it turns set-id bits off.
* **No-new-privs is not inherited across `fork`.** The module says so
  (`attributes.rs:40`): "Nothing is inherited across `fork` ... A child
  starts from the defaults". Linux copies it into every child. Nothing
  depends on the difference yet. Seccomp does: a filter installed under
  no-new-privs is only safe if every descendant keeps no-new-privs too
  (rule SR4). **This is a bug to fix first** (landing S3).

**Signals.**
* **SIGSYS.** It is 31, synchronous, delivered first, with a
  core-dumping default (`syscall/signal.rs:100`, `:139`).
* **Forcing.** `signal::force` (`signal.rs:934`) is Linux's
  `force_sig_info_to_task`: it unblocks the signal and resets an ignored
  or blocked disposition to the default. `deliver::force`
  (`deliver.rs:475`) forces it on the current thread.
* **No `_sigsys` in the signal information.** `signal::Origin`
  (`signal.rs:147`) has no variant for it, and nothing fills
  `si_call_addr`, `si_syscall` or `si_arch`. An i386 frame's
  information is made by shifting the 64-bit union down 4 bytes
  (`linux-abi/sigframe32.rs:184`). That is right for the unions of
  integers it carries today, and wrong for `_sigsys`, whose first member
  is a pointer (§3.6).
* **Restart.** A restart is recorded per thread (`signal.rs:619`) and
  resolved on the way out by rewinding the program counter to the call
  instruction (`UserContext::rewind_syscall`, one per architecture).
* **Delivery** happens in `deliver::return_to_user` (`deliver.rs:153`),
  which the core calls through `trap::ReturnPath`.

**Absent.**
* ptrace: no number, no stops. `TracerPid:` is always 0
  (`src/lib/fs/procfs/src/status.rs:132`).
* Any BPF, classic or extended.
* A per-call audit hook. `audit.rs` records the item's own decisions.
* `NoNewPrivs`, `Seccomp` and `Seccomp_filters` in `/proc/<pid>/status`
  (`status.rs:116`).

**Planned already.** `docs/ARCHITECTURE.md` §6 and the SysML model (`07-isolation.sysml`
`Seccomp`, `02-structure.sysml` `seccompBpf`) place the interpreter in
`src/lib/kernel/seccomp`, `forbid(unsafe_code)`, as "a pure function over
bytes and therefore fuzzable and Miri-able", and the check "on entry,
before dispatch".

---

## 3. The design

### 3.1 The interpreter and its verifier: `src/lib/kernel/seccomp`

A `no_std` crate with no `unsafe` and no allocation in the run path:

```
pub struct Insn { code: u16, jt: u8, jf: u8, k: u32 }        sock_filter, as read from the program
pub struct Program(Box<[Insn]>)                              only `verify` makes one
pub fn verify(raw: &[Insn]) -> Result<Program, Invalid>
pub fn run(program: &Program, data: &SeccompData) -> u32     never fails, never loops
pub struct SeccompData { nr: i32, arch: u32, ip: u64, args: [u64; 6] }
```

**`verify`** is Linux's `bpf_check_classic` followed by its
`seccomp_check_filter`, rule for rule:

1. **Length** is from 1 to 4096 (`BPF_MAXINSNS`).
2. **Only these opcodes:**
   * `RET K` and `RET A`;
   * `ALU` `ADD`, `SUB`, `MUL`, `DIV`, `AND`, `OR`, `XOR`, `LSH` and
     `RSH`, against `K` or `X`, and `NEG`;
   * `LD IMM`, `LDX IMM`, `LD MEM`, `LDX MEM`, `ST` and `STX`;
   * `TAX` and `TXA`;
   * `LD|W|ABS`;
   * `LD|W|LEN` and `LDX|W|LEN`, which read as the constant 64, the size
     of `seccomp_data`;
   * `JA`, and `JEQ`, `JGT`, `JGE` and `JSET` against `K` or `X`.

   Everything else is refused: `MOD`, the half-word and byte loads, `IND`
   and `MSH` among them.
3. **`LD|W|ABS` must be word-aligned and inside `seccomp_data`**
   (`k < 64`, `k % 4 == 0`). There is no ancillary range: Linux's
   `SKF_AD_*` loads are socket-only.
4. **Constant operands are checked:** `DIV` by a constant zero is refused,
   and so is a constant shift of 32 or more. The scratch index
   `LD/LDX MEM`, `ST`, `STX` must be below 16.
5. **Jumps only go forward and stay inside the program:** `JA` needs
   `k < len - pc - 1`, and a conditional jump needs `pc + jt + 1 < len`
   and `pc + jf + 1 < len`. Offsets are unsigned, so there are no
   backward jumps.
6. **The last instruction is a `RET`.**
7. **No read of scratch memory before a store on every path** (Linux's
   `check_load_and_stores`). This is a forward pass with a 16-bit mask of
   valid slots per instruction, joined by intersection where paths meet.
   CVE-2010-4158 was a filter reading uninitialised scratch words of the
   kernel stack. Here the scratch is zeroed as well, so that rule costs
   nothing if the pass is ever wrong. The check is kept anyway, so that
   Linux's refusals are ours.

**`run`** is the classic machine. It has `A`, `X` and sixteen scratch
words, all zero at the start. The program counter only moves forward, so a
verified program of `n` instructions stops within `n` steps. The loop is
bounded by the program length as well, as a belt.

**Arithmetic** is 32-bit and wraps. A runtime division by an `X` of zero
makes the program return 0, which is `KILL_THREAD`. That is what Linux's
conversion to its internal BPF emits for `DIV X`.

**A shift by `X`** is masked to five bits. Linux's interpreter does this
(`kernel/bpf/core.c`'s `SHT` macro), and so does x86 hardware. S1
re-reads the macro at 551c722f and pins the behaviour with a host test.

**Evidence, in the crate:**
* **Host tests.** One per opcode and per refusal of `verify`, each citing
  the line of Linux's check it mirrors.
* **Chromium's own filters as fixtures.** The nine programs of §1.1, run
  against `seccomp_data` for a set of calls, must give the actions the
  host's kernel gave. `strace` recorded each call's result, so the oracle
  is the real kernel. The fixture is extracted by a script into the
  crate's `tests/data`.
* **Miri** over the tests.
* **A fuzzer** (`src/tests/fuzz`, `seccomp_verify_run`). For any bytes it
  checks four properties:
  * `verify` never panics;
  * a verified program's `run` never panics and takes at most `len`
    steps;
  * a verified program never loads past byte 63;
  * `verify` agrees with a second, deliberately naive checker written
    from the rules above.

### 3.2 What a filter sees, per ABI

One `SeccompData` per call, filled from the entry's own registers before
anything else looks at the call:

| Entry (`Abi`) | `arch` | `nr` | `args[0..6]` | `instruction_pointer` |
|---|---|---|---|---|
| x86-64 `SYSCALL` (`Native`) | `0xC000003E` | `RAX`, low 32 bits | `RDI RSI RDX R10 R8 R9` | `RCX`: the instruction after `SYSCALL` |
| x86-64 `int $0x80` (`Compat`) | `0x40000003` | `EAX` | `EBX ECX EDX ESI EDI EBP`, zero-extended | `RIP` after `int $0x80` |
| AArch64 `svc` (`Native`) | `0xC00000B7` | `x8`, low 32 bits | `x0`-`x5` | `ELR_EL1`: after `svc` |
| ARMv7-A `svc` (`Native`) | `0x40000028` | `r7` | `r0`-`r5`, zero-extended | `pc` after `svc` (ARM or Thumb) |

Three rules follow, and each has a check (§8).

* **SR1. The architecture token is the entry's.** It comes from the same
  `Abi` that picks the table, never from the process's image. A 64-bit
  program executing `int $0x80` is filtered as i386, with i386's numbers,
  exactly as it is dispatched. Filter authors defend against this
  confusion by checking `arch` first, as Chromium's filter does. The
  kernel's part is to tell the truth, and the truth is the entry.
* **SR2. The number a filter sees and the number dispatched are the same
  number.** The filter sees the low 32 bits as an `int`, as on Linux.
  Ferrix dispatches on the whole register, so a number with upper bits set
  is in no table and is `ENOSYS` whatever the filter said. A number is
  either dispatched as the value the filter judged, or not at all. The
  native range (`0x1000` to `0x1FFF`) goes through the filter like any
  other number (§3.3), and the ARM private range from `0xf0000` too.
* **SR3. What the filter judged is what runs.** The arguments are copied
  once from the saved frame. The dispatcher reads the same frame, which
  no other thread can write: a register frame is per thread, and there is
  no ptrace. Pointer arguments are values to a filter, as on Linux. A
  filter cannot and must not dereference memory another thread could
  change.

### 3.3 The hook: every entry, before anything answers

**Why not `dispatch_with`.** The obvious place is the dispatcher, where
the SysML model's `seccompCheck` sits ("on entry, before dispatch"). But
§2 found calls that never reach it: `rt_sigreturn` on all three
architectures, i386 `sigreturn`, x86-64 `arch_prctl` and ARM `set_tls`.
A filter must see those too:
* Chromium's baseline policy traps `arch_prctl` altogether: it is in
  `SyscallSets::IsPrctl`, one of the sets `IsBaselinePolicyWatched`
  answers with `CrashSIGSYS`
  (`sandbox/linux/seccomp-bpf-helpers/baseline_policy.cc`). It allows
  the ARM private calls, `set_tls` among them (`IsArmPrivate`), so those
  must reach the filter with their own numbers.
* Linux's strict mode is exactly "`read`, `write`, `exit`, `sigreturn`".
* A filter that denies `rt_sigreturn` and is not obeyed would be a
  sandbox that lies.

**So the check runs first in the core's own entry.** The core already
owns the way in and takes a registered answer for the rest
(`trap::set_syscall_entry`, F-09's pattern). It gains a second
registration of the same kind:

```
// trap.rs (core)
pub(crate) type SyscallFilter = fn(&SyscallArgs) -> Verdict;   // ip is a field of SyscallArgs
pub(crate) enum Verdict { Continue, Errno(u32) }                // built: S2 (S4 adds Trap)
static SYSCALL_FILTER: Once<SyscallFilter>;
pub(crate) fn filter_system_call(args: &SyscallArgs) -> Option<Outcome>
```

The sketch of the first review had `Answer(Outcome)`, which would have let
one bug in the load turn a `getpid` into `Outcome::Enter`, an `execve`'s
jump. The consultant refused it (S2's review, §12), and the built type has
no variant that carries an `Outcome`: the core builds the answer itself,
from an errno it clamps to 4095.

**Each architecture's entry calls `trap::filter_system_call` first.** It
does so with the `SyscallArgs` it built and the instruction pointer from
its frame, *before* its own early answers and before `trap::system_call`.
On `Some(outcome)`, which is always `Return` of `-errno`, it applies the
outcome exactly as it applies the dispatcher's. On `None` it goes on as
today.

**What that touches.** It is four call sites in `arch/` (x86-64 `SYSCALL`,
x86-64 `int $0x80`, AArch64 `svc`, ARMv7-A `svc`), all core, plus the
registration in `trap.rs`. The personality registers its filter from
`main.rs` beside the dispatcher (`syscall::seccomp::check`). The core names
nothing above it. With nothing registered, every call is `Continue`, so the
core alone behaves as today.

**Cost.** An unfiltered thread pays one acquiring load of the `Once`, one
indirect call, and a relaxed load of its own "filtered" flag (§3.4). A
filtered thread also runs its chain. §1.1's filter executes around 20 to
40 instructions for a common call, which is a binary search.

**Why the core, and why this is still the load's policy.** The core's
change is plumbing: one more registered function, called at the top of
four paths it already owns. What a filter decides is the personality's.
Every `Verdict` it can return makes a call do *less* than it would have:
* fail it with an errno, which the core clamps to 4095 (`Verdict::Errno`);
* from S4, `TRAP`'s rolled-back registers (`Verdict::Trap`, whose value the
  core computes itself from the call, so the filter chooses nothing);
* end the thread or the process through the paths `exit` and a fatal
  signal already use, which the filter's own body takes before it answers.

A `Verdict` can never make the core or the item do something a call
could not already make it do. That is the argument os-9f is asked to
accept (§4, §11 Q1). The alternative, moving each early answer behind the
dispatcher, is a larger change to the same core files.

**Native calls.** A filtered Linux process's native-range calls are
filtered too. The native entry is `Abi::Native`, and the hook runs before
`dispatch_with`'s range split. They carry the architecture's own token and
their raw number (`0x1000` to `0x1FFF`). An allowlist filter, which is
what Chromium and systemd's `@system-service` write, refuses them as it
refuses any number it does not know. A denylist filter allows them, as it
allows any number its author did not think of. That is Linux's behaviour
for a new system call, and the reason Linux's documentation tells authors
to allowlist.

**Native children keep the filter.** A native `process_create` from a
filtered process gives the child its creator thread's filter chain and
no-new-privs (`launch::load_native`, beside the credentials and fs context
it already passes). Otherwise one call would leave the sandbox, as
NAMESPACES §2.5 found for the mount namespace. This is the one contact
point with the item's `native.rs`, which is unchanged: it already passes
the creator.

Whether native calls should instead be refused outright to a filtered
process, whatever its filter says, is question Q2.

### 3.4 Filters: where they live and who has them

**Per thread, shared.** Linux keeps the filter pointer per thread
(`task_struct.seccomp`). Threads of one process can have different
filters, and `TSYNC` exists to make them equal. Ferrix does the same, on
`Thread`:

```
pub(crate) struct Seccomp {           // in Thread, behind its own SpinLock (a leaf)
    mode: Mode,                       // Disabled | Strict | Filter
    filter: Option<Arc<Filter>>,      // the newest; each points at the one before
}
filtered: AtomicBool                  // in Thread: mode != Disabled, for the fast path

pub(crate) struct Filter {
    program: seccomp::Program,        // verified; immutable
    log: bool,                        // SECCOMP_FILTER_FLAG_LOG
    previous: Option<Arc<Filter>>,
    length: u32,                      // this filter's instructions plus 4, and all before it
    charge: Charge,                   // F-37 (§6)
}
```

**No-new-privs.** It stays per process, as `attributes.rs` keeps it. Linux
keeps it per thread, but a `TSYNC` from a thread with no-new-privs sets it
on every thread. The one way threads differ is a thread that set it alone
without `TSYNC`, and that difference matters only for that thread's own
later `execve`, which ends every other thread anyway.

**No-new-privs is inherited by `fork`, `clone` and a native
`process_create`, as on Linux.** This fixes §2's gap. It moves the flag
from the attributes table onto `Process` (an `AtomicBool`, set-only),
because a child must be born with it and the table "cannot see a child
being made".

**Inheritance:**

| Event | Filter chain | Mode |
|---|---|---|
| `fork`, `clone` without `CLONE_THREAD` | the calling thread's `Arc`, shared | copied |
| `clone(CLONE_THREAD)` | the calling thread's `Arc`, taken under the process's thread-list lock (§3.7) | copied |
| native `process_create` | the creating thread's | copied |
| `execve` | kept | kept |
| exit | the thread's `Arc` dropped | -- |

**Release is iterative.** Dropping the last reference to a filter drops
its predecessor, and so on down the chain. A chain can be 32768 / 5 ≈
6,554 filters long (§6), so a recursive `Drop` would run the kernel stack
out. `Filter`'s `Drop` takes `previous` and walks the chain in a loop,
stopping at the first filter someone else still holds (`Arc::into_inner`).
This is Linux's `__put_seccomp_filter`. A check makes the longest chain
and drops it (§8).

**Evaluation.** The hook reads `filtered`. If it is clear, it answers
`Continue`. If it is set, it takes the thread's leaf lock, clones the
newest `Arc` (one atomic increment), and releases the lock. It then walks
the chain *in a loop, newest first*, running each program. The lock is not
held while a filter runs, so `TSYNC` never waits on a running filter, and a
filter never runs under a spin lock.

### 3.5 Installing: `seccomp(2)` and `prctl`

Everything is in a new `syscall/seccomp.rs` (load). Checks run in Linux's
order (`kernel/seccomp.c`: `do_seccomp`, `seccomp_set_mode_filter`,
`seccomp_prepare_filter`, `seccomp_attach_filter`):

1. **Unknown `op`:** `EINVAL`.
2. **`SET_MODE_STRICT`:** `flags` 0 and `args` `NULL`, else `EINVAL`.
3. **`SET_MODE_FILTER`, flags first:**
   * any bit outside the six: `EINVAL`;
   * `NEW_LISTENER` or `WAIT_KILLABLE_RECV`: `EINVAL`, since they are
     not built.

   Only then is `struct sock_fprog` copied in, so a `NULL` pointer is
   `EFAULT`. That is Chromium's probe.
4. **Length:** 0 or above 4096 is `EINVAL`.
5. **Privilege:** the process has no-new-privs, or is privileged. Else
   `EACCES`.
   * **Privileged.** Linux asks for `CAP_SYS_ADMIN` in the caller's user
     namespace. Here, until N4, that is `privileged()`. From N4 it is
     `ns_capable(caller's user namespace, CAP_SYS_ADMIN)`. The
     capability only lets a program skip no-new-privs, and a filter can
     only take away, so a root inside a child user namespace gains nothing
     by it.
   * **Why the rule exists.** Without it, an unprivileged program could
     install a filter answering `setuid` with 0 and then `execve` a
     set-uid program. That program would believe it had dropped
     privileges and had not.
6. **Copy and verify** the program (§3.1): `EINVAL` if it is refused.
   `EFAULT` if the copy faults.
7. **Mode:** a thread already in strict mode, or dead, gets `EINVAL`
   (Linux's `seccomp_may_assign_mode`). Filter mode may be added to again
   and again.
8. **Length of the chain:** the new filter's instructions plus, for every
   filter already in the chain, its instructions plus 4. Above 32768
   (Linux's `MAX_INSNS_PER_PATH`) it is `ENOMEM`.
9. **Charge** the filter to the job (§6): `ENOMEM` past its limit.
10. **Attach.**
    * Without `TSYNC`: `previous` is the thread's current chain; set the
      thread's filter, mode and `filtered`, in that order.
    * With `TSYNC`: §3.7.
    * Return 0, or a tid under `TSYNC`.

**The other operations:**
* **`GET_ACTION_AVAIL`:** `flags` 0. It reads a `u32` action. It answers
  0 for `KILL_PROCESS`, `KILL_THREAD`, `TRAP`, `ERRNO`, `TRACE`, `LOG` and
  `ALLOW`, and `EOPNOTSUPP` for `USER_NOTIF` and anything else. `TRACE`
  is "available" because a filter may return it (§3.5a).
* **`GET_NOTIF_SIZES`:** `flags` 0. It writes Linux's three sizes, which
  cost nothing. A program that then asks for a listener gets `EINVAL` at
  step 3.
* **`prctl(PR_SET_SECCOMP, 1)`:** strict mode, the same as
  `SET_MODE_STRICT`.
* **`prctl(PR_SET_SECCOMP, 2, prog)`:** `SET_MODE_FILTER` with no flags.
  bubblewrap uses only this.
* **`PR_GET_SECCOMP`:** the mode, 0 or 2. In strict mode the `prctl` call
  itself kills the caller, as on Linux.

### 3.5a Actions, and which one wins

A call runs every filter of the chain. Linux keeps the result whose action
is lowest as a *signed* 32-bit number (`seccomp_run_filters`), and on a tie
the newest filter's, so its data is the one returned. That gives the order
`KILL_PROCESS > KILL_THREAD > TRAP > ERRNO > USER_NOTIF > TRACE > LOG >
ALLOW`. An action value the kernel does not know counts as `KILL_PROCESS`.

| Result | What happens | Registers the program sees |
|---|---|---|
| `ALLOW` | `Continue` | as the call leaves them |
| `LOG` | `Continue`, and a line on the kernel log (rate-limited, as `unanswered`'s: pid, comm, arch, nr, ip) | as the call leaves them |
| `ERRNO\|e` | the call does not run. The return value is `-min(e, 4095)`. | the return register holds it |
| `TRAP\|d` | the call does not run. `SIGSYS` is forced (§3.6). | rolled back: see §3.6 |
| `TRACE\|d` | no tracer can exist, so `-ENOSYS`, as Linux answers when no tracer asked for `PTRACE_O_TRACESECCOMP` | the return register holds it |
| `USER_NOTIF` | no listener can exist, so `-ENOSYS`, as Linux answers | the return register holds it |
| `KILL_THREAD` | the calling thread ends as if killed by `SIGSYS`. If it is the process's last live thread, the whole process is killed by `SIGSYS` with its core flag, as Linux does. | none: nothing returns to user mode |
| `KILL_PROCESS`, or unknown | the process is killed by `SIGSYS`, dumping core as far as Ferrix dumps (Linux's `HANDLER_EXIT`). No handler runs. | none |

**Filter-wide logging.** With `SECCOMP_FILTER_FLAG_LOG`, every action but
`ALLOW` that a filter returns is logged as `LOG` is. Linux logs through
its audit subsystem. Ferrix's `audit.rs` records the item's decisions, and
a filter's decision is the personality's, so it goes to the kernel log
(question Q4).

**Kill paths.** Both kills go through the paths `exit` and a fatal signal
already use:
* `KILL_THREAD` uses `process::exit_thread_current`
  (`process.rs:1939`);
* `KILL_PROCESS` uses `process::kill` (`:1919`).

The thread is marked dead first (Linux's `SECCOMP_MODE_DEAD`), so no
further call of its own is served should anything return to it. **The
hook ends it, or marks it dead and answers `Errno` for the call it is in,
and never lets the thread reach user mode again.**

**Strict mode** allows `read`, `write`, `exit` and `sigreturn`, in the
entry's own numbers (i386's `sigreturn` 119, x86-64's `rt_sigreturn` 15,
since x86-64 has no plain `sigreturn`). Anything else is `KILL_THREAD`,
which Linux logs and does with `SIGKILL`. That makes it a filter of
Ferrix's own, built by the same crate from a fixed table, with a host test
per ABI.

### 3.6 `TRAP`: the signal Chromium lives on

`TRAP|d` does three things, in order:

1. **The call does not run.**
2. **The registers are rolled back** to what they were when the program
   made the call. That is Linux's `syscall_rollback`, per architecture.
   The facade gains one function, `arch::syscall_rollback_value(abi,
   number, args) -> isize`: the value to put in the return register so
   that the frame reads as it did at the call. The hook answers
   `Verdict::Trap` (S4), and the core writes that value itself: no filter
   chooses a register value.

   | Entry | Rollback | So the handler finds |
   |---|---|---|
   | x86-64 | `RAX = nr` | `RAX == si_syscall` |
   | i386 | `EAX = nr` | `EAX == si_syscall` |
   | AArch64 | `x0 = args[0]` | `x8 == si_syscall` (never written), and the call's first argument in `x0` |
   | ARMv7-A | `r0 = args[0]` | `r7 == si_syscall`, and the first argument in `r0` |

3. **`SIGSYS` is forced on the calling thread.** It uses
   `deliver::force` / `signal::force`: unblocked, and reset to the default
   if it was ignored or blocked. That is Linux's `force_sig_seccomp` with
   `HANDLER_CURRENT`. A program cannot escape a trap by blocking `SIGSYS`;
   it dies of it instead. It is delivered on the way out of this very
   call, before any other instruction of the program runs, so the frame
   the handler gets is the rolled-back one.

**The signal information** is a new `Origin::Sys {call_addr, syscall,
arch}` with `si_errno = d`. Its layout differs by word size, so it is
written per ABI, not shifted:

| ABI | `si_signo` | `si_errno` | `si_code` | `_call_addr` | `_syscall` | `_arch` |
|---|---|---|---|---|---|---|
| 64-bit (x86-64, AArch64) | 0 | 4 | 8 (= 1) | 16, 8 bytes | 24 | 28 |
| 32-bit (i386, ARMv7-A) | 0 | 4 | 8 (= 1) | 12, 4 bytes | 16 | 20 |

**Where the offsets come from and what changes.** They come from
`asm-generic/siginfo.h`, where the union starts after three `int`s and is
aligned to the pointer. S4 checks them against a `sizeof`/`offsetof`
program compiled for each target on nazuna, as `attributes.rs` records for
`robust_list_head`. `sigframe32::siginfo_from_64`'s shift stays right for
every other origin. `Sys` bypasses it, which S4's check proves for the i386
frame specifically.

**Values.**
* `_call_addr` is `instruction_pointer`: the instruction after the
  call, which is also the frame's program counter. So
  `si_call_addr == SECCOMP_IP(ctx)` holds.
* `_syscall` is `nr`.
* `_arch` is the entry's token.

**A trap handler usually makes system calls of its own.** Chromium's make
`fstatat` out of `newfstatat` and similar. Those are new calls and go
through the filter again. So does the handler's `rt_sigreturn`, which is
why §3.3 puts the hook in front of the architecture's own `rt_sigreturn`.

### 3.7 `TSYNC`: all threads, or none

For `seccomp(SET_MODE_FILTER, TSYNC [| TSYNC_ESRCH])`, after steps 1 to 9
of §3.5 have succeeded, and with the new filter made and charged but
attached to nobody:

1. **Take the process's thread-list lock.** It is what
   `Thread::sibling` / `add_thread` hold to publish a new thread, and what
   `Process::threads` holds to list them.
2. **If an `execve` has claimed the process** (`exec_thread`), answer
   `EAGAIN`. The exec is about to end this thread anyway. Linux serialises
   the two with `cred_guard_mutex`, and the program sees nothing.
3. **Check every other live thread** under its leaf lock. A thread whose
   mode is `Disabled` passes. So does one whose chain is an ancestor of the
   caller's current chain, where none counts as an ancestor of any chain.
   Linux's `is_ancestor` compares pointers down the caller's chain.
   * A thread in strict mode fails, and so does one with a chain that is
     not an ancestor.
   * **On the first failure,** release everything, drop the new filter,
     and answer that thread's tid as a positive number. With
     `TSYNC_ESRCH`, answer `ESRCH` instead.
4. **Attach.** Attach the filter to the caller as in §3.5 step 10. Then,
   for every other live thread under its leaf lock, set its chain to the
   caller's new one (one `Arc` each; the old chain is dropped *after* the
   locks are released), set its mode to `Filter` if it was `Disabled`, and
   set `filtered`.
5. **Release the lock and return 0.**

**Races, each closed by the thread-list lock:**
* **A thread being made during the sync** either is already in the list,
  so step 4 reaches it, or copies its creator's chain after step 4, so it
  gets the new one. `clone(CLONE_THREAD)` reads the creator's filter *under
  the thread-list lock* in `add_thread` for exactly this reason.
* **A thread that is ending** is either in the list and set, which is
  harmless, or gone.
* **An `execve`** is step 2.

**No-new-privs** needs no step: it is per process here (§3.4).

**Allocation.** Nothing is allocated under the spin locks: the filter and
its charge exist before step 1, and the old chains are dropped after
step 5 (the lock rule of NAMESPACES §6).

### 3.8 Signals and restarted calls

* **A call restarted after a signal re-executes the system call
  instruction** (`rewind_syscall`), so it is a new call and goes through
  the filter again. `restart_syscall`, which a restart block uses, is a
  number like any other and is filtered as such. Chromium's baseline
  allows it (`SyscallSets::IsKernelInternalApi`). On i386 and ARM it is
  number 0, a value filters must not special-case.
* **`ERRNO` and `TRAP` never produce a restart code.** An errno a filter
  chose is returned as is, even `EINTR`. A filter could ask for errno 512,
  `ERESTARTSYS`. That value is internal to Ferrix and must never reach
  `mark_restart`. The hook's `-e` is written straight into the return
  register and never passes `linux::dispatch`'s restart handling.
  **Rule SR7**, with a check.
* **Why `TRAP` must follow its call at once.** A `SIGSYS` from `TRAP` is
  synchronous and delivered first. Any other pending signal waits until
  it is delivered, and its handler's frame is built on top of `SIGSYS`'s.
  If the thread is killed first, that is fine. What must not happen is
  the program running on past the trapped call.
* **`sigaltstack`, masks and `SA_NODEFER`** behave as for any handler.
  Chromium relies on `SA_NODEFER` so that a trap inside its handler traps
  again rather than killing it.

### 3.9 Speculation: `SPEC_ALLOW` is accepted and changes nothing

Linux, on installing a filter without `SPEC_ALLOW`, turns on the
speculative-store-bypass and indirect-branch mitigations for the thread
(`arch_seccomp_spec_mitigate`). Chromium asks it not to, and then disables
indirect-branch speculation itself through `PR_SET_SPECULATION_CTRL`.

On Ferrix these mitigations are always on for every program
(`docs/certification/SPECULATION.md`: "the item cannot know which of its
programs run a JIT or a sandbox"). So:
* **`SPEC_ALLOW`** is accepted and turns nothing off, and no filter or
  flag can turn off a mitigation.
* **`PR_GET_SPECULATION_CTRL`** answers `PR_SPEC_FORCE_DISABLE` for
  store bypass and indirect branches: "mitigated, and not yours to
  change".
* **`PR_SET_SPECULATION_CTRL`** accepts a request for more mitigation and
  refuses one for less, with `EPERM`, as Linux does for a force-disabled
  feature.

Chromium's `DisableIBSpec` reads the force-disabled answer and returns.

### 3.10 `/proc/<pid>/status`, and what is left out

**`status` gains three lines, after `Groups`/`NStgid`, in Linux's
order:**
* `NoNewPrivs:\t0|1`
* `Seccomp:\t0|1|2`
* `Seccomp_filters:\tN`, the chain's length

They come from `ferrix_procfs::status::render` with fields filled by
`fs/procfs/render.rs`. For a thread's `status`, they are that thread's.

**Left out, each answered as a Linux without it answers:**
* **`USER_NOTIF`**: the listener descriptor, its `ioctl`s, `ADDFD`.
  Container managers use it (LXD, crun). Chromium, Steam, bubblewrap and
  systemd do not. It adds a kernel object that blocks a thread on another
  process's answer, a class that has had its own races upstream. **Not
  offered**, so `GET_ACTION_AVAIL` says `EOPNOTSUPP` and libseccomp knows.
* **ptrace and `TRACE`**. Ferrix has no ptrace. When it does, `TRACE`
  gains Linux's stop, with the recheck after the tracer and the "number
  -1 skips" rule. That is Linux's fix for CVE-2019-2054, a tracer changing
  the number after the filter ran. It also gains
  `PTRACE_O_SUSPEND_SECCOMP` and `PTRACE_SECCOMP_GET_FILTER`. Until then
  a `TRACE` result is `ENOSYS`, never "allow".
* **Linux's per-filter action cache**, a bitmap of numbers a filter
  always allows. It is a speed-up with its own emulator to get right.
  Measured first (§10, risk 3).
* **`bpf(2)`, extended BPF, socket filters.** `SO_ATTACH_FILTER` could
  reuse §3.1's interpreter one day, with socket loads. Not in this
  design.

---

## 4. Where it lives: the certified item and the load

| File | Ring | What |
|---|---|---|
| `src/lib/kernel/seccomp` (new crate) | outside the kernel crate, like `src/lib/kernel/kmem` | verifier and interpreter |
| `syscall/seccomp.rs` (new) | load | install, `prctl`'s part, the chain, `TSYNC`, the hook's body, the actions |
| `syscall/seccomp_check.rs` (new) | load (a test pattern) | the boot checks, §8 |
| `syscall/attributes.rs`, `thread.rs`, `process.rs`, `family.rs`, `exec.rs`, `launch.rs`, `signal.rs`, `deliver.rs`, `linux.rs` | load | `prctl` options, the per-thread state, inheritance, no-new-privs moved to `Process`, `Origin::Sys` |
| `src/lib/proto/linux-abi` (`nr.rs`, `sigframe32.rs`) | outside | the four numbers, `_sigsys` |
| `fs/procfs`, `src/lib/fs/procfs` | load / outside | the status lines |
| **`trap.rs`** | **core** | the `SyscallFilter` registration and `filter_system_call` (§3.3) |
| **`arch/x86_64/syscall.rs`, `arch/x86_64/mod.rs`, `arch/aarch64/trap.rs`, `arch/armv7a/trap.rs`** | **core** | one call each at the top of the system call path; the instruction pointer passed; `syscall_rollback_value` in each facade |
| `main.rs` | item (composition root) | registering `syscall::seccomp::check`; the check's call |
| `panic/catalog.rs` | core, as data | the new FX codes: FX-1302 onwards, the stage's next free ones |
| `stages_check.rs` | item | the `seccomp` line |

**This design is mostly load and touches the core.** NAMESPACES touched
the item at two points (`native.rs`'s child, and audit) without editing
either. This one edits the core in five files, for the hook (§3.3). The
item's `native.rs` is unchanged, as in NAMESPACES §2.5: the native child's
filter comes through `launch::load_native`, which is load.

**What the core's change is and is not.**
* **It is** one registered function pointer, called at the top of paths
  the core owns, whose result the path applies with the code it already
  has.
* **It is not** a new decision in the core. The core does not interpret a
  filter, hold one, or know what seccomp is.

With nothing registered it is `Continue`, and the core's own boot checks
run with nothing registered, so they test the core as it was.

**What the Security Target is asked to say.** Seccomp is a Linux-personality
facility, like the uid model. The ST claims nothing for it: a program that
escapes its filter is a defect in the load, bounded by the item's own
enforcement (address spaces, handles, quotas), which no filter is part of.
It gets a paragraph in `docs/certification/VULNERABILITY-ANALYSIS.md`'s
"What this analysis does not cover", after the namespaces' entry, with §5's
defect classes as its table. It is written as each landing makes its
evidence real, as the namespaces' is.

**The consultant reviews it** for the same reasons as the namespaces and
one more:
* it adds kernel heap a program can make and keep (F-37);
* it changes `fork`'s inheritance of no-new-privs;
* it is the first design since F-09 to add a hook to the core's system
  call path, on every call of every program.

---

## 5. Security: rules, the defect each answers, and its evidence

Rules are SR1 to SR14, and landings S0 to S8 (§9). Each rule's evidence
names the landing that brings it. The vulnerability analysis's entry
(§4) is this table.

| Rule | Defect class it answers | Evidence (landing) |
|---|---|---|
| **SR1** the `arch` token is the entry's, as the table is | x86-64 filters bypassed by `int $0x80` or x32 numbers (Linux's man page, "Caveats"; the x32 overlap before 5.4) | the `seccomp` line: a 64-bit program's `int $0x80` `getpid` reaches a filter that answers `ERRNO` only for `arch == I386`; negative control: token from the process's image (S2) |
| **SR2** the number filtered is the number dispatched | a filter judging one number while another runs | host test: every table's number round-trips; boot check: `RAX = 1<<32 \| getpid` is `ENOSYS` whatever the filter allows (S2) |
| **SR3** every entry is filtered, early answers included | a sandbox that denies `rt_sigreturn` or `arch_prctl` and is not obeyed | the `seccomp` line: a filter answering `ERRNO` for each of `arch_prctl` (x86-64), `set_tls` (ARM), and `KILL_PROCESS` for `rt_sigreturn`, each obeyed on its architecture; negative control: the hook moved into `dispatch_with` (S2) |
| **SR4** no-new-privs before an unprivileged filter, and inherited by every child | a filter confusing a set-uid program (the reason for the rule, `Documentation/userspace-api/no_new_privs.rst`); a child escaping no-new-privs by being forked | `EACCES` without it; a forked child of a no-new-privs parent reads `NoNewPrivs: 1` and runs a set-uid file as its caller; negative control: `fork` not copying it (S3) |
| **SR5** filters only accumulate, and the strictest wins | a later filter loosening an earlier one | a chain of `ALLOW` over `ERRNO` stays `ERRNO`; a tie keeps the newest filter's data; host tests of the order with every action pair (S1, S3) |
| **SR6** a filtered process's children are filtered: `fork`, `clone`, `execve`, native `process_create` | leaving the sandbox through a native child (NAMESPACES §2.5's class) | the `seccomp` line: a native child of a filtered creator is refused the call its creator's filter refuses; negative control: `load_native` passing no filter (S3) |
| **SR7** a filter's errno is returned as is: capped at 4095, never a restart | `ERRNO` of 512 (`ERESTARTSYS`) turning into a restart loop or an internal code leaking | a filter returning `ERRNO\|512` gets `-512` to the program, once (S3) |
| **SR8** the verifier is Linux's, and runs are bounded | unbounded or out-of-range programs; reads of uninitialised scratch (CVE-2010-4158) | the crate's host tests, Miri, the fuzzer's properties (S1) |
| **SR9** `TRAP` cannot be blocked or ignored, and the program runs no instruction past the call | a sandbox whose traps can be switched off by `sigprocmask` | `SIGSYS` blocked and ignored, then a trapped call: the handler runs, or the process dies of `SIGSYS`; never the next instruction (S4) |
| **SR10** `TSYNC` is all or nothing, and no thread is left behind or made during it without the filter | Linux's `TSYNC` races with `clone` and `exec` | the `seccomp` line: eight threads, one of them `clone`ing in a loop during the sync, then every thread refused; a thread with a foreign filter makes `TSYNC` fail with its tid and changes no thread; negative control: the creator's filter read outside the thread-list lock, shown to leave a thread unfiltered with a delay inserted there (S5) |
| **SR11** a chain is released without recursion | kernel stack overflow on a long chain's `Drop` | the longest allowed chain (6,554 one-instruction filters) made in a child that then exits (S3) |
| **SR12** a filter's memory is charged | F-37's class: kernel heap a program keeps without bound | the `kmem` line: filters installed in a loop until `ENOMEM` at the job's limit, a sibling installs one, both read zero after (S3) |
| **SR13** unbuilt actions fail closed | `TRACE` or `USER_NOTIF` read as "allow" by a kernel without a tracer or listener | each returns `ENOSYS`, and the call does not run (S4) |
| **SR14** no filter or flag lowers a speculation mitigation | `SPEC_ALLOW` read as "turn SSBD off" | `SPEC_ALLOW` accepted; `PR_SET_SPECULATION_CTRL` to enable refused `EPERM` (S4) |

---

## 6. F-37 and limits

| Kind | Made by | Charged |
|---|---|---|
| a filter: its header and `8 × len` bytes of program | `seccomp(SET_MODE_FILTER)`, `prctl(PR_SET_SECCOMP, 2)` | to the job of the calling process, before anything is attached; released when the last thread and child holding it goes |
| the verifier's working mask | the same call | on the stack: 4096 × 2 bytes is too much for a kernel stack, so it is one charged, freed allocation per install |

**Limits.**
* 4096 instructions per filter.
* 32768 per chain, counting 4 extra for each filter (Linux's
  `MAX_INSNS_PER_PATH`). This also bounds a call's filtering to 32768
  steps. At a few nanoseconds a step that is tens of microseconds in the
  worst case. §1.1's filters run a few dozen steps.

A job cannot fill memory with filters past its limit. A thread cannot make
its own calls slower than its chain's length allows.

---

## 7. Locks

| Lock | Kind | Guards | Taken after |
|---|---|---|---|
| a process's thread list | `SpinLock` (exists) | the list; with it, `TSYNC` and `add_thread`'s copy of the creator's chain | nothing new |
| a thread's `Seccomp` | `SpinLock`, per thread (new) | `mode`, `filter` | the thread list, or nothing; a leaf |
| `filtered` | `AtomicBool` | the fast path's "has a chain" | written under the leaf lock, after `filter` and `mode`, with release; read with acquire |
| `Process::no_new_privs` | `AtomicBool`, set-only (moved) | -- | -- |

Rules:
* No filter runs under any lock.
* Nothing is allocated or freed under the two spin locks. The new filter
  exists before them, and old chains drop after them.
* The leaf lock is never held while the signal locks are taken. `TRAP`
  forces its signal after evaluation, with no seccomp lock held.

**Why the hook may open interrupts for the walk of a chain** (the consultant's
S3 condition 2; `syscall::seccomp::check`). The core's contract is that the
hook is entered and left with interrupts masked; a body that runs up to
32,768 steps opens them for the walk and masks them again before it returns.
That is safe at each entry because, at the point where the entry calls the
hook, everything the entry set up is complete, lives on the calling task's own
kernel stack or in its own thread object, and is exactly what the entry's
dispatch, which the same entry runs with interrupts open a few instructions
later and which may block, already relies on across a preemption:
* **x86-64 `SYSCALL`.** `SFMASK` cleared `IF` on entry, the stub swapped `GS` and
  switched to the task's kernel stack, and `ferrix_syscall_entry` was handed a
  `SyscallFrame` on that stack, with the user registers saved in it. The hook
  runs before `answer_here` and the dispatcher, which enable interrupts
  themselves for the call. A preemption mid-walk saves and restores the task's
  kernel context like any other; the frame is the task's own and no other task
  names it. The way out disables interrupts again before `GS` is swapped on a
  live stack, and so does the hook before it returns.
* **x86-64 `int $0x80`.** An interrupt gate cleared `IF`, the hardware pushed
  the frame onto the task's kernel stack and `system_call` received it as a
  `TrapFrame`: the same argument.
* **AArch64 `svc` and ARMv7-A `svc`.** The exception entry masked the
  interrupts, saved the registers into the `TrapFrame` on the task's kernel
  stack, and `system_call` received it; the dispatcher enables interrupts the
  same way after the hook.
What a preempted walk holds is the thread's `Arc` of the newest filter (a
reference, so a `TSYNC` replacing the chain cannot free it) and nothing else:
no lock, no per-processor value, no pointer into another task. It reads
`seccomp_data`, built from the frame, which no other task can write: a frame is
per thread (SR3). It can migrate to another processor between filters, which
costs nothing it relied on, since it uses no per-processor data after
`sched::current` (which disables interrupts for its own look and restores them).

---

## 8. Checks

### 8.1 Boot checks

They live in `syscall/seccomp_check.rs`, on a `seccomp` line with a cost
entry and FX codes from FX-1302. The calls are driven *through the core's
entry* as a program's are. Most boot checks call `linux::handle` directly
(`syscall/check.rs:723`). That skips the hook, so these checks use a
ring-3 program, as the i386 and `sem` checks do, or the core's entry with
a real frame.

Each negative control is run once, quoted in the commit, and never
committed. Each stops the boot with its own message.

| Check | Negative control: what it sabotages, and the message it must stop with | Landing |
|---|---|---|
| Probe answers: `prctl(PR_SET_SECCOMP, 2, NULL)` and `seccomp(SET_MODE_FILTER, f, NULL)` are `EFAULT` for `f` in `{0, TSYNC, LOG, SPEC_ALLOW, TSYNC_ESRCH}` and `EINVAL` for `NEW_LISTENER` and bit 6 | the program copied before the flags are checked: "an unknown seccomp flag was answered EFAULT" | S3 |
| `EACCES` without no-new-privs as uid 1000; accepted as root | the privilege step removed: "an unprivileged filter was installed without no-new-privs" | S3 |
| A filter answering `ERRNO\|EPERM` for `getppid` and `ENOSYS` for `clone3`: `getppid` is `-EPERM`, `clone3` `-ENOSYS`, `getpid` runs | the hook not registered: "a filtered getppid ran" | S3 |
| Order: `ALLOW` newest over `ERRNO` oldest gives `ERRNO`; two `ERRNO`s give the newest's data; `KILL_PROCESS` over everything | the chain run oldest-first with the last result kept: "a newer ALLOW overrode an older ERRNO" | S3 |
| Inheritance: a forked child, an `execve`d image, a `clone(CLONE_THREAD)` thread and a native child are each refused `getppid`; `NoNewPrivs: 1`, `Seccomp: 2`, `Seccomp_filters: N` in each one's `status` | `fork` not copying no-new-privs: "a forked child of a no-new-privs process could gain privileges"; `load_native` passing no filter: "a native child of a filtered process was not filtered" | S3 |
| `KILL_THREAD` of one of two threads ends that thread only, and the process's exit shows `SIGSYS` when the last is killed; `KILL_PROCESS` ends both | `KILL_THREAD` implemented as `exit`: "a killed thread's process exited normally" | S3 |
| `ERRNO\|512` returns `-512` once; `ERRNO\|5000` returns `-4095` | the value passed through `linux::dispatch`'s restart path: "a filter's ERESTARTSYS restarted the call" | S3 |
| Strict mode: `read` and `write` run, `getpid` kills | none beyond the table's host test | S3 |
| The longest chain made and released; the `kmem` fill | `Drop` recursive: the boot's stack overflow is the message; the filter uncharged: "kmem: a seccomp filter was not charged to its job" | S3 |
| Every entry filtered: `arch_prctl` (x86-64), `set_tls` (ARM), `rt_sigreturn` and i386 `sigreturn`, each obeyed on its architecture | the filter call moved below the early answers: "arch_prctl was answered before the filter" | S2 |
| `arch`: a 64-bit program's `int $0x80` is filtered with `0x40000003` and i386's numbers; a filter written for x86-64 alone does not see it as x86-64 | the token from the image, not the entry: "an int 0x80 call was filtered as x86-64" | S2 |
| `TRAP`: the handler sees `si_code 1`, `si_errno d`, `si_call_addr` = its context's pc, `si_syscall` = its context's number register, `si_arch` right, the first argument intact; it writes a result, which the program gets; on i386 too | the rollback left out (the return register holds `-ENOSYS`): "SIGSYS's context lost the syscall number"; i386 `_sigsys` shifted like the other origins: "i386 si_syscall was not at offset 16" | S4 |
| `TRAP` with `SIGSYS` blocked, then ignored: the process dies of `SIGSYS`; with `SA_NODEFER`, a trap inside the handler traps again | `force` replaced by `post`: "a blocked SIGSYS let a trapped call return" | S4 |
| `TRACE` and `USER_NOTIF` results are `ENOSYS` and the call does not run | `TRACE` read as `ALLOW`: "a TRACE result let the call run" | S4 |
| `TSYNC`: §5 SR10's three cases | the creator's chain read outside the thread-list lock, with a delay inserted: "a thread made during TSYNC was not filtered" | S5 |

### 8.2 `cargo xtask test-seccomp`: the user-space test plan

This is a gate like `test-sem` (`tools/common/xtask/src/sem.rs`): a guest
program built for `x86_64`, `i686`, `aarch64` and `armv7` musl, booted as
init, its lines matched, its negative controls as Cargo features. It lives
in `src/tests/seccomp` and is Rust with the `libc` crate, as the other
guest programs are. It writes raw `sock_filter` arrays with helpers shaped
like libseccomp's and Chromium's `BPF_STMT`/`BPF_JUMP`, so a reader of a C
filter recognises them. Its cases:

1. A **filter for two architectures**, like Flatpak's and the spike's
   `noipc.py`. It checks `arch`, then answers `ENOSYS` for System V IPC in
   x86-64's and i386's numbers. The same filter image is used by the
   64-bit and the 32-bit build, and each is refused its own calls only.
2. **Chromium's shape.** A filter generated at build time to the shape of
   §1.1: an `arch` check, the x32 bit, a binary search, `JSET` and `AND`
   on arguments, `TRAP`s with data. A `SIGSYS` handler checks Chromium's
   five sanity conditions and emulates one call by writing its result.
3. **bubblewrap's path.** `prctl(PR_SET_SECCOMP, 2, &prog)` after
   `PR_SET_NO_NEW_PRIVS`, then `execve` of a second program that finds
   itself filtered.
4. **The probes** of §8.1's first line, as a program sees them.

Then two runs of real programs:

* **Linux's own selftest**, `tools/testing/selftests/seccomp/seccomp_bpf.c`
  (about 5,000 lines, kselftest's harness). Built static against musl
  with nazuna's cross compilers (the only C guest program, so S6 adds the
  build step), run as init. Its results are compared with a committed list
  of expected skips and failures: the ptrace and `USER_NOTIF` tests, and
  any the list names with a reason. A new failure fails the gate, and a
  new pass must be removed from the list. It is Linux's own statement of
  seccomp's semantics, so it is the oracle this design's rules are
  checked against.
* **Chromium, from S7 on:**
  * `cargo xtask test-chrome-window` (and headless) without `--no-sandbox`;
  * chrome://sandbox read from the page: "Seccomp-BPF sandbox: Yes", and
    once the namespace layer is there, "You are adequately sandboxed";
  * a renderer's `/proc/<pid>/status` reading `Seccomp: 2`.

  CEF's own page under `steamwebhelper` is S8's.

---

## 9. The landings

In points. Each lands on its own, with the image row of `docs/BACKLOG.md`:
* `cargo xtask check`;
* a release build of all three architectures;
* four boots, with armv7a at `--smp 2` and x86_64 under KVM;
* `test-shell` with its busyboxes and `test-vfs`, since this is stage 7
  work.

S2 and S5 add `test-threads --arch all`. The consultant's OK on each diff
comes before `land.sh take`.

| | Landing | Gives | Gate beyond the row | Unlocks | Points |
|---|---|---|---|---|---|
| S0 | This document | the design | `cargo xtask check` | -- | -- |
| S1 | `src/lib/kernel/seccomp`: verifier and interpreter (§3.1), host tests with Chromium's nine filters as fixtures, Miri, the fuzzer | SR8 | the crate's tests, Miri, a fuzzing run | nothing a program sees | 3 |
| S2 | The hook (§3.3): `SyscallArgs` gains the instruction pointer; `trap::filter_system_call` registered, called first at all four entries, before the early answers; `arch::syscall_rollback_value`; `seccomp_data` per ABI (§3.2). The registered body answers `Continue` until S3 | SR1, SR2, SR3 | the "every entry filtered" and `arch` checks, with a test-only filter | nothing a program sees; the core change reviewed alone | 4 |
| S3 | Filters: `seccomp(2)` in the four tables, `prctl`'s options, no-new-privs moved to `Process` and inherited, per-thread chains and their inheritance (including native children), iterative release, `ALLOW`, `ERRNO`, `KILL_THREAD`, `KILL_PROCESS`, `LOG`, strict mode, `GET_ACTION_AVAIL`, `GET_NOTIF_SIZES`, the `status` lines, F-37 | SR4 to SR8, SR11, SR12 | §8.1's S3 lines | **stage 13's exit clause "a seccomp filter that blocks a syscall"**; bubblewrap's `--seccomp`; Flatpak's and systemd's filters; init's L13 `SystemCallFilter=`; chrome://sandbox's "Seccomp-BPF sandbox" row turns Yes (the probe passes) | 6 |
| S4 | `TRAP`: `Origin::Sys` with the per-ABI layouts, the rollback, forced `SIGSYS`; `TRACE` and `USER_NOTIF` as `ENOSYS`; `SPEC_ALLOW` and `PR_*_SPECULATION_CTRL` | SR9, SR13, SR14 | §8.1's S4 lines | Chromium's renderer policies can run (they are made of `TRAP`s) | 4 |
| S5 | `TSYNC`, `TSYNC_ESRCH` (§3.7) | SR10 | §8.1's S5 line; `test-threads --arch all` | chrome://sandbox's TSYNC row; Chromium's multi-threaded start; libseccomp's `seccomp_attr_set(SCMP_FLTATR_CTL_TSYNC)` | 3 |
| S6 | `cargo xtask test-seccomp` (§8.2): the guest program on four ABIs, and Linux's `seccomp_bpf` selftest with its skip list, built on nazuna | the oracle | `test-seccomp --arch all` | confidence: Linux's own tests pass on Ferrix | 4 |
| NP-pid | *The namespaces stream's, not this document's.* Pid namespaces as Chromium's zygote uses them (§1.2): `CLONE_NEWPID` from `clone` and `unshare` under a user namespace, nested, `/proc/self/ns/pid`, the new namespace's pid 1 reaping its orphans, pids translated in `SCM_CREDENTIALS`, `getpid` and procfs as seen from inside | -- | the namespaces stream's | **required**: without it Chrome with its sandbox does not start (§1.2, run v2) | unsized; os-98's first guess is more than N4's 8 |
| NP-net | *The namespaces stream's, optional.* An empty network namespace, with loopback alone | -- | the namespaces stream's | only chrome://sandbox's "adequately sandboxed" verdict: Chrome runs sandboxed without it (§1.2, run v3) | unsized; small if an empty one with loopback suffices, larger otherwise |
| S7 | Chrome sandboxed: `test-chrome-window` without `--no-sandbox`, with S3 to S5, N4 and NP-pid | -- | `test-chrome-window`; a renderer's `Seccomp: 2`; chrome://sandbox read in a windowed run | Chromium's sandbox on Ferrix | 3, plus what it finds |
| S8 | `steamwebhelper` without `-no-cef-sandbox`: `docs/STEAM.md` §3's row closed, inside pressure-vessel once N7 has it | -- | `test-steam-bootstrap --arch x86_64` | **the customer's end point for the helper's sandbox** | 2, plus what it finds |

**Totals:**
* **20 points for S1 to S6**, the seccomp half, complete and gated on its
  own.
* **25 with S7 and S8.** Those two also need N4 (NAMESPACES, 8 points)
  and NP-pid, unsized and not yet the namespaces stream's plan (§11, R1).
  NP-net matters only for the verdict row.

**Order:**
* S1 and S2 are independent and can go in parallel.
* S3 needs both. The stage's exit clause is met at S3, 13 points in.
* S4 and S5 are independent of each other.
* S6 can start on S3 and grows with S4 and S5.
* S7 needs S4, N4 and NP-pid.
* S8 needs S7, and N6 or N7.

The critical path to the helper's sandbox is therefore **not** seccomp: it
is pid namespaces, which nobody has designed yet (CONVENTIONS, splitting
rule 1). R1 should be answered, and NP-pid designed, before S3 starts, so
that the pid half is built while seccomp is.

---

## 10. Risks

1. **The core change is on every call.**
   * **The risk.** S2 edits four entry paths that every program's every
     call takes.
   * **Mitigation.**
     * With no filter registered they behave as today. S2 lands with a
       registered body that only answers `Continue`, so its gate measures
       the plumbing alone.
     * `test-threads`, `test-shell` and the KVM boot run on it before
       anything can filter.
     * The early answers keep their order after the hook, so a
       `rt_sigreturn` frame restore is untouched.
2. **i386's frame.**
   * **The risk.** `_sigsys` is the first signal information whose
     32-bit layout is not the 64-bit one shifted. S4's offsets are checked
     against `offsetof` on nazuna, and the i386 `TRAP` check reads them
     from a real handler.
   * **Unrelated, and still open.** The 32-bit frame still carries
     `FXSAVE` alone (BACKLOG P2).
3. **Cost per call.**
   * **Unfiltered programs.** They pay an indirect call and an atomic
     load. S2 measures `getpid` in a loop before and after, on x86_64
     under KVM, and records it in the commit.
   * **Filtered programs.** Chromium's renderers pay a few dozen
     interpreted instructions per call. If that shows in
     `bench-chrome-video`, Linux's per-filter bitmap cache is the known
     remedy, a landing of its own with its own emulator and checks. It is
     not assumed.
4. **Chromium needs more than seccomp.**
   * **The risk.** §1.2's pid namespace requirement, now measured, means
     S7 cannot pass on this document's landings and N4 alone.
   * **Chosen:** S3 to S5 are gated on their own, without Chromium, by
     `test-seccomp` and Linux's selftest.
   * **Rejected:** a trial with `--disable-namespace-sandbox`, which is
     `LOG(FATAL)` without the set-uid sandbox.
5. **Linux's selftest is large.**
   * **The risk.** Its expected-failure list could hide real gaps.
   * **Mitigation.** Every entry names the unbuilt feature it tests
     (ptrace, `USER_NOTIF`, `SECCOMP_RET_TRACE` stops) or a reason. The
     consultant reviews the list with S6.
6. **Other streams in the same files.**
   * **The risk.** N4 edits `credentials.rs`, `attributes.rs` (`capget`,
     `PR_CAPBSET_*`), `family.rs` and procfs's `status`, which S3 also
     touches.
   * **Mitigation.** Rebase conflicts are in rows and lines, handled by
     CONVENTIONS rule 4. §3.5's privilege step is written to take N4's
     `ns_capable` when it lands.

---

## 11. Open questions

**For os-9f (certification):**

* **Q1. The core hook.** Is §3.3 acceptable as argued: a registered
  function called first at all four system call entries, which can only
  answer less? The alternative is moving each architecture's early
  answers (`arch_prctl`, `set_tls`, `rt_sigreturn`, `sigreturn`) behind
  the dispatcher, a larger edit to the same core files. Does either need
  a finding opened, or a boot check of the core itself with nothing
  registered?
* **Q2. Native calls from a filtered process.** §3.3 filters them by
  number, like any call, so a denylist filter lets them through as it
  lets through any number it does not name. Should a filtered process be
  refused the native range outright, whatever its filter says? That is
  stricter than Linux, which has no such range. init's `dirclient`, a
  Linux program, makes native calls, so a service with
  `SystemCallFilter=` and the directory would then need an exemption.
* **Q3. The Security Target.** Is a paragraph in the vulnerability
  analysis's "does not cover", with §5's table, the right record, as for
  the namespaces? Or does a facility that confines programs, used by the
  browser, deserve an OE objective of its own?
* **Q4. Audit.** A seccomp kill or refusal is the personality's decision,
  not the TSF's, so §3.5a logs it to the kernel log and not to
  `audit.rs`. Agree? P.ACCOUNTABILITY lists "a call refused", and a
  reviewer may read that wider.
* **Q5. No-new-privs on `fork`.** S3 changes `fork` to copy no-new-privs,
  which Linux does and Ferrix never did. It is safe in the direction it
  moves: a child can only gain the restriction. Should it land ahead of
  S3, alone, as a fix?

**For os-98 (namespaces):**

* **R1. Pid namespaces (NP-pid).** Measured (§1.2): with user namespaces
  and seccomp but `CLONE_NEWPID` refused, Chrome 151 with its sandbox on
  dies at start, whether `/proc/self/ns/pid` exists (v1, `SIGTRAP`) or
  not (v2, `zygote_host_impl_linux.cc` fatal). It never falls back to the
  set-uid sandbox while the user namespace probe passes. So pid
  namespaces are required. NAMESPACES §1.5 keeps `CLONE_NEWPID` at
  `EINVAL`, and N4 to N7 do not include it.

  What Chromium uses:
  * nested, unprivileged under a user namespace, from `clone`;
  * `/proc/self/ns/pid`;
  * the zygote as pid 1 reaping its orphans;
  * a renderer per namespace;
  * pids translated in `SCM_CREDENTIALS`.

  Will the namespaces stream design and own NP-pid, and at what size?
  Its first guess is more than N4's 8 points.
* **R2. Network namespaces (NP-net).** Measured (§1.2, v3): Chrome drops
  `CLONE_NEWNET` when `/proc/self/ns/net` is missing and runs fully
  sandboxed otherwise. chrome://sandbox's verdict would then say "not
  adequately sandboxed". Is that verdict part of the customer's end
  point? If so, does an empty network namespace with loopback alone
  suffice? Chromium's renderers make no network calls of their own, so it
  should.
* **R3. Nesting depth.** The zygote makes a user namespace inside the one
  its capability probe made. Inside pressure-vessel that is three deep,
  under bwrap's. N4's limit of 32 covers it. Does N4's check exercise a
  nested `unshare(CLONE_NEWUSER)` with maps written at each level?
* **R4. `chroot` into a directory that is gone.** The zygote's child does
  `chroot("/proc/self/fdinfo/")` and exits, leaving the zygote rooted in a
  dead process's directory. Chromium then `CHECK`s that `/proc` is
  unreachable. N4's `CAP_SYS_CHROOT` rule allows the `chroot`. Does
  procfs keep a dead process's `fdinfo` directory walkable as an empty
  root, as Linux does?
* **R5. `CLONE_NEWUSER` errors.** Chromium `PCHECK`s the errno of a
  refused `CLONE_NEWUSER`: `EPERM`, `EUSERS`, `EINVAL` or `ENOSPC`, and
  never `ENOSYS`. Ferrix answers `EINVAL` today. Every refusal N4 adds
  must stay in that set.

**For whoever takes S6 (not blocking):**

* **R6.** libseccomp's start-up probes (`src/system.c`) and Firefox's
  content sandbox (`security/sandbox/linux/`) were not read for this
  design. S6 reads both, and adds to §1.3 what either needs beyond §1.5.

---

## 12. Where it stands (2026-09-30)

Designed on 813d1ea8 after tracing Chrome 151's sandbox start on nazuna,
running it with pid and network namespaces refused (§1.2), and reading
Linux's `kernel/seccomp.c` and `net/core/filter.c` at 551c722f and
Chromium's sandbox sources at `main`. Nothing is built. The
consultant's review of the design, and os-98's answers to R1 to R5, come
before S1.

**The design's review (certification consultant, 2026-09-30): OK to
build**, with these answers to §11 and conditions on the landings.

* **Q1, the hook.** Accepted in the core, on F-09's terms. A `Verdict` can
  produce only what the dispatcher already could: an errno, a value without
  running the call, or the existing exit and fatal-signal paths. It gets a
  requirement in `trap` and a vulnerability-analysis row ("a registered
  filter makes the core do more than the call could"), with a check. The
  time a filter chain may take per call is bounded by the verifier and the
  chain (Linux: 4096 instructions a program, 32768 a path), and the bound
  goes into `MEMORY-AND-TIMING.md`, since AoU-4 now includes a program a
  user supplies on every call. The hook allocates nothing, takes no
  sleeping lock and reads registers only. A process with no filter pays the
  `Once` load and nothing else.
* **Q2, native calls.** Native-range calls carry an `arch` value of their
  own in `seccomp_data`, one Linux never uses, as an i386 call on x86-64
  carries `AUDIT_ARCH_I386`. A filter that checks `arch`, as Chromium's and
  systemd's do, then refuses them by its own rule, and a filter that wants
  them allows that `arch` explicitly. It gets an SR row and a check both
  ways. A hook in the personality's dispatcher would miss native calls and
  the arch entries' early answers, so it cannot carry this design.
* **Q3.** No objective of its own: seccomp is a program restricting
  itself, and claiming it would pull the verifier and the interpreter into
  the item. A "does not cover" paragraph goes in the vulnerability
  analysis and the Security Target. Because the interpreter runs
  user-supplied bytecode in ring 0, S1's `src/lib/kernel/seccomp` is
  `forbid(unsafe_code)`, has a fuzz target (a verified program ends within
  the bound and reads nothing outside `seccomp_data`), and has a host test
  for every rejection the verifier makes.
* **Q4.** A kill goes to the kernel log, rate-limited, not to `audit.rs`,
  which records the TSF's own decisions (F-21b).
* **Q5.** Done ahead of S1: `no_new_privs` and dumpability are inherited
  by fork children and native children, and kept or reset at `execve` as
  Linux keeps them (5df02c3b, reviewed).

A seccomp implementation built on branch `stage13-seccomp` before this
design landed puts its hook in the personality's dispatcher and its state
per process. Its crate is §3.1's and may become S1 under S1's conditions.
Its hook and state do not land.

**Owners (2026-09-30, the customer's instruction to os-7c).** os-7c takes
stage 13 to done, S1 to S6 included; os-fd keeps S7 and S8. NP-pid and NP-net
are os-7c's too (os-98 yielded them), built as `stage13-pidns` (`docs/PIDNS.md`)
and `stage13-netns` (`docs/NETNS.md`).

**S1 built (2026-09-30, os-7c, `stage13-s1`).** `src/lib/kernel/seccomp`:
`verify` (Linux's `bpf_check_classic` and `seccomp_check_filter`, rule for
rule, with the scratch-store pass of `check_load_and_stores`), `Program`,
`run` (and `run_counted`), `run_all`, `SeccompData`, the actions and
`more_restrictive`; `forbid(unsafe_code)`; no allocation in the run path.
Evidence: 23 host tests, a `verify` refusal or acceptance per rule and per
opcode, each of the eight rules; `tests/agree.rs`, which holds `verify` to a
second checker (`tests/naive/`, written by a different method) over 300,000
seeded programs; `tests/chrome.rs`, which runs three of Chrome 151's filters
(`tests/data/chrome-<n>.bpf`, extracted by `tests/extract.py`) against the
answer the real kernel gave each of the calls 0 to 449 (`chrome-<n>.verdicts`,
made by `tests/oracle.c` on nazuna); and the fuzz target
`seccomp_verify_run`, which holds the same two checkers to each other on any
bytes, and a verified program's run to its length and to the data. The one difference from the real kernel: x86-64 `uretprobe` (335) and
`uprobe` (336) pass through Linux's seccomp unfiltered, and Ferrix, with no
probes, lets a filter judge them like any number it does not know (stricter).
Nine of
Chrome's processes installed three distinct programs, of 657, 661 and 662
instructions.

**S1's review (certification consultant os-ad, 2026-10-01, after the fact:
63b0e70c and 5fd9c1fb landed without one): OK with conditions.** Q3's terms
are met: `forbid(unsafe_code)`; no allocation in `run` or `run_all`, only
`verify`'s, through `try_reserve_exact`; `run` is total for a verified
program, its loop bounded by the program's length, shifts masked, a division
by an `X` of zero answering `KILL_THREAD` as Linux does; a host test for every
rejection but `Invalid::NoMemory`; the fuzz target. Nothing in `src/kernel`
uses the crate yet, so none of the following is reachable; each binds S2 or
S3, and S3 does not land without them.

* **`MAX_INSNS_PER_PATH` is `1 << 18`** (`lib.rs:44`), eight times Linux's
  `(1 << 18) / sizeof(struct sock_filter)`, 32768, which §6 promises. Fix it,
  enforce it at install with 4 more for each filter, and test the limit
  (S3).
* **`check_scratch` carries nothing from a `RET` to the next instruction**
  (`verify.rs:327`), where Linux's `check_load_and_stores` carries the stored
  words on. So `0 JEQ jt=2 jf=0; 1 ST M[0]; 2 JA 1; 3 RET; 4 LD M[0]; 5 RET A`
  passes here and is refused by Linux. Not a safety matter, since scratch
  words start at zero, but §3.1 rule 7 claims Linux's refusals. Fix both
  checkers (`tests/naive/` makes the same choice, which is why `agree.rs`
  cannot see it), with Linux's answer recorded by `oracle.c` (S3).
* **`run_all` of no filters answers `ALLOW`.** Linux answers `KILL_PROCESS`
  for a thread in filter mode with no filter. The hook must never run an
  empty chain in filter mode, or must answer that case with a kill (S3).
* **The per-call bound** -- 32768 steps, and the measured cost of a step --
  goes into `docs/certification/MEMORY-AND-TIMING.md` under AoU-4 (S2).
* **The chain walk** handed to `run_all` allocates nothing, takes no sleeping
  lock and runs under no lock (S3).
* Miri runs the crate's unit tests only (`--lib`), not `agree.rs` or
  `chrome.rs`; out-of-range jumps are tested for `JEQ` alone of the
  conditional jumps. Notes, not conditions.

**S1's conditions, the crate's part (os-7c, `stage13-s1b`, 2026-10-01).**
Built ahead of S3, since none of it needs the kernel:
* `MAX_INSNS_PER_PATH` is `(1 << 18) / 8`, 32768, and the crate states the
  rule S3 enforces: `fits_path(earlier, new_len)` is Linux's refusal
  (`total_insns > MAX_INSNS_PER_PATH`, the new filter's length against every
  earlier filter's length and four) and `path_cost` what the chain then counts.
  A host test pins it: seven 4096-instruction filters fit and an eighth does
  not; 6,554 one-instruction filters fit and a 6,555th does not; exactly at the
  limit passes, one over does not.
* `check_scratch` is `check_load_and_stores` line for line: one running set of
  the stored words, never reset at a `RET`, narrowed by the jumps that reach an
  instruction, made everything after an unconditional jump. The naive checker
  states the same rule as the edges into each instruction. Six small programs
  in `tests/data/scratch-<n>.bpf` carry the answer of the real kernel (Linux
  7.0 on nazuna, `oracle.c --accept`): programs 1, 2 and 6 are refused with
  `EINVAL`, 3, 4 and 5 are accepted, and `tests/scratch.rs` holds `verify` and
  the naive checker to each. S1 accepted 1 and 6.
* `run_all` of no filters answers `KILL_PROCESS`, as Linux's
  `seccomp_run_filters` does for `WARN_ON(f == NULL)`; the hook's own body
  still never runs an empty chain, which S3 checks.


**S2 built (2026-10-01, os-7c, `stage13-s2`).** The hook of §3.3, and
nothing that filters yet. `trap::set_syscall_filter` and
`trap::filter_system_call` are registered like `set_syscall_entry`;
`SyscallArgs` gained `ip`, the instruction after the call (so the hook
takes `&SyscallArgs` alone, where §3.3's first sketch passed the pointer
beside it); each of the four entries -- x86-64 `SYSCALL` and `int $0x80`,
AArch64 `svc`, ARMv7-A `svc` -- fills `ip` from its saved program
counter and asks the filter first, before its early answers and before the
native range is split, applying the answer as the dispatcher's. The x86-64
`SYSCALL` entry was split (`answer_here`, `enter_program`) to stay under
the complexity floor and nothing else about it changed.

**The consultant's review (2026-10-01) and what it changed.** The hook and
its placement were accepted. The first form of the verdict, `Answer(Outcome)`,
was refused: `Outcome::Enter` would have let one bug in the load turn a
`getpid` into an `execve`'s jump (and on x86-64 into compat mode), and
`Return(isize)` was unclamped. The built `Verdict` is `Continue` or
`Errno(u32)` and has no variant that carries an `Outcome`; the core builds
the answer in `trap::ask`, clamping the errno to 4095. The hook is
entered and left with interrupts masked; a body that runs a chain opens them
for the walk and closes them again (`MEMORY-AND-TIMING.md` §2.2b has the
bound, 32,768 steps, and what a step costs measured in the guest: 27.2 ns
on x86-64, 25.8 ns on AArch64, 60.2 ns on ARMv7-A, so at most 0.9, 0.8 and
2.0 ms a call). ARMv7-A, which had no requirements, has
`L.armv7a.1` and `L.armv7a.2` (`docs/sysml/21-armv7a-requirements.sysml`),
and `L.trap.7`'s two claims that nothing exercised -- nothing registered is
`Continue`, the first registration stands -- are checked on a slot of the
check's own. The coverage anchor carried across the hook and dropped as
unmeasured is `enter_compat_after_execve(entry, stack)` in
`arch/x86_64/syscall.rs`, line 543 before this landing, which is now inside
`enter_program`; the next `cargo xtask coverage` measures it.

**Deviations from the design, accepted by the consultant and recorded.**
* The hook is `fn(&SyscallArgs) -> Verdict` with `ip` a field of
  `SyscallArgs`, not a second parameter, and `filter_system_call` answers
  `Option<Outcome>`.
* The boot check's probe, a test seam that shows what a filter was shown and
  which a filter program cannot say, stays after S3, so a thread with no
  filter pays the registration's load and the probe word's, where §3.3 prices
  the first alone.
* `NATIVE_ARCH` (`0xC000_0F1F`) carries the 64-bit flag on ARMv7-A too, where
  no native register is 64 bits wide.
* SR2 is stricter than Linux: a number with bits above the 32nd is dispatched
  as no call, where Linux masks it and runs the low half. `docs/BACKLOG.md`
  has the row.
* `FX-1302` sits in `catalog::ALL` in code order, not in numeric order beside
  `FX-1301`; the generated page sorts.

Evidence: the `seccomp` boot line (FX-1302, `syscall/seccomp_check.rs`),
which drives each entry of the architecture through
`arch::drive_system_call` with frames of its own -- `linux::handle` would
skip the hook -- and requires every call an entry answers itself
(`arch_prctl`, `set_tls`, `sigreturn`, `rt_sigreturn`) to reach the filter
once, before that answer, with its number, instruction pointer and first
argument as the frame held them, and the filter's value to come back
(SR3); the token to be the entry's, worked out from `e_machine` and not
from the kernel's own table, so that an `int $0x80` call is i386's and a
filter written for x86-64 alone does not judge it (SR1); a number with
bits above the 32nd to be judged as its low half and dispatched as no call
(SR2); a native-range call to carry the native token, refused by a
filter that refuses every foreign `arch` and let through by one that
allows that token by name (Q2, both ways); and the core to cut an errno
to 4095 and to keep 512 an errno. A second line reads what the hook costs.

**The consultant's verdict: OK if re-gated.** Its first pass found every
earlier log older than the final tree, so the landing was conditional on
`cargo xtask check`, the three boots and `test-threads --arch all` run again
on the landing's own commit, each log recording it. That is met: the
`s2f-check`, `s2f-x86`, `s2f-a64`, `s2f-arm` and `s2f-thr` logs on nazuna
start `gate commit: bc9aaa38` and end with exit 0 (the three boots
`FERRIX-BOOT-OK stages 1-12`); the rebase onto the `main` of 2026-10-01 18:40
touched no kernel, library or boot file, so the gate stands.

**Gates run before that** (`stage13-s2`, side ref `os7s/s2`, nazuna):
`cargo xtask check` exit 0; `test-boot` on x86-64, AArch64 and ARMv7-A at
`--smp 2` each `FERRIX-BOOT-OK stages 1-12`; `test-threads --arch all` and
`test-init --arch all` the same; `test-shell --arch all` as the landing
message says. Negative controls, each a throwaway branch with one sabotage and
a `NEGATIVE CONTROL` line in the boot log, each stopping the boot with the
check's own message:

| Control (sabotage) | Boot | Message |
|---|---|---|
| the SYSCALL entry skips the filter for `arch_prctl` | x86-64 | `arch_prctl was answered before the filter` |
| the svc entry skips the filter for `rt_sigreturn` | AArch64 | `rt_sigreturn was answered before the filter` |
| the svc entry skips the filter for `set_tls` | ARMv7-A | `set_tls was answered before the filter` |
| `int $0x80` carries x86-64's token | x86-64 | `an int 0x80 call was filtered as x86-64` |
| the SYSCALL entry cuts the number to 32 bits | x86-64 | `a number with bits above the 32nd set was dispatched as a call` |
| native-range calls get no native token | x86-64 | `a native call was not filtered under the native token` |
| the core does not clamp an errno | x86-64 | `the core let a filter's errno out of 0 to 4095 through` |
| an empty slot answers every call | x86-64 | `a call was answered with no filter registered` |
| a registration into a slot of the check's own registers nothing | x86-64 | `a later registration replaced the first, or the first was not asked` |

**S3 built (2026-10-01, os-7c, `stage13-s3`).** Filters. `seccomp(2)` is in
the four tables (317, 354, 277, 383, each read from the headers) and
`prctl(PR_SET_SECCOMP)`, `PR_GET_SECCOMP`, `GET_ACTION_AVAIL` and
`GET_NOTIF_SIZES` answer as Linux's `do_seccomp` does, in its order: the flags
before the program is read (a NULL program is `EFAULT`, Chromium's probe), the
length, the privilege rule (no-new-privs or privilege, else `EACCES`), the
verifier, the mode, the chain's bound. A `Filter` is an `Arc` of a verified
program that points at the one before it; a thread holds the newest in a
`State` behind a leaf lock of its own, with a `filtered` flag and a
machine-wide flag that no thread has ever held one, so that until one has,
no call looks for its thread. A fork child takes its parent's chain, a thread
its creator's, a native child its creator's through `launch::load_native`
(`inherit_native`); `execve` keeps it, as it keeps the thread. The hook runs
the chain newest first, with interrupts open and under no lock, allocating
nothing, and the strictest answer wins, the newest filter's data on a tie.
`ALLOW`, `ERRNO` (the core cuts it to 4095, so 512 is an errno and never a
restart: SR7), `KILL_THREAD` (the process's last live thread: the process, by
`SIGSYS`; any other: that thread alone, which leaves through `exit_thread_current`
with status 128 + `SIGSYS`, so that a leader killed this way while another
thread lives is what `wait` reports for the process when the last one leaves),
`KILL_PROCESS`, `LOG` (one rate-limited line on the console, not
the audit log, Q4), strict mode, `TRACE` and `USER_NOTIF` (`ENOSYS`: no
tracer, no listener, SR13), and an action nobody defined (a kill) are built; a
`TRAP` is a kill until S4. A thread in filter mode with no filter, which
nothing can make, is a kill and not "allow" (the consultant's S1 condition 3);
the chain's bound is the crate's `fits_path` with four more counted for each
filter (condition 1, `ENOMEM` past 32,768). A filter is charged to the job that
installed it before its program is copied (F-37, SR12) and released by a walk,
never by recursion, when the last thread and child holding it goes (SR11).
A native child's first state is *kept* on its process and cloned by each first
thread made for it, not taken: a start that is refused (a bad argument handle,
no memory for the thread) and made again makes a new first thread, which
must start filtered too (the consultant's B1; taking it let a filtered
process run an unfiltered child by `process_start` with a bad handle and then
a good one). Installing needs no-new-privs or `CAP_SYS_ADMIN` in the caller's
own user namespace (`holds`, as Linux's `ns_capable`). The flags a thread
publishes are written when they change, not on every call, and reads of its
state take the leaf lock and store nothing.
`/proc/<pid>/status` has `NoNewPrivs`, `Seccomp` and `Seccomp_filters`.
`TSYNC` is refused `EINVAL` until S5.

Evidence: the `seccomp` boot line of FX-1303
(`syscall/seccomp_filters_check.rs`, a load-ring file listed in
`certification-item.json`). A task that is a real thread of a check process
installs filters through `seccomp(2)` and `prctl` as a program does and makes
the calls they judge through the core's own entry (`arch::drive_system_call`),
which on the Arm pair then ends a thread whose process was ended, as the
vector does and a driven call skips. It requires: Chromium's probes answered
as Linux answers them; `EACCES` for uid 1000 without no-new-privs and success
with it; `getppid` failed `EPERM`, `clone3` `ENOSYS`, `getpid` run; an older
`ERRNO` under a newer `ALLOW` still `ERRNO`, the newest data on a tie;
`ERRNO|512` coming back `-512` and `ERRNO|5000` `-4095`; `TRACE` and
`USER_NOTIF` `ENOSYS`; `LOG` letting the call go on; a fork child, a thread
and a native child each refused the call their creator's filter refuses, with
`Seccomp: 2` and `Seccomp_filters: 2` in their `status`; a killed thread's
process ending by `SIGSYS` (a `KILL_PROCESS` the same, a thread in strict mode
by `SIGKILL`); a thread in filter mode with no filter ended; the longest chain
Linux allows, 6,554 one-instruction filters, made and released outside the
thread's lock; a native child whose start was refused and made again still
filtered, with its creator's no-new-privs; one thread of a process killed by its
filter, alone (the process lives and the others' calls run), and the first
thread killed while another lives, the process ending by `SIGSYS` when the
other leaves; and, in the `kmem` line, filters made until a job's memory limit refused one `ENOMEM`,
a sibling job made one, and both read zero after. Each control below is a
throwaway branch with one sabotage and a `NEGATIVE CONTROL` line, and stopped
the boot with the check's own message:

| Control (sabotage) | Message |
|---|---|
| the program is read before the flags are checked | `an unknown seccomp flag was answered EFAULT` |
| the no-new-privs rule is not applied | `an unprivileged filter was installed without no-new-privs` |
| the hook never learns a thread holds a filter | `a filtered getppid ran` |
| the chain keeps the oldest filter's answer | `the newest filter's data did not stand on a tie` |
| a fork child's thread starts with no filter | `a forked child of a filtered process was not filtered` |
| a clone's thread starts with no filter | `a thread of a filtered process was not filtered` |
| a native child is given no filter | `a native child of a filtered process was not filtered once its start was refused and made again` |
| a killed last thread ends its process as exit 0 | `a killed thread's process exited otherwise than by SIGSYS` |
| an ERRNO of 512 is answered 4 | `a filter's ERESTARTSYS restarted the call` |
| strict mode allows every call | `strict mode let getpid run` |
| the chain has no length bound | `a chain grew past what Linux allows` |
| a filter is charged nothing to its job | `kmem: a job made more than its limit could hold` |
| a thread in filter mode with no filter is let go on | `a thread in filter mode with no filter was let go on` |
| a chain is released by the default recursive drop | `a chain's release was recursive, nested as deep as the chain is long` |
| an ERRNO of 5000 is answered 7 | `a filter's errno was not cut to 4095` |
| an action nobody defined lets the call go on | `an action nobody defined did not end the process` |
| a native child's first state is taken, not kept | `a native child of a filtered process was not filtered once its start was refused and made again` |
| a thread killed by its filter leaves with no status | `a killed leader's process did not end by SIGSYS` |
| a killed thread always ends its whole process | `a thread killed by its filter took its whole process with it` |

What a call costs the worst chains, read in the guest (`seccomp` line): the
6,554-filter chain, 401 us a call; seven 4,096-instruction filters, the most
steps there can be, 732 us; releasing the long chain, 8.1 ms, which in
production can be the reaper's last drop with preemption off and is the one
cost S3 adds there (a BACKLOG row). `MEMORY-AND-TIMING.md` §2.2b has them.

What stands on the code and not on a check, each a BACKLOG row: `execve`
keeping the chain (no boot check runs an `execve`; S6's guest program does:
bubblewrap's path), and `LOG`'s line (read by hand in the boot log). The
several-thread `KILL_THREAD` is a check now, with started threads.

**S4 built (2026-10-01, os-7c, `stage13-s4`).** `TRAP`, after the
consultant's design note (os-ad, 2026-10-01: "OK to build as described", with
three conditions, each met below).
* **The core.** `Verdict` gains `Trap`, which carries nothing. `trap::ask`
  answers it with `arch::syscall_rollback_value` of the call as the entry read
  it, the S2 function: the number for `RAX`/`EAX` on x86, the original first
  argument for `x0`/`r0` on the Arm pair, where the number stays in `x8`/`r7`.
  A filter chooses no value, and the value is one taken from the caller's own
  registers, so nothing of the kernel's can reach a program's return register
  this way. `L.trap.8` states it (reserved on `main` first), and the `seccomp`
  line of FX-1302 checks it with a slot of its own for first arguments `0x1111`,
  -512, -516 and -1. The core changes in `trap.rs` alone.
* **The personality.** `syscall::seccomp::trap` forces `SIGSYS` on the
  running thread through `deliver::force` (`signal::force`: unblocked, and
  reset to the default if it was ignored or blocked, as Linux's
  `force_sig_info_to_task` does, so a program that blocks `SIGSYS` dies of a
  trap rather than handling it), as `Origin::Sys` with
  `si_code` `SYS_SECCOMP`, `si_errno` the filter's data, `_call_addr` the
  instruction after the call, `_syscall` the number and `_arch` the entry's
  token. The signal is queued before `trap::ask` returns, so the return path of
  this very call delivers it, from the rolled-back frame, before any other
  instruction of the program runs. If the signal is fatal at once (no handler,
  so the default action) or there is no thread, the process is killed through
  `process::kill`, which a process already ending takes as a no-op: a thread
  being killed gets no second, conflicting delivery.
* **Native calls fail closed** (condition 2). A `TRAP` for a native-range call
  (`arch` `NATIVE_ARCH`) is `KILL_PROCESS`: `SIGSYS` is a Linux signal the
  native ABI cannot express. A native process with no Linux signal state never
  reaches the filter at all, since the hook finds no personality thread.
* **Restart codes** (condition 3). The rollback can put a value that reads as
  `ERESTARTSYS` (-512) back in `x0`/`r0`, the program's own first argument.
  It is written as the return value through `Outcome::Return` and never
  passes the dispatcher, which alone marks a call for restart
  (`mark_restart`); the filters check traps a call whose first argument is
  -512 and requires no restart marked.
* **i386.** `_sigsys` is a pointer and two `int`s, so the 32-bit layout is not
  the 64-bit one shifted: `sigframe32::siginfo_from_64` converts this origin
  field by field (`_call_addr` at 12, `_syscall` at 16, `_arch` at 20), with
  a host test.
* **`GET_ACTION_AVAIL`** answers 0 for `TRAP` now; `USER_NOTIF` stays
  `EOPNOTSUPP`.
* **Speculation** (§3.9, SR14). `PR_GET_SPECULATION_CTRL` answers
  `PR_SPEC_PRCTL | PR_SPEC_FORCE_DISABLE` (9) for store bypass and indirect
  branches and `ENODEV` for any other feature; `PR_SET_SPECULATION_CTRL`
  accepts a request for more mitigation, refuses `PR_SPEC_ENABLE` with
  `EPERM`, a value that is no control with `ERANGE`; `SPEC_ALLOW` turns
  nothing off.

Evidence: FX-1302's core case (above) and FX-1303's: a `TRAP` in a thread with
a handler (unblocked, as Chromium installs it) traps the call and leaves
`SIGSYS` pending and deliverable, with an `Origin::Sys` whose bytes are at
Linux's offsets on this machine and, on x86-64, at i386's after the
conversion; no restart is marked, for a first argument of -512 too; with
`SIGSYS` blocked and ignored a trapped call ends the process by `SIGSYS`; a
trapped native-range call ends the process though a handler is installed; and
the speculation answers above. The frame a handler sees, its write into the
context and `SA_NODEFER` are S6's guest program on four ABIs. Negative
controls, each through `fleet/gate.sh control --expect` (the INDEX lines are
in the landing's message):

| Control (sabotage) | Message |
|---|---|
| a trapped call answered -38 instead of rolled back | `SIGSYS's context lost the syscall number` |
| the i386 conversion off for a trap | `i386 si_syscall was not at offset 16` |
| `SIGSYS` posted, not forced | `a blocked and ignored SIGSYS let a trapped call return` |
| `PR_SPEC_ENABLE` accepted | `PR_SET_SPECULATION_CTRL enabled a mitigation` |
| the trap's data left out of `si_errno` | `a trapped call's siginfo did not carry the call, its ip and its arch` |
| a native-range trap raising `SIGSYS` | `a trapped native-range call returned` |

**S5 built (2026-10-01, os-7c, `stage13-s5`).** `TSYNC` and `TSYNC_ESRCH`
(§3.7). `seccomp(SET_MODE_FILTER, TSYNC)` makes and charges the filter, then
takes the process's thread-list lock; under it an `execve` that has claimed
the process answers `EAGAIN`, every other live thread is checked under its
leaf lock (no seccomp passes; a chain that is an ancestor of the caller's
passes, Linux's `is_ancestor`; strict mode or a chain of its own fails the
whole call with that thread's id, or `ESRCH` with `TSYNC_ESRCH`, and no thread
changes), and then the filter is attached to the caller as `attach` does and
every other thread's chain becomes the caller's new one. The list of threads
and the room for the chains they give up are made before the lock, and the
given-up chains are dropped after it, so nothing is allocated or freed under
it. `TSYNC_ESRCH` without `TSYNC` is `EINVAL`.

The race SR10 names is closed where the design put it: a thread made by
`clone(CLONE_THREAD)` no longer copies its creator's chain when the `Thread`
is built (the consultant's note on S5: `Thread::sibling` copied it outside the
lock) but in `Process::add_thread_from`, under the thread-list lock, as it is
listed. A thread being made during a sync is therefore either listed already,
and reached by the sync, or copies its creator's chain after the sync gave it
the new one.

Evidence: the `seccomp` line of FX-1303. On thread objects: a thread made
before any filter and one made after the first both take the second filter by
`TSYNC`, the first with the whole chain; a thread with a chain of its own makes
`TSYNC` answer its id and `TSYNC|TSYNC_ESRCH` answer `ESRCH`, with every
thread's chain unchanged; `TSYNC_ESRCH` alone is `EINVAL`. With running
threads: eight threads, and a ninth that makes forty more one after another, a
`TSYNC` from the first thread in the middle of that, and then every thread of
the process -- 49, the forty included -- refused the call the filter refuses.
Negative controls, each through `fleet/gate.sh control --expect`:

| Control (sabotage) | Message |
|---|---|
| the creator's chain read before the thread is listed, with a 300 us delay between | `a thread made during TSYNC was not filtered` |
| every filtered thread counted as an ancestor | `a TSYNC blocked by a thread with a chain of its own did not answer its id` |
| `TSYNC_ESRCH` ignored | `a blocked TSYNC with TSYNC_ESRCH did not answer ESRCH` |
| the other threads' chains left as they were | `a thread was left without the chain TSYNC gave` |

What stands on the code: `EAGAIN` for an `execve` in progress (no boot check
claims an exec), a BACKLOG row. The walk of each other thread's chain for
`is_ancestor` runs under the thread-list lock and is bounded by the chain's
length (6,554), a cost recorded in BACKLOG.

S6 (`test-seccomp`) follows.
