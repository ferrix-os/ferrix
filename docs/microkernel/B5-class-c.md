# B5's class C: what leaves with the personality, row by row

Ledger 5782 (4)-(7), on `docs/microkernel/B5.md`. These ids stay on `evidence-orphans.json`, marked, until
`personality=user` builds on `develop` and the step's check of the attribute lands: then each requirement
gains `attribute applies_when = "personality=kernel"`, the step fails an id so marked whose unit is still
compiled into a `personality=user` image, or whose parent or child is unscoped (5782 (5), (6)).

## Accepted, each with its successor (5782 (7))

| Id | In `fast` and `max` the behaviour is | Successor |
|---|---|---|
| H.TRAP.9 | signal frames written by `linuxd` | `test-linuxd`'s signal row (mk-lift), K1's context check; children L.x86_64.42, 45, 46, 174, 209, 210 and L.aarch64.9 scoped with it |
| L.x86_64.122 | `XSTATE_BV` before `XRSTOR` is still a ring-0 hazard, on K1's image | K1.md §5.2's refusal and its id. K4's `from_supervisor` shares the functions: those stay compiled in, outside the scope |
| H.TRAP.12 | the Linux decode is `linuxd`'s | `linuxd`'s dispatch, its rows; children L.x86_64.140, L.syscall.2, L.aarch64.4 scoped with it |
| L.x86_64.119 | as H.TRAP.12 | as H.TRAP.12 |
| L.x86_64.66 | a restart is `linuxd`'s on K1's context | `test-linuxd`'s stop and continue row |
| L.x86_64.63 | fork's child resumes in `linuxd`, on `SPACE_CLONE` and K1 | L.user.160 (K2's clone), `test-linuxd`'s fork row |
| L.x86_64.101 | `vdso_spec`'s image comes from `linuxd`'s build; `arch/` is core, so the source moves or stays compiled out | `linuxd`'s `vdso` row; **named at the flag's landing** |
| H.SCHED.9 | the read runs `linuxd`'s image over the kernel's time page. **L.sched.6 is core and stays**: re-parented to the time page's own requirement | `linuxd`'s `vdso` row |
| L.user.76 | K2 maps the vDSO for `linuxd` | K2's ceiling on a time-page handle with no `WRITE` (L.object.234, 236) |
| L.user.77, 130, 131 | vDSO placement, `linuxd`'s | `linuxd`'s `vdso` row |
| L.user.98 | `/proc/<pid>/maps` is `linuxd`'s | no kernel behaviour remains; `linuxd`'s procfs row |
| L.object.91 | execve's seal is `linuxd`'s protocol over K11 | K11's bootstrap rows |
| L.object.192 | the chardev core's HELLO is compiled out | no kernel behaviour remains in ring 0 |
| L.object.251, L.device.50 | `process_create` is compiled out | L.object.290 is the new call's own; none for `image_bytes` |

## Not class C: to be split (5782 (4))

| Id | Split |
|---|---|
| H.DMA.10 | a security rule whose children L.device.44-48 are core and stay: the publishing half scoped with L.object.192, the rest verified from a core check (class A) |
| L.syscall.20, 23 | `end_record`, `has_end`, `sched::work::post`, `notify` have core callers: the kernel half is class B on `Bare`, the personality half scoped |
| L.syscall.21, 22 | `post_to_tasks`, `post_own`: the same split, unless a build shows they compile out |
| L.x86_64.69 | restated to `enter_program`'s path: `ferrix_syscall_entry` stays |
| L.btrfs.120, 121, L.user.175 | `ferrix_btrfs_write` is linked by `btrfsd`, so these apply in `fast` and `max`: L.btrfs.121 and the crate half of L.btrfs.120 take host tests or a `btrfsd` row; the kernel's reclaim half and `/proc/meminfo`'s are scoped |

## Where it stands

* 2026-11-04: written; nothing scoped yet. The ids stay on the list until the flag builds on `develop`.
