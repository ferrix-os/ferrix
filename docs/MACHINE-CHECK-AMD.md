# Machine check on AMD: the vendor's own rules, and its bank names

Design version 1, 2026-10-30, branch `feature/mce-amd`. Item 1 of the
customer's eight hardware-error follow-ups (2026-10-30: "go for all items
on the list then start with 1, 6, 7"). It builds on `docs/MACHINE-CHECK.md`
(landed d787815b1; the consultant's ledger 4772, 4795, 4810), which
classifies every record by Intel's rules.

## 1. Why

The customer's machine is an AMD Ryzen 9 9900X. AMD reports no
`MCG_SER_P`, so `decode::classify` calls every uncorrected record fatal
there: fail-closed and correct, but every consumed poisoned page stops the
kernel, where Linux ends only the program. And the `mce:` lines print raw
registers with no word for which part of the core they came from, while on
an AMD part with SMCA each bank says what it is (`MCA_IPID`): L2 cache,
load-store unit, memory controller. For an overclocker that is the
difference between "core 3's L2" and a hex number.

Bit 55 also means something else on AMD: task context corrupt (`TCC`), not
Intel's `AR`. Today's code never reads it as `AR` on AMD only because
`MCG_SER_P` is absent; this design makes the vendor decide, explicitly.

## 2. AMD's rules (Linux's `mce_severity_amd`, AMD APM vol. 2 §9.3, PPR for family 19h/1Ah)

The kernel reads, once per processor at bring-up, `CPUID 8000_0007h:EBX`:
bit 0 `OverflowRecov`, bit 1 `SUCCOR` (software uncorrectable error
containment and recovery), bit 3 `SMCA` (scalable MCA). Status bits that
differ from Intel's: 55 `TCC`, 54 `PADDRV` (SMCA), 44 `Deferred`, 43
`Poison`.

| Record | AMD class | Action |
|---|---|---|
| `VAL` clear | empty | none |
| `PCC` | fatal | stop |
| `Deferred` | deferred: uncorrected, nothing consumed it yet | the poll logs it and retires its frame where the address is usable (as UCNA) |
| `UC` clear | corrected | the poll logs and counts it |
| `UC`, `OVER`, no `OverflowRecov` | fatal | stop |
| `UC`, no `SUCCOR` | fatal | stop |
| `UC`, `TCC` | fatal | stop (the interrupted task's own context is lost; Linux's `error_context` reaches the same through `RIPV`/`EIPV`) |
| `UC`, `SUCCOR`, from ring 3, `RIPV` and `EIPV`... | action required | §3 |
| `UC`, `SUCCOR`, from ring 0 | fatal | stop (no `copy_mc` yet: item 2) |

AMD's machine check is not broadcast, so the bystander rule of
`MACHINE-CHECK.md` §3.3 does not arise; it stays as written, unused.

**A usable address** (Linux's `amd_mce_is_memory_error` and
`amd_mce_usable_address`): on an SMCA part, where the bank's `MCA_CONFIG`
bit 11 (`PADDRV` supported) is set, `STATUS.PADDRV`; otherwise the
`Poison` bit; on a part without SMCA, the `Poison` bit alone (Linux's
northbridge bank-4 special case is a pre-Zen part: refused here). The
address in `MCA_ADDR` is cut to its valid bits: the least significant bit
in `MCA_ADDR` 61:56 (or `STATUS` 29:24 where the bank says so, Linux's
`lsb_in_status`), the address being bits 55:lsb; a recoverable lsb above
12 names no one page and is not usable, as on Intel.

## 3. What changes in the kernel

* **`decode.rs`** gains `Vendor { Intel, Amd { succor, overflow_recov, smca } }`
  and `classify_amd`, `usable_address_amd`; `action` takes the vendor's
  class. Intel's functions are unchanged. A vendor that is neither
  (Hygon is AMD's family 18h and takes AMD's rules; Zhaoxin, Centaur and
  anything else take Intel's when `MCG_SER_P` says so, and fatal for every
  uncorrected record otherwise) is said at bring-up.
* **Bring-up** records the vendor in the processor's record, and on an
  SMCA part reads each bank's `MCA_CONFIG` (`0xC000_2004 + 0x10b`) and
  `MCA_IPID` (`+5`) through the checked read: the `PADDRV` bit into a
  per-processor bank mask, the IPID kept for the log. A refused read
  leaves the bank's address unusable and its IPID 0, and is said; it
  does not refuse the processor (the legacy `0x400` registers it polls
  are AMD's aliases of the SMCA ones, PPR "MCA legacy MSRs").
* **The poll and the report** add `ipid=<hex>` to an SMCA bank's line, and
  `synd=<hex>` where `STATUS.SYNDV` (bit 53) says `MCA_SYND` (`+6`) holds
  one. `deferred` joins the classes the poll logs.
* **No word for the bank in the kernel.** "Keep the kernel slim": the
  table of `(HWID, McaType)` to a name -- L2 cache, LS, UMC, ... (Linux's
  `smca_hwid_mcatypes`, 40 entries) -- is lode's. lode reads the `mce:`
  line's `ipid`, takes `HWID` (43:32) and `McaType` (63:48), and adds
  `bank_type=` to the record (`src/lib/log/lode`, ring 3). The Settings
  app and a reader see "cpu 3 L2 cache corrected"; the kernel stays a
  printer of registers.

## 4. What does not change

Intel's path, its classifier table and every gate row of
`MACHINE-CHECK.md` keep their meaning: the vendor decides which classifier
runs, and nothing else moves. AMD's deferred-error interrupt (an APIC
vector) and `MCA_DESTAT` are not used: the poll finds deferred records in
`MCA_STATUS` within its period, as it finds corrected ones (CMCI is item
4; the deferred interrupt rides with it).

## 5. Requirements (to reserve)

`L.x86_64.233`-`240`: the vendor and its CPUID bits recorded at bring-up;
AMD's classifier over a table; AMD's usable address and its lsb cut;
SMCA's `CONFIG` and `IPID` read through the checked read, a refusal said
and not fatal; `ipid=` and `synd=` on the lines; `deferred` logged and
retired; a ring-3 consumption under `SUCCOR` signalled; without `SUCCOR`
every uncorrected record stops. `H.FAIL.8`'s statement gains "or, on AMD,
an uncorrected record a ring-3 program consumed where the processor
reports SUCCOR". lode's bank names need no item requirement (ring 3,
load); they get lode's own test.

## 6. How it is checked

* **Stage 3**: `classify_amd` and `usable_address_amd` over tables (every
  row of §2's, each bit flipped; the lsb cut for 12, 0, 13 and the
  status-held variant); the vendor choice over `CPUID` strings.
* **The vendor matrix.** The gates' `qemu64` model says `AuthenticAMD`,
  so from this change it takes AMD's rules. Each `test-mce` mode runs
  twice under TCG:
  * **Intel**: `-cpu qemu64,vendor=GenuineIntel,...`, QEMU's default
    `MCG_SER_P`: today's records and expectations, unchanged.
  * **AMD**: `qemu64,+succor,+overflow-recov`: SRAR becomes a record with
    `UC`, `Poison`, `ADDRV`, `MISCV`, no `S`/`AR`; the victim gets
    `SIGBUS BUS_MCEERR_AR`; a `Deferred` record replaces SRAO; without
    `+succor` the same consumption stops as FX-9005 (fail-closed).
  * QEMU emulates no SMCA register: the SMCA reads are refused there (the
    checked read's `#GP`, or 0 under TCG), so `ipid=` is absent and the
    `PADDRV` path is argued from the code plus the lsb-cut table; the
    customer's Ryzen is the hardware run (BACKLOG row, and only when the
    customer chooses to boot Ferrix bare metal on nazuna).
* **KVM on nazuna (AMD)**: whether KVM offers `SUCCOR` to the guest is
  **to measure first** (`CPUID 8000_0007h` in a boot line). If it does,
  the AMD rows run under KVM too; if not, they require the fail-closed
  stop there, as today.
* **lode**: a unit test over IPIDs for each of Linux's 40 types and an
  unknown one; a gate row booting a corrected record with an injected
  `ipid=` line through `/dev/kmsg` is not possible (lode reads the kernel's
  lines), so the unit test is the evidence.
* **Negative controls**: `classify_amd` made to ignore `SUCCOR`, the AMD
  consumption row stops; the vendor choice forced to Intel on an AMD
  model, stage 3's vendor table fails; the lsb cut removed, the table
  fails.

## 7. Where it stands

Design written; waiting for the certification consultant's design review.
