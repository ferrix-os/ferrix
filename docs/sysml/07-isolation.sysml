/*
 * Ferrix — SysML v2 model, part 7: isolation.
 *
 * docs/ARCHITECTURE.md §6: namespaces, cgroups v2, seccomp, credentials.
 * Stage 13, designed in from the start so that no global table has to be
 * found later. The credentials exist since stage 7, and cgroup v2 since
 * 2026-09-23 (docs/CGROUPS.md), over the job tree. Of the namespaces,
 * mount namespaces are in (docs/NAMESPACES.md, N1 to N3): unshare and clone
 * copy one, and bubblewrap runs as root. The other kinds still answer as a
 * Linux built without them does, and setns refuses. Seccomp is not built.
 */
package FerrixIsolation {
    private import ScalarValues::*;
    private import FerrixLifecycle::*;

    enum def NamespaceKind {
        Pid; Mount; Uts; Ipc; Net; User; Cgroup; Time;
    }

    #planned part def Namespace {
        @stage { number = 13; }
        attribute kind : NamespaceKind;
        part parent : Namespace[0..1];
    }

    #planned part def NsSet {
        @stage { number = 13; }
        doc /* One namespace of each kind, held by every task. Every table
             * that would otherwise be global — pids, mounts, hostname, ... —
             * is reached through this, from the first line. */
        ref part namespaces : Namespace[8];
    }

    #inProgress part def Namespaces {
        @stage { number = 13; }
        doc /* Steam's user and mount namespaces first (docs/NAMESPACES.md,
             * N1 to N6). N1 (2026-09-28): every mount's own flags enforced,
             * MS_REMOUNT, and mountinfo. N2 (2026-09-30): binds of
             * directories, files and sockets, MS_REC, MNT_DETACH of a
             * subtree, and a superblock per filesystem. N3 (2026-09-30):
             * mount namespaces, copied by unshare and clone, pivot_root, a
             * native child kept in its creator's, and openat2's resolve
             * flags. N4, user namespaces, is next. Network namespaces
             * (docs/NETNS.md, built on stage13-netns, not landed): a stack
             * per namespace, a down loopback, veth pairs, devices that move
             * with their ring, CAP_NET_ADMIN and CAP_NET_RAW over the owner. */
        part every : Namespace[8..*];
        action unshare;
        action setns;
    }

    enum def CgroupController {
        Cpu;      // weight and bandwidth handed to the scheduling classes
        Memory;   // where reclaim and the OOM killer are scoped
        Io;       // per-cgroup bandwidth in the I/O scheduler
        Pids;
    }

    #inProgress part def Cgroup {
        @stage { number = 13; }
        attribute controllers : CgroupController[0..*];
        part children : Cgroup[0..*];
        attribute memoryLimit : Natural[0..1];
        attribute cpuWeight : Natural[0..1];
        attribute pidsMax : Natural[0..1];
    }

    #inProgress part def Cgroups {
        @stage { number = 13; }
        doc /* One unified hierarchy (v2), exposed as cgroupfs. cpu is not a
             * separate mechanism: it is bandwidth and weight handed to the
             * scheduling classes. memory scopes reclaim and the OOM kill.
             *
             * Built (docs/CGROUPS.md, 55 of its 85 points): every cgroup a
             * Job, cgroupfs mounted as cgroup2, cgroup.events, clone3 into a
             * cgroup, delegation by chown; pids, memory's charging and its
             * scoped OOM kill, and cpu.weight, over the certification's job
             * quotas. Left: the rest of memory.stat, memory's reclaim,
             * freezing, cpu.max and io. */
        part root : Cgroup;
    }

    #planned part def Seccomp {
        @stage { number = 13; }
        doc /* Classic BPF filters evaluated on syscall entry. The interpreter
             * is a pure function over bytes in src/lib/, fuzzable and Miri-able. */
        part interpreter : ClassicBpfInterpreter;
        action installFilter;
        action evaluate;
    }

    #planned part def ClassicBpfInterpreter {
        @stage { number = 13; }
        doc /* src/lib/kernel/seccomp, owed before seccomp is built. */
    }

    #implemented attribute def Credentials {
        @stage { number = 7; }
        doc /* Unix: uid, gid, supplementary groups, POSIX capability sets,
             * no-new-privs. They sit on top of the handle system rather than
             * beside it.
             *
             * src/kernel/src/syscall/credentials.rs: Linux's four user ids and
             * four group ids and the supplementary groups on every Process,
             * copied by fork, kept by execve with the saved and filesystem
             * ids made the effective ones, and changed by the set*id calls
             * under kernel/sys.c's rules. They are enforced: the VFS checks
             * file permissions against the filesystem ids, and the calls
             * only root may make, and those that reach another user's
             * processes, refuse anyone else. A set-user-id program runs as
             * its file's owner unless PR_SET_NO_NEW_PRIVS forbade it. */
        attribute uid : Natural;
        attribute gid : Natural;
        attribute groups : Natural[0..*];
        #planned attribute capabilities : String {
            doc /* No capability sets are kept: an effective uid of 0 stands
                 * for every capability, capget reports every set empty for
                 * any other, and capset refuses a process whose real or
                 * saved uid is 0 and whose effective uid is not. */
        }
        attribute noNewPrivs : Boolean;
    }
}
