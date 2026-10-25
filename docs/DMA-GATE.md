# Every bus master gated on isolation

Session: amd-vi (subagent of the PO's session), 2026-10-24. Branch
`feature/devmgr-isolation-gate`, from `develop`. Status: **BUILT** (see "As built" at the end); the design was reviewed
before any code (OK IF (1)-(9), ledger 3906), and G4 was decided by the
customer.

## 0. The decision

The customer, 2026-10-24, answering the consultant's D9 of the AMD-Vi design
review (`docs/AMD-VI.md` §8, ledger 3867):

> Yes, gate every DMA-capable device on isolation, as the consultant
> recommended. In claimed images every bus master (GPU, NIC, disk, USB,
> ...) gets a driver only with DMA_TRANSLATED and INTERRUPTS_ISOLATED. Dev
> images get a named, loudly announced opt-out (boot line plus audit).

The GPU's half landed first, on its own (`bugfix/gpu-gate-dma-translated`,
ledger 3891): `gpu::hand_over` requires both bits, and `device_isolation`
answers bit 0 for the domain a device is given before any pin
(`L.device.55`, `L.device.56`). This design widens the same rule to every
driver devmgr starts.

**Policy stays in ring 3.** devmgr decides; the kernel only answers how a
device is isolated (it already does) and forwards one boot option it does
not interpret.

## 1. The rule

Before devmgr starts any driver (`main.rs`, the loop at about line 548, and
the restart path at about line 1789), it asks `device_isolation` of the
device, through the handle it keeps:

| answer | devmgr |
|---|---|
| `NO_DMA` (new bit 2, §2) | starts the driver: the node reaches no memory (the firmware framebuffer's node, the x86-64 power node), so there is no DMA to isolate |
| `DMA_TRANSLATED` and `INTERRUPTS_ISOLATED` | starts the driver |
| anything else, **without** the opt-out | refuses the device, with a line naming the bit that is missing and the location, as the GPU's refusal does; the device counts as failed in REPORT |
| anything else, **with** the opt-out | starts the driver, and says so on its own line, at warning level, every time (§3) |

The GPU keeps its own hand-over (mark, isolation, budget). Its isolation step
becomes this shared rule, so there is one decision in one place:
`devmgr-proto::isolation::admit(bits, opt_out) -> Admit`, a pure function
that is host-tested, with every line text.

**What "bus master" means here.** Every node that reaches memory. devmgr
cannot tell from a device's class whether it masters the bus: a USB host,
a NIC, a disk controller, a sound card and a virtio function all do, and
even a device whose class suggests otherwise can be programmed to. The
kernel already knows which nodes reach no memory (`DeviceNode::reaches_memory`),
and says so through the new bit. Every other node is treated as a bus
master.

## 2. The kernel's part: one bit and one forwarded word

1. **`DEVICE_ISOLATION_NO_DMA` (bit 2)**: set when `!node.reaches_memory()`.
   Bits 0 and 1 are unchanged. About 4 lines in `syscall/native.rs`, plus
   the constant in `native-abi`. Requirement `L.device.57`.
2. **The opt-out word**, `ferrix.unisolated` (a flag, registered in
   `build-flags.toml`'s `boot_options`):
   * no certified profile permits it (as `ferrix.efifb`, `ferrix.intelgpu`
     and `ferrix.powerctl`): a boot with it carries no claim, so a claimed
     image is exactly one booted without it;
   * the kernel reads it with `option_in`, prints one line at stage 10 --
     `devmgr   ferrix.unisolated: devmgr may start drivers of devices whose
     DMA is not translated or whose interrupts are not isolated; their
     drivers are trusted with all of memory; this configuration carries no
     claim` -- writes an audit record (a new `audit` event,
     `UNISOLATED_ALLOWED`, kernel subject, at boot), and sets one flag in the
     DEVICES message devmgr reads (`devmgr-proto::Devices`, a `flags` word;
     its first bit `UNISOLATED_ALLOWED`);
   * devmgr reads the flag and nothing else: it never parses the command
     line.

   About 25 kernel lines in `discovery/devmgr.rs` and `audit`. Requirement
   `L.device.58` (forwarded, said, audited) and `L.device.59` (devmgr's rule,
   host-tested, traced to `device_isolation` as `L.device.27` and
   `L.device.56` are).

Nothing else in the kernel changes. A device that devmgr starts under the
opt-out is still confined as far as the hardware allows: a translated
domain where there is one, degraded trusted mode announced at its first
pin where there is not (V-03).

## 3. What a boot says

Without the word, on a machine where a device is not isolated:

```
  devmgr   net 08:00.0 not started: its DMA is not translated (device_isolation 0x0)
```

With it, beside the kernel's stage 10 line, one line per device started
unisolated:

```
  devmgr   UNISOLATED: net 08:00.0 started with device_isolation 0x0 under ferrix.unisolated: its driver is trusted with all of memory
```

The REPORT line counts devices refused and devices started unisolated.

## 4. The transition: which machines and rows lose drivers

This is the part the decision changes most, so it is listed machine by
machine.

| machine | today | with the gate, no word | needs |
|---|---|---|---|
| QEMU x86-64 rows (intel-iommu, intremap=on) | every PCI function behind the translating, remapping unit | unchanged: every node answers 0x3 | nothing |
| `test-nvrm`'s unisolated boot (intremap=off) | only the GPU refused | **every driver refused**, since bit 1 is clear for all | the row stays as it is (it checks the GPU's refusal, and init is `nvrm-hold`); its expectations gain the other refusals, or it boots with the word and judges only the GPU |
| QEMU aarch64 rows (SMMUv3 under ACPI, GICv2m) | translated, interrupts not isolated (bit 1 never set under v2m) | **every driver refused** | decision T1 below |
| QEMU armv7a rows (no SMMU driven) | untranslated, degraded | **every driver refused** | decision T1 below |
| the N150 (two VT-d units, both translating and remapping, 0 bypassing, 0 unresolved: box log 2026-10-24) | 0x3 everywhere | unchanged | nothing |
| nazuna before AMD-Vi | no unit found, every function bypassing | **every driver refused**: the RTL8111H and the SN720's NVMe too | the dev image's GRUB entry carries `ferrix.unisolated` until AMD-Vi lands |
| nazuna with AMD-Vi | translated and remapped, if the root complex and the unit's own function do not count as bypassing (`docs/AMD-VI.md`, decision D10) | unchanged | AMD-Vi |
| the Pixel 7 (crosvm, virtio-mmio), the DK1 (STM32MP157, no IOMMU driven) | untranslated, degraded | **every driver refused** | the word in their dev images, or decision T1 |

**Decision T1, asked of the consultant and the customer.** Every Arm row
loses its drivers without the word, because Ferrix cannot yet isolate
interrupts on GICv2m or translate on ARMv7-A. There are three ways:

* **(a)** Arm's claimed rows boot with the word and lose their claim until
  Arm isolation exists. That is honest, but the Arm evidence stops
  counting.
* **(b)** A dated exception, as `present`'s `personality=kernel` has: the
  `present` profile permits `ferrix.unisolated` on aarch64 and armv7a only,
  and V-03's degraded residual (already stated for both) covers it. It
  ends when each architecture isolates: GICv3 ITS rows and an SMMU on
  ARMv7-A.
* **(c)** On aarch64 alone, gate on DMA_TRANSLATED only, leaving
  interrupts' residual as V-03 states it today. Its evidence then shows
  translation, where it exists.

The recommendation is (b), as the least change to what is claimed today:
the claim already carries Arm's degraded residual, and x86-64 gets the full
rule at once.

## 5. Checks and controls

* Host (`devmgr-proto`): `admit` for every bit combination, with and
  without the opt-out, and every line. Control: `admit` ignoring bit 0, so
  its test fails.
* Boot, x86-64 KVM: stage 10's check reads `NO_DMA` set for the
  framebuffer's or the power node, and clear for every PCI function
  (`L.device.57`). Control: the bit never set, so the check panics.
* Boot, x86-64 with `--unisolated-interrupts` (intremap=off, as
  test-nvrm's unisolated boot runs it): devmgr refuses every device by
  name. With the word added it starts them, with the UNISOLATED lines and
  the kernel's line and audit record (`L.device.58`, `L.device.59`).
  Controls: the flag not forwarded, so the second boot refuses; devmgr
  ignoring the flag, the same.
* Rows: `cargo xtask check`; test-boot x86_64 KVM, aarch64, armv7a
  `--smp 2` (with T1's choice); test-net; the disk rows; test-nvrm.

## 6. Requirements (reserve `L.device.57-59`)

| id | statement in short |
|---|---|
| 57 | `device_isolation` answers bit 2 for a node that reaches no memory |
| 58 | `ferrix.unisolated` is forwarded to devmgr in DEVICES' flags, said on a line, and audited; no certified profile permits it |
| 59 | devmgr starts a driver of a node that reaches memory only with bits 0 and 1, or under the forwarded opt-out, with a line for every device it starts so |

## 7. Kernel lines

| file | lines (est.) |
|---|---|
| `syscall/native.rs` (bit 2) | +4 |
| `discovery/devmgr.rs` (read, say, forward) | +20 |
| `audit` (the event) | +6 |
| `iommu/domain_check.rs` (the check) | +40 (check code) |
| ring 3: `devmgr-proto` (`admit`, the flags word), `devmgr` (the call at both start paths) | +150 |

## 8. Handed to other sessions

* **The bare-metal preparation for nazuna** (SN720 slot B): the dev image's
  GRUB entry needs `ferrix.unisolated` until AMD-Vi lands, or nazuna's NIC
  and NVMe drivers are refused once this lands. With AMD-Vi, no word.
* **The NIC agent** (nazuna's RTL8111H): the same word, for the same
  reason.
* The N150 needs nothing.

## 9. Decisions asked of the consultant

* **G1** the rule of §1, and `admit` as its one place.
* **G2** `NO_DMA` as bit 2, from `reaches_memory`.
* **G3** `ferrix.unisolated`: its name, permitted by no certified profile,
  forwarded as a DEVICES flag, said and audited by the kernel.
* **G4** T1: (a), (b) or (c).
* **G5** test-nvrm's unisolated row: new expectations, or booted with the
  word.
* The ids `L.device.57-59`.

## Consultant verdict

2026-10-24, design review before code (ledger
`~/.local/share/ferrix/cert-consultant/reviews.md` line 3906; the customer's
decision is recorded at 3905): **OK IF (1)-(9)**. This is an internal
review, not independent verification (F-27).

1. **One choke point.** `admit` runs where devmgr gives a device handle to
   any program, not once per start loop.
   * The send takes the `Admit` value, so no path can skip it.
   * The as-built lists every hand-off, each with a host test or a
     check-script entry: start, restart, T0 restarts, snd restart, the GPU
     `hand_over`, the efifb lend, and any handle passed on to a child.
2. **Fail closed.**
   * An unreadable `device_isolation` refuses the device.
   * A DEVICES message without the flags word, or with an unknown flag,
     gives no opt-out.
   * Only the bare word counts; any value form is said and ignored.
3. **`NO_DMA` exact.** Bit 2 is set iff `!reaches_memory`. L.device.57's
   check also shows that such a node has no domain and is refused
   `vmo_pin`. The control (the bit never set) must read FIRED.
4. **The word.**
   * The kernel reads it with `option_in`.
   * Its kernel line and the `UNISOLATED_ALLOWED` audit record both come
     before devmgr starts, and the audit event is registered where audit
     events are listed.
   * The DEVICES flags word gets an encode/decode host test.
   * Controls, each FIRED: the flag not forwarded; the kernel line or the
     audit record suppressed.
5. **devmgr's lines.** One `UNISOLATED` line per device started that way,
   and REPORT counts. `admit` is host-tested over every combination of bits
   0-2, with and without the opt-out, and every line text. The control
   (bit 0 ignored) must read FIRED.
6. **Profiles.**
   * No certified profile permits the word (except as G4 decides), and the
     registry check refuses it, with a control.
   * SAFETY-MANUAL lists the opt-out as outside the reference configuration.
   * ITEM.md and RUNTIME-SETTINGS list the option.
   * V-03 is rewritten.
7. **G4 is the customer's.** It amends their decision. It is recorded with
   end conditions in CLAIM.md and ITEM.md.
8. **G5.** The intremap=off boot keeps no word, and its expectations name
   every refusal. The with-word boot is a row of its own.
9. **Process.**
   * Reserve L.device.57-59 first, in a docs-only landing.
   * Land after 491223854.
   * Controls via `gate.sh control --expect`, each FIRED on the landing
     patch.
   * Rows: check, x86_64 kvm, aarch64, armv7a `--smp 2`, test-net, the disk
     rows, test-nvrm.
   * Before landing, tell the §8 sessions and the Pixel 7 and DK1 owners.
     Their dev images carry the word.

**Decisions**

* **G1:** OK if (1) and (2).
* **G2:** OK if (3).
* **G3:** OK if (2) and (4).
* **G5:** per (8).
* **Ids:** confirmed.

**G4 (T1), the consultant's view, for the customer:**

* **aarch64: (c).** Gate on `DMA_TRANSLATED` alone, dated until GICv3 ITS
  rows exist. The SMMU already translates there, so (c) keeps a real guard,
  and it needs no per-architecture boot-option permit.
* **armv7a: (b), dated until an SMMU is driven, or (a).** (b) needs a
  per-architecture permit, which `build-flags.toml` does not have today.

If the customer wants one rule for both, (b) is acceptable, with its end
conditions written.

## Code review

2026-10-25, feature/devmgr-isolation-gate at 1361cf72f (ledger line
4153): **OK IF (1)-(7)**. G4 is confirmed as the customer decided it:
aarch64 takes (c), and armv7a takes (a), out of the claim. The conditions,
and how this branch meets them:

1. `Admitted` is neither Copy nor Clone. It carries the device's location,
   and `start` refuses an admission made for another device (host test
   `an_admission_admits_its_own_device_alone`).
2. The bare word and a valued form together are said, and give no opt-out
   (`admission::opt_out`, the kernel's `read_unisolated`; host test
   `the_opt_out_is_the_bare_word_alone`).
3. A recovery boot with `ferrix.vtd=off` or `ferrix.amdvi=off` needs
   `ferrix.unisolated` too (RUNTIME-SETTINGS, SAFETY-MANUAL AoU-8). The PO
   has been told about the GRUB recovery entries of the N150 and nazuna.
4. That no driver passes a device handle on to a child is argued, not
   checked (below).
5. Controls, each FIRED on the landing patch: the six planned, plus (1)'s
   mismatch and (2)'s conflict.
6. Rows: check, test-boot x86_64 kvm, aarch64 and armv7a `--smp 2`,
   test-net, the disk rows, test-nvrm and test-power. It lands after
   AMD-Vi.
7. The commit message cites 3905, 3906 and 4153.

**Final review** (2026-10-25, at 4944e918c; ledger line 4180): **OK**.

Pool round dg3 (logs `~/.local/share/ferrix/logs/queue/dg3-*.log`), every
one on the landing patch:

* **Rows, PASSED:** check; test-boot x86_64 kvm, aarch64 and armv7a
  `--smp 2`; test-net; test-nvme; test-ab-boot; test-nvrm; test-power.
* **Controls, FIRED:**
  * `bit0`, `admits` and `conflict`, each by its host test;
  * `present`, by the profile rule;
  * `nodma`, by `check_no_dma` on test-power;
  * `forward`, by devmgr's report reading 0 started and 3 failed on
    ARMv7-A;
  * `line`, by xtask's "never said";
  * `audit`, by `kernel_store`.

## As built

Branch `feature/devmgr-isolation-gate`, on `feature/amd-vi`.

**The rule** is `devmgr-proto::admission::admit(bits, aarch64,
unisolated_allowed) -> Admit`. Its `Admitted` value has a private field, so
only `admit` can make one, and `devmgr`'s `start` takes it by value. Every
path that gives a device handle to a program goes through `start`
(condition (1)):

| hand-off | path | admitted by |
|---|---|---|
| boot start | `run`'s loop, `start` | `admit` before `start`; test-boot rows |
| restart after a death, T0 driver restarts, snd restart, BIND through sysfs, an update's new image and its fallback | `launch_again`, `start` | `admit` before `start`; test-restart, test-snd-restart |
| the GPU | `hand_over_gpu` (mark, both bits, budget), then `start` | both, so a GPU needs both bits on every architecture; test-nvrm |
| the efifb lend | the framebuffer's node, `start` | `NO_DMA` admits it; the firmware-screen boot |
| a keeper's worker (`Plan::Engine`) | `start_keeper` from `start` | as `start` |
| `drvupdated`, `devmgr-events` | `spawn_updater`, `spawn_events` | given no device handle |

**Argued, not checked** (ledger 4153 (4)): `start` is devmgr's only path
that sends a device handle. The only `write_with` that carries handles is in
`launch_in`; `launch` and `launch_in` are called only from the `start_*`
functions and `start_keeper`; and those are called only from `start`. A
driver may pass its own handle on to a child it starts, but that child runs
in the driver's job, with the device's rights and no more, and the device
was admitted already. The admission is of the device, and what the
driver's job does with it stays inside that device's own domain and
interrupt routes. A refused device's handle is never sent at all.

**Deviations from the design**

* The audit record is a configuration item, `Config::Unisolated` (10), and
  not a separate event. `start_audit` records it with the other items,
  before devmgr starts, and `audit::check::kernel_store` reads it back.
  `BOOT_RECORDS` is now 12 and `CONFIG_ITEMS` 10, and H.AUD.2 says "twelve".
* devmgr's REPORT message is unchanged. A refused device counts as failed
  in it. The devices started under the word are counted on devmgr's own
  line, `devmgr   N devices started under ferrix.unisolated: ...`, which
  test-boot holds equal to the number of `UNISOLATED` lines.
* devmgr's one architecture difference, translation alone on AArch64, is
  `arch::AARCH64` behind the program's own `src/arch/` facade, as the
  layering rule asks. The count of unisolated starts is kept on each
  `Started` entry and said by `report`. `start` uses the admission up
  (`Admitted::starts`).
* The `UNISOLATED` line does not print the bits. Its text is
  `devmgr   UNISOLATED: <kind> <bus:dev.fn> started under
  ferrix.unisolated: its DMA is not translated or its interrupts are not
  isolated, and its driver is trusted with all of memory`.
* The registry check that refuses the word in a certified profile is a key
  of the word's own registry entry, `claimless = true`, read by
  `check-item-boundary.py`'s profile rule, with a self-test.
* G4 (a) for ARMv7-A takes it out of both claimed-architecture lists.
  xtask writes the word into every ARMv7-A image (`arch_cmdline`) and into
  a board card's `DEFAULTS.TXT` (the DK1). The Pixel 7 loader puts the word
  in both its command lines, the phone's and the crosvm guest's. An AArch64
  image booted with `FERRIX_ARM_MACHINE=acpi=off` carries it too: the
  device-tree path brings no SMMU up.

**Checks and controls**

| what | check | control |
|---|---|---|
| L.device.57 | stage 10's `check_no_dma` asks every node: bit 2 exactly where nothing reaches memory, each such node with no domain and refused `vmo_pin` `WRONG_TYPE` | the bit never set, on test-power's boot, which has the power node |
| L.device.58 | the DEVICES flags host test (an unknown flag gives no opt-out); `kernel_store` reads `Config::Unisolated`; test-boot's `unisolated_problem` requires the kernel's line on ARMv7-A | the flag not forwarded; the kernel line suppressed; the audit item recorded 0 |
| L.device.58, registry | `check-item-boundary.py` refuses a certified profile that permits the word | the word added to `present.toml` |
| L.device.59 | `admission_tests.rs` over bits 0-2, both architectures, with and without the opt-out, and every line; test-boot's `unisolated_problem`; test-nvrm's intremap=off boot requires a refusal for every device reported failed | `admit` ignoring bit 0 |
