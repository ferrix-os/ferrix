# Build flags and kernel profiles

**Design for review, 2026-10-13.** This is M0's configuration system from
`docs/MICROKERNEL.md` §12: the flags that select what a Ferrix kernel
contains, and the three profiles built from them. It passed the
certification consultant's design review before any code, as
`MICROKERNEL.md` requires of M0's configuration system; what is built is in
*Where it stands*, at the end.

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
the second value. **"Allows" lists, for each value, the levels it allows**,
explicitly (§4: levels compare standard by standard, so `sc4` covers
`present` but `sc2` does not; DF1, ledger 931). "All" means `sc4`,
`present`, `sc2` and `none`.

| Flag | Values | What it selects | `max` | `fast` | `desktop` | `present` | Allows | Today | Lands |
|---|---|---|---|---|---|---|---|---|---|
| `personality` | `user`, `kernel` | the Linux layer as `linuxd` in user space, or built into ring 0 | `user` | `user` | `kernel` | `kernel` (dated exception, ends at M5's flip) | `user`: all; `kernel`: `none` | in ring 0 only | M5 |
| `fs` | `server`, `kernel` | btrfs as `btrfsd`, or the btrfs crates in ring 0 with today's glue. `kernel` requires `personality=kernel` | `server` | `server` | `kernel` | `kernel` (the same exception) | `server`: all; `kernel`: `none` | in ring 0 only | M5 |
| `mitigations` | `on`, `off` | F-31's speculation defences (Zenbleed's and GDS's fixes are outside it, below) | `on` | `on` | `on` | `on` | `on`: all; `off`: `none` | `--cfg ferrix_mitigations_off`, renamed | M0 |
| `partition_speculation` | `exit`, `partition` | `exit`: an IBPB (with IBPB_BRTYPE's semantics) on every restricted exit, none on entry, the full IBPB at every switch between programs and partitions; on a processor reporting SRSO_NO, BTC_NO, no BHI and no ITS, with the return predictor cleared at the root switch, the exit IBPB may be skipped, decided at boot, printed, failing closed (`MICROKERNEL.md` §7, ledger 941; nazuna does not qualify); on AArch64 the Spectre-BHB clearing sequence on every exit. `partition`: barriers only at partition switches. Requires `personality=user` | `exit` (the customer: 1.5x seL4 for `max`, 660 ns) | `partition` | not applicable | not applicable | `exit`: all; `partition`: `sc2`, `none` | — | M3 |
| `ipc_fastpath` | `off`, `on` | the fast path for `channel_write_read` (0x1013) | `off` until the 2026-10-02 rule is met, then `on` | `on` (the rule binds `max` only) | `on` | `off` (today's certified default) | `off`: all; `on`: `sc2`, `none`, and `sc4` and `present` once the rule is met | boot option `ferrix.fastpath` (L.x86_64.159) | M0 |
| `restricted_fastpath` | `off`, `on` | K1's fastpath for the restricted exit and re-entry. Requires `personality=user` | per K1's review | `on` | not applicable | not applicable | per K1's review | — | M3 |
| `kernel_memory` | `retyped`, `heap` | K16's memory from the job's own reservation, or today's fallible, charged shared heap | `retyped` | `heap` | `heap` | `heap` | `retyped`: all; `heap`: `present` (AoU-5), `sc2` (under `fast`'s justification), `none` | `heap` only | M6 |
| `schedule` | `windows`, `edf`, `fair` | K9's static windows with EDF inside them; stage 14's HardRt (partitioned EDF with CBS); or today's fair scheduler | `windows` | `edf` | `fair` | `fair` | `windows`: all; `edf`: `sc2`, `present`, `none`; `fair`: `present` (with AoU-4: no timing claim), `none` | `fair` only | M3 |
| `checks` | `on`, `off` | the in-kernel boot checks compiled in. With `on`, the boot option `ferrix.checks` still chooses whether they run, as today; a certified profile permits it only with the checks running. `off` is for ad-hoc builds only, and `xtask check` builds an `off` variant on every architecture so it cannot rot (BF7) | `on` | `on` | `on` | `on` | `on`: all; `off`: `none` | always compiled in | M0 |
| `program_barrier` | `switch`, `session` | `switch`: the barrier at every switch between programs not in one marked speculation domain (today's rule, SPECULATION §3). `session`: §3.1 below | `switch` | `switch` | `session` | `switch` | `switch`: all; `session`: `none` | `switch` | M0 |
| `ssbd` | `always`, `on_request` | Speculative Store Bypass Disable for every program (today), or per thread for programs that ask (§3.1) | `always` | `always` | `on_request` | `always` | `always`: all; `on_request`: `none` | `always` | M0 |
| `smt` | `off`, `stibp` | `off`: the kernel runs one hardware thread per core (§3.1); `stibp`: sibling threads run, with STIBP or eIBRS/AutoIBRS's own cover | `off` | `stibp` | `stibp` | `stibp` | `off`: all; `stibp`: `sc2`, `present`, `none` | `stibp` (SMT runs today; AoU-11 asks the integrator to disable it only on MDS-affected parts) | M0 |
| `runtime_trades` | `none`, `game_mode` | whether the image contains game mode's privileged, per-application run-time trades (§3.1); with `none` their code is absent | `none` | `none` | `game_mode` | `none` | `none`: all; `game_mode`: `none` | not built | with game mode's design |

### 3.1 The speculation flags' mechanisms (ledger 931)

* **`program_barrier=session`** (DF2) is a **desktop-only departure** from
  the 2026-10-01 domain rules, at level `none`; those rules stay unchanged
  for every certified value. (a) Under `session` a domain is a marked job
  **and its whole subtree**, since a session's applications run in child
  cgroup jobs; a move within the subtree stays in the domain, a move out
  leaves it. (b) A program outside every domain keeps the barrier to and
  from everything: "barriers only between domains" never means "none for
  unmarked programs". (c) A program that asks by
  `prctl(PR_SPEC_INDIRECT_BRANCH)`, or runs under a seccomp filter, leaves
  the session's domain, so a browser's sandboxed renderer, running a
  website's code, keeps the barrier against the rest of the session. (d)
  **F-64 is fixed before `session`'s code lands**: `session` makes domain
  leaves common (every `su`, `sudo`, `login`, `pkexec`, and (c)). (e)
  `SPECULATION.md` gains a desktop section naming `session`'s exposure.
* **`ssbd=on_request`** (DF3): SSBD is a per-thread setting, written at a
  context switch only when the next thread's setting differs (`SPEC_CTRL`,
  or `VIRT_SPEC_CTRL` under AMD's KVM). The kernel runs with the current
  thread's setting, as Linux does, and that exposure is named in
  `SPECULATION.md`. `PR_GET_SPECULATION_CTRL` answers truthfully in every
  profile, and inheritance across `fork` and `exec` follows Linux.
* **`smt=off`** (DF4) is done by the kernel, not left to firmware. (a)
  x86-64: siblings are found from CPUID's topology leaves (0x1F, 0xB); only
  each core's first thread is started for work, and the others are parked by
  the kernel after the setup a parked thread needs (on Intel, `CR4.MCE` set:
  a machine-check broadcast reaching a sibling left in wait-for-SIPI with
  `CR4.MCE` clear shuts the machine down, which is why Linux brings siblings
  up once under `nosmt`). Firmware with SMT off is accepted as equivalent and
  named on the boot line. (b) AArch64: a check only, since the A78AE has no
  SMT; `MPIDR_EL1.MT` reads 1 there, so topology comes from the firmware
  tables' CPU map. (c) A boot check counts the threads running per core and
  requires 1 under `smt=off`, with a negative control that starts one
  sibling and fires it.
* **Game mode's trades** (DF5), under `runtime_trades=game_mode`: per
  application and audited, and only from the relaxable set around the game's
  job: the program barrier, SSBD and STIBP. They never trade `desktop`'s
  always-on list, since the kernel's entry defences protect the whole machine
  from the game. Turning everything off is `mitigations=off`, a build, never
  a run-time trade.
* **Moving Zenbleed's and GDS's fixes out of the `mitigations` flag** (DF6)
  is a core code change (`speculation::vector_leak` and the
  `mitigations=off` build), and it changes `SPECULATION.md` §1's "off
  compiles out every side-channel defence". It gets its own small review
  with M0's flag work, and AoU-11's Zenbleed and GDS sentence is restated.
* **The light values show on every desktop boot** (DF8): a check that
  `desktop`'s boot line names `session`, `on_request` and `game_mode`,
  besides the planted controls (`program_barrier=session` in `max`,
  `runtime_trades=game_mode` in `fast`, both refused) and (c)'s sibling
  count.

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
* **Stand-ins until each value's own landing** (ledger 1039, condition 2).
  `desktop` names `ipc_fastpath=on`, `program_barrier=session`,
  `ssbd=on_request` and `runtime_trades=game_mode`, whose code is not built
  yet; `build-flags.toml` marks each `built = "stand-in"` with the value it
  `stands_in`. **Desktop's `build` line and its `FAU_GEN.1` b) record
  (`Config::Build`, `docs/certification/AUDIT.md`) name the declared value
  while the image runs the stand-in, today's stricter code**: `off`,
  `switch`, `always` and `none`. A stand-in is allowed only at level `none`,
  so no certified profile carries one, and the line overstates what the
  image relaxes, never understates it. Each value's own landing replaces its
  stand-in and this sentence's entry; DF8's check that desktop's line names
  `session`, `on_request` and `game_mode` stays throughout.
* **The boot options that stay** are a **registry** in `build-flags.toml`
  (BF4): `ferrix.checks`, `ferrix.root`, `ferrix.init`, `ferrix.onexit`,
  `ferrix.fbcon`, `ferrix.btrfs` and `ferrix.btrfs.seed` (test-only),
  `console=` (ARMv7-A, `arch/armv7a/console.rs`), `drm.edid_firmware`
  (`interfaces/display/edid.rs`), `nokaslr`, `nosmp`, and `ferrix.devmgr`
  until M5; and, read since the design, `ferrix.fbcon.small`,
  `ferrix.pstore` (read by the loader, through
  `ferrix_bootinfo::pstore::switch`), `ferrix.efifb`, `ferrix.apic`
  (x86-64) and `ferrix.panic` (`halt` in every certified profile; `reset`,
  SAFETY-MANUAL AoU-32's stop-gap, in `desktop` alone). Beside the scan of
  reads, `check-build-flags.py` scans every `"ferrix.` string literal in
  `src/lib` and `src/boot` outside their tests: each is a declared option,
  in a file its `LIBRARY_READERS` names, or on its `NOT_OPTIONS` list with
  a reason (the loader's `ferrix.slot` words for `bootctl`,
  `ferrix.devmgr.update`, `ferrix.test`). **What the scan cannot see** is an
  option a library reads whose name does not start with `ferrix.` and is
  not yet a registry key, such as a new Linux-style word like `console=`;
  such an option is found only by a reviewer, or by the scan of reads once
  the reading file is in `LIBRARY_READERS`. Each profile names the **values** of each it permits, not only
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
* The four new flags' design review: **OK IF** DF1 to DF8, ledger line 931,
  folded in (the `present` column and explicit levels; §3.1's mechanisms;
  the contact drafts corrected outside the repository). The 1.5x-seL4 rule
  for `max`: `hardened` refused, `exit` redefined as an IBPB on exit only
  (ledger 941, HR1 to HR7).
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
* M0's code may start (the consultant: BF1 to BF4 written in). The
  fast-path move gets its own item review (BF6).
* **M0's parts A and D, built** (branch `m0-flags`, 2026-10-13, for the
  consultant's review before landing): the flag file and the four profiles
  (§5); the resolver, `--kernel-profile`, `--set` and `cargo xtask flags`;
  the flags reach the kernel crate alone, as `cargo::rustc-cfg` from its
  build script, with `--check-cfg` from the flag file and `unexpected_cfgs`
  denied; `mitigations` moved, with byte-identical kernels for `present` and
  `desktop` on all three architectures; the `build` boot line and its audit
  record (`Config::Build`); the gate, `tools/common/check/check-boot-line.py`,
  which reads the profile with its own parser; the profile rule in
  `check-item-boundary.py`, with each ring's and crate's `level` in
  `certification-item.json`; the source checks (`check-build-flags.py`);
  `checks=off`; `cargo xtask test-flags`; the load ratchet
  (`check-load-ring.py`). Decided where this document is silent, and open
  to review: (1) a value `desktop` names whose code is not built yet
  (`ipc_fastpath=on`, `program_barrier=session`, `ssbd=on_request`,
  `runtime_trades=game_mode`) builds as a *stand-in* for today's stricter
  value, only at level `none`, and the line still names it, which
  overstates what the image relaxes and never understates it; (2) the
  registry also declares `noactlr` (ARMv7-A, read today) and
  `ferrix.fastpath` (until BF6 moves it); (3) `ferrix-sync`'s clamp reads no
  cfg any more (rule 6 b), so `mitigations=off` keeps the two library
  clamps; (4) `max` has `restricted_fastpath=off` until K1's review; (5) a
  `--set` boot is refused by the gate unless `--no-claim` names it ad hoc.
* Landing review: **OK IF** (1) to (5), ledger line 1039, each of those
  decisions accepted. Met on the landing commit: (1) `xtask check`'s
  Ferrocene step (`check-ferrocene-rust.py`, M0 E) type-checks the kernel
  with the flags xtask resolves for each certified profile that builds,
  `present` today; (2) the stand-ins are said in words, in §3.1's list and
  in `AUDIT.md`; (3) `ITEM.md` §5 and `certification-item.json` say
  "type-checks (clippy)" and name the boots that link those builds; (4) the
  profile rule's self-test fails a buildable profile whose components sit
  below its level, so the note becomes a failure by `lands` alone; (5) the
  rows on the landing commit, named in its message. Owed: one L id for
  `build_flags::report` and `audit_words` before part B's first landing
  (`docs/BACKLOG.md`).
* Conditions check: **OK** on 9c22baf4a, ledger line 1051, transferring to
  a rebase under its R1 to R4. The branch was rebased twice more without
  conflict (onto 42e7bd521 and 17ebb8a4e, cited in the commit message).
* **Rebased onto main bf33c4cf9 (2026-10-17), with conflicts**, so R1 does
  not hold and the rows and the changed checks' controls run again. What
  main brought that the branch had to take in: xtask's command registry
  (`flags` and `test-flags` are now `Task`s), the kernel's lint over every
  package under `src/kernel`, the coverage profile and `--gdb`; the core's
  `boot_checks` (the `checks=off` constant moved there, so the core's own
  checks, the APIC's and the IOMMU's, compile out with the rest); the audit
  store's `Config::Pstore` and `Config::Efifb`, so `Config::Build` is 7,
  bring-up records seven configuration items, and the pinned boot records
  grow from eight to nine (`audit::BOOT_RECORDS`, H.AUD.2 restated), since
  the start-up record and seven items would otherwise push "the boot brought
  up" out of them; and four boot options read
  since the design, declared in the registry (above). `present` permits
  `ferrix.fbcon.small`, `ferrix.pstore=on` (the reference) and
  `ferrix.apic`, not `ferrix.efifb` (SAFETY-MANUAL AoU-31); `desktop` permits
  all four; `fast` and `max` none of them. Then rebased onto main 3a29477a7
  (panic-reboot): `ferrix.panic` declared, `halt` permitted in `present`,
  `max` and `fast`, `halt` and `reset` in `desktop`.
* Diff check of that rebase: **OK IF** (1) to (5), ledger line 2208 (on
  90084bf64). Accepted: `BOOT_RECORDS` 9 with H.AUD.2 restated (this branch,
  landing first, keeps `Config::Build` = 7; vtd-n150 renumbers to 8 and 10,
  ledger 2179 (4)); the permits as proposed; `checks=off`'s constant in the
  core's `boot_checks.rs`. Owed and done on the landing commit: (1) rebased
  onto main 241b704d5, conflicts in derived files alone, regenerated; (2)
  the general `"ferrix.` literal scan in `check-build-flags.py`, with its
  `NOT_OPTIONS` list, a self-test case and a control; (3) a boot control
  for H.AUD.2 with `BOOT_RECORDS` left at 8; (4) `coverage-owed.json`
  names `pinned()`'s edited lines and the new units for the next coverage
  run; (5) this entry. The landing hash and the rows' tags are in the
  landing commit's message. `check-build-flags.py` now
  follows `module::CONST` to the reader file of that module, and reads the
  library files named in its `LIBRARY_READERS` (`ferrix_bootinfo::pstore`),
  each with a self-test case. For the consultant's diff check before
  landing.
