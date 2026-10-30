# Machine check on AMD: the vendor's own rules, and its bank names

Design version 2, 2026-10-30, branch `feature/mce-amd`. Item 1 of the
customer's eight hardware-error follow-ups (2026-10-30: "go for all items
on the list then start with 1, 6, 7"). It builds on `docs/MACHINE-CHECK.md`
(landed d787815b1; the consultant's ledger 4772, 4795, 4810), which
classifies every record by Intel's rules. Version 1 (19b56cf8c) had the
consultant's design review OK IF A1-A13 (ledger 4820, §7); this version
carries A2-A5 and A8 (A1); "Changes from version 1" says what changed.

## 1. Why

The customer's machine is an AMD Ryzen 9 9900X (Zen 5, SMCA). AMD reports
no `MCG_SER_P`, so `decode::classify` calls every uncorrected record fatal
there: fail-closed and correct, but every consumed poisoned page stops the
kernel, where Linux ends only the program. And the `mce:` lines print raw
registers with no word for which part of the core they came from, while on
an SMCA part each bank says what it is (`MCA_IPID`): L2 cache, load-store
unit, memory controller. For an overclocker that is the difference between
"core 3's L2" and a hex number.

Bit 55 also means something else on AMD: task context corrupt (`TCC`), not
Intel's `AR`. Today's code never reads it as `AR` on AMD only because
`MCG_SER_P` is absent; this design makes the vendor decide, explicitly.

## 2. AMD's rules

**What this follows** (A2). Linux 6.18.54 (the local tree,
`/home/sebastian/crp123i-port/clean/linux-6.18.54`): `mce_severity_amd`,
`mce_usable_address`/`amd_mce_usable_address`, `smca_extract_err_addr` and
`smca_configure` in `arch/x86/kernel/cpu/mce/`. Two rules 6.18 does not
have, each with its source:

* **`PADDRV`** (`MCA_STATUS` bit 54, supported where `MCA_CONFIG` bit 11
  says so): Linux commit 821f5fe4dbcb, "x86/mce: Add support for physical
  address valid bit" (2025-11-18, after 6.18): "PhysAddrValidSupported bit
  (MCA_CONFIG[11]) serves as the architectural indicator and states if
  PhysAddrV bit (MCA_STATUS[54]) is Reserved or if it indicates validity of
  SPA in MCA_ADDR ... Use and prefer MCA_STATUS[PhysAddrV] when checking
  for a usable address." The commit names Zen 6 as the first part with
  it; the customer's Zen 5 takes the `Poison` path.
* **`TCC`** (bit 55): no Linux source -- 6.18 does not test it. It follows
  the consultant's ruling (b) (ledger 4820): the task's context is lost, so
  from ring 3 the task is killed by `MACHINE-CHECK.md`'s `RIPV`-clear path
  (handler never run, frame retired); in ring 0, or with `EIPV` clear, it
  stops. This is strictly no more permissive than 6.18, which would deliver
  the signal.

The kernel reads, once per processor at bring-up, `CPUID 8000_0007h:EBX`:
bit 0 `OverflowRecov`, bit 1 `SUCCOR`, bit 3 `SMCA`; the raw value goes on
the boot line.

**The vendor** (ruling (a)): AMD's rules for `AuthenticAMD` and
`HygonGenuine`; Intel's for `GenuineIntel` and Zhaoxin's `"  Shanghai  "`;
every other string -- `CentaurHauls`, a hypervisor's own, anything unknown
-- gets **no recovery**: corrected records are logged, every uncorrected
record stops, whatever `MCG_SER_P` says (QEMU sets it on every model, AMD
ones included). Said once at bring-up.

**The class of one AMD record** (6.18's `mce_severity_amd`):

| Record | Class |
|---|---|
| `VAL` clear | empty |
| `PCC` | fatal |
| `Deferred` (bit 44) | deferred: uncorrected, nothing consumed it |
| `UC` clear | corrected |
| `UC`, `OVER`, no `OverflowRecov` | fatal |
| `UC`, no `SUCCOR` | fatal |
| `UC`, otherwise | action required (AMD) |

**The action for an AMD record in a machine check** (A4). AMD's
action-required class is its own (`Class::AmdAr`), and never takes
`action()`'s `Unaffected` arm: AMD's machine check is not broadcast, and a
record left valid on an `EIPV`-clear return would be a fail-open.

| Class | Where, `MCG_STATUS`, `TCC` | Action |
|---|---|---|
| corrected, deferred | any | leave for the poll |
| AMD action required | ring 3, `EIPV` clear | stop |
| AMD action required | ring 3, `EIPV`, no usable address | stop |
| AMD action required | ring 3, `EIPV`, usable, `RIPV`, `TCC` clear | signal: `SIGBUS BUS_MCEERR_AR`, the handler runs |
| AMD action required | ring 3, `EIPV`, usable, `RIPV` clear or `TCC` | kill (the handler never runs) |
| AMD action required | ring 0 | stop (`copy_mc` is item 2) |
| fatal | any | stop |

**A usable address** (A3): `ADDRV` first, always. Then, on an SMCA bank
whose bring-up `CONFIG` read succeeded with bit 11 set, `STATUS.PADDRV`
(821f5fe4dbcb); otherwise the `Poison` bit (43), as 6.18 on SMCA; on a part
without SMCA, `ADDRV` and `Poison`, with no cut, the frame containing the
address (6.18's northbridge bank-4 case is pre-Zen and refused). On SMCA
the address is cut to its valid bits, as `smca_extract_err_addr`: where the
bank's `CONFIG` bit 8 says the lsb is held in `STATUS` 29:24, the address
is bits 56:lsb; otherwise lsb is `MCA_ADDR` 61:56 and the address is bits
55:lsb. A cut lsb above 12 names no one page: not usable.

**A deferred record** is logged by the poll. Its frame is retired only
where `PADDRV` (sourced above) makes the address physical; never by
`Poison` alone (6.18's comment: Poison does not cover deferred errors) and
never on a part without SMCA. Under QEMU, which has no SMCA, a deferred
record is "logged, not retired".

**McaX** (A5, decided: set). Linux 6.18's `smca_configure` writes
`MCA_CONFIG` bit 32 on every SMCA bank, its comment: "OS is required to set
the MCAX bit to acknowledge that it is now using the new MSR ranges and new
registers under each bank. ... If the bit is not set, uncorrectable errors
will cause a system panic." The customer's 9900X is SMCA, so without it no
recovery is reachable there. Bring-up sets bit 32 and nothing else -- the
value read, with bit 32 or'd in, written back through the checked write
(`L.x86_64.240`). It does not set Linux's `DeferredIntType` (bits 38:37):
the deferred interrupt is item 4's.

## 3. What changes in the kernel

* **`decode.rs`**: `Vendor` (`Intel`, `Amd { succor, overflow_recov,
  smca }`, `NoRecovery`) and `vendor_of(cpuid 0 bytes)`; `classify_amd`,
  `amd_action`, `usable_address_amd`, `cut_address`; `Class::Deferred` and
  `Class::AmdAr`. Intel's `classify`, `usable_address` and `action` are
  unchanged and called only for `Vendor::Intel`; `NoRecovery` maps every
  uncorrected record to fatal. All pure, tabled in stage 3 (A7).
* **Bring-up** records the vendor and the `8000_0007h` bits in the
  processor's record. On an SMCA part, one new function, `smca_bank_up`,
  reads each bank's `MCA_CONFIG` (`0xC000_2004 + 0x10b`) and `MCA_IPID`
  (`+5`) through the checked read, keeps `CONFIG` bits 8 and 11 and the
  IPID per bank, and writes `CONFIG` with bit 32 set through the checked
  write. A refused read or write leaves that bank's address unusable and
  its IPID 0, and is said; it does not refuse the processor (the legacy
  `0x400` registers the kernel polls are AMD's aliases of the SMCA ones).
* **`SYND`** (`+6`) is read only for a valid bank with `STATUS.SYNDV`
  (bit 53) whose bring-up reads succeeded, through the checked read, in
  the poll and in the stop report (the report already reads `MCG_STATUS`
  checked, `report()`'s precedent).
* **Lines**: an SMCA bank's poll and report lines gain `ipid=<hex>` (from
  bring-up's copy) and, with `SYNDV`, `synd=<hex>`. `deferred` joins the
  classes the poll logs.
* **No word for the bank in the kernel.** The `(HWID, McaType)` to name
  table (Linux's `smca_hwid_mcatypes`) is lode's: lode reads `ipid=`, takes
  `HWID` (43:32) and `McaType` (63:48), and adds `bank_type=` to the
  record (`src/lib/log/lode`, ring 3), with its own unit test.
* **`check-device-access.py`** allows, in `mce.rs`'s `smca_bank_up` alone,
  the checked reads of offsets 4 and 5 and the checked write of offset 4;
  in the poll's and the report's SYND read, offset 6, read only. Nothing
  else may touch `0xC000_2000`+.

## 4. What does not change

Intel's path and its gate rows keep their meaning: the vendor decides which
classifier runs. AMD's deferred-error interrupt and `MCA_DESTAT` are not
used (item 4, with CMCI); the poll finds deferred records in `MCA_STATUS`
within its period.

## 5. Requirements (A9: reserve `L.x86_64.233`-`242`)

One check each:

* **.233** the vendor and its rules: AMD for `AuthenticAMD`/`HygonGenuine`,
  Intel for `GenuineIntel`/`"  Shanghai  "`, no recovery for every other
  string; the `8000_0007h` bits recorded and printed (stage 3's vendor
  table; the boot line).
* **.234** AMD's classifier (stage 3 table).
* **.235** AMD's usable address and the SMCA cut (stage 3 table).
* **.236** AMD's action in ring 3: stop on `EIPV` clear or no usable
  address, signal on `RIPV` with `TCC` clear, never the bystander arm
  (stage 3; `test-mce-amd-srar`, `-amd-eipv-clear`).
* **.237** `TCC` or `RIPV` clear from ring 3 kills (`-amd-tcc`,
  `-amd-kill`).
* **.238** any AMD uncorrected record in ring 0 stops (`-amd-ring0`).
* **.239** SMCA `CONFIG`/`IPID` read at bring-up through the checked read,
  a refusal said and not fatal; `SYND` only with `SYNDV` (argued from the
  code; check-device-access.py's allowance).
* **.240** McaX set, nothing else in `MCA_CONFIG` changed, through the
  checked write (the pure `with_mcax` tabled; the write argued).
* **.241** `ipid=` and `synd=` on the lines (the pure line builder tabled).
* **.242** a deferred record logged, retired only by `PADDRV`
  (`-amd-deferred`: logged, not retired).

Restated: **.221** (bring-up records the vendor and bits), **.224** and
**.225** (Intel's classifier and the bystander rule scoped to Intel-rule
vendors), **.228** (per vendor). `H.FAIL.8` per A10.

## 6. How it is checked

* **Stage 3**: tables for `vendor_of`, `classify_amd`, `amd_action`
  (every row of §2's, `EIPV`/`RIPV`/`TCC` flipped, the bystander row
  that must not return), `usable_address_amd` (`ADDRV` dropped, `Poison`,
  `PADDRV` with and without `CONFIG` bit 11, non-SMCA), `cut_address`
  (lsb 12, 0, 13, 21; status-held), `with_mcax`, and the line builder.
* **The vendor matrix** (A8). `xtask`'s `qemu64` says `AuthenticAMD` under
  TCG *and* KVM, so every mode pins its vendor and bits in its own `-cpu`:
  * **Intel** (`vendor=GenuineIntel`): today's eight modes, unchanged. The
    broadcast mode stays Intel-only: QEMU refuses a broadcast injection
    for a non-Intel vendor.
  * **AMD** (`vendor=AuthenticAMD,+succor,+overflow-recov`, and the
    variants named), each a `test-mce-amd-<mode>`: `srar` (signalled,
    `err=0x0`), `kill` (`RIPV` clear), `tcc` (`TCC` set: killed), `eipv`
    (`EIPV` clear: stops), `pcc` (stops), `nosuccor` (`-succor`: stops),
    `over` (`OVER`, `-overflow-recov`: stops), `deferred` (logged, not
    retired), `ring0` (injected while the processor is in ring 0: stops),
    `corrected` (logged once), `recover-off` (stops).
  * QEMU sets no `SMCA` bit, so the SMCA branch is never entered in any
    gate: the reads and the McaX write are argued from the code, the
    pure functions over their values tabled, and the coverage
    justification names the unreached reads (A7). Hygon has no model in
    any gate either; it takes AMD's rules by the vendor table alone.
* **Where AMD recovery runs.** QEMU's TCG does not offer `SUCCOR` or
  `OverflowRecov` (their feature word's `tcg_features` is 0: "TCG doesn't
  support requested feature ... succor"), so under TCG every AMD mode that
  would recover requires the fail-closed stop instead. KVM on nazuna's AMD
  host passes both. Measured on 1bdb00dcf by `gate.sh`
  (`~/.local/share/ferrix/logs/queue/amd-1bdb00dcf-amd-srar-kvm.log`):
  `cpuid 80000007 ebx=0x3`, recovery reported, the victim's handler
  printed `code=4 ... lsb=12 err=0x0`; under TCG (`...-amd-srar-tcg.log`)
  `ebx=0x0`, recovery not reported, stopped as FX-9005. So AMD recovery is
  shown under KVM, on the host's own AMD processor, and the fail-closed
  rules under TCG.
* **lode**: a unit test over IPIDs for Linux's types and an unknown one,
  and over lines with and without `ipid=`.
* **Hardware**: a BACKLOG row for the customer's Ryzen 9 9900X, run only if
  the customer chooses a bare-metal boot: bring-up's `ipid=`, McaX and
  `PADDRV` state, and any corrected record; recovery on hardware only with
  an injector (ACPI EINJ), whether the board has one said.
* **Negative controls** (A12), each `gate.sh control --expect`: `SUCCOR`
  ignored (the `nosuccor` row fails); the vendor forced to Intel on an AMD
  model (stage 3's vendor table); the cut removed (stage 3's table); `TCC`
  treated as resumable (the `tcc` row fails); AMD's class let into the
  bystander arm (the `eipv` row fails); `ADDRV` dropped (stage 3); an
  unknown vendor given Intel's rules (stage 3's vendor table).

## Changes from version 1

The rules' sources named (A2), `TCC` per ruling (b) instead of "fatal",
`ADDRV` first and deferred retired only by `PADDRV` (A3), the ring-3 rule
completed with its own class (A4), McaX set (A5), the vendor table per
ruling (a), §6's SMCA wording corrected and every mode pinning its vendor
(A8), and ten requirements instead of eight (A9).

## 7. Where it stands

Design v1 written (19b56cf8c). **Design review: OK IF A1-A13** (the
consultant's ledger line 4820, `~/.local/share/ferrix/cert-consultant/reviews.md`,
2026-10-30; internal review, not independent verification, F-27). Checked
against Linux 6.18.54: its `mce_severity_amd` has no `TCC` test, and its
`amd_mce_usable_address` requires `ADDRV` and then, on SMCA, `Poison` alone
(no `PADDRV`); Linux sets `MCA_CONFIG.McaX`, which this design does not.

* Rulings: (a) AMD's rules for `AuthenticAMD` and `HygonGenuine`, Intel's
  for `GenuineIntel` and Zhaoxin's `  Shanghai  `; every other vendor
  string gets no recovery whatever `MCG_SER_P` says. (b) `TCC` from ring 3
  (`EIPV` set, usable address) kills the task by the `RIPV`-clear path,
  handler never run; `TCC` in ring 0 or with `EIPV` clear stops. (c) the
  matrix accepted, each mode pinning its vendor and bits. (d) SMCA argued,
  on a pure split tabled in stage 3. (e) `H.FAIL.8` widened as A10 words it.
* A1 a design v2 on this branch before code, carrying A2-A5 and A8.
* A2 sources named for every rule Linux 6.18 does not have (`PADDRV`,
  `CONFIG` bit 11, `TCC`), else 6.18's rule; the status-held cut 56:lsb.
* A3 `ADDRV` always first; `Poison`, or sourced `PADDRV`; a deferred record
  retired only by `PADDRV`, so under TCG logged, not retired.
* A4 AMD's action-required class never takes the bystander (`Unaffected`)
  arm; ring 3: `EIPV` clear or no usable address stops, `RIPV` set and
  `TCC` clear signals, `RIPV` clear or `TCC` kills; ring 0 stops.
* A5 `McaX`: set through the checked write (allowance, requirement,
  control), or left clear and recovery not offered on SMCA parts, said in
  §2 and AoU-36.
* A6 SMCA reads only through the checked read; `IPID` cached at bring-up;
  `SYND` only with `SYNDV` on a bank whose bring-up reads succeeded; the
  device-access allowance one function, offsets 4/5/6, reads only.
* A7 pure functions tabled in stage 3; §6 corrected (qemu64 never enters
  the SMCA branch); BACKLOG row for the Ryzen 9 9900X, run only on the
  customer's bare-metal choice; recovery there only with an injector.
* A8 every `test-mce` mode pins vendor and bits; the AMD modes listed in the
  ledger line; broadcast Intel-only; the KVM `CPUID 8000_0007h` measurement
  first, hashed, recorded here; BACKLOG line 48 updated.
* A9 ids reserved from `L.x86_64.233` as many as one check each needs
  (likely .233-.242); .221, .224, .225, .228 restated.
* A10 `H.FAIL.8` and AoU-36 per A4/A5/A7; `ferrix.mce.recover=off` shown on
  an AMD-rule mode.
* A11 VULNERABILITY-ANALYSIS: `ipid=`/`synd=` to privileged kmsg only; new
  inputs bounded.
* A12 controls: §6's three, plus `TCC` resumable, AMD class let into the
  bystander arm, `ADDRV` dropped, an unknown vendor given Intel's rules.
* A13 code review against ledger 4820 before take; rows as 4795 V4 plus
  A8's modes; coverage carried; commits cite 4820 and 4772.

Design v2 written (A1), carrying A2-A5 and A8, for the consultant's check
at the code review.
