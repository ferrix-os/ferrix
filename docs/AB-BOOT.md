# A/B system slots: a trial boot with a way back

Version 1, written 2026-10-11 by ab-boot for D8 of `docs/DEV-ON-FERRIX.md`
(§4.4 (1), §10). The customer's ask: rebuild and update Ferrix from inside
Ferrix, its kernel included, and a kernel that panics at boot must come back
on the good slot by itself -- in this design, on the next reset, with no
other action (§6.3). This document is the loader's half and the
on-disk formats both halves share; the user-space half (`bootctl`, the
installer's layout, `test-install --slots`) is ab-sys's, built on the crate
named in §3.

**Signed slots are designed, not built** (`docs/SIGNING.md`, 2026-10-24, for
the consultant's review). When they land, a slot header gains version 2
with an LMS/HSS-signed manifest in its second 4 KiB, the loader refuses an
unsigned slot or rescue image where the profile requires signing, and a
TPM 2.0 floor stops older releases. §1, §2, §4 steps 5 and 7, §6, §8 and §9
then change. Until then everything below stands as written: slots verify
by SHA-256 alone, with no authenticity claim.

## 1. What it is, and what it is not

* Two **system slots**, A and B, raw partitions each holding what the ESP's
  `FERRIX/` holds today: a kernel, an initramfs and a command line (§2).
* A **control record** on a small partition of its own (§3): the good slot,
  the trial slot and the tries left, in two checksummed copies.
* The **loader** (`src/boot/common/uefi`) reads the record through UEFI's
  Block I/O protocol, and before it starts a trial slot it takes one try off
  and writes the record back, confirmed by reading it again (§4). With no try
  left, or a trial slot that does not verify, it boots the good slot.
* `bootctl` (ab-sys) writes an image into the idle slot and arms a try
  (`bootctl try`), and marks the running trial good (`bootctl good`).

It is not: an A/B of the loader itself (the loader on the ESP is the fixed
point the scheme stands on; it is changed by the installer, §7); a second
root (one btrfs root is shared, `DEV-ON-FERRIX.md` §9, 4, and §6 below);
secure or measured boot (the slot's hashes detect a torn or wrong write,
they do not authenticate; §8); a way back from a kernel that *hangs* without
a reset (§6.3).

## 2. The slot

A slot is a GPT partition of type **Ferrix system slot**,
`5475b681-82f5-47da-a5a1-55db0e7e2bd3`. Exactly two such partitions on the
disk the loader was started from; **A is the one with the lower GPT entry
index**, B the other. (The SN720: p4 is A, p5 is B, 4 GiB each.)

Raw, no file system: Ferrix has no FAT writer (INSTALLER.md I4), and a raw
slot is a byte copy for `bootctl` and a few block reads for the loader, with
no dependence on firmware binding a FAT driver to a non-ESP partition.

```
byte 0     slot header, 4096 bytes (all little-endian)
             0  magic       "FXSLOTHD"
             8  version     u16 = 1
            10  header_len  u16 = 4096
            12  blobs       u32 = 3
            16  generation  u64   the record sequence bootctl armed it under (informational)
            24  label       [u8; 32]  e.g. "ferrix 0.1.0 a6d863da3", NUL padded (informational)
            56  blob[0]     kernel:   offset u64, length u64, sha256 [u8; 32]   (48 bytes)
           104  blob[1]     initrd:   same; length 0 = none
           152  blob[2]     cmdline:  same; UTF-8, at most 2048 bytes
           200..4064        zero
          4064  seal        sha256 of bytes 0..4064
byte 4096+ the blobs, each starting on a 4096-byte boundary
```

A header **verifies** when: magic, version, `header_len`, `blobs` match; the
seal matches; bytes 200..4064 are zero; every blob starts at or after 4096 on
a 4096 boundary, ends inside the partition, and no two overlap; the kernel's
length is non-zero; the command line is at most 2048 bytes. A **slot
verifies** when its header does and each blob's SHA-256 over the bytes read
matches its entry, and the command line is UTF-8. The loader hashes the bytes
it then hands over -- one read, hashed in memory, no second read between
check and use.

## 3. The control record

A GPT partition of type **Ferrix boot control**,
`c29aa8c5-c19c-4147-838f-01accca28be4`, exactly one on the boot disk, at
least 128 KiB (the SN720's p3 is 1 MiB). It holds **two copies** of a
512-byte record:

* copy 0 at byte 0 of the partition, copy 1 at byte 65536 -- 64 KiB apart,
  so no device with physical sectors up to 64 KiB holds both in one sector;
* each written as one whole logical block of the device (512 or 4096 bytes):
  the record, then zeroes.

```
  0  magic       "FXBOOTCR"
  8  version     u16 = 1
 10  length      u16 = 512
 12  reserved    u32 = 0
 16  sequence    u64   higher is newer
 24  good        u8    0 = A, 1 = B
 25  trial       u8    0 = A, 1 = B, 0xFF = none
 26  tries       u8    tries left for the trial, 0..=7
 27..508         zero
508  crc         u32   CRC-32C (Castagnoli) of bytes 0..508
```

A copy is **valid** when magic, version and length match, the CRC matches,
the reserved bytes are zero, `good` is 0 or 1, `trial` is 0, 1 or 0xFF and
not `good`, `tries` ≤ 7, and `trial` = none implies `tries` = 0.

**Reading:** the current record is the valid copy with the higher sequence;
copy 0 on a tie. No valid copy is *uninitialised* (the SN720's p3 as it is
now, zeroed): see §4's table.

**Writing** (loader and `bootctl` alike): the new record's sequence is the
current one's plus one; it goes to **the copy that does not hold the current
record**; then `FlushBlocks` / `fsync`; then the copy is **read back and
compared byte for byte**. Only a confirmed write counts. A write torn by a
power cut damages only the older copy, so the record reads as if the write
had never happened: the two-copy scheme makes every write atomic, and the
read-back makes a lost write visible to the writer that needs to know.

The formats, the selection, the next-write computation and the decision of
§4 are one `no_std`, `forbid(unsafe_code)`, allocation-free crate,
**`ferrix-slots`** (`src/lib/proto/slots`), with its own CRC-32C, CRC-32
(for the GPT) and SHA-256, used by the loader and by `bootctl`. Its host
tests: round trips, every field's refusal, the selection over every pair of
{valid old, valid new, torn, zero}, a write torn at every byte of the block,
SHA-256 against the FIPS 180-4 vectors, the GPT scan against tables
`ferrix-partition` writes, and the decision table of §4 exhaustively over
the record's states.

## 4. The loader's algorithm

```
1. Find the boot disk: the loader's LoadedImage.DeviceHandle's device path
   ends in a HardDrive node (the ESP); the path before it, located with
   LocateDevicePath(BlockIo), is the whole disk. No HardDrive node (today's
   bare FAT image, `run`, the DK1 card) -> step 7, exactly as today.
2. Read the GPT through that BlockIo: primary header at LBA 1 (signature,
   header CRC-32, MyLBA = 1, sizes), entry array (CRC-32). Find exactly one
   control partition and exactly two slots, each inside the header's usable
   blocks (so the record can never sit on the table itself). The GPT
   is hostile input: a header or entry size or count out of bounds, an
   entry whose first LBA is past its last, and a control or slot partition
   overlapping any other entry are each refused. Anything else -> say
   why, step 7. The loader reads no other disk.
3. Read both record copies; select (§3).
4. Make the attempt list (a pure function, `ferrix_slots::plan`):
      record                          attempts, in order
      uninitialised                   A, B                    (no write, ever)
      trial t, tries > 0              t as TRIAL, good  (no fallback: the
                                      good slot's other is t, unrecorded)
      trial t, tries = 0 (failed)     good only (a failed trial is never a
                                      fallback; the ESP's rescue comes first)
      no trial                        good, other as FALLBACK
5. For each attempt: read the slot header and blobs into memory, verify (§2);
   a slot whose command line with the loader's words does not fit the boot
   info's command-line area does not verify (no truncation).
   Not verified -> say why, free, next attempt. A TRIAL attempt that
   verified: write the record with tries - 1 (§3's write, read back);
   not confirmed -> say so, free, next attempt (the good slot). Only then
   go on with this slot's kernel, initramfs and command line.
6. No attempt verified -> step 7.
7. Today's path: FERRIX/KERNEL.ELF, INITRD.IMG, CMDLINE.TXT, DEFAULTS.TXT
   from the ESP (the rescue path on an installed disk, the only path
   elsewhere). Absent -> FERRIX-PANIC loader: no slot verifies and the ESP
   holds no kernel.
```

The loader **writes nothing but one block of the control partition**, at the
byte offset of one copy, and only in step 5. The block address is computed
by a pure function of the GPT entry, the copy index and the block size, and
checked inside the control partition's range -- a range the scan already
proved overlaps no other entry -- before the call.

**The loader says which path it took**, in one line the gate reads:
`  boot     slot <a|b> <trial|good|fallback> seq <n>` or `  boot     esp`.

**The command line** handed to a kernel started from a slot is the loader's
words first -- the kernel takes the first word with a key, so a slot's own
command line cannot forge them -- then the slot's:

```
ferrix.slot=a|b ferrix.slot.boot=trial|good|fallback ferrix.slot.seq=<sequence> ferrix.slot.disk=<disk GUID> <the slot's command line>
```

`ferrix.slot.disk` is the boot disk's GPT disk GUID, lowercase in its usual
text form, so that `bootctl` writes only the disk the loader booted from
(the consultant's G4, for ab-sys's S1). The longest words -- `fallback`, a
sequence of 2^64 - 1, the GUID -- and the longest slot command line (2048
bytes) fit the boot info's command-line area with room to spare; a host
test holds that against `ferrix-bootinfo`'s `BootInfo`.

`ferrix.slot.seq` is the sequence of the record the loader acted on (the one
it wrote, for a trial). The ESP's `CMDLINE.TXT` and `DEFAULTS.TXT` are not
read on a slot boot: the slot carries the whole command line. The kernel
needs no change: it ignores keys it does not know, and `/proc/cmdline` shows
them to `bootctl`, which reads them first-match, as the kernel's
`option_in` does. The reference configuration's options apply to a slot's
command line as they do to `CMDLINE.TXT` (SAFETY-MANUAL).

Cost: one GPT read, two record reads, one write + flush + read for a trial,
and SHA-256 over the kernel and initramfs (measured in §9's gate, expected
well under a second at hundreds of MB/s).

## 5. `bootctl` (ab-sys builds it; the rules it must keep)

* **Writes only partitions of the two Ferrix types**, found by type GUID on
  the disk holding the running slot, never by name or number -- the guard
  that keeps the SN720's p1 (the host's live swap) and every other
  partition unwritable.
* `bootctl try <image> [--tries N]` (N ≤ 7, default 1): the idle slot is the
  one that is not `good` (never the running slot, never `good`). 1. If the
  record arms a trial of the idle slot, first write it disarmed (trial none,
  tries 0). 2. Write the image to the idle slot, fsync, read back and verify
  it as the loader does (§2). 3. Write the record: trial = idle, tries = N.
* `bootctl good`: allowed only when `/proc/cmdline` says
  `ferrix.slot.boot=trial`, and the record's trial is that slot and its
  sequence is the `ferrix.slot.seq` booted (so a later re-arm is not marked
  by an older boot). Writes good = that slot, trial none, tries 0.
* `bootctl rollback`: takes an armed trial back -- writes the record with
  trial none, tries 0 and good unchanged -- and nothing else. It never makes
  a slot good, whichever slot is running; with no trial armed it writes
  nothing (the consultant's S4, ledger line 732).
* `bootctl status`: the record, both slots' verification, and what booted;
  a trial whose tries are spent is shown as exhausted.
* `bootctl init`: writes a first record (good = the slot that verifies, A if
  both) when there is no valid copy; refuses otherwise.

## 6. Failure cases

| Case | What happens |
|---|---|
| Power lost while the loader writes the decrement | the older copy is torn, the newer still says tries = n; the read-back fails or never happens; the next boot decrements again from n. At worst one try is not counted, never one extra boot of the trial without a write: the trial is started only after a confirmed write |
| Power lost during `bootctl try`, step 1 or 3 | as above: the record is the old one or the new one |
| Power lost during `bootctl try`, step 2 | the record is disarmed (step 1) or never armed, so the half-written idle slot is not tried; were it tried, it would not verify |
| Trial kernel panics at boot | the kernel halts (`panic.rs`); after a reset the record has tries = 0 and the loader boots good. §6.3 |
| Trial boots but never says `good` (userland broken) | same: the next reset boots good |
| Good slot does not verify (bit rot, a bug in `bootctl`), **no trial armed** | the other slot as FALLBACK, then the ESP's rescue files |
| Good slot does not verify, a trial armed or its tries spent | the trial, if armed and verified, after its recorded try; otherwise the ESP's rescue files next -- never the trial slot as a FALLBACK |
| Good slot does not verify, and the other slot is a **failed trial** (trial set, tries 0) | the ESP's rescue files; the failed trial is never started again by the loader (the consultant's A2) |
| Both slots bad, no rescue files | the loader stops with FERRIX-PANIC loader and says which check failed on each slot |
| Record uninitialised or both copies corrupt | A, then B, no write; `bootctl init` repairs |
| The disk lies about flush | outside the design: the read-back catches a write the device did not keep; a device that acknowledges, returns the new bytes and then loses them on power-off is not survived |
| Trial slot damages the root volume | not survived by a slot (one shared root, §6.2) |

### 6.2 One shared root

The kernel unpacks the initramfs onto the btrfs root whenever the archive's
length and CRC-32C differ from the root's stamp (`fs/root_disk.rs`). So a
trial slot's system files overwrite the root's, and the next boot of the
good slot finds its own archive differs from the stamp and puts its files
back; files only the trial carried stay behind. A kernel that panics before
the root switch never touches the root. This works without a kernel change;
a snapshot before a trial is the later answer (`DEV-ON-FERRIX.md` §9, 4).

### 6.3 A panic halts; it does not reset

`panic.rs` halts the processor, and UEFI's watchdog ends at
`ExitBootServices`. So "comes back by itself" means: **on the next reset**
(a person, or a power cycle), with no other action and no rescue medium. The
gate (§9) models that reset as a power cycle of the guest. An automatic
reset after a trial boot's panic -- `ferrix.panic=reset`, which the loader
would add only on a TRIAL boot -- is a change to the core's `panic.rs` and is
**proposed as a separate review, not part of this design**; so is a
hardware watchdog for a trial that hangs rather than panics.

## 7. What it changes, and where

| Where | Change |
|---|---|
| `src/lib/proto/slots` (new) | `ferrix-slots`: §2's header, §3's record, the selection, the next write, §4's plan, the GPT scan, CRC-32, CRC-32C, SHA-256; `no_std`, no `unsafe`, no allocation |
| `src/boot/common/uefi` | Block I/O and Device Path protocol bindings (`uefi/protocols.rs`); `LocateDevicePath` (`tables.rs`); a `slots.rs` doing §4 steps 1-6 through `Services`; `main.rs` takes the kernel, initramfs and command line from a slot or, as today, from the ESP |
| `tools/common/xtask` | a slot image writer (the crate's encoder), `test-ab-boot` (§9) |
| kernel | **none** |
| installer, `bootctl`, `test-install --slots` | ab-sys |

The loader on the ESP is not A/B'd: `bootctl` never writes the ESP. A new
loader is installed by the installer (or by hand), which is a one-way step
exactly as it is today.

## 8. The certified item

The loader is in the reference configuration and **outside the TOE**,
carried by A.FIRMWARE (`SECURITY-TARGET.md` §1.2, §3.3); the unsafe and panic
audits read `src/boot/common/uefi`. Nothing in `src/kernel` changes, so the
item's rings, its coverage and `TRACEABILITY.md` do not move.

What the item's documents gain:

* **A.FIRMWARE** names the slot path: the loader delivers the image of the
  slot the control record selects, verified against the slot header's
  SHA-256 digests, and writes nothing but one block of the control partition.
* **V-12** stays true and gains a sentence: the slot hashes detect a torn or
  misdirected write and are no integrity claim against an attacker -- whoever
  writes a slot writes its header. No secure or measured boot is claimed.
* **SAFETY-MANUAL.md, an assumption of use**: the availability argument --
  a failed update does not leave the machine unbootable, given a reset --
  and its limits (§6: hang, shared root, lying flush); the integrator
  resets a machine whose trial boot stopped.
* **ITEM.md's reference-configuration table**: a row saying the loader
  boots either the ESP's files or a slot the control record selects, and
  that the loader still has **no Cargo feature and no `cfg`** (the controls
  of §9 are source sabotages run by `fleet/gate.sh control`).
* A.FIRMWARE and V-12 also say **who writes a slot**: root, through the raw
  device, who can already overwrite the ESP; the image is verified only
  against its own header, with no authenticity claim. A.FIRMWARE names the
  ESP rescue path.
* The new crate stays **unclassified** in `certification-item.json` (the
  consultant's ruling); its files are under the unsafe/panic audits' roots
  (`src/lib` is).

**The loader's specified behaviour, P1-P5** (no requirement ids, the
consultant's ruling; they are the SAFETY-MANUAL assumption of use's
statement, and §9 names the gate case or control for each):

* **P1** a trial slot is started only after the record with one try fewer
  was written, flushed and read back;
* **P2** with no try left, the good slot (and never the failed trial before
  the ESP's rescue files);
* **P3** a slot is started only if it verifies (§2);
* **P4** the loader writes only the control partition's two record blocks,
  inside a range that overlaps no other GPT entry;
* **P5** a slot boot's command line begins with the loader's slot words and
  is never truncated.

## 9. Self-checks and negative controls

**The gate, `cargo xtask test-ab-boot`** (x86-64, OVMF, KVM; later on
AArch64): a virtual disk with the SN720's layout scaled down -- p1 a dummy
"swap" partition filled with a pattern, p2 an ESP holding only the loader,
p3 the control partition, p4/p5 slots, p6 a root -- built by xtask with
`ferrix-partition` and `ferrix-slots`. Each boot is a fresh QEMU process on
the same disk file (a power cycle).

| # | Setup | Required |
|---|---|---|
| 1 | good A; trial B with a system that panics at boot; tries 1 | boot 1: loader says it decremented, B starts, `FERRIX-PANIC`; boot 2: loader boots A as good, `FERRIX-BOOT-…`, `ferrix.slot=a` in `/proc/cmdline`; record tries 0, sequence +1 |
| 2 | good A; trial B, a good system; tries 1 | boot 1: B as trial; the record is marked good (by `bootctl good` once ab-sys's lands; by the crate from the host until then); boot 2: B as good |
| 3 | as 1, newest record copy corrupted | the older copy is used, and the loader writes over the corrupt one |
| 4 | as 2, one byte of B's initramfs flipped | B does not verify; A boots; no write (sequence unchanged) |
| 5 | record zeroed | A boots, no write |
| 6 | good A with A's kernel blob corrupted; trial B failed (tries 0); ESP holds the rescue files | the loader boots `esp`, never B |
| all | | p1's pattern hashes the same after every boot |

**Negative controls**: source sabotages run by `fleet/gate.sh control
<ref> <tag> <file> <old> <new> --expect-line <text> -- test-ab-boot`, each
FIRED on the gate's failure line, recorded by the log's hash (the
consultant's A1, A7); no `cfg`, no feature:

* `no_decrement` (the decrement-before-start removed): case 1 boots B on
  boot 2 again -- the loop the decrement prevents;
* `no_verify` (the blob hash comparison removed): case 4 starts B's
  corrupted initramfs;
* the range check removed, with the record's copy offset sabotaged to fall
  in p1: p1's hash changes;
* the failed-trial ordering removed (`plan` falls back to the failed trial):
  case 6 starts B;
* `no_readback`: a host test of `ferrix_slots::write_record`, since a
  dropped write cannot be made in QEMU: a disk double that drops writes is
  refused with the read-back and, with the read-back sabotaged, accepted.

| Property | Shown by |
|---|---|
| P1 | case 1, control `no_decrement`; host test + control `no_readback` |
| P2 | case 1 boot 2, case 6, control on the failed-trial ordering |
| P3 | case 4, control `no_verify` |
| P4 | p1's hash after every boot, control on the range check; host tests of the GPT refusals |
| P5 | case 1/2's `/proc/cmdline` check; host test of the words and the fit |

The `test-boot` rows for x86-64 and AArch64 are unchanged: their images have
no HardDrive node, and the loader takes today's path.

Host tests of `ferrix-slots` as §3 lists, in `check`.

## 10. Slices

| | Slice | Points |
|---|---|---|
| AB1 | `ferrix-slots` and its host tests; this document | 3 |
| AB2 | The loader: Block I/O, GPT, record, slots, plan, command line | 5 |
| AB3 | `test-ab-boot` with its controls | 3 |
| (ab-sys) | `bootctl`, installer `--slots`, `test-install --slots` | theirs |

## 11. Where it stands

2026-10-11: this design; the consultant's design verdict **OK IF A1-A10**
(ledger `~/.local/share/ferrix/cert-consultant/reviews.md` line 727), its
conditions folded in above; no requirement ids (line 727 (a)). AB1-AB3
built on branch `ab-boot`; the code review's verdict **OK IF G1-G6** (line
733): G1 Ferrix partitions only inside the usable blocks, G4
`ferrix.slot.disk`, G5 §6's qualified rows, G3 AoU-28 also taken by
softgl-clock -- whichever lands second renumbers. Both landed on
2026-10-11 as AoU-28; on 2026-10-12 this design's became **AoU-29**
(SAFETY-MANUAL §4, SECURITY-TARGET A.FIRMWARE, ITEM.md), and
`check-safety-requirements.py` now refuses an id the manual heads twice. A10: nothing writes the SN720 (the p3-p5
retype, `bootctl init`, the first slot) without the customer's OK for that
write phase, through the PO, with the address list first. AB2's loader code
goes back to the consultant for code review before landing, with the
A.FIRMWARE, V-12, SAFETY-MANUAL and ITEM.md edits.
