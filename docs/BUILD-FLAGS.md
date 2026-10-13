# Build flags and kernel profiles

**Design for review, 2026-10-13.** This is M0's configuration system from
`docs/MICROKERNEL.md` §12: the flags that select what a Ferrix kernel
contains, and the three profiles built from them. Nothing here is built yet.
It gets the certification consultant's design review before any code, as
`MICROKERNEL.md` requires of M0's configuration system.

The customer's words (2026-10-13): "3 builds sounds good, so a good start
would then be to define the build time flags?", and "yes desktop build is a
hybrid kernel, while max and fast are true microkernels". Asked whether the
IPC fast path becomes a build flag: "Build-time flag".

## 1. The three kernels

| Profile | Shape | Claim | For |
|---|---|---|---|
| **`max`** | microkernel | IEC 61508-3 SC 4 / ISO 26262 ASIL D (`MICROKERNEL.md` §6.5) | safety products at the top level |
| **`fast`** | microkernel | IEC 61508-3 SC 2 / ISO 26262 ASIL B, no timing claim | safety products that want speed |
| **`desktop`** | **hybrid**: the Linux layer and btrfs in ring 0 | none | the customer's machines, gaming, development |

`max` and `fast` have the same shape: only the kernel in ring 0, the Linux
layer in `linuxd`, btrfs in `btrfsd`. They differ in what they relax for
speed (§3). `desktop` is today's Ferrix, kept: the same Linux layer built
into ring 0 through the shim (`MICROKERNEL.md` §4), so it never pays the
restricted-mode round trip.

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
   compared. That keeps xtask's resolver at T2 / TQL-5.
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
| `partition_speculation` | `exit`, `partition` | the barrier on every restricted exit and entry, or only at partition switches (§11 Q5, Q16). `partition` requires `personality=user` | per Q16 | `partition` | not applicable | `exit`: SC 4; `partition`: per Q16 (SC 2 at least) | — | M3 |
| `ipc_fastpath` | `off`, `on` | the fast path for `channel_write_read` (0x1013) | `off` until the 2026-10-02 rule is met, then `on` | `on` | `on` | `off`: SC 4; `on`: SC 2, then SC 4 when the rule is met | boot option `ferrix.fastpath` (L.x86_64.159) | M0 |
| `restricted_fastpath` | `off`, `on` | K1's fastpath for the restricted exit and re-entry. Requires `personality=user` | per K1's review | `on` | not applicable | per K1's review | — | M3 |
| `kernel_memory` | `retyped`, `heap` | K16's memory from the job's own reservation, or today's fallible, charged shared heap | `retyped` | `heap` | `heap` | `retyped`: SC 4; `heap`: SC 2, under `fast`'s justification | `heap` only | M6 |
| `schedule` | `windows`, `edf`, `fair` | K9's static windows with EDF inside them; stage 14's HardRt (partitioned EDF with CBS); or today's fair scheduler | `windows` | `edf` | `fair` | `windows`: SC 4; `edf`: SC 2; `fair`: none | `fair` only | M3 |
| `checks` | `on`, `off` | the in-kernel boot checks compiled in. With `on`, the boot option `ferrix.checks` still chooses whether they run, as today | `on` | `on` | `on` | `on`: SC 4; `off`: none | always compiled in | M0 |

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
  `desktop`; it is designed apart and changes no flag here.
* **The boot options that stay**: `ferrix.checks`, `ferrix.root`,
  `ferrix.init`, `ferrix.onexit`, `ferrix.fbcon`, `ferrix.btrfs` and
  `ferrix.btrfs.seed` (test-only), `nokaslr`, `nosmp`. Each profile lists
  which of them its certified configuration permits, and `ITEM.md` §5 counts
  them per profile.

**The fast path's rule** (the customer's call of 2026-10-02,
`OPAQUE-KERNEL.md` §9.7, kept with the mechanism changed from a boot option
to this flag): `max` builds with `ipc_fastpath=off` until one coverage run
has measured the fast path and its vulnerability-analysis entry exists; then
`max`'s value becomes `on`, recorded as a dated change to the profile.
Moving the switch from boot time to build time is an item change with its
own review, and L.x86_64.159 is restated for it.

## 4. Levels

| Level | Means |
|---|---|
| `none` | no claim |
| `sc2` | IEC 61508-3 SC 2 and ISO 26262 ASIL B |
| `sc4` | IEC 61508-3 SC 4 and ISO 26262 ASIL D |

A value that allows `sc4` allows `sc2` and `none`. A profile's level is the
lowest level any of its values allows, and must equal the level the profile
claims; the gate fails a profile whose values allow less than its claim.

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
boot_options = ["ferrix.checks", "ferrix.root", "ferrix.onexit"]

[flags]
personality = "user"
fs = "server"
mitigations = "on"
partition_speculation = "partition"
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

Until M3 to M6 build the second values, most flags have one value only, and
the profiles differ in `ipc_fastpath` and the image content. M0 builds:

1. The flag file and the three profile files, with today's values.
2. The resolver in xtask, the `--check-cfg` generation, and `unexpected_cfgs`
   denied.
3. `mitigations` moved onto the new scheme (`ferrix_mitigations="off"` in
   place of `ferrix_mitigations_off`), byte-identical images otherwise.
4. `ipc_fastpath` moved from the boot option to the flag (the item change
   above).
5. `checks=off` as a build choice for ad-hoc builds.
6. The kernel's boot line from `cfg!`, and the gate's comparison of it.
7. The profile rule in `check-item-boundary.py`, with
   `certification-item.json` gaining the flags' levels, the components'
   levels and one reference configuration per certified profile.
8. `xtask check` building all three profiles on three architectures.

`desktop` is the profile today's tree already is. Until M5, the value
`personality=user` has no code behind it (nor `fs=server`), so `max` and
`fast` cannot be built as their files name them: xtask refuses a value whose
stage has not landed, by a `lands` field in the flag file. Question 4 asks
whether their files exist from M0 and fail that way, or appear at M5.

## 7. Questions for the consultant

1. Is the level vocabulary (`none`, `sc2`, `sc4`) enough, or does the
   manifest need the four standards' levels apart (DO-178C, EN 50716, CC)?
2. Rule 6: is a kernel module of `cfg!` constants, printed at boot, enough
   independence from xtask, given that xtask also chose the flags?
3. `checks=off` removes the boot checks from ad-hoc builds only. Is that
   right, or should no build ever lack them?
4. Until M5, `max` and `fast` are not buildable as their claims need. Should
   their profile files exist from M0 with `personality=kernel` and fail the
   gate (as §6 says), or appear only at M5?

## Where it stands

Design on branch `microkernel-plan`, 2026-10-13. Sent to the certification
consultant for a design review before code.
