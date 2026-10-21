# Namespace and cgroup follow-ons  ·  *unsized*

What [stage 13](stage-13-namespaces-cgroups-v2-seccomp.md) was first written to
hold and does not, since the customer set its scope on 2026-10-21 to what is on
`main`: the seven namespaces, cgroups v2 with `cpu`, `memory`, `io` and `pids`
as built, seccomp, Syscall User Dispatch and `test-container`. Each item below
was checked against `main`'s code on that day and is not built. None is
counted in the sized scope.

| Item | What `main` shows | Where the work is |
|---|---|---|
| Time namespaces | `CLONE_NEWTIME` is refused | branch `stage13-timens` (6 of 9 commits not on `main`) |
| Unprivileged mounting (N5), `test-bwrap` as uid 1000 | `may_mount` asks `CAP_SYS_ADMIN` only | branches `stage13-n5` (15 commits), `stage13-bwrap-user` (16); Steam's N5 to N7 (stage 22) stand on it |
| `memory.reclaim` | no code | L.object.182-183 reserved for the memory controller's leftovers |
| `io.weight` and `io.latency` | `fs/blkio.rs` says "Not built" | L.object.184-187 reserved with `cpu.max.burst` and `cpu.idle` |
| `cpu.max.burst` and `cpu.idle` | no code | L.object.184-187 reserved |
| Reclaim of the dentry and inode caches | not built | `docs/CGROUPS.md` §7 |
| `anon` and `pagetables` in `memory.stat` | no match in the code | `docs/CGROUPS.md` |
| Linux's own `seccomp_bpf` selftest | no gate | a BACKLOG row, `docs/SECCOMP.md` §8.2 |

The ids L.object.182-187 stay reserved on `main` and are written with the
code. The takeover guide for the branches is
[stage-13-handover.md](stage-13-handover.md).
