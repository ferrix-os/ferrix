# Backlog detail: Verification audit: claims without evidence

Detail kept out of [`docs/BACKLOG.md`](../BACKLOG.md), which holds a short line per item linking here. Each item's text is as it stood in the backlog when it was split out (2026-10-16); add new findings here, under the item's id, and keep the backlog line short.

### va-001

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **N4 (d171ffe5): four of its ten negative controls have no log anywhere**: the range test narrowed, the writer's check, the chroot test, and set-id honoured in a namespace. Five more are in `logs/os7c/control-n4-*.log`, on the earlier tip 5bc2affc. The tenth, `privileged()` ignoring the namespace, was re-run by the certification consultant on be17d882 (`queue/osad-ctl-n4-privileged.log`: "kernel root inside a namespace it made set the host name"). Run each of the four on `main` with `gate.sh control --expect`, and keep the logs. No armv7a boot at four processors and no `build --arch all --release` ran on any N4 tip  **Closed 2026-10-07 (po10-findings):** the four controls ran on `main` 4066f41dd with `gate.sh control --expect`, each FIRED (panic) on its own message under `test-boot --arch x86_64`: `po10f-ctl-n4-range` (`range_to_kernel` asks only the first id; "a run spanning two of the parent's extents ... was accepted"), `po10f-ctl-n4-writer` (`write_map` drops the writer's `capable_over` over the parent; "a map opened by root was written wider by an unprivileged holder"), `po10f-ctl-n4-chroot` (`make_user_namespace`'s chroot test off; "a chrooted process made a user namespace"), `po10f-ctl-n4-setid` (`Credentials::exec` honours set-id bits in a child; "execve of a set-id file changed an id in a child namespace"), logs `~/.local/share/ferrix/logs/queue/<tag>.log`; `build --arch all --release` and the armv7a boot at four processors PASSED on the same `main` (batch 20261007T105031Z rows -3 and -10) | closed |

### va-002

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **318987db (programs under `src/user/system`): rows that ran only on a pre-rebase work in progress**. ferrousli's 770 debug and 770 release tests, and `test-shell`, `test-apps`, `test-init` and `test-badapple`, ran on bbe00b25 over 9da6c78d. The rebase then took `main`'s changes to moved files (`ferrousli/src/pthread.rs`, `tests/c_linux.rs`, the drivers, xtask's compositor). Re-run them on `main`. `docs/APPS.md` §10 says the rename was "gated by the full `check`", but that check, run with ferrousli, failed at `c_mount`'s 30-second limit; the line should say so  2026-10-05 (po5-c1): `test-shell` (zinc and ferrousli) and `test-init --arch all` ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -1, -5; `os7c-po-four-shell`); ferrousli's own tests (`check --ferrousli`), `test-apps --arch all` and `test-badapple` have not. `docs/APPS.md` went to the apps repository on 2026-10-03, so its sentence is that repository's to correct  **Closed 2026-10-07 (po10-findings):** the rest PASSED on `main` 4066f41dd: `check --ferrousli` with ferrousli's debug and release tests, none failed (`po10f-ferrousli`), `test-apps --arch all` (`po10f-apps`) and `test-badapple --arch x86_64` (`po10f-badapple`, the architecture 318987db ran it on) | closed |

### va-003

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **c5c92781 (F-55): its one behavioural claim ran only before its rebase.** `test-xwindow` (`f55-land/xwindow.log`), the armv7a boot at four processors and the release build all ran over caa9e3ea. The rebase to 9d56ea93 took `main`'s change to `panic/catalog.rs` and the generated files. Re-run `test-xwindow` and the armv7a boot on `main`  The armv7a boot at four processors ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), row -18). `test-xwindow` has not: still owed (2026-10-05, po5-c1)  **Closed 2026-10-07 (po10-findings):** `test-xwindow --arch x86_64` PASSED on `main` 4066f41dd (`po10f-xwindow`) | closed |

### va-004

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **1dcc433f (MSI-X storm bound): its release build and its x86_64 boot under TCG ran only before its rebase** (`os35-wake/A-release.log`, `A-boot-x86_64.log`). The rebase took `main`'s changes to `device.rs` and `stages_check.rs`, both files it touches; the four boots after it were KVM and Arm. Its review, recorded only by its author at landing, was confirmed by the certification consultant on 2026-10-01, and both negative controls are logged (`os35-wake/nc-a-{lost,storm}.log`). Run `build --arch all --release` and `test-boot --arch x86_64` on `main`  **Closed 2026-10-05 (po5-c1):** both ran on `main` since, PASSED: `build --arch all --release` and `test-boot --arch x86_64 --accel tcg` in batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -2 and -16; again on c19aeefcb (`po5-docs-build`, `po5-docs-boot-x86-tcg`) | closed |

### va-005

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **1384e6e6 (N3) landed outside the landing lock**, by a plain push, and its busybox `test-shell` rows (ferrousli, musl, glibc), both `test-vfs` rows and `test-init --arch all` ran only on 992fc6e6, before the amend that changed `family.rs` and `fd.rs` on the clone and `openat` paths. Re-run them on `main`. The namespaces paragraph under *Waiting on the customer* still ends at N2  2026-10-05 (po5-c1): `test-init --arch all` and ferrousli's `test-shell` and `test-vfs` (`--arch all`) ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -5, -1, -4); the musl and glibc busybox `test-shell` and the musl `test-vfs` have not  **Closed 2026-10-07 (po10-findings):** all three PASSED on `main` 4066f41dd on x86_64: `test-shell` with the musl busybox (`po10f-shell-musl`) and with `/usr/bin/busybox` (`po10f-shell-glibc`), `test-vfs` with the musl busybox (`po10f-vfs-musl`); the namespaces paragraph under *Waiting on the customer* now runs to N4 and NP | closed |

### va-006

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **cc4e14af (the discovery Finder) claims a coverage carry it does not contain**: "Coverage: carried from caa9e3ea", but the commit has no evidence file. The carry landed a minute later in 65639967, from d86821bb. Its "five arguments" are seven, and two line numbers were off by one; da45a113 corrected both in `docs/certification/TODO.md`. Its boots were not re-run after the rebase over d86821bb, which changed five kernel files in comments and path strings only  **Closed 2026-10-07 (po10-findings):** the image row ran on `main` 4066f41dd, which contains it, PASSED: `check`, `build --arch all --release`, `test-boot` x86_64 under KVM and TCG, aarch64, armv7a and armv7a `--smp 2` (`~/.local/share/ferrix/logs/queue/batch-20261007T105031Z-N.log`, rows -4, -3, -11, -9, -7, -10, -8) | closed |

### va-007

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **d5896c75: its boots ran only before a rebase that changed `main.rs`**, with only `check` re-run after it. `docs/roadmap/stage-10-userspace-drivers.md` credits it with "ACPI or the device tree, decided once", which is fa4e88a6. The same roadmap's "os-9f reviewed each step" for the Finder series is contradicted by da45a113, which records them as reviewed after the fact  **Closed 2026-10-07 (po10-findings):** the boots ran on `main` 4066f41dd in the image row above (batch 20261007T105031Z, rows -11, -9, -7, -10, -8), PASSED; the roadmap now credits fa4e88a6, names d5896c75 for the `reserved` line, and says the series was reviewed after the fact by os-ad (da45a113) | closed |

### va-008

| Item | Owner |
|---|---|
| **Ten item landings on 2026-09-30 moved `main` outside the landing lock**: 1384e6e6; the discovery series c21abe11, aa78061e, f8ab2970, d5896c75, fa4e88a6, c2477dc1, cc4e14af and 2be909b3; 318987db, which went in inside another session's 39-second take; and bce1fc14 and 8f7801de. The lock log's `main` field reads the gate host's `main`, which moves only when a lander pushes it there, so its hashes do not show what landed on the Windows checkout | the product owner |

### va-009

| Item | Owner |
|---|---|
| **37 code and xtask landings since 2026-09-30 name no gate in their message**, among them ferrousli's loader stack, whose row in the gate table asks for Debian's dynamic busybox twice. Nothing required the gates in the message until `AGENTS.md` (2026-10-01). Those loader landings are the next audit; a first run was stopped before it reported. Mutation testing (`cargo mutants`, installed on the gate host) ran on four crates at 79826d1c: 23 missed of 491, untriaged (seccomp 9, objects 6, kmem 5, crng 3; `logs/os-db-mutants`, triage notes in the ledger); nine item crates not run | open (the auditor's seat is empty) |

### va-010

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **Closed 2026-10-01: 278a2f9d (`pkg`) landed outside the landing lock** at 18:24, while os7c-s2 held it; its `land.sh take` was piped through `tail`, which hid the refusal. S2 (ff5e45ef) landed on top; the two share no file. The combined tip was gated afterwards: `check`, `test-pkg --arch x86_64` and a KVM boot of ff5e45ef, all PASSED (`logs/queue/os3c-ff5e-*.log`, fleet log 18:33:38). Run `land.sh take` on its own line and test its exit before any push | closed |

### va-011

Done; removed from the backlog on 2026-10-16.

| Item | Owner |
|---|---|
| **bf9efba95 (speculation domains) landed ahead of its full gate**, on the customer's word (Decisions, 2026-10-01). Logs are `~/.local/share/ferrix/logs/queue/<tag>.log` on the gate host, and `fleet/gate.sh`'s INDEX holds each verdict. At the wind-down these were in: all eight controls, `osc7-sd4-c1` to `-c8`, `FIRED (panic)`; `osc7-sd4-check`, `-build`, `-boot-x86-kvm`, `-boot-x86` (which ran under KVM, not TCG), `-boot-a64` and `-boot-a32` PASSED on 96f0d28c8, the tree before the rebase; `osc7-sd5-check` PASSED on 9c04ac274; and `osc7-land-check`, `-boot-kvm`, `-boot-tcg` and `-boot-a64` PASSED on bf9efba95 itself. Still owed, queued by `~/ferrix-logs/osc7-sd4-row.sh` and still running on the gate host at the wind-down: `osc7-sd4-boot-a32-2`, `-shell`, `-threads`, `-vfs-ferrousli` and `-vfs-musl`. A control reading DID NOT FIRE, or any row failing, is red on `main`: tell the product owner and the consultant, and fix forward. 2026-10-05 (po5-c1): of the five, `-boot-a32-2`, `-shell`, `-threads` and `-vfs-ferrousli` ran on `main` since, PASSED (batch 20261004T194624Z on f55e8ab28 (on `main`, after it; `~/.local/share/ferrix/logs/queue/batch-20261004T194624Z-N.log`), rows -15, -6, -4; `os7c-po-four-shell`); `-vfs-musl` has not  **Closed 2026-10-07 (po10-findings):** `test-vfs --arch x86_64` with the musl busybox PASSED on `main` 4066f41dd (`po10f-vfs-musl`) | closed |
