# Namespace and cgroup follow-ons  ·  *unsized*  ·  built 2026-10-26, landing owed

What [stage 13](stage-13-namespaces-cgroups-v2-seccomp.md) was first written to
hold and does not, since the customer set its scope on 2026-10-21 to what is on
`main`: the seven namespaces, cgroups v2 with `cpu`, `memory`, `io` and `pids`
as built, seccomp, Syscall User Dispatch and `test-container`. Each item below
was checked against `main`'s code on that day and is not built.
`memory.reclaim`, `cpu.max.burst`, `cpu.idle`, `io.weight`, `io.latency`, and
`anon` and `pagetables` in `memory.stat` were built since, on 2026-10-26
(`docs/CGROUPS.md` §10, §12 and §13, L.object.182 to 185), and so, the same
day, was every row below, on the branch `claude/ferrix-roadmap-completion-hkxlpp`: **all of
stage 13's follow-ons are built.** None is counted in the sized scope.

What each still owes before it lands on `develop` is the gate host's, which
this container cannot reach: the `gate.sh control --expect` runs of the
negative controls (each fired here with `test-boot`), the gate rows on the
landing hash with the accelerator named, the reviews' ledger lines copied
to the gate host's ledger, `test-vfs` and `test-init` (busybox), and
`test-bwrap` as uid 1000 (bubblewrap is fetched from deb.debian.org). Each
commit names its review and what it owes.

| Item | Done | Where |
|---|---|---|
| ~~Time namespaces~~ | ✓ 2026-10-26, ported from `park/stage13-timens` (0400bdbe; review OK IF) | `docs/NAMESPACES.md` §12.1 |
| ~~Unprivileged mounting (N5), `test-bwrap` as uid 1000~~ | ✓ 2026-10-26, ported from `park/po6/n5` and `park/po6/bwrap-user` (a6706f86; review NOT YET, then OK IF, with a hole the first review missed fixed); `test-bwrap` as uid 1000 owed on the gate host | `docs/NAMESPACES.md`, "N5's port"; Steam's N5 to N7 (stage 22) stand on it |
| ~~Requirements for the files of `cpu.max.burst` and `cpu.idle`~~ | ✓ 2026-10-26: the files' refusals are the load ring's and claim none (the consultant's ruling, ledger line 10); the item's one refusal, the native weight an idle job refuses, is L.object.187 | `docs/CGROUPS.md` §12 |
| ~~Reclaim of the dentry and inode caches~~ | ✓ 2026-10-26 (6e965ee2, L.object.186; review OK IF) | `docs/CGROUPS.md` §10 |
| ~~Linux's own `seccomp_bpf` selftest~~ | ✓ 2026-10-26, `cargo xtask test-seccomp-selftest` (e57f6068), passing on all three architectures | `docs/SECCOMP.md` §8.2 |

No id stays reserved for them: L.object.182 to 187 are written. The takeover guide for the branches is
[stage-13-handover.md](stage-13-handover.md).
