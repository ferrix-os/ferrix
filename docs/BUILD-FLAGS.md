# Build flags and kernel profiles

**Design for review, 2026-10-13.** This is M0's configuration system from
`docs/MICROKERNEL.md` §12: the flags that select what a Ferrix kernel
contains, and the three profiles built from them. Nothing here is built yet.
It gets the certification consultant's design review before any code, as
`MICROKERNEL.md` requires of M0's configuration system.

The customer's words (2026-10-13): "3 builds sounds good, so a good start
would then be to define the build time flags?", and "yes desktop build is a
hybrid kernel, while max and fast are true microkernels". Asked whether the
IPC fast path becomes a build flag: "Build-time flag". Asked whether the
2026-10-02 fast-path rule binds `fast` too: "Only max".

## 1. The three kernels

| Profile | Shape | Claim | For |
|---|---|---|---|
| **`max`** | microkernel | IEC 61508-3 SC 4 / ISO 26262 ASIL D (`MICROKERNEL.md` §6.5) | safety products at the top level |
| **`fast`** | microkernel | IEC 61508-3 SC 2 / ISO 26262 ASIL B, no timing claim | safety products that want speed |
| **`desktop`** | **hybrid**: the Linux layer and btrfs in ring 0 | none | the customer's machines, gaming, development |
| **`present`** (interim, until the flip) | **hybrid**: today's tree | **today's targets** (EAL5+, DAL C, IEC 62304 Class C, EN 50716 SIL 2; the customer, ledger 869) | today's reference configuration, so the gate already runs over the configuration that carries today's claim |

`max` and `fast` have the same shape: only the kernel in ring 0, the Linux
layer in `linuxd`, btrfs in `btrfsd`. They differ in what they relax for
speed (§3). `desktop` is today's Ferrix, kept: the same Linux layer built
into ring 0 through the shim (`MICROKERNEL.md` §4), so it never pays the
restricted-mode round trip.

**`present`** keeps today's item its claim until the switch (BF1, ledger
914): the customer decided that today's kernel keeps its four targets until
then, and `personality=kernel` alone would allow only `none`. So `present`
is today's `reference_configuration` as a profile, at its own level
(`present`, §4), with `personality=kernel` and `fs=kernel` allowed **by a
dated exception that ends at M5's flip**. When the flip lands, `present` is
deleted. **Between the flip and M7's evidence no profile carries a claim**,
as the customer's "new kernel, from M7" implies; `max` and `fast` carry
their claims from M7.

## 2. The rules

These come from `MICROKERNEL.md` §12 and the consultant's conditions on it
(ledger lines 903 and 913):

1. **Every flag is chosen at compile time.** xtask passes it to rustc as a
   key-value `--cfg`. A flag is never a Cargo feature (`kernel_cargo_features`
   stays 0) and never a boot option. Code a profile does not select is not in
   its image, so it is neither dead nor deactivated code there.
2. **Every flag and value is declared.** xtask generates rustc's
   `--check-cfg` list from the flag file (§5), and the kernel's workspace
   denies `unexpected_cfgs` (it warns today, `Cargo.toml` line 248). A `cfg`
   naming an undeclared flag or value fails the build.
3. **Every value declares the highest level it allows** (§4's levels). A
   profile may use a value only if the value allows the profile's level, and
   a component may be linked into ring 0 only if it is certified at the
   profile's level or above. The item-boundary gate checks both, with a
   planted QM component in a certified profile as its negative control.
4. **Certified profiles set every flag explicitly.** No defaults apply to
   `max` or `fast`, so adding a flag forces a decision for each of them.
5. **Only named profiles carry a claim.** Any other combination builds and
   boots, and its boot line says `no claim`.
6. **The kernel reports what it is.** A kernel module computes the boot
   line's flag set from the same `cfg!` predicates that select the code,
   never from data xtask writes. The gate compares the line with the
   profile's expected set and ties the shipped image's hash to the run it
   compared. That keeps xtask's resolver at T2 / TQL-5, under four further
   rules (BF3): (a) the gate takes the expected set from the profile file,
   never from xtask's output; (b) **flags are read only in the kernel
   crate**, and a check fails if any crate under `src/lib` names a `ferrix_`
   cfg (a crate built without the cfg would silently take the other branch,
   which `--check-cfg` cannot see), with a planted use as its negative
   control; (c) a build whose `--set` differs from its profile is refused by
   the gate, as a negative control; (d) the line is also the start-up
   configuration audit record (FAU_GEN.1 b), so the Security Target and
   `AUDIT.md` change with it.
7. **Each certified profile is its own reference configuration** in
   `certification-item.json`, with its own coverage, rows and traceability
   per architecture. A requirement that holds only under some flags says so,
   and its check runs in those profiles.

## 3. The flags

`cfg` names take the form `ferrix_<flag>="<value>"`. "Today" says where the
flag's choice lives now; "Lands" is the `MICROKERNEL.md` stage that builds
the second value.

| Flag | Values | What it selects | `max` | `fast` | `desktop` | Highest level per value | Today | Lands |
|---|---|---|---|---|---|---|---|---|
| `personality` | `user`, `kernel` | the Linux layer as `linuxd` in user space, or built into ring 0 | `user` | `user` | `kernel` | `user`: SC 4; `kernel`: none | in ring 0 only | M5 |
| `fs` | `server`, `kernel` | btrfs as `btrfsd`, or the btrfs crates in ring 0 with today's glue. `kernel` requires `personality=kernel` | `server` | `server` | `kernel` | `server`: SC 4; `kernel`: none | in ring 0 only | M5 |
| `mitigations` | `on`, `off` | F-31's speculation defences | `on` | `on` | `on` | `on`: SC 4; `off`: none | `--cfg ferrix_mitigations_off`, renamed | M0 |
| `partition_speculation` | `exit`, `partition` | the barrier on every restricted exit and entry, or only at partition switches (§11 Q5, Q16). `partition` requires `personality=user` | `exit` (decided 2026-10-13, ledger 926) | `partition` | not applicable | `exit`: SC 4; `partition`: SC 2 | — | M3 |
| `ipc_fastpath` | `off`, `on` | the fast path for `channel_write_read` (0x1013) | `off` until the 2026-10-02 rule is met, then `on` | `on` (the customer: the rule binds `max` only) | `on` | `off`: SC 4; `on`: SC 2, and SC 4 once the rule is met | boot option `ferrix.fastpath` (L.x86_64.159) | M0 |
| `restricted_fastpath` | `off`, `on` | K1's fastpath for the restricted exit and re-entry. Requires `personality=user` | per K1's review | `on` | not applicable | per K1's review | — | M3 |
| `kernel_memory` | `retyped`, `heap` | K16's memory from the job's own reservation, or today's fallible, charged shared heap | `retyped` | `heap` | `heap` | `retyped`: SC 4; `heap`: SC 2, under `fast`'s justification | `heap` only | M6 |
| `schedule` | `windows`, `edf`, `fair` | K9's static windows with EDF inside them; stage 14's HardRt (partitioned EDF with CBS); or today's fair scheduler | `windows` | `edf` | `fair` | `windows`: SC 4; `edf`: SC 2; `fair`: none | `fair` only | M3 |
| `checks` | `on`, `off` | the in-kernel boot checks compiled in. With `on`, the boot option `ferrix.checks` still chooses whether they run, as today; a certified profile permits it only with the checks running. `off` is for ad-hoc builds only, and `xtask check` builds an `off` variant on every architecture so it cannot rot, as it does `mitigations=off` today (BF7) | `on` | `on` | `on` | `on`: SC 4; `off`: none | always compiled in | M0 |
| `program_barrier` | `switch`, `session` | `switch`: the barrier at every switch between programs not in one marked speculation domain (today's rule, SPECULATION §3). `session`: barriers only between domains, each login session's job marked by sessiond at creation under the 2026-10-01 rules (MANAGE holder, at creation, audited); set-id `execve`, a credential change and `PR_SET_DUMPABLE` still leave the domain, so `su`, `login` and other users' sessions keep the barrier | `switch` | `switch` | `session` | `switch`: SC 4; `session`: none | `switch` | M0 |
| `ssbd` | `always`, `on_request` | Speculative Store Bypass Disable for every program (today), or only for programs that ask by `prctl` or `seccomp`, as Linux does by default | `always` | `always` | `on_request` | `always`: SC 4; `on_request`: none | `always` | M0 |
| `smt` | `off`, `stibp` | `off`: one hardware thread per core is started (IBPB does not cover cross-thread channels; AoU-11 today); `stibp`: sibling threads run, with STIBP (or eIBRS/AutoIBRS's own cover) | `off` | `stibp` | `stibp` | `off`: SC 4; `stibp`: SC 2 | AoU-11 (SMT off assumed) | M0 |
| `runtime_trades` | `none`, `game_mode` | whether the image contains game mode's privileged, per-application run-time trades of protections (the customer, 2026-10-11); with `none` their code is absent | `none` | `none` | `game_mode` | `none`: SC 4; `game_mode`: none | not built | with game mode's design |

Not flags, and why:

* **The architecture** is a build axis, not a flag: every profile builds for
  x86-64, AArch64 and ARMv7-A, and each profile names the architectures its
  claim covers (`max` and `fast`: AArch64 for now, `MICROKERNEL.md` §6.5).
* **`ferrix.devmgr=kernel|init`** retires with `DEVMGR_START` at M5
  (`MICROKERNEL.md` §3). Until then it stays the boot option it is.
* **`driver_updates`** (whether the image carries `drvupdated`) is image
  content, not kernel code. It stays in `reference_configuration` and each
  profile names its value: `absent` in `max` and `fast`.
* **Criticality levels** (K17) are set per job at run time by the partition
  manager, not per build.
* **Game mode** (the customer, 2026-10-11) is a privileged run-time trade on
  `desktop`; it is designed apart. Whether its code is in an image at all is
  the `runtime_trades` flag.
* **Zenbleed's and Gather Data Sampling's fixes leave the `mitigations`
  flag** and stay on in every profile, `mitigations=off` included: they stop
  real register leaks at no measurable cost, so they are not a speed trade
  (ledger 926).
* **What stays on in `desktop`'s default** (ledger 926): eIBRS or AutoIBRS,
  the Spectre v1 clamps, the SWAPGS fence and register clearing on entry,
  the return-stack refill or ERAPS at switches, VERW on MDS-affected
  processors, SMEP, SMAP, PAN, W^X and KASLR. **`desktop` is deactivatable**:
  `--set mitigations=off` builds a desktop with none of the speed-trade
  defences and no claim, as today, and game mode's privileged run-time
  trades act when `runtime_trades=game_mode`.
* **Nothing of `desktop`'s leaks into a certified profile**: every lighter
  value above has level `none`, so the profile rule refuses it in `max`,
  `fast` or `present` (planted controls: `program_barrier=session` in `max`,
  `runtime_trades=game_mode` in `fast`, both refused); no certified image
  contains a run-time relaxation path, since the code is absent, not
  switched off; and the boot line and its audit record name every value.
* **The boot options that stay** are a **registry** in `build-flags.toml`
  (BF4): `ferrix.checks`, `ferrix.root`, `ferrix.init`, `ferrix.onexit`,
  `ferrix.fbcon`, `ferrix.btrfs` and `ferrix.btrfs.seed` (test-only),
  `console=` (ARMv7-A, `arch/armv7a/console.rs`), `drm.edid_firmware`
  (`interfaces/display/edid.rs`), `nokaslr`, `nosmp`, and `ferrix.devmgr`
  until M5. Each profile names the **values** of each it permits, not only
  the names: a certified profile permits `ferrix.checks` only with the
  checks running, and never `ferrix.btrfs.seed`. A check fails on any read
  of an option the registry does not declare, with a planted reader as its
  negative control. `ITEM.md` §5 counts them per profile.
* **Build settings that change code without a cfg** are recorded in each
  profile and compared like the flags: the Cargo profile, the toolchain,
  the RUSTFLAGS and codegen options xtask passes, and the link mode
  (`build.rs` chooses it from `CARGO_CFG_FERRIX_MITIGATIONS_OFF` today).

**The fast path's rule** (the customer's call of 2026-10-02,
`OPAQUE-KERNEL.md` §9.7, kept with the mechanism changed from a boot option
to this flag): `max` builds with `ipc_fastpath=off` until one coverage run
has measured the fast path and its vulnerability-analysis entry exists; then
`max`'s value becomes `on`, recorded as a dated change to the profile. The
rule binds `max` only (the customer, 2026-10-13: "Only max"), so `fast`
builds with `on` from M0, and `fast`'s own ASIL B evidence covers the fast
path: its coverage measured in `fast`'s image and its
vulnerability-analysis entry written for `fast`.

Moving the switch from boot time to build time is an item change with its
own design note and review (BF6). The note lists: L.x86_64.159 and
L.x86_64.161 restated; the composition-root hook entry in
`certification-item.json` and `ITEM.md` §2's link-time hooks; whether the
system-call entry's flag test compiles out (a core and architecture change);
stage 9's boot line; FAU_GEN.1 b); the gate rows that pass
`--kernel-option ferrix.fastpath=on` today (BACKLOG row 436 among them),
moved to profile builds; the fast path's negative controls fired again under
the flag; coverage per profile, since the code is absent from `max`'s image
while the flag is off; and `ITEM.md` §5's count of boot options.

## 4. Levels

**Each level is defined once, in the flag file, as its full set of targets**
(BF2), and the level names are labels only. **"Allows" means per-standard
dominance** (BF10, ledger 925): a value allows a level only if, for every
standard in that level's set, the value's own set reaches at least that
target. So `present` and `sc2` are incomparable (`present` has DAL C and
EAL5+, which `sc2` lacks; `sc2` has ASIL B, which `present` lacks), and a
value that allows `sc2` does not thereby allow `present`. The gate has a test
for exactly that pair.

| Level | IEC 61508-3 | ISO 26262 | EN 50716 | DO-178C | IEC 62304 | Common Criteria (the TOE) |
|---|---|---|---|---|---|---|
| `none` | — | — | — | — | — | — |
| `present` (interim, today's item) | SC 2 | — | SIL 2 | DAL C | Class C | EAL5+ |
| `sc2` (`fast`) | SC 2 | ASIL B | SIL 2 | — | Class C | as `fast`'s ST says |
| `sc4` (`max`) | SC 4 | ASIL D | SIL 4 | DAL A objectives | Class C | EAL6+ |

A profile's claim must be allowed by every one of its values, standard by
standard; the gate fails a profile with any value that does not reach the
claim's target in some standard.

## 5. Files and commands

**The flag file**, `tools/common/data/build-flags.toml`, declares each flag:
its values, each value's highest level, and its `requires` and
`conflicts`, as Kconfig's `depends on` does:

```toml
[flags.personality]
values = { user = "sc4", kernel = "none" }
why = "the Linux layer in linuxd, or built into ring 0 (MICROKERNEL.md §4)"

[flags.fs]
values = { server = "sc4", kernel = "none" }
requires = { kernel = ["personality=kernel"] }
```

**The profile files**, `tools/common/data/profiles/{max,fast,desktop}.toml`,
set every flag (rule 4), the level, the architectures the claim covers, the
image content and the boot options the certified configuration permits:

```toml
level = "sc2"
claim_architectures = ["aarch64"]
image = { driver_updates = "absent" }
boot_options = { "ferrix.checks" = ["run"], "ferrix.root" = ["*"], "ferrix.onexit" = ["*"] }
build = { cargo_profile = "release", toolchain = "1.97.1" }
link = { x86_64 = "static PIE, PIC code model", aarch64 = "static PIE", armv7a = "fixed address with relocations" }

[flags]
personality = "user"
fs = "server"
mitigations = "on"
partition_speculation = "partition"
program_barrier = "switch"
ssbd = "always"
smt = "stibp"
runtime_trades = "none"
ipc_fastpath = "on"
restricted_fastpath = "on"
kernel_memory = "heap"
schedule = "edf"
checks = "on"
```

**The commands.** `cargo xtask build --kernel-profile fast` builds a profile
(`--profile` is already Cargo's release or debug profile, so the new option
has its own name). `--set flag=value` changes one flag for an ad-hoc build,
which then carries no claim. `cargo xtask check` builds every named profile
on every architecture. `cargo xtask flags` prints the resolved set for a
profile.

**The boot line**, printed early by every kernel:

```
build profile=fast level=sc2 personality=user fs=server mitigations=on partition_speculation=partition ipc_fastpath=on restricted_fastpath=on kernel_memory=heap schedule=edf checks=on
```

The kernel computes every word of it from `cfg!` (rule 6). An ad-hoc build
prints `profile=none level=none`.

## 6. What M0 builds

Until M3 to M6 build the second values, most flags have one value only. M0
builds:

1. The flag file (flags, levels, the boot-option registry, `lands`) and the
   four profile files: `present`, `desktop`, `max` and `fast`.
2. The resolver in xtask, the `--check-cfg` generation, and `unexpected_cfgs`
   denied.
3. `mitigations` moved onto the new scheme (`ferrix_mitigations="off"` in
   place of `ferrix_mitigations_off`), `build.rs`'s link-mode choice with it,
   and **byte-identical images** as its check.
4. `ipc_fastpath` moved from the boot option to the flag (the item change
   of §3, under its own review).
5. `checks=off` as a build choice for ad-hoc builds, built by `xtask check`.
6. The kernel's boot line from `cfg!`, the gate's comparison of it, and the
   audit record it becomes.
7. The profile rule in `check-item-boundary.py`, with
   `certification-item.json` gaining the levels, the flags' levels and the
   components' levels, and `present` as today's reference configuration.
8. The checks of §2 and §3: no `ferrix_` cfg outside the kernel crate, no
   undeclared boot option read, a `--set` build refused, each with its
   negative control.
9. `xtask check` building `present` and `desktop` on three architectures.

**`max` and `fast` before M5** (BF8): their files exist from M0 with their
true values, never with placeholders. The gate validates them statically
(every flag set, levels consistent, the profile rule), with a negative
control that a profile with an undeclared or below-level value fails. xtask
refuses to build them by `lands`, with a clear message naming the stage, and
`xtask check` reports them as "not yet buildable", not as red. So rule 4
forces a decision for both of them for every flag added from M0 on.

## 7. The consultant's answers (ledger 914)

1. **Levels**: an ordered scale for the gate, each level defined as its full
   set of targets (§4).
2. **Rule 6**: enough independence, with BF3's four additions (§2).
3. **`checks=off`**: right, for ad-hoc builds only, built by `xtask check`
   (§3).
4. **`max` and `fast` before M5**: files from M0 with true values, checked
   statically, refused by `lands` (§6).

## Where it stands

Design on branch `microkernel-plan`, 2026-10-13.

* First design review before code: **OK IF** BF1 to BF9, ledger line 914 of
  `~/.local/share/ferrix/cert-consultant/reviews.md`. This text folds them
  in: the interim `present` profile (BF1); levels as target sets (BF2); the
  resolver's four rules (BF3); the boot-option registry and the build
  settings (BF4); `ipc_fastpath` in `fast` per the customer's "Only max"
  (BF5); the fast-path move's design note (BF6); `checks=off` (BF7); `max`
  and `fast` before M5 (BF8). To be checked against the diff.
* Diff check: **OK IF** BF10, ledger line 925, and **M0's code may start**.
  BF10 is folded in: per-standard dominance with the scale as labels and a
  test for the incomparable `present`/`sc2` pair (§4); no claim between the
  flip and M7 (§1); the link mode per architecture (§5).
* **The customer delegated the barrier, the contacts and the board to the
  consultant** (2026-10-13: "let the cert agent decide. ideally it shall be
  almost not realizable on desktop, maybe even deactivateable, as it is not
  certified anyways, and max security for max"). Its decisions, ledger line
  926: `max` with `partition_speculation=exit`, `smt=off`; `fast` with
  `partition`, `smt=stibp`; `desktop` with `program_barrier=session`,
  `ssbd=on_request`, `runtime_trades=game_mode`, and Zenbleed's and GDS's
  fixes out of the `mitigations` flag. **The four new flags need this
  document's design review before any code** (sent).
* M0's code may start (the consultant: BF1 to BF4 written in) (the consultant: BF1
  to BF4 written in). The fast-path move gets its own item review (BF6).
