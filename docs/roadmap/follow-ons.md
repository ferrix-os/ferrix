# Namespace and cgroup follow-ons  ·  *unsized*

What [stage 13](stage-13-namespaces-cgroups-v2-seccomp.md) was first written to
hold and does not, since the customer set its scope on 2026-10-21 to what is on
`main`: the seven namespaces, cgroups v2 with `cpu`, `memory`, `io` and `pids`
as built, seccomp, Syscall User Dispatch and `test-container`. Each item below
was checked against `main`'s code on that day and is not built.
`memory.reclaim`, `cpu.max.burst`, `cpu.idle`, `io.weight`, `io.latency`, and
`anon` and `pagetables` in `memory.stat` were built since, on 2026-10-26
(`docs/CGROUPS.md` §10, §12 and §13, L.object.182 to 185). None is
counted in the sized scope.

| Item | What `main` shows | Where the work is |
|---|---|---|
| Time namespaces | `CLONE_NEWTIME` is refused | branch `stage13-timens` (6 of 9 commits not on `main`) |
| Unprivileged mounting (N5), `test-bwrap` as uid 1000 | `may_mount` asks `CAP_SYS_ADMIN` only | branches `stage13-n5` (15 commits), `stage13-bwrap-user` (16); Steam's N5 to N7 (stage 22) stand on it |
| Requirements for the files of `cpu.max.burst` and `cpu.idle` | built and checked by stage 13's `cpu` line (`burst_files`, `idle_files`) and host tests, named by no requirement | an id to reserve on `develop` (the consultant's conditions, 2026-10-26) |
| Reclaim of the dentry and inode caches | not built | `docs/CGROUPS.md` §7 |
| Linux's own `seccomp_bpf` selftest | no gate | a BACKLOG row, `docs/SECCOMP.md` §8.2 |

No id stays reserved for them. The takeover guide for the branches is
[stage-13-handover.md](stage-13-handover.md).
