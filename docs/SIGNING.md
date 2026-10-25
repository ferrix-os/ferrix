# Product signing: one LMS/HSS scheme for slots, configuration and drivers

Version 2, written 2026-10-24 by product-signing. **A design for review, before
any code.** Nothing here is built. Nothing in another document changes its
claims because of it (the pointers in `AB-BOOT.md` and `BUILD-FLAGS.md` say
what will change).

**The customer's decision (2026-10-24).** Ferrix signs its product artifacts
with a stateful hash-based signature: **LMS/HSS** (NIST SP 800-208 [1],
RFC 8554 [2]). XMSS^MT (RFC 8391 [3]) is used only with a written reason.
Signing happens only inside an HSM that enforces the one-time-key state.
There is **no second signature**, classical or hybrid, on the artifacts this
scheme covers.

**What this answers.** It answers the open question of `DEVMGR.md` §4.1, "how
is a new image signed, and whose key verifies it?" (D3, and SAFETY-MANUAL
AoU-26). It also answers the signing half of the consultant's ruling on
signed configuration for `fast` (ledger
`~/.local/share/ferrix/cert-consultant/reviews.md` line 3564: (S4), (S11),
(S12)). And it gives the A/B slots of `AB-BOOT.md`, which today verify by
SHA-256 alone, an authenticity check. The consultant's design verdict on
the scheme was **NOT YET until a design covers (P1) to (P10)** (ledger line
3608). The sections below take them one at a time. Each ends in a table of
its decisions. §12 records the customer's answers to the questions
version 1 asked.

**What it is not.** It is not measured boot. It does not attest to a
remote party. It does not change how firmware verifies the loader: UEFI
Secure Boot's classical chain stays as it is (§6.3). And the kernel gets no
verifier (§5).

## 1. (P1) One signed manifest, one key per kind

### 1.1 The three kinds

| Kind | Code | The artifact | Who verifies, and when | Where the manifest travels |
|---|---|---|---|---|
| **System slot** | 1 | One A/B slot image: its header, then the kernel, initramfs and command line (`AB-BOOT.md` §2). Also the ESP's rescue image in a signed configuration (§9.2) | The UEFI loader, before it starts the slot (AB-BOOT §4, step 5); `bootctl try` first, as an early refusal with no claim | The slot partition's second 4 KiB block (slot header version 2, §1.3) |
| **Configuration package** | 2 | One configuration package of 3564's (S1)-(S6): the typed, bounded data `fast` applies at a boot (S7) | init's configuration verifier, before any unit reads the configuration (3564 (S12)) | The package file's first 4 KiB |
| **Driver image** | 3 | One native driver image that `drvupdated` hands to `devmgr` (`DEVMGR.md` §4.1) | `devmgr`, on its own copy of the VMO, before step 1 of "what devmgr checks" | The first 4 KiB of the bytes the client sends after its 48-byte header |
| *Revocation* | 4 | A list of revoked keys (trees) of one kind (§7.4) | The verifier of the kind it names | A file next to that kind's anchors (§7.4) |

**One format, one verifier.** The four kinds share the manifest below and one
verifier crate (§5). They differ only in four things: the anchor set that
verifies them, the kind code the caller expects, the meaning of `subject`, and
what `payload_sha256` covers.

**A key per kind.** Kinds 1, 2 and 3 each have their own keys, two per kind,
each with its own public key (§3.3, §4.1). An anchor names the kind it verifies.
The verifier takes its anchor set from the caller's expected kind, never
from the kind field in the manifest. So a signature of kind 1, 2 or 3 fails
under another of those kinds' keys before any field is read. That holds
even if a manifest of another kind carried the right kind code, which it
cannot, because the code is signed. The kind field in the signed bytes is a
second, independent separation.

**Kind 4 is the exception.** A revocation manifest is signed under its
target kind's keys (§3.3, §7.4), so a revocation of kind k and a manifest
of kind k share one anchor set. **Only the signed kind field separates
them.** The verifier therefore checks the kind before anything else of the
body is used. §8.1's sig-bind names both directions as cases: a slot
manifest presented where a revocation is expected, and a revocation
presented as a slot. The kind check's control is in §8.3.

### 1.2 The manifest

A **manifest block** is 4096 bytes. All integers are little-endian. The
**signed message is bytes 0..512**, exactly as written. HSS signs it as its
message `M` (RFC 8554 §6.2), with the randomiser `C` HSS itself prepends
(SP 800-208 §9.2).

```
  0  magic             "FXSIGMAN"
  8  format            u16 = 1
 10  body_len          u16 = 512            the signed message is bytes 0..512
 12  kind              u16                  1 slot, 2 config, 3 driver, 4 revocation
 14  flags             u16 = 0
 16  security_version  u32                  the anti-rollback number of this kind (§7)
 20  anchor_id         u16                  which root of the kind signed it (§3.3)
 22  profile           u8                   1 max, 2 fast, 3 present, 4 desktop; 0xFF development (§6.4)
 23  arch              u8                   1 x86-64, 2 AArch64, 3 ARMv7-A
 24  family            u32                  hardware family (§1.4)
 28  reserved          u32 = 0
 32  release           u64                  release sequence, informational (it orders nothing)
 40  label             [u8; 32]             e.g. "ferrix 0.4.0 a1708b49c", NUL padded, informational
 72  subject           [u8; 32]             driver kind: the driver's name as UPDATE names it;
                                            config kind: the package's name; slot kind: zero
104  payload_len       u64
112  payload_sha256    [u8; 32]             slot: the slot header's seal (AB-BOOT §2, SHA-256 of
                                            header bytes 0..4064); config: the package bytes after
                                            this block; driver: the image bytes after this block
144  floors            [u32; 4]             slot kind only: the minimum security_version this release
                                            sets for kinds 1..4 (§7.2); zero in every other kind
160  target_kind       u16                  revocation kind only: the kind whose trees it revokes
162  revoked_count     u16                  revocation kind only, at most 20
164  zero              [u8; 12]
176  revoked           [[u8; 16]; 20]       revocation kind only: the revoked trees' identifiers I
496  zero              [u8; 16]
512  sig_len           u32                  at most 3532 (§2.2)
516  signature         [u8; sig_len]        an HSS signature, RFC 8554 §6.2
516 + sig_len .. 4096  zero
```

A manifest **verifies** only when all of these hold:

* magic, format and `body_len` match;
* every reserved or zero byte is zero, in the body and after the signature;
* `sig_len` is within its bound and the signature parses exactly to `sig_len`
  bytes, with no byte short or left over;
* every LMS and LM-OTS type code in it is one of §2's allowed set, and the
  level count `Nspk + 1` equals the anchor's `L`;
* the signature verifies under the anchor `(expected kind, anchor_id)`;
* no tree's `I` in the signature (with `L = 1`, the key's own) is revoked
  for this kind (§7.4);
* kind, profile, arch, family and `subject` equal what the caller expects;
* `security_version` is at least the caller's floor for this kind (§7).

Only then does the verifier return the payload's length and digest. The
caller hashes the payload bytes it will use, from one copy in memory, and
compares them. That is AB-BOOT's rule today: no second read between check
and use.

**Why the slot's payload digest is the header's seal.** The slot header
already binds the three blobs by their SHA-256 (AB-BOOT §2), and the loader
already recomputes the seal. A signed seal therefore authenticates the
kernel, the initramfs and the command line, with no hashing beyond what the
loader does today.

**Every signed byte is fixed at release.** In header version 1, bytes
16..24, `generation`, hold the record sequence `bootctl` armed the slot
under (`ferrix-slots`'s `lay_out`). That value is per device, inside the
seal, and the signer cannot know it. `bootctl sync`'s copy of a slot to the
other partition would change it again. So in header version 2,
**`generation` is zero**, and the loader refuses a version-2 header whose
bytes 16..24 are not zero. What the signature covers is exactly header
bytes 0..4064, as the release wrote them: magic, version, lengths, the zero
`generation`, the label, the three blob entries and the zero padding. Then
the blobs follow through their digests. A slot written by `bootctl` is the
release's slot image byte for byte. Slice S4 carries a host test and a
control for this: a version-2 header with a non-zero `generation` must be
refused.

### 1.3 Where each kind's manifest sits

* **Slot, header version 2.** Bytes 0..4096 of the slot are the header as
  today. Its `version` becomes 2, and blobs start at or after 8192. Bytes
  4096..8192 are the manifest block. Header version 1 stays readable, and is
  refused wherever signing is required (§9.1).
* **Configuration package.** The manifest block, then the package bytes
  (`payload_len` of them). init's verifier stages and activates the
  configuration as 3564 (S7) says.
* **Driver image.** `drvupdate`'s 48-byte header, then the manifest block,
  then the image. The 8 MiB limit stays on the image. `devmgr` verifies its
  own copy, the one no other process holds a handle to (`DEVMGR.md` §4.1).
  `drvupdated` may refuse early with the same crate, but the check that
  counts is `devmgr`'s, for the same reason `devmgr` copies the VMO at all.
  The 3564 brief named drvupdated as the verifier. This design moves the
  counting check to `devmgr`, because only `devmgr`'s copy is free of a
  time-of-check-to-time-of-use window.

### 1.4 Profile and hardware family

`profile` and `arch` are the build's (BUILD-FLAGS rule 6's boot line).
`family` is a number from a registry, `tools/common/data/hardware-families.toml`
(new in slice S1), with one row per family and the rule its loader uses to
recognise it.

| Family | Id | Recognised by |
|---|---|---|
| x86-64 UEFI PC | 0x0101 | arch; UEFI |
| x86-64 Intel N150 (the SOYO M4 Pro) | 0x0102 | SMBIOS processor and board strings (`ferrix-smbios`, already in the loader) |
| AArch64 UEFI virt (QEMU) | 0x0201 | arch; SMBIOS system "QEMU" |
| AArch64 Pixel 7 | 0x0202 | its own loader (`src/boot/vendor/google/pixel7`) |
| ARMv7-A UEFI virt (QEMU) | 0x0301 | arch; SMBIOS system "QEMU" |
| ARMv7-A STM32MP157 (the DK1) | 0x0302 | the device tree's `st,stm32mp157` compatible |

A release signs a slot manifest per (profile, arch, family) it ships.

* **In a claimed profile**, the loader accepts only the families that
  profile's claim names. It accepts each of them exactly, with **no
  generic-family fallback**. A claim is made of tested hardware, and a
  generic signature would extend it to hardware nobody tested.
* **In a profile without a claim** (`desktop`, development images), the
  loader accepts its own specific family, then the generic family of its
  arch (0x0101, 0x0201, 0x0301), in that order.

The rule is fixed in the registry and the profile file, not in code.

| Decision | Choice | Reason |
|---|---|---|
| Formats | One 4 KiB manifest block for every kind; the signed message is its first 512 bytes | One parser and one verifier to qualify; a fixed size keeps the parse free of allocation |
| Domain separation | Separate keys per kind, chosen by the caller's expected kind; the kind is also signed | A signature of one kind cannot verify as another under any field value |
| Bound fields | kind, security version, payload digest and length, profile, arch, hardware family, subject, anchor | (P1)'s list, plus `subject` so that a driver image cannot be put on another driver, and `anchor_id` so that the anchor lookup is signed |
| Slot binding | `payload_sha256` = the slot header's seal | Authenticates the three blobs through hashes the loader already checks |
| Driver check | `devmgr` on its own copy | The only copy with no time-of-check-to-time-of-use window |

## 2. (P2) Parameter sets

### 2.1 The choice

**Decided by the customer (2026-10-24, ledger lines 3719 and 3731, §12
Q1):** plain LMS, one tree per key, `L = 1`, with **two independent keys
per kind**. HSS 10/15, this design's first proposal, stays a later option:
the verifier accepts both, so a move to HSS costs a new key, not a new
format (§4.3).

| Item | Choice | From |
|---|---|---|
| Hash | SHA-256/192 (SHA-256 truncated to 192 bits, `n = m = 24`) | SP 800-208 §4.2 [1]; code points RFC 9858 [4]; CNSA 2.0 recommends SHA-256/192 for LMS [5] |
| LM-OTS | `LMOTS_SHA256_N24_W4`, type 0x00000007: `n = 24`, `w = 4`, `p = 51` | SP 800-208 Table 3; RFC 9858 Table 1 |
| LMS | `LMS_SHA256_M24_H20`, type 0x0000000D: 1,048,576 leaves | SP 800-208 Table 4; RFC 9858 Table 2 |
| HSS levels | `L = 1`: the HSS encoding of RFC 8554 §6 with `Nspk = 0`, so that the format is the same if a later key has `L = 2` | RFC 8554 §6; SP 800-208 §7 (independent keys on different modules, its first option) |
| Signature | 1,744 bytes (4 bytes of `Nspk`, then 1,740 of LMS signature) | RFC 9858 Table 3, row "20, W=4"; recomputed below |
| Keys per kind | two, `anchor_id` 0 and 1, each generated in its own module at its own site | §4 |
| Allowed in the verifier | LM-OTS: N24_W4 only; LMS: M24_H10, M24_H15, M24_H20; `L` 1 or 2 | This choice, and HSS 10/15 as the later option. Every other code is refused |

**What CNSA 2.0 says.** As quoted, the CNSA 2.0 FAQ approves LMS and XMSS
from SP 800-208 for national security systems, and excludes HSS and XMSS^MT
(source 5). `L = 1` keeps that open. None of Ferrix's four certification
targets requires CNSA 2.0 (ledger 3719 Q1).

The SHA-256/192 code points were "TBD" in SP 800-208's Tables 3 and 4. They
were assigned by RFC 9858 (October 2025) and are in IANA's LMS registry [6].
The one SHA-256 implementation the verifier uses serves both the truncated
hash and the slot hashes (§5.2).

### 2.2 Tree height against the signature count

**Budget per key** (§12 Q7): **2^20 = 1,048,576 signatures** per key, over
a key life of 25 years: 115 a day. With two keys per kind that is 2^21 per
kind. A release signs one slot manifest per (profile, arch, family) it
ships, which is at most 4 × 6 = 24 today. It also signs one manifest per
driver image and configuration package it ships, and the revocation
manifests and re-signing of §7.4. Even daily releases of every slot
combination use 24 of the 115. Product releases are not daily, and CI,
nightly and gate images never use a product key: they use development keys
(§6.4). **An alarm fires at 50%** of a key's leaves (`CKA_HSS_KEYS_REMAINING`
below 2^19, read by the signing tooling before and after every signature,
§3.4), which is when its successor is generated and its anchor ships
(§4.2).

| Configuration | Capacity per key | Per kind | Notes |
|---|---|---|---|
| **LMS H20, `L = 1` (chosen)** | 2^20 | 2^21 with two keys | one key generation of about 2^20 × 767 ≈ 800 million hash calls per key, once, in its module |
| HSS 10/15 (the later option) | 2^25 | 2^26 | needs a top-tree module and certifying ceremonies (§4.3) |
| LMS H25 | 2^25 | — | not used: a key generation 32 times longer, for no need (ledger 3719 Q7) |

### 2.3 Signature size and verify time on the slowest target

The slowest target is the **ARMv7-A loader on the DK1**: a Cortex-A7 that
mainline TF-A clocks at **650 MHz** (`docs/vendor/st/stm32mp157-dk.md`).
Verify time is dominated by the Winternitz chains: at most `p × (2^w − 1)`
hash calls per level, each over 23 + n = 47 bytes, which is one SHA-256
block. To that each level adds the public-key hash K (over 22 + p·n bytes),
the message hash Q, and h tree nodes (two blocks each).

**The cost of one SHA-256 call on a Cortex-A7** is from eBACS [7], machine
`berry2` (Cortex-A7, BCM2836): **28.24 cycles per byte** for long messages
(about 1,807 cycles a block), and **380.53 cycles per byte** for an 8-byte
message, about 3,044 cycles for a whole one-block call. Both figures are
medians of the fastest implementation measured there (copy kept in
`~/.local/share/ferrix/signing-bench/`). Ferrix's own portable SHA-256 is
not measured on the DK1. The bound below therefore allows **three times**
that cost. Using the board needs the product owner, and this design asks for
it in slice S2.

| Parameters (`L`, heights, `w`) | Signature (bytes) | Worst-case chain calls | Worst case, eBACS cost | Worst case, 3× budget | Host check (2 × H5, Ryzen 9 9900X, mean) |
|---|---|---|---|---|---|
| 2, 10/15, W4, n=24 (the later HSS option) | 3,172 | 1,530 | 4.85 M cycles = 7.5 ms | 22 ms | 35 µs |
| 2, 10/15, W8, n=24 | 1,972 | 13,260 | 40.5 M = 62 ms | 187 ms | 258 µs |
| 2, 10/15, W4, n=32 | 5,236 | 2,010 | 6.4 M = 9.8 ms | 29 ms | 46 µs |
| **1, 20, W4, n=24 (chosen)** | **1,744** | 765 | 2.46 M cycles = **3.8 ms** | **11 ms** | 18 µs (H5) |
| 2, 20/20, W4, n=24 (largest allowed) | 3,532 | 1,530 | 4.9 M = 7.5 ms | 23 ms | — |

The signature sizes are RFC 9858 Table 3's, recomputed from RFC 8554 §4-§6.
The host column measures `hbs-lms` 0.1.1 [8] verifying 24 signatures of
200-byte messages, 20 times each, built `--release`. It uses the same
parameters as the row, but with H5 trees, so that key generation stays
short. A taller tree adds only h tree hashes, which is under 3% of the total
here. On that host one 47-byte SHA-256 call takes 36 ns, so the two-level
row's 35 µs is about 980 calls, and the chosen one-level row's 18 µs about
half that: the analytic mean (about 51 × 7.5 chain calls per level, plus K,
Q and the tree) within 10%. The program and its output
are in `~/.local/share/ferrix/signing-bench/`, outside the tree. No
dependency was added.

**W4 rather than W8.** W8 saves 1,200 bytes per signature, which no kind
needs: the block has 4,096 bytes. It costs about eight times the verify time
and the HSM's signing time. W4 also keeps the DK1's bound near 11 ms, a
fraction of what the loader already spends hashing a slot's blobs. At the
same 28.24 cycles per byte, a 32 MiB kernel and initramfs take 1.46 s at
650 MHz.

**n = 24 rather than 32.** The security level is at least 8n − 1 = 191 bits
under the random-oracle argument of RFC 9858 §8. That is the 192-bit level
CNSA 2.0 recommends for LMS [5]. It costs about 20% less verify time and 40%
less size than n = 32.

**The bound MEMORY-AND-TIMING will state** (slice S2): one verify is at most
50 ms on the DK1 at 650 MHz, the 3× budget with room for the parse. The
measured worst case on the board replaces the estimate before any claim.

**Measured (S2, 2026-10-24, `MEMORY-AND-TIMING.md` §2.2z).** On the DK1 at
650 MHz, in U-Boot's state as the loader runs:

* RFC 9858 A.4 (H20, W4, 435 chain steps) verifies in 7.65 ms cold and
  7.62 ms warm.
* A full manifest whose body was chosen to need 555 steps verifies in
  9.56 ms cold and 9.55 ms warm.
* The upper bound computed from those two measured points, all 765
  steps (not itself measured), is 12.9 ms for L = 1 and 25.9 ms for
  L = 2, inside the 50 ms bound.
* One SHA-256 call costs 3.07× eBACS's, slightly over the assumed 3×.
  The bound holds with a 3.9× margin.
* SHA-256 over 32 MiB took 6.59 s (127.7 cycles per byte, 4.5× eBACS's).
  That is the slot hashing's cost in S4, not the signature's.

**After S4's SHA-256 fix (2026-10-24, the same section).** The slowness
was our code, not memory or a missing accelerator. With the fix, A.4
verifies in 2.88 ms cold, the 555-step manifest in 3.56 ms, and the
computed 765-step bound is 4.85 ms (L = 1) and about 9.7 ms (L = 2), a
margin of 10×. SHA-256 over 32 MiB takes 2.29 s (44.4 cycles per byte).
The S2 figures above are the record of S1's code.

## 3. (P3) State custody in the HSM

### 3.1 What the HSM must be

| Requirement | Source | How it is shown |
|---|---|---|
| Key generation and signing only in a hardware module validated to FIPS 140-2 or 140-3 **level 3 or higher** physical security, in an approved mode with no bypass | SP 800-208 §8.1 | The CMVP certificate, which must list LMS key generation and signature generation (§12 Q2) |
| The private key is never exported, and the entropy source is inside the module | SP 800-208 §8.1 | Certificate and security policy; the module's LMS key is created non-extractable, and its backup feature is off for it (§4) |
| The leaf index `q` is incremented and **stored in non-volatile memory before a signature is released** or another request accepted | SP 800-208 §8.1, §9.1 | The security policy's statement. A power-cut test at acceptance: cut power during a signing loop, power on, and check that no `q` repeats and at most one is skipped |
| A tree's root is signed by its parent **once** (only if HSS is ever used, §4.3) | SP 800-208 §8.1, footnote 9 | Not applicable with `L = 1`: no key has a parent |

Vendors that state LMS/HSS signing with the state kept in the module (to be
re-checked against CMVP at procurement):

* Utimaco's Quantum Protect package for the u.trust General Purpose HSM
  Se-Series: LMS, HSS, XMSS and XMSS^MT through PKCS #11, with the state
  managed in the HSM [9];
* Thales's Luna HSMs: LMS per SP 800-208 and PKCS #11 3.1 [10].

PKCS #11 3.1 defines `CKM_HSS` and `CKK_HSS`, with `CKA_HSS_KEYS_REMAINING`
reporting the unspent leaves [11]. The signing pipeline reads it before and
after every signature (§3.4).

### 3.2 Custodians

Signing needs an **m-of-n quorum** of named custodians, at least 2 of 3, on
the HSM's own authentication (smart cards or PINs): from the first claimed
release, the customer and two external custodians under contract (§12 Q3).
Until then only development keys exist, and single-person custody is a
stated residual. The custodians, their
roles and their changes are a configuration item under configuration
management, as 3564 (S4) asks. Key ceremonies need the quorum plus a witness
who is not a custodian: the independent assessor (§12 Q3).

### 3.3 Keys and anchors

Each kind has **two keys**, `anchor_id` 0 and 1, each an LMS key (`L = 1`)
in its own module at its own site (§4.1). Both keys of each kind are
anchors in every image that verifies the kind (§6). Revocations of kind k
are signed under kind k's keys. That makes **six product keys**, in two
modules. In the text below, "root" means one of these keys.

**When the modules exist** (§12 Q2). No HSM is bought until the first
claimed release with signed updates. Until then no product key exists,
only development keys in the soft signer (§6.4), and no claimed image is
released with signed updates.

### 3.4 The signature log

Each signature appends one line to the **signature log**, a git repository
of its own (`ferrix-os/signing-log`, private, signed commits, under
configuration management). A line holds:

`time, kind, anchor_id, key I, q, manifest SHA-256 (bytes 0..512), payload SHA-256, release, custodians`

The pipeline (§8.6) checks before it publishes:

* the `(I, q)` pair is new in the log. A **repeat is an incident**. It is
  reported to the customer, the key is revoked (§7.4), and nothing
  signed with it after the repeat ships;
* `q` is above every `q` logged for that `I`, so a module that went back is
  seen;
* `CKA_HSS_KEYS_REMAINING` fell by exactly one.

| Decision | Choice | Reason |
|---|---|---|
| Module | FIPS 140 level 3 or higher, LMS listed on its CMVP certificate, key non-extractable | SP 800-208 §8.1 |
| State | The module commits `q` before it releases a signature, shown by a power-cut acceptance test | SP 800-208 §8.1, §9.1: a power loss may skip an index, never reuse one |
| People | m-of-n custodians, at least 2 of 3, named under configuration management | 3564 (S4) |
| Record | The signature log in its own repository; a repeated `(I, q)` is an incident | (P3); detection independent of the module |

## 4. (P4) Backup without reuse

**Rewritten for `L = 1`** (the customer's decision, ledger 3731; the
consultant's small re-review of this section is asked for there). Nothing
in this scheme copies private state. SP 800-208 §8.1 forbids the export it
would need, and a restored backup is exactly the reuse its §1.2 warns of.
Resilience comes from **more keys**, never from copies.

### 4.1 Two independent keys per kind

SP 800-208 §7 gives this as its first option: "create multiple stateful HBS
keys on different cryptographic modules and then configure clients to
accept signatures created using any of these keys".

* Each kind has **two keys**, `anchor_id` 0 and 1. Each is an LMS key
  (`L = 1`, H20) generated inside its own module, at its own site: module A
  holds key 0 of every kind, and module B holds key 1. That makes **three
  kinds × two keys = six product keys** in two modules. Revocations of kind k
  are signed under kind k's keys (§1.1, §7.4).
* **Both anchors of each kind are in every image** that verifies that kind
  (§6.1), so a manifest signed with either key verifies on every device.
* A release signs each manifest with **one** key, never both: one artifact,
  one signature. Normally that is key 0, with key 1 as the standby. Each key
  is used from its own module only, so no leaf can be used twice across
  modules.
* There is no top tree, no lower-tree ceremony and no composition. The
  HSM's own `CKM_HSS` signature with `Nspk = 0` is the manifest's signature
  as it stands.

| Anchors in an image | Kind 1 (slot) | Kind 2 (config) | Kind 3 (driver) |
|---|---|---|---|
| The UEFI loader | product keys 0 and 1 (revocation of kind 1 uses them too) | — | — |
| init's configuration verifier | — | product keys 0 and 1 | — |
| `devmgr` | — | — | product keys 0 and 1 |

Six product anchors in all, each 52 bytes (`u32str(1) || u32str(lms type)
|| u32str(ots type) || I || T[1]`). The development anchors of §6.4 are
separate keys with their own ids.

### 4.2 Loss, exhaustion and compromise

Every step runs under the quorum (§3.2, from the first claimed release) and
is logged. The runbook is written with slice S6.

| Event | Procedure | Lost | What devices need |
|---|---|---|---|
| A module fails or is lost | Sign with the kind's other key (key 1, in module B). Generate a replacement key in a new module, and ship its anchor in the next loader (kind 1) or slot (kinds 2, 3). Revoke the lost key (§7.4) only if loss of custody is suspected | The lost key's unspent leaves | Nothing at once: the other key verifies everywhere. The replacement anchor arrives with an update |
| A key reaches 50% of its leaves (the alarm) | Generate its successor in the same module, ship the successor's anchor a release ahead of need, and switch once the anchor is in the field | Nothing | An update that carries the new anchor |
| A key is exhausted | Not reached in operation: the 50% alarm comes first. A module refuses to sign with an exhausted key (SP 800-208 §8.1) | — | — |
| Both modules lost at once | No signing until a new key's anchor reaches devices. Kind 1 needs a new loader, written by the installer; kinds 2 and 3 then need a slot release signed under the new kind-1 key | Signing, for that time | A new loader, then a slot |
| Suspected compromise or a repeated `(I, q)` | Revoke the key by its `I` (§7.4), signed under the kind's other key; ship anchors without it (§6.2) and raise the floor (§7) | That key | A revocation file, or a new loader (kind 1) or slot (other kinds) |
| A compromised key has already revoked the other key (§7.4 (c)) | Treat as "both modules lost": new keys in new modules, their anchors in a new loader written by the installer, then a slot release under the new kind-1 key | Both keys of the kind | A new loader, then a slot |
| A vendor's state-aware backup | Not used until assessed against this section, by a change to this document. Utimaco's "OTS-preserving framework" [12] is one such scheme | — | — |

**The upgrade path** (ledger 3719 Q2). Each module added later gets new keys
of its own, whose anchors ship in the next loader or initramfs release. Old
keys keep verifying until they are retired.

### 4.3 HSS later, if wanted

If a later key should be HSS 10/15, as this design first proposed, the
format and the verifier need no change: `L = 2` and M24_H10/H15 are already
in the allowed set (§2.1). Such a key would bring the top-tree module and
the certifying composition that version 1 of this section described (each
lower tree's bare LMS public key signed once by the top tree's
single-level `CKM_HSS` key, joined by the pipeline into RFC 8554 §6.2's
`signed_pub_key`), under a change to this document and its own review.

**Index ranges of one tree in several modules are never used.** They would
need the tree's seed in each of those modules, which is the copy §8.1
forbids. The consultant withdrew that alternative at ledger 3667.

| Decision | Choice | Reason |
|---|---|---|
| Resilience | Two independent LMS H20 keys per kind, in two modules at two sites; both anchors in every image; one signature per artifact | SP 800-208 §7, first option; no private state is copied; no top-tree single point of failure |
| Succession | 50% alarm, then the successor's anchor ships a release ahead of need | §12 Q7; an anchor change needs an update |
| Index ranges in one tree | Never | They need the seed in several modules, which §8.1 forbids |
| HSS | A later option, with no format change | The verifier accepts `L = 2` already |


## 5. (P5) The verifier crate

### 5.1 What it is

**`ferrix-sigverify`** is a new crate at `src/lib/crypto/sigverify`
(`LAYOUT.md`: cryptography that checks). It is `no_std`, `forbid(unsafe_code)`,
and allocates nothing: every buffer is a slice its caller lends, and the
largest is the 4 KiB block. It holds:

* the manifest's parse and every refusal of §1.2;
* LM-OTS, LMS and HSS **verification only**, per RFC 8554 §4.6, §5.4.2 and
  §6.3, for the allowed set of §2.1;
* the policy comparison of §1.2's last four checks;
* the anchor type, and a `const` table form that the loader, init's
  verifier and `devmgr` compile in (§6).

It contains no key generation, no signing and no private-key state. **No
signing code exists in any image.** The signer is the HSM. The development
signer (§6.4) is a host tool in `tools/`, which no image links.
`check-item-boundary.py` gains a rule: no crate an image links may depend on
a signing crate, with a planted dependency as its negative control.

**One SHA-256.** `ferrix-slots` has its own SHA-256 today (AB-BOOT §3). It
moves into `ferrix-sha256` (`src/lib/crypto/sha256`), which both crates use,
so one implementation is qualified once. Its FIPS 180-4 tests move with it,
joined by the SHA-256/192 vector of RFC 9858 §2.1.

**Why not an existing crate.** The two published `no_std` Rust LMS crates
are not suitable as product code. Both are Apache-2.0 licensed (RustCrypto's
also MIT), so the licences are compatible. The problems are technical:

* `hbs-lms` 0.1.1 [8] (Fraunhofer AISEC) has SHA-256/192 and HSS without
  allocation. But it writes the type codes of RFC 8554's 256-bit sets (LMS
  5-9, LM-OTS 1-4) whatever the hash, so its SHA-256/192 keys do not carry
  RFC 9858's codes (read in its `lms/parameters.rs` and
  `lm_ots/parameters.rs`). It also ships signing in the same crate, and has
  a non-standard H2 set.
* RustCrypto's `lms-signature` 0.1.0-rc.2 [13] has only the 256-bit sets,
  LMS without HSS, and allocates.

`hbs-lms` serves as a host-test cross-check (a dev-dependency, never in an
image) for the parts it does conform on, with the codes mapped, and as the
performance proxy of §2.3. The verifier itself is about 600 lines, written
against RFC 8554's algorithms and qualified as §8 says.

### 5.2 Who uses it

| User | Kind | Ring | Note |
|---|---|---|---|
| The UEFI loader (`src/boot/common/uefi`) | slot, revocation | firmware time | Before it starts any slot, and the rescue image (§9.2) |
| The Pixel 7 loader | — | firmware time | Not used. The Pixel 7 has no slot path, so it has **no slot authenticity claim** and no rollback claim (§7.3) |
| init's configuration verifier | config, revocation | ring 3 | 3564 (S12): run before any unit reads the configuration |
| `devmgr` | driver, revocation | ring 3 | On its own copy of the image (§1.3) |
| `bootctl try` | slot | ring 3 | An early refusal only; the loader's check is the one that counts |
| The kernel | **none** | ring 0 | No verifier in the kernel (3564 (S12); keep the kernel slim). A check, slice S1, fails if any kernel ELF holds a `ferrix_sigverify` symbol, with a planted use as its control |

### 5.3 Its classification, and that of every enforcement point

**The crate.** `ferrix-sigverify` is classified in `certification-item.json`'s
`crates` as `item`. It is part of the item's evidence: the update function,
TSF for FPT_TUD_EXT.1 and the H.SIGN requirements (§10.3). It never appears
in `in_ring0_when`, because no profile links it into ring 0. `ferrix-smbios`
is the precedent: an item crate the UEFI loader calls. `ferrix-sha256` goes
with it.

**Its level uses the existing mechanism.** Today's schema has no per-crate
`"level"` key: a crate's entry carries `ring`, `path`, `in_ring0_when`,
`below` and `above`, and the ring carries the level. The manifest's own comment ("A crate may
name a level of its own") has no field behind it yet. So the crate takes the
item ring's level, `present` today, and moves with the ring as `fast`'s and
`max`'s levels are built. **The level it must reach is decided** (§12 Q9):
`max` gets no signed updates until its claim is built (M7), so the
verifier ships in `fast` and `present` and needs both **`sc2` and
`present`**, which are incomparable (BUILD-FLAGS §4), with statement and
branch coverage. An **`sc4`** verifier (DAL A-like structural coverage,
MC/DC, independence) is an M7 work item, needed before any `max` product is
fielded.

If the crate ever needs a level the ring does not carry, the `"level"` key
becomes a new schema field, with its change to `check-item-boundary.py`, a
self-test, and its own review. It is not assumed here. `sc4` stays the
crate's target from its first line: MC/DC-ready structure, no recursion
and bounded loops. The crate depends on nothing but
`ferrix-sha256` and `core`, so no upward reference results.

**The enforcement points.** The claim rests on the callers as much as on the
crate: a caller that ignores a refusal undoes it. So each check that acts on
a result moves into a classified crate, and the program around it becomes a
thin caller, as the loader's SMBIOS path is today.

| Enforcement point | Today | Intended classification |
|---|---|---|
| `ferrix-sigverify`, `ferrix-sha256` | not built | `item` |
| `ferrix-slots` (parses the header whose seal is signed; the plan) | unclassified | `item`, with `ferrix-sha256`, since the signed bytes pass through it |
| The loader's slot, floor and TPM path (`src/boot/common/uefi/src/slots.rs`, and the TPM code S5 adds) | in no ring: the loader is outside the TOE | Its decisions (what to start, whether to raise, the lock) move into an `item` crate, `ferrix-boot-policy` (`src/lib/proto/slots` or a sibling), over a `Services`-style trait for Block I/O and TCG2. `src/boot/common/uefi` keeps the protocol bindings, outside the rings, and the boot cases of §8.4 show the bindings deliver what the crate decided |
| init's configuration verifier | not built | an `item` crate under `src/lib/init/`, called by init before any unit reads configuration |
| `devmgr`'s kind-3 check | not built | a function of an `item` crate (in `drvupdate-proto` or beside it) that takes `devmgr`'s copy and returns the decision; `devmgr` itself stays load |

For each of these, the H.SIGN requirement it holds names the boot sub-case
that shows the caller acts on the result.

| Decision | Choice | Reason |
|---|---|---|
| Crate | `ferrix-sigverify`, `no_std`, no `unsafe`, no allocation, verify only | (P5); a fixed-size format makes it possible |
| Hash | One `ferrix-sha256`, moved out of `ferrix-slots` | One implementation to qualify |
| Users | The loader, init's verifier, `devmgr`, `bootctl`; never the kernel | 3564 (S12); a symbol check with a control holds it |
| Existing crates | Neither is used as product code; `hbs-lms` is a host cross-check and the benchmark | Type codes, signing code in the crate, allocation (§5.1) |
| Classification | `item`, at the item ring's level, the target level by §12 Q9; the enforcement points move into `item` crates | BUILD-FLAGS rule 3; the schema has no per-crate level |

## 6. (P6) Trust anchors and the chain from firmware

### 6.1 The chain

```
platform firmware ─UEFI Secure Boot: X.509 db / dbx, Authenticode─▶ the Ferrix UEFI loader (PE)
   the loader ─LMS/HSS, slot anchors compiled in─▶ slot manifest ─SHA-256 seal─▶ kernel, initramfs, command line
      initramfs ─────────────────────────────────▶ init's verifier (config anchors compiled in)  ─▶ configuration package
      initramfs ─────────────────────────────────▶ devmgr (driver anchors compiled in)           ─▶ driver image
```

* **Slot anchors** (the two keys of kind 1) are compiled into the UEFI loader, from
  `tools/common/data/signing-anchors.toml` through a generated `const`. An
  anchor is an HSS public key, RFC 8554 §6.1: `L`, the key's types, `I`
  and root, 52 bytes for n = 24. It is tagged with its kind and
  `anchor_id`.
* **Config and driver anchors** are compiled into init's verifier and into
  `devmgr`. Both are in the initramfs, which the slot manifest covers. So
  they are as authentic as the slot, and they change only with a signed
  slot. `devmgr` reads no files (`DEVMGR.md` §4.1), and with compiled-in
  anchors it still reads none.
* The floors and the revocation state reach ring 3 as **loader words**
  (§7.2). AB-BOOT §4 already puts the loader's words first, so a slot's own
  command line cannot forge them.

### 6.2 Changing an anchor

A new root of kind 1 reaches devices in **a new loader**, which the
installer writes (AB-BOOT §7: the loader is not in a slot). A new root of
kind 2 or 3 reaches them in a signed slot. A root is retired the same way,
by shipping anchors without it, and is backed by a security-version floor
(§7) so that the older images that still trust it cannot come back. Since
the loader is not A/B'd, an old loader's return is stopped by Secure Boot's
`dbx`, not by this scheme (§6.3).

### 6.3 The handover from Secure Boot, and its residual

UEFI verifies PE images against the `db` and `dbx` signature databases, with
Authenticode and X.509 (UEFI 2.10 §32 [14]). It has no LMS. So the
**loader carries the one classical signature the firmware can check**, and
nothing else in this scheme carries any. That is not the dual signature the
customer ruled out: no artifact is signed twice. The firmware's link and
this scheme's links are different artifacts.

The integrator enrols Ferrix's `db` certificate, or Ferrix ships a loader
signed under a key the platform already trusts. The customer chose the
first: integrators enrol Ferrix's own `db` certificate (§12 Q4). Its key lives in
the same HSMs, under the same quorum. It is a classical key (RSA-2048 or
better, as firmware supports).

**Residual, stated as such:** the root of the chain is the platform's own
classical signature check. An adversary who can forge an Authenticode
signature under an enrolled key, or who controls the firmware, starts a
loader of their own. Against that, this scheme adds nothing. Where Secure
Boot is off, the chain has no root at all. On such a machine the scheme
detects only what it would detect on any machine: a slot, package or image
signed by no Ferrix key. It is not an authenticity claim against root, who
can then replace the loader. SECURITY-TARGET's A.FIRMWARE keeps this part of
the burden (§10.3).

**The firmware's state is recorded.** Before anything else, the loader
reads the UEFI global variables `SecureBoot` and `SetupMode` (UEFI 2.10
§3.3 [14]). It prints one line,
`  boot     secureboot <on|off> mode <user|setup>`, and passes
`ferrix.secureboot=on|off` among its words. The kernel's start-up audit
record carries that word (FAU_GEN.1 b), as it carries the build line.
**The authenticity claim is conditional on `secureboot=on` and
`mode=user`.** With either one otherwise, the boot still verifies what §1.2
says. But the claim is stated as absent, and the line says why. The
SAFETY-MANUAL AoU (§10.3) states the same condition for the integrator.

### 6.4 Development builds and the soft signer

**During development, signing uses software keys only** (the customer,
2026-10-24; the consultant's OK IF (D1)-(D5), ledger 3731). There is no HSM
and no FPGA until the first claimed release with signed updates (§12 Q2).
Gates, CI, nightly and development images are signed by the **soft
signer**, `ferrix-softsign` (`tools/common/release/softsign`, a host
program, slice S1). **It is never used for a release** (D4).

**What it is.**

* **(D3) The same surface as the product path.** Its interface is the
  subset of PKCS #11 3.1 the pipeline uses with an HSM [11]:
  * `C_GenerateKeyPair` with `CKM_HSS_KEY_PAIR_GEN` (0x4032), the key type
    `CKK_HSS` (0x46) and the attributes `CKA_HSS_LEVELS`,
    `CKA_HSS_LMS_TYPE` and `CKA_HSS_LMOTS_TYPE`;
  * `C_GetAttributeValue` for `CKA_VALUE` (the HSS public key) and
    `CKA_HSS_KEYS_REMAINING`;
  * `C_Sign` with `CKM_HSS` (0x4033).

  The constants are pkcs11t.h 3.01's. The tooling talks to a `Token` trait
  with exactly these operations. The soft signer is one implementation, and
  a PKCS #11 module loaded from a library path (slice S6) is the other, so
  moving to an HSM changes the configured module and the anchors, not the
  tooling. It signs the same 512 manifest bytes in the same 4 KiB block as
  the product path, and its signatures are RFC 8554 HSS signatures with
  `Nspk = 0`, exactly as an HSM's are.
* **(D2) The HSM's state rule, in software.** A key's private state is one
  file: the seed, `I`, the parameter types and the next leaf `q`. The
  signer holds an exclusive lock (`flock`) on it while it signs. To sign,
  it:
  1. reads `q`;
  2. writes `q + 1` to a temporary file, `fsync`s it, renames it over the
     state file, and `fsync`s the directory;
  3. only then computes and releases the signature with leaf `q`.

  A crash may skip a leaf, never reuse one. The signer refuses to sign if
  it cannot read the state, lock it, or advance it, and if `q` has reached
  `2^h`. It also appends `(I, q, manifest SHA-256)` to a local signature
  log beside the state, `fsync`ed before the signature is released. It
  refuses when the state's `q` is not above every `q` that log holds for
  `I`. So a restored older state file is refused. A test does exactly that:
  restore an older copy of the state, ask for a signature, and expect a
  refusal. **Residual, stated for development:** the log sits beside the
  state, so restoring the whole key directory restores both, and the soft
  signer cannot see it (ledger 3738 (4)). The state directory is local to
  one host, never on a network file system. Neither matters for a release,
  which the soft signer never signs. An HSM keeps both inside its boundary.
* **(D1) Its keys are development keys only.** Development keys have kind
  roots of their own and anchor ids of their own, `0x8000` and up, distinct
  from every product key's 0 and 1. They are listed by public key in
  `signing-anchors.toml`'s `[development]` table. The soft signer generates
  its own keys and has no import, so it cannot hold a product key: those are
  generated non-extractable inside an HSM. Before each signature it also
  refuses a key whose public key appears in the `[release]` table.
* **(D5) No private key in the repository or an image.** Development
  private state lives outside the tree, in
  `~/.local/share/ferrix/signing-dev/`, mode 0600. A check refuses any file
  in the tree that parses as soft-signer state. A development key committed
  by mistake is revoked by replacing its development anchor: new key, new
  anchor id, and the old anchor deleted from `[development]`.
* **Its tree.** Development keys default to `LMS_SHA256_M24_H10` (1,024
  signatures), so that one key generation takes about a second. H15 and
  H20 are accepted for tests of the product heights. The tree's nodes are
  cached beside the state, which is public data, so that a signature needs
  only the one-time signature's hashes.

There are three kinds of image, and they never mix:

| Image | Its anchor table | Its manifests' profile byte | Its build line |
|---|---|---|---|
| Shipped (release) image of a claimed profile (level other than `none`) | release anchors only | the profile's own | `profile=<name> level=<level>` |
| Development image (no profile) | development anchors | 0xFF | `profile=none level=none`, as today |
| **The signing gate's image** (§8.4) | development anchors in place of the release table | the named profile under test, so that the profile check is exercised as shipped | `profile=<name> level=none anchors=development`, and **no claim** |

* **How the gate's image is made.** xtask builds the named profile, then
  links the loader again with only its anchor table replaced. The table
  sits in a section of its own, `.fxanch` (§10.6: PE names are at most 8
  bytes), of fixed size. The gate
  image's build line says `anchors=development` and `level=none`, and the
  boot check compares that line with the profile file under BUILD-FLAGS rule
  6 a. So the image cannot pass as the profile's.
* **The two loaders differ only in the table.** The gate compares the gate
  loader's PE with the shipped loader built from the same commit, byte for
  byte, outside `.fxanch` and the PE checksum and Authenticode
  fields. A difference anywhere else fails the gate. Its control is a
  one-byte change outside the section, which must read FIRED.
* **What S3 refuses, from today (D1).** At build time, xtask refuses the
  UEFI loader and every initramfs of an image of a **claimed profile** (any
  level other than `none`) that holds a development key. It checks at the
  two points every image's loader and uncompressed initramfs pass through
  after their last change: `fat::write_image_carrying` for every image,
  and `flash` for a card. It also checks the loader as it is built. The
  match is a key's tree root `T`, its last 24 bytes, which any anchor
  table carries whatever layout it gives the rest. S4's tables keep `T`
  contiguous. `check-signing-anchors.py --scan` applies the same rule to
  any file. A planted root was refused in a real image build, and the
  control read FIRED (as-built, S3).
* **What comes with S4's tables.** The release-image scan of the loader,
  `init` and `devmgr`, with its planted-release control. Also the gate's
  relinked loader: it is the only image of a claimed profile that may
  carry a development anchor, and only under the gate's output directory.

| Decision | Choice | Reason |
|---|---|---|
| Slot anchors | Compiled into the UEFI loader; two keys | The loader is the first Ferrix code that runs, and the one Secure Boot verifies |
| Config and driver anchors | Compiled into init's verifier and `devmgr`, inside the signed initramfs | `devmgr` reads no files; the anchors are as authentic as the slot |
| Handover | Secure Boot's Authenticode verifies the loader; the loader verifies LMS | UEFI has no LMS; no artifact is signed twice |
| Residual | The firmware's classical chain is outside this scheme; with Secure Boot off there is no root | Stated in A.FIRMWARE and VULNERABILITY-ANALYSIS (§10.3) |
| Firmware state | `SecureBoot` and `SetupMode` on the loader's line, in its words and in the start-up audit record; the authenticity claim needs `on` and `user` | FAU_GEN.1 b; the claim's condition is visible per boot |
| Development | Development anchors only in development images (profile 0xFF) and the gate's relinked loader (`level=none anchors=development`); a byte comparison outside the anchor section, and a search of shipped images, each with a control | Gates can sign; a shipped image never trusts a development key |
| Soft signer | `ferrix-softsign`: the PKCS #11 3.1 HSS subset, `q` committed before release, a reuse check against its own log, development keys only, never for a release | The customer's decision; ledger 3731 (D1)-(D5) |

### 6.5 A CI development key

The customer decided on 2026-10-24 that CI signs with a development key
of its own. Its design, reviewed OK IF (1)-(8) at ledger 4005 and still
waiting for the customer's own OK, is parked on branch
`feature/signing-ci-key`. No key or secret exists.

## 7. (P7) Revocation and anti-rollback

### 7.1 Security versions

Each kind has a **security version**, `security_version`. It is not the
release number. It rises only when a release fixes something an older
release must no longer be allowed to bring back, as Android's rollback index
does. A release that keeps the security version keeps the A/B fallback
working (§7.2). The release process records each rise and its reason in the
signature log.

The **floor** of a kind is the lowest `security_version` a device accepts.
It is held where **root cannot lower it**, and it is raised **only after a
trial boot is marked good** (3564 (S7); AB-BOOT §5's `bootctl good`).

### 7.2 Floors on a board with a TPM 2.0

**Revised after the consultant's review (ledger line 3667, (2)); this
subsection gets its own design re-review before any S5 code.** The first
version protected the indices with `TPMA_NV_POLICY_DELETE`. TPM2_NV_DefineSpace
refuses that attribute unless the platform hierarchy defines the index. The
reference implementation's `NV_DefineSpace.c` checks it: "If
TPMA_NV_POLICY_DELETE is SET, then the index must be defined by the
platform" [16]. Neither the installer nor a UEFI application holds platform
authorisation, so that protection is not available. What follows replaces
it.

**The storage.** Two NV indices per kind, both defined by the **owner
hierarchy** (`TPMA_NV_PLATFORMCREATE` clear) at installation. Indices are
TPM 2.0 Library Specification objects [15].

| Index | Type and attributes | Defined by | Changed by |
|---|---|---|---|
| counter k | `TPM_NT_COUNTER`; `TPMA_NV_AUTHWRITE`, `TPMA_NV_AUTHREAD`, `TPMA_NV_WRITE_STCLEAR`, `TPMA_NV_NO_DA`; empty authValue | the installer, with owner authorisation | `TPM2_NV_Increment` alone: up by one, never down. The loader locks it every boot (step 5) |
| base k | `TPM_NT_ORDINARY`, 8 bytes; `TPMA_NV_AUTHWRITE`, `TPMA_NV_WRITEDEFINE`, `TPMA_NV_AUTHREAD`, `TPMA_NV_NO_DA` | the installer, with owner authorisation | written once with counter k's value at provisioning, then `TPM2_NV_WriteLock`, which `TPMA_NV_WRITEDEFINE` makes permanent |

**The floor is `counter − base`.** The counter cannot fall, and the base
cannot be written again. So the floor can fall only if one of the two
indices is **deleted and defined again**: a new base written higher, or a
new counter that starts lower than the old one had reached. Only owner
authorisation can delete an owner-created index (`TPM2_NV_UndefineSpace`;
the reference `NV_UndefineSpace.c` [16]). Platform authorisation can too,
and so can `TPM2_Clear`, which removes every owner-created index. So the
installer takes those three authorisations out of the running system's
reach:

1. **Owner.** The installer requires an owner authorisation it can use:
   empty, as delivered, or supplied by the person installing. It sets
   `ownerAuth` to a random value that it does not keep. It sets an
   `ownerPolicy` (`TPM2_SetPrimaryPolicy`) that allows, through
   `TPM2_PolicyCommandCode`, the owner commands an operating system still
   needs, `TPM2_CreatePrimary`, `TPM2_EvictControl` and `TPM2_NV_DefineSpace`,
   and **no** `TPM2_NV_UndefineSpace`, `TPM2_HierarchyChangeAuth` or
   `TPM2_SetPrimaryPolicy`. After that, no one can delete these indices
   through the owner hierarchy. The TPM stays usable for keys under the
   owner hierarchy.
2. **Lockout.** It sets `lockoutAuth` to a random value it does not keep,
   after `TPM2_ClearControl(disable = YES)`. So `TPM2_Clear` through the
   lockout hierarchy is refused, and cannot be turned back on without
   platform authorisation.
3. **Platform.** It is the firmware's. Firmware gives it up before it
   starts the loader. It uses it only for a clear the person at the machine
   confirms through the TCG Physical Presence Interface. That is an
   assumption of use: physical-presence confirmation stays enabled in the
   firmware, and the firmware setup is password-protected (§10.3).

The loader checks that this state holds before it trusts a floor: step 1
below reads `TPMA_PERMANENT` (`ownerAuthSet`, `lockoutAuthSet`,
`disableClear`) and each index's public area.

**What is still possible, and what it gives.** A physical-presence clear
removes the indices and resets the owner hierarchy. The loader then finds
them unprovisioned and refuses (§9.1). But root can then provision again
with its own authorisations, and a re-provisioned floor starts at zero:
**a clear followed by root's re-provisioning is a rollback.** Whether a new
counter after a clear starts above the old one's value is not shown. The
reference `NvFlushHierarchy`, which `TPM2_Clear` runs, deletes counters
without updating the kept maximum that `NvInitialCounter` reads [16].
So the TPM claim is: **root cannot lower a floor without someone
confirming a TPM clear at the machine.** That is a claim against a remote
or software attacker with root, not against one with physical access.

**Whether this holds on PTT and fTPM is not yet shown.** Slice S5 tests, on
the N150's PTT and with `swtpm` in the gate, through the product owner:

* that `ownerPolicy` restricts owner commands as specified;
* that `TPM2_NV_UndefineSpace` and `TPM2_Clear` are refused afterwards;
* that `disableClear` holds.

If any of these does not hold on a family, §7.3 states that family's
rollback claim as **absent** until a mechanism that holds is designed and
reviewed.

**Only on a dedicated claimed install** (§12 Q10). The installer takes the
owner and lockout authorisations only when the person installing chooses a
claimed profile on a dedicated machine, explicitly, at install time. A
dual-boot or desktop install never takes them, and its anti-rollback is
stated absent (`ferrix.rollback=absent`).

**Who writes them: the loader alone,** before `ExitBootServices`, through
`EFI_TCG2_PROTOCOL.SubmitCommand` [17].

1. `TPM2_GetCapability` for `TPMA_PERMANENT`: `ownerAuthSet`,
   `lockoutAuthSet` and `disableClear` must all be set. Then
   `TPM2_NV_ReadPublic` on each index. A missing index, one whose
   attributes, name algorithm or policy are not exactly the installer's,
   or a clear `TPMA_PERMANENT` bit, makes the TPM **unprovisioned**
   (§9.1).
2. Read each counter and base, and compute the floors.
3. Choose and verify the slot as AB-BOOT §4 does, with `floor[1]` as the
   slot's floor.
4. **Raise** only when the slot being started is the **good** slot (AB-BOOT
   attempt kind `good`, never `trial` or `fallback`) and its manifest
   verified. For each kind k, raise its counter until `floor[k]` reaches the
   slot manifest's `floors[k]`, where `floors[1]` is its own
   `security_version`. Raise `floor[1]` only as far as the rescue image's
   `security_version` (§9.2), so that a raise never leaves the machine
   without a rescue path. Do at most 16 increments per kind per boot, so
   the time stays bounded: more are completed over later boots, and while
   any are owed, the loader's line says `floor raise pending`. Then read
   back and compare.
5. `TPM2_NV_WriteLock` on every counter, on **every** boot, raised or not.
   With `TPMA_NV_WRITE_STCLEAR` this locks it until the next TPM reset, so
   nothing after the loader can increment it.
6. Pass `ferrix.floor.slot=… ferrix.floor.config=… ferrix.floor.driver=…
   ferrix.floor.revocation=…` and `ferrix.rollback=tpm` among the loader's
   words.

**Why the slot carries the other kinds' floors.** Config packages and driver
images are verified in ring 3, where no TPM driver exists, and none is
needed if the loader is the only writer. A config package's or a driver
image's floor therefore rises with the slot release that raises it. A
release that raises a carried floor also raises the slot's own
`security_version`. Otherwise an older slot with the same security version
but a lower carried floor could be booted to undo it. The cost is cadence:
a configuration's floor rises only at a slot release, not at the
configuration's own good trial. The customer chose this cadence (§12 Q8).
**The residual window is stated:** until the next slot release, a
configuration or driver package can be rolled back to the floor the current
slot carries. A ring-3 TPM path comes only if field data asks for it, under
its own review.

**What the A/B fallback becomes.** Once `floor[1]` rises, the other slot,
still holding the older release, is below it and is refused as a fallback.
That is the purpose of anti-rollback. `bootctl` gains `bootctl sync`, which
writes the good slot's release into the idle slot once the good is marked,
so that the fallback exists again. AB-BOOT's §6 table gains rows for it
(§10.2).

**Residual of the TPM path.**

* A clear confirmed at the machine, followed by root's own provisioning, is
  a rollback, as described above. A clear without the re-provisioning is a
  denial of service: the loader refuses.
* Intel PTT and AMD fTPM keep their NV state in flash under the firmware's
  control. Their resistance to a physical replay of that flash is the
  platform's (SAFETY-MANUAL AoU, §10.3).
* The gate shows the protection, in `test-ab-boot` case S9 (§8.4).
  Provisioned as above, the booted system tries to undefine the base and
  define it higher, then an older release is armed. The TPM must refuse the
  undefine, and the older release must be refused as below the floor. Two
  controls, each run through `gate.sh control`:
  * provisioning without the owner step: the undefine succeeds, but the
    loader's `TPMA_PERMANENT` check refuses the TPM as unprovisioned. That
    shows the check is what stands behind a weak provisioning;
  * the same, with the loader's `TPMA_PERMANENT` and public-area checks
    sabotaged: the older release boots. It must read FIRED.

### 7.3 Each board

| Board (family) | Tamper-resistant monotonic storage | Anti-rollback claim |
|---|---|---|
| x86-64 UEFI PC with TPM 2.0 enabled (0x0101) | TPM 2.0 NV counters as §7.2 | **absent until S5** shows §7.2's protections on that TPM; then **present against a software attacker with root**, provided a TPM clear needs confirmation at the machine (§7.2) |
| Intel N150, SOYO M4 Pro (0x0102) | Intel PTT, a firmware TPM 2.0: dmidecode "TPM Device, Vendor ID INTC, Specification Version 2.0, Firmware Revision 600.18", and the ACPI TPM2 table in its dmesg (`~/.local/share/ferrix/n150-linux/box-2026-10-16/`) | as the row above, once S5 has run on the box |
| nazuna, ASUS ProArt B650 (the SN720 machine; 0x0101) | AMD fTPM 2.0 (`/sys/class/tpm/tpm0`, version 2) | as the row above; not a release target |
| x86-64 UEFI PC without a TPM, or with it disabled | none | **absent** |
| QEMU x86-64 (OVMF) | `swtpm` behind `tpm-crb` for the gate (installed on the host) | **absent**: the gate's TPM is software and resets with its state file |
| QEMU AArch64 virt, the architecture `max` and `fast` name (0x0201) | none, unless `swtpm` behind `tpm-tis-device` with a TPM-enabled ArmVirtQemu | **absent**; the board that will carry the claim is §12 Q6 |
| Pixel 7 (0x0202) | UFS RPMB and the Titan M2's rollback indices exist, but they belong to Google's bootloader and TEE. With the bootloader unlocked, as Ferrix boots today, Ferrix reaches neither. Re-locking with an AVB custom key would make AVB enforce a rollback index for the whole boot image under its own RSA signature: a persistent write to the phone (§12 Q5) | **absent**. The Pixel 7 has no slot path either, so there is **no slot authenticity claim** on it: its loader carries the kernel and initramfs it was built with (V-12), and nothing verifies them |
| STM32MP157D-DK1 (0x0302) | none usable: microSD only, and OP-TEE's RPMB secure storage needs eMMC. OP-TEE's REE file system can be rolled back without RPMB. The BSEC OTP fuses could hold a fused counter, but fusing is irreversible: a persistent write needing the customer (§12 Q5) | **absent** |
| QEMU ARMv7-A virt (0x0301) | none | **absent** |

**Where the claim is absent**, the loader passes `ferrix.rollback=absent` and
the floors as 0. The boot line and the start-up audit record say so
(FAU_GEN.1 b). Verification of signature, kind, profile and family still
holds. What is not claimed is that an older signed release cannot be
booted. A profile whose claim needs anti-rollback names the families it
claims (§10.1), and a family without storage is not among them.

### 7.4 Revoking a key

* **A key** is revoked by a **revocation manifest** (kind 4). Its
  `target_kind` names the kind, and its `revoked` field lists up to 20
  trees' `I`: with `L = 1`, the keys' own identifiers, and with a later HSS
  key, its lower trees'. It is signed under that kind's **other** key, and
  its `security_version` is the revocation epoch.
  * For **kind 1**, the loader reads it from `FERRIX/REVOKED.SIG` on the ESP.
    Its floor is the kind-4 counter (§7.2). A floor above zero with no file
    at or above it fails closed (§9.1), so deleting the file does not undo
    a revocation. The loader raises the revocation floor to the file's epoch
    only when the good slot still verifies with the list applied. That way
    a revocation cannot strand a machine on its own. Until then, the list
    applies in memory on every boot.
  * For **kinds 2 and 3**, the revocation manifests are part of the slot
    image's initramfs, covered by its signature and its floor.
* **A key** is retired for good by anchors without it (§6.2) and a raised
  floor.
* A revoked key's past signatures fail at once wherever the list is
  applied. Before it publishes the revocation, the release process re-signs
  under the other key whatever must keep booting.

**Revocation between peer keys** (ledger 3738 (1)). With `L = 1` a kind's
two keys are peers: whoever holds either can sign a revocation of the
other. So:

* **(a) A revocation that lists its own signing key's `I` is refused.**
  A key never revokes itself, so a revocation always comes from the other
  key.
* **(b) The list in force applies to the revocation file itself.** When
  `REVOKED.SIG` (or a kind-2/3 revocation in the initramfs) is verified,
  the list already in force applies: the compiled-in list, and the last
  revocation applied whose epoch the floor holds. A newer file signed by a
  key that is already revoked is therefore refused, and so is any later
  revocation it would carry.
* **(c) The residual.** If one key is compromised and the attacker has
  already signed with it a revocation of the other key, the kind is left
  with no unrevoked key. Recovery is only the "both modules lost" path of
  §4.2. VULNERABILITY-ANALYSIS gets this row with slice S7.
* **(d)** §8.1 checks (a) and (b) with a row each, and §8.3 lists both as
  negative tests.

| Decision | Choice | Reason |
|---|---|---|
| Rollback storage | TPM 2.0 NV counter per kind and a write-once base, both owner-created; the owner and lockout authorisations taken out of reach, `ownerPolicy` without undefine; the loader checks `TPMA_PERMANENT`, is the only writer, and locks the counters every boot | Hardware-monotonic counter; no ring-3 TPM driver; 3564 (S7)'s "raised only after a good trial". `TPMA_NV_POLICY_DELETE` needs the platform hierarchy and is not available (ledger 3667 (2)); re-reviewed before S5 |
| What the TPM claim is | Root cannot lower a floor without a TPM clear confirmed at the machine | A clear and root's re-provisioning is a rollback; stated, not hidden |
| Raise point | The loader, on a boot of the good slot, to the floors that slot's manifest carries; at most 16 steps per boot | Raised only after `bootctl good`; bounded time |
| Other kinds' floors | Carried by the slot manifest; a carried raise also raises the slot's version | No path for ring 3 to lower or raise; the cost is cadence (§12 Q8) |
| Boards without storage | DK1, Pixel 7, every QEMU target, PCs without a TPM: the claim is **absent**, said on the boot line | (P7) |
| Revocation | Kind-4 manifests per target kind, with their own floor; roots retire through anchors | Deleting a file cannot undo a revocation |

## 8. (P8) Qualifying the verifier

The verifier is **product software** at the level of the profile that ships
it (§5.3), not a tool. Its evidence:

### 8.1 Requirements, one check each

Requirement ids are reserved before code (§10.4). Each requirement below has
one host test, or one boot sub-case, and a negative control.

| Requirement | Statement (short) | Check |
|---|---|---|
| sig-format | A manifest that breaks any rule of §1.2's structure is refused, with its reason | host: every field, with a sabotage control |
| sig-types | Only §2.1's type codes and level counts are accepted | host: every code 0..=0x18 and 0xFFFFFFFF, both levels |
| sig-lmots | LM-OTS verification per RFC 8554 §4.6, Algorithm 4b | host: vectors (§8.2) |
| sig-lms | LMS verification per RFC 8554 §5.4.2 | host: vectors |
| sig-hss | HSS verification per RFC 8554 §6.3, with `Nspk + 1 = L` | host: vectors |
| sig-anchor | The anchor set is the caller's expected kind's, never the manifest's | host: kind-1 signature against kind-2 anchors |
| sig-bind | kind, profile, arch, family, subject and floor equal the caller's | host: each field wrong by one; a slot manifest presented where a revocation of kind 1 is expected, and a revocation of kind 1 presented as a slot (they share kind 1's anchors, §1.1) |
| sig-revoke | A signature by a revoked key (or through a revoked tree) is refused | host |
| sig-revoke-self | A revocation listing its own signing key's `I` is refused | host |
| sig-revoke-chain | A revocation signed by a key the list in force already revokes is refused | host |
| sig-payload | The caller's payload digest equals `payload_sha256`, and `payload_len` matches | host and boot: AB-BOOT case 4's flipped byte |
| sig-time | One verify is at most 50 ms on the DK1 at 650 MHz | DK1 measurement of the worst-case signature (S2); `MEMORY-AND-TIMING.md` |
| sig-stack | One verify uses at most S bytes of stack and no heap, S from the build's stack report | the stack report of the loader's and `devmgr`'s builds; `MEMORY-AND-TIMING.md` |
| sig-loader-* | The loader's slot path: refuse unsigned or wrong-kind slots in a signed profile, the floors, the raise, the lock (§7.2 steps 1-6) | `test-ab-boot` cases (§8.4) |
| sig-cfg-*, sig-drv-* | init's and `devmgr`'s refusals (§9) | boot sub-cases in their own slices |

### 8.2 Test vectors

* RFC 8554 Appendix F, Test Case 1 (SHA-256/256, `L = 2`): **a refusal by
  type code and level count only**. The verifier computes only 24-byte
  hashes (`N` is fixed at 24), so a SHA-256/256 signature is never verified,
  only refused (ledger 3803 (6)).
* RFC 9858 Appendix A.1 (SHA-256/192, W8, H5), which verifies only through
  the test-only wide set compiled under `cfg(test)`, and **A.4 (SHA-256/192,
  W4, H20, `L = 1`)**, which is the product set itself.
* NIST ACVP LMS vectors [18], `LMS-sigVer-1.0` and `LMS-sigVer-SP800-208` in
  usnistgov/ACVP-Server's `gen-val/json-files`: all 16 sample cases, run
  through the same code in the wide set. **The published sample has no
  N24_W4 case with M24_H10, H15 or H20**: its sets are M24_H5/N24_W1 and
  M24_H10/N24_W2. ACVP vectors for the product set itself are owed before
  the first claimed release (`docs/backlog/p2.md`, the row "ACVP vectors
  for the product LMS set").
* Signatures the development signer makes, and those of `hbs-lms` 0.1.1
  with its type codes mapped (§5.1). The cross-check is a second,
  independent implementation, not a vector source.

### 8.3 Negative tests, one per field, each with a control

For every byte of a valid manifest block and signature (4,096 positions),
flipping each bit must give a refusal, and the expected reason where the
field is known. Then, one by one:

* a wrong type code at each level;
* `q` at 2^h, and `q` changed in the signature;
* `Nspk` too high and too low;
* a signature truncated by one byte, extended by one byte, and
  `sig_len` off by one each way;
* the wrong kind, profile, arch, family and subject;
* `security_version` at floor − 1, and the floor itself (accepted);
* a revoked `I`;
* a revocation listing its own signing key (§7.4 (a));
* a revocation signed by an already revoked key (§7.4 (b));
* a payload one byte short or long.

Each of the verifier's comparisons gets a source sabotage run by
`fleet/gate.sh control <ref> <tag> <file> <old> <new> --expect-line <text>`,
which must read **FIRED** and is recorded by its log's hash, as AB-BOOT §9
does. For example: the Winternitz chain's final comparison removed; the
kind check removed; the floor check removed; the revocation check removed;
the zero-padding check removed; the `Nspk + 1 = L` check removed. No `cfg`
and no feature.

### 8.4 The gate

`cargo xtask test-ab-boot` gains signed cases on x86-64 (OVMF, KVM, with
`swtpm`). They use development anchors in a claimed-profile build. Only the
gate's image may carry them, by a gate-only exception named in `ITEM.md`:

| # | Setup | Required |
|---|---|---|
| S1 | slot B signed for kind 2 | refused, "wrong kind"; A boots |
| S2 | slot B signed, its kernel byte flipped after signing | refused at the seal; A boots |
| S3 | slot B's manifest for another family | refused, "wrong family" |
| S4 | floor 3; slot B at version 2 | refused, "below floor"; no TPM write |
| S5 | trial B at version 4, marked good, next boot | the counter rises to 4 on the good boot, not the trial boot; locked after |
| S6 | after S5, a write to the counter from the booted system | refused by the TPM (locked) |
| S7 | revocation file deleted after its floor rose | the loader refuses and says why |
| S8 | TPM index deleted (swtpm state reset) | "unprovisioned": refused in the claimed profile |
| S9 | the booted system undefines the base and defines it higher, then arms an older release | the undefine is refused by the TPM; the older release is refused below the floor; the two controls of §7.2 |
| S10 | a version-2 slot header with a non-zero `generation` | refused (§1.2) |
| S11 | rescue image below a raised floor | the floor is not raised past it (§9.2) |

### 8.5 Fuzzing, coverage, bounds

* **Fuzzing.** A `cargo fuzz` target over the whole 4 KiB block and the
  anchor set, seeded from the vectors, runs with the fuzz corpus tooling
  (`tools/common/gen`, fuzz corpus seeds). Its run time and findings are in
  the slice's evidence.
* **Coverage.** At the criterion of the crate's level: MC/DC for `sc4`;
  statement and branch for `sc2`/`present`, as their rows say. The coverage
  is measured over the host tests and the gate.
* **Bounded time and memory.** There is no allocation, no recursion, and
  every loop is bounded by a type code's constant (`p`, `2^w − 1`, `h`,
  `L ≤ 2`). The work is not the same for every input: the length of each
  Winternitz chain depends on the message digest's digits. But it is
  bounded by **p × (2^w − 1) chain calls per level** (765 for N24_W4), plus
  the K, Q and h tree hashes, and the bound of §2.3 is computed from that
  worst case. Timing reveals nothing secret: the signature and the message
  are public. The stated bounds go into `MEMORY-AND-TIMING.md`.

### 8.6 Tool confidence: the signer and the HSM

The signing pipeline, `tools/common/release/sign` (slice S6), builds the
manifest from the release's artifacts, sends bytes 0..512 to the HSM over
PKCS #11, writes the block and appends the signature log. It is a tool
whose output becomes product data:

* class T3 under IEC 61508-3 7.4.4 [19];
* TI2 under ISO 26262-8 clause 11 [20]: a wrong digest, kind or profile
  could be signed.

**The detection measure is the device's own check.** Every field the tool
writes is verified independently by the qualified verifier on the device:
digest, kind, profile, arch, family, subject, floor. Before publishing, the
pipeline also runs the host build of the same `ferrix-sigverify` on each
block, and boots the release's slot images in the release gate. A wrong
signature or a wrong field therefore cannot reach a device as a working
artifact. It is refused there, fail closed (§9). This gives TD1, hence TCL1,
under ISO 26262-8 §11.4.5, and the 61508-3 7.4.4.10 argument of an
independent check of the output. The one error the device cannot detect is
**OTS-key reuse**: two valid signatures from one leaf. It is the HSM's to
prevent (§3.1: the CMVP-validated state handling, and the power-cut
acceptance test), and the log's to detect (§3.4), with revocation (§7.4) as
the reaction. `TOOLS.md` gets both rows.

**The soft signer** (§6.4) is a development tool, classified in `TOOLS.md`
with no qualification beyond that row (ledger 3731 (D4)). Its output is
checked by the same independent verifier on the device (TD1), and it never
signs a release. Its state rule and reuse check are tested anyway, so that
the custody protocol is exercised before any HSM exists (D2).

| Decision | Choice | Reason |
|---|---|---|
| Status | Product software at the shipping profile's level, requirements one check each | (P8) |
| Vectors | RFC 8554 App. F, RFC 9858 App. A (A.4 in the product set), ACVP LMS sigVer | Three independent sources |
| Negatives | Every bit flip, every field, and a FIRED control per comparison | (P8) |
| Tool confidence | Signer T3 / TI2, TD1 through the device's independent check → TCL1; OTS reuse on the HSM and the log | ISO 26262-8 cl. 11, IEC 61508-3 7.4.4 |

## 9. (P9) Fail closed, every refusal audited

### 9.1 What each verifier does on a refusal

| Where | Refused | What happens |
|---|---|---|
| Loader, a slot | Any rule of §1.2, header version 1 in a signed profile, or below `floor[1]` | That attempt is not started. The next attempt in AB-BOOT §4's list is tried; a TRIAL attempt that fails verification costs no try and is never decremented (AB-BOOT P1 stands) |
| Loader, no slot verifies | — | The rescue image (§9.2), verified the same way; otherwise `FERRIX-PANIC loader`, with each slot's reason |
| Loader, rollback storage unprovisioned (§7.2 step 1) | In a profile whose claim needs it: everything | `FERRIX-PANIC loader: rollback storage missing`; the installer re-provisions. In a profile without that claim: boot, with `ferrix.rollback=absent` |
| Loader, revocation floor above zero and no file at or above it | Every slot | As "no slot verifies" |
| init, a configuration package | Any rule, below `floor[2]` | The previous active package stays (3564 (S7)); the refused one is not staged; with no valid package at all, the boot stops before any unit reads configuration |
| `devmgr`, a driver image | Any rule, below `floor[3]`, `subject` not the driver UPDATE names | The answer `refused`; nothing is stopped; the running driver keeps its image (`DEVMGR.md` §4.1, step 1 is never reached) |
| `bootctl try` | Any rule | Nothing is written to the idle slot |

### 9.2 The rescue image in a signed configuration

AB-BOOT §4 step 7 boots `FERRIX/KERNEL.ELF`, `INITRD.IMG` and `CMDLINE.TXT`
unverified. In a profile that requires signing, the loader reads
`FERRIX/RESCUE.SLT` instead: a slot image file, header version 2, with its
manifest, verified exactly as a slot is, **against `floor[1]` like a slot**.
The unsigned files are not read. A profile without signing keeps today's
path.

**A raise never leaves the machine without a rescue path.** The ESP is
written only by the installer (AB-BOOT §7: `bootctl` never writes it). So:

* the loader raises `floor[1]` no higher than the rescue image's
  `security_version` (§7.2 step 4);
* a release that raises the slot's security version ships its rescue image,
  and the installer's ESP update writes it;
* until that update runs, the floor waits, and the loader's line says
  `floor raise pending (rescue at <v>)`.

AB-BOOT §6 gains rows for this: a raise held back by an old rescue image;
the rescue image below the floor (refused, the machine boots its good
slot); and a fallback refused below the floor after a raise.

### 9.3 Every refusal is audited

* **The loader** prints one line the gate reads,
  `  sign     refused slot <a|b|rescue>: <reason> (kind <k> anchor <a> version <v> floor <f>)`,
  before the attempt line it already prints. The line goes to the console
  and the boot log, and to the event-log partition when a panic report
  follows (`POSTMORTEM.md` §4).
* **init and `devmgr`** write an audit record to lode's journal: the kind,
  the reason, the label, the manifest's SHA-256, the floor, and the uid of
  the client for a driver update. 3564 (S6)'s boot record names the
  configuration that ran.
* **The reasons** are one enum in `ferrix-sigverify`, so every verifier
  names them the same way: `Malformed(field)`, `TypeCode`, `Levels`,
  `UnknownAnchor`, `BadSignature`, `Revoked`, `WrongKind`, `WrongProfile`,
  `WrongArch`, `WrongFamily`, `WrongSubject`, `BelowFloor`, `PayloadMismatch`,
  `Unsigned`, `RollbackUnprovisioned`.

| Decision | Choice | Reason |
|---|---|---|
| Refusal | Never applied; the slot falls back, the configuration keeps the previous package, the driver keeps its image | (P9); 3564 (S7); `DEVMGR.md` §4.1 |
| Rescue path | Signed too, in a signed profile | Otherwise root writes the ESP's files and the scheme is bypassed |
| Audit | A loader line; a lode audit record from init and `devmgr`; one reason enum | (P9); FAU_GEN.1 |

## 10. (P10) What changes, and in what order

### 10.1 Flags and profiles

| Key | Values | `max` | `fast` | `present` | `desktop` | Note |
|---|---|---|---|---|---|---|
| `slot_signing` (image, new) | `off`, `required` | `off` until M7, with no slot path: `max` is updated by a fresh install (§12 Q9) | `required` | `off` until a release key exists, then `required` as a dated change (§10.6) | `off` | Says whether the loader refuses header version 1 and the unsigned rescue files |
| `rollback` (image, new) | `absent`, `tpm` | per claimed family | per claimed family | `tpm` on the families that have it | `absent` | A profile names the families its rollback claim covers |
| `config_updates` (3564 (S11)) | `absent`, `signed` | `absent` (3564 Q4) | `absent` until (S1)-(S6) exist, then `signed` as a dated change | not set by this design | not set | This design is the signing half of (S4) only |
| `driver_updates` | `absent`, … | `absent` | `absent` | `absent` | `absent` | D3 becomes buildable; the reference configuration stays `absent` (3564 (S11)); a live driver swap is a live change |

The keys go to BUILD-FLAGS §3's "not flags" list as image content, each with
its row and review, as `driver_updates` is.

### 10.2 The follow-ons, and what each needs before code

* **drvupdated D3.** `devmgr` verifies kind 3, as in §1.3 and §9.1. The
  driver floor reaches `devmgr` without a kernel change, through init's
  start arguments, once `ferrix.devmgr=init` is the only path (M5). D3
  becomes buildable. AoU-26 is rewritten for a signed update, and stays "not
  in a claimed configuration" while `driver_updates` is `absent`.
* **`fast`'s `config_updates = "signed"`.** This design gives (S4)'s
  signing, key custody, revocation and anti-rollback. (S1)-(S3), (S5) and
  (S6) are still owed by the configuration design, and the move needs
  3564's review of all six.
* **A/B slots.** They move from SHA-256 alone to signed manifests: header
  version 2, the loader's slot and rescue paths, the TPM floors, `bootctl
  try`'s early check and `bootctl sync`. Header version 2 has a zero
  `generation` (§1.2). AB-BOOT.md's §1 ("not secure boot"), §2, §4 step 5
  and step 7, §6's table (with §9.2's rescue and floor rows), §8
  (A.FIRMWARE, V-12) and §9's cases change with slice S4. Its P1-P5 stay, and the loader's new
  properties become P6-P9 (§8.1's sig-loader-*).

### 10.3 The certification documents (with the slice that makes each true)

| Document | Change |
|---|---|
| `SECURITY-TARGET.md` | §1.2 and A.FIRMWARE: the loader verifies a slot's LMS signature and the floors; A.FIRMWARE keeps UEFI, Secure Boot and the loader's own integrity (§6.3). New SFRs: **FCS_COP.1/SigVer** (HSS/LMS verification, SP 800-208, the parameters of §2.1) and **FPT_TUD_EXT.1** (integrity of updates: slot, configuration, driver; the extended component as the OS protection profile words it); FPT_TUD_EXT.2's anti-rollback where `rollback=tpm`; FAU_GEN.1 gains the refusals and the Secure Boot state (§6.3). The authenticity claim is stated as conditional on `secureboot=on` and `mode=user`, and the rollback claim on the TPM state of §7.2. §9.2 "No trusted boot path" is narrowed, not removed |
| `VULNERABILITY-ANALYSIS.md` | V-12 restated: verified where signing is required, residual = the classical firmware link and Secure Boot off. New rows: OTS-key reuse at the signer (HSM, log, revocation); the parser of hostile signature bytes (fuzzed, §8.5); rollback on families without storage (absent claim); TPM clear by physical presence (denial of service, not rollback); a fault-injected verify (physical; out of scope, stated) |
| `SAFETY-MANUAL.md` | AoU-26 and AoU-29 updated. New AoUs: the integrator enables Secure Boot in user mode with Ferrix's `db` entry (the claim holds only then, §6.3); provisions the TPM through the installer; keeps TCG Physical Presence confirmation enabled and the firmware setup password-protected, so that a TPM clear needs a person at the machine (§7.2); boards without storage carry no rollback claim; the custodians and the signature log are under configuration management |
| `ITEM.md`, `certification-item.json` | `ferrix-sigverify` and `ferrix-sha256` classified (§5.3); the gate-only development-anchor exception (§8.4) |
| `MEMORY-AND-TIMING.md` | The verify bound (§2.3, §8.5) |
| `TOOLS.md` | The signing pipeline and the HSM (§8.6) |

### 10.4 Requirement ids

None are written here. Before the first code slice, a docs-only landing
reserves **`H.SIGN.1-6`**, **`L.sigverify.1-24`** and **`L.sha256.1-6`** in
`tools/common/data/requirement-reservations.json`, under the lock and with
the consultant's review (CONVENTIONS, "Requirement ids are reserved before
they are written"). `H.SIGN` is the hazard-level family: authenticity,
binding, anti-rollback, revocation, fail-closed and audit, each stating the
verifier's own decision. Its parent is `ASR-7` alone (ledger 3738 (7)):
O.VALIDATE is the system-call boundary's objective and does not fit. S4
adds the update objective, its threat, FCS_COP.1/SigVer and FPT_TUD_EXT.1,
and re-parents H.SIGN then. `L.sigverify` holds §8.1's rows, and `L.sha256`
holds `ferrix-sha256`'s. None of the three prefixes is used on `develop`.
What the loader, init and `devmgr` do with a refusal gets ids in their own
slices, reserved the same way.

### 10.5 Slices

| | Slice | Needs first | Points |
|---|---|---|---|
| S0 | This design; the consultant's verdict; the reservation landing | — | 2 |
| S1 | `ferrix-sha256` (new; `ferrix-slots` moves onto it in S4); `ferrix-sigverify` (format, LM-OTS, LMS, HSS, policy), its host tests, vectors, controls, fuzz target and coverage; `ferrix-softsign`, the development soft signer (§6.4) | S0; the reservation of §10.4 | 8 |
| S2 | The DK1 measurement of §2.3 and `MEMORY-AND-TIMING.md`'s bound | S1; the product owner's OK for the board | 2 |
| S3 | The anchors file; xtask's refusal of development keys in claimed images and of the verifier in any kernel; `check-signing-anchors.py`; the hardware-family registry; the kernel-closure and no-signing-dependency rules | S1 | 3 |
| S4 | The loader: header version 2, the slot and rescue paths, refusal lines; the generated anchor table in the loader (`init`'s and `devmgr`'s come with S8 and S9, §10.6), with the release-image scan; the gate's relinked loader and its byte comparison; `bootctl try`'s check; `test-ab-boot` S1-S3 | S1, S3 | 5 |
| S5 | TPM floors: the installer's provisioning (owner and lockout authorisations, `ownerPolicy`), the loader's `TPMA_PERMANENT` check, read, raise and lock through TCG2, the words; `bootctl sync`; `test-ab-boot` S4-S11 with `swtpm`; the check on the N150's PTT | S4; §12 Q10; the consultant's re-review of §7.2; the product owner's OK for the box | 8 |
| S6 | The PKCS #11 module backend of the `Token` trait, the signature log repository, the runbook of §4, the HSM acceptance test | the HSMs, bought for the first claimed release with signed updates (§12 Q2) | 5 |
| S7 | Revocation manifests (kind 4) in the loader | S5 | 3 |
| S8 | init's configuration verifier | 3564 (S1)-(S6) | theirs |
| S9 | `devmgr`'s check (D3) | S1, S3 | 3 |

### 10.6 Slice S4's design (reviewed, ledger 4016; 2026-10-24)

S4 makes the UEFI loader verify a slot's manifest. This section is S4's
plan as the code will build it. Where it departs from the text above, it
says so.

**What does not change.** A slot or ESP written today keeps booting.

* The control record, header version 1 and the boot info are unchanged.
* The loader's command-line words keep their form. S4 adds one word at
  the end of them, `ferrix.slot.sig=<anchor id>|none`. The kernel
  ignores it, and `bootctl` reads words first-match.
* Every profile that builds an image today keeps `slot_signing = "off"`
  (below). With it off, the loader still boots header version 1 and the
  ESP's unsigned files, exactly as today.
* nazuna's SN720 runs a development image's loader from the ESP, with
  slot B written as header version 1. A new loader of a development image
  boots it unchanged. It reaches nazuna only when that disk is installed
  again.

**Header version 2** (§1.3, in `ferrix-slots`):

* version 2, bytes 16..24 (`generation`) zero, refused otherwise (§1.2);
* bytes 4096..8192 the manifest block, every blob at or after 8192;
* `SlotHeader::lay_out` gains a version argument. Version 1 keeps today's
  layout. `bootctl` writes the version its image is told to.

**The decision, in an item crate.** §5.3 asks for the enforcement point
to move into an `item` crate. S4 builds it as **`ferrix-boot-policy`**,
in `src/lib/proto/boot-policy`, `no_std`, no `unsafe`, no allocation. Its
one entry point takes:

* the slot header page and manifest block as read;
* the trust policy (below);
* the detected family;
* the floors, all zero until S5.

It returns either `Start { anchor_id, security_version }` or a `Refusal`
with its reason. The blobs' hashes are still compared in `ferrix-slots`
(`Blob::matches`), on the one copy the loader hands over. The loader's
`slots.rs` keeps the Block I/O, and it starts a slot only on `Start` with
every blob matched. `ferrix-slots` moves onto `ferrix-sha256` and becomes
`item` too, as §5.3 says, since the signed seal passes through it.

| Header | `slot_signing = off` | `slot_signing = required` |
|---|---|---|
| version 1 | started on its hashes, as today; `ferrix.slot.sig=none` | refused, `Unsigned` |
| version 2 | verified; refused on any refusal | verified; refused on any refusal |
| ESP, no slots (`run`, the DK1 card, a bare image) | today's unsigned files | `FERRIX/RESCUE.SLT` only, verified as a slot (§9.2); the unsigned files are not read |

With `off`, a version-2 header is never started unverified. `off` means
"unsigned slots are accepted", not "signatures are ignored", so a
version-2 slot cannot be downgraded to its hashes alone.

**The trust policy.** One generated block holds the slot-signing mode
and the slot kind's anchors: `version u16, mode u8, count u8`, then up
to eight entries of `kind u16, anchor_id u16, public_key [u8; 52]`
(56 bytes each), padded to 512 bytes. Each key's root `T` stays
contiguous, so S3's match by `T` still finds it.

* It sits in a PE section of its own, `.fxanch`. PE images limit section
  names to 8 bytes, so the first draft's `.ferrix_anchors` could not be
  used (§6.4 now says `.fxanch`).
* The loader reads the block through `core::hint::black_box` of its
  address, so the compiler cannot fold the anchors into code outside the
  section. The byte comparison below holds that.
* `xtask gen-anchors` writes the block from `signing-anchors.toml` and the
  profile into `src/boot/common/uefi/anchors/<table>.bin`, one per table:
  `release-off`, `release-required`, `development-off` and
  `development-required`. `--check`, in `cargo xtask check`, refuses a
  file that differs from what the generator would write.
* Which block a build takes is an environment variable read by the
  loader's `build.rs` (`FERRIX_ANCHORS`), set only by xtask. It is a build
  input, not a Cargo feature or `cfg`, so the loader keeps §8's "no
  feature, no cfg".

| Image | Its block |
|---|---|
| Development image (no profile) | `development-off` |
| An image of a profile (`max`, `fast`, `present`, `desktop`) | `release-<its slot_signing>` |
| The signing gate's image | `development-required`, the profile's own otherwise; its build line says `anchors=development slot_signing=required level=none` |

The release table is empty until the HSMs exist. **A profile set to
`required` therefore refuses every slot and has no rescue path**, so no
profile buildable today can be `required` yet.

**The profile key** `slot_signing` goes in each profile file's `image`
table, beside `slot_families`, the families the loader accepts in order.
This departs from §10.1, which set `present` to `required` once S4 lands.

| Profile | `slot_signing` | `slot_families` | Note |
|---|---|---|---|
| `present` | `off` | its claim's families | `required` is a dated change once a release key exists (S6); flipping it before then makes every installed `present` disk unbootable |
| `fast` | `required` | its claim's families | Not buildable before M5 |
| `max` | `off` | none | No slot path (§12 Q9) |
| `desktop` | `off` | its own family, then the generic one | §1.4 |

The generator refuses `required` in a profile that is buildable today
while `[release]` is empty, with its own self-test.

**The family** is detected by the registry's rule where the loader already
has the data:

* SMBIOS through `ferrix-smbios` for the N150 and QEMU;
* the architecture's generic family otherwise.

The DK1's device-tree rule is not built, because the DK1 card has no slot
path (AB-BOOT §4 step 1).
`ferrix-boot-policy` holds the rule from the registry. The loader passes
it the SMBIOS strings.

**Floors and revocation.** `ferrix.rollback=absent` on every board until
S5: the floors passed are zero, and the line says so. The revoked list is
empty until S7.

**Refusal lines** (§9.3), before the attempt line the loader already
prints:
`  sign     refused slot <a|b|rescue>: <reason> (kind <k> anchor <a> version <v> floor <f>)`,
and on a start, `  sign     slot <a|b|rescue> anchor <a> version <v>`.

**`bootctl try`'s check** uses `ferrix-boot-policy` with the running
image's block, which the running system cannot read from the loader. So
`bootctl` is built with
the same generated block as its image's loader, from the same build
variable, and writes nothing to the idle slot on a refusal (§9.1). The
check is advisory: the loader alone decides.

**The release-image scan** (§6.4, "what comes with S4's tables"). This
replaces nothing S3 does; it adds a check at the same two points
(`fat::write_image_carrying`, `flash`). The loader of an image of a
profile must hold `.fxanch` byte-equal to `release-<mode>`. Anything
else is refused, and so is a development root anywhere. Its control
plants a development block in a `present` image, which must read FIRED.

**The gate's relinked loader** (§6.4). `test-ab-boot --signed` builds the
named profile's loader, then builds it again with
`FERRIX_ANCHORS=development-required`. It compares the two PE files byte
for byte, outside `.fxanch` and the PE checksum, and fails on any other
difference. Its control changes one byte outside the section, and must
read FIRED. The gate's image is written only under the gate's output
directory, never to `build/`.

**`init` and `devmgr`.** §10.5 put their generated tables in S4. They
have no verifier to read them until S8 and S9, so a table there would be
dead data that no test reaches. S4 builds the generator and the scan for
any binary, and **S8 and S9 add the two tables**, with their own scan
rows. This departs from §10.5; the consultant accepted it (ledger 4016).

**The gate.** `test-ab-boot --signed` on x86-64 runs §8.4's S1-S3 and
S10, with a rescue case:

* S1, wrong kind;
* S2, a flipped kernel byte;
* S3, a wrong family;
* S10, a non-zero `generation`;
* R1, a signed `RESCUE.SLT` boots when both slots are refused, and a
  missing one panics with each slot's reason.

The unsigned cases of AB-BOOT §9 keep running as they are, against
development images with `off`. S4-S9 and S11 need S5.

**Requirements.** The loader's ids are reserved before code (§10.4), in
a docs-only landing with the consultant's review (ledger 4032):

* `L.bootpolicy.1-12` for `ferrix-boot-policy`;
* `L.uefisig.1-5` for the loader's own behaviour, §8.1's `sig-loader-*`
  rows (AB-BOOT's P6 onwards);
* `H.SIGN.7`, the loader starts only what the policy returned `Start`
  for;
* `L.slots.1-20` for `ferrix-slots`.

Each is listed below with its statement and check.

**The DK1's slot-hash cost.** Measured and fixed in this slice: 2.29 s
per 32 MiB in software (MEMORY-AND-TIMING §2.2z). HASH1 waits for the
consultant's ruling on a peripheral in the trust path.

**The consultant's review** of this design is **OK IF (1)-(12)**, ledger
4016. (1) is the reservation, and (2) is this text. The rest bind the code:

3. **The block fails closed.** `ferrix-boot-policy` parses it, and each of
   these refuses every slot with its own `Refusal`, with a host test and a
   control: an unknown version or mode, a count above 8, non-zero padding,
   a kind other than the slot kind, and a block shorter than 512 bytes.
4. **The build fails closed.** The loader's `build.rs` declares
   `rerun-if-env-changed=FERRIX_ANCHORS` and `rerun-if-changed` on the four
   block files. `FERRIX_ANCHORS` unset or unknown is a build error, never a
   default block. `.fxanch` is read-only, and the scan checks its flags and
   size.
5. **The comparison is the evidence.** `black_box` is only the means. The
   two builds are reproducible apart from the section and the PE checksum,
   and a third build of the same block compares equal to show it.
6. **The family fails closed.** Missing or bad SMBIOS gives the generic
   family, which a claimed profile without the generic one refuses. The
   sentence on a DK1 rescue image was dropped, since nothing tests it.
7. **Gate cases for this slice's own rules**, beside S1-S3, S10 and R1:
   * N1, a version-2 slot with a bad signature under `off` is refused;
   * N2, a version-1 slot under `required` is refused (`Unsigned`), and the
     ESP's unsigned files are not read;
   * N3, a version-2 slot under `release-off` (an empty table) is refused;
   * N4, a slot command line carrying `ferrix.slot.sig=` changes nothing.

   Controls, each FIRED by the case named: the downgrade (N1), unsigned
   (N2), a start on a `Refusal` (S1, S3), a start without every blob
   matched (S2), the generation (S10), the scan and the relink.
8. **`ferrix-slots` into the item.** Its own SHA-256 is removed for
   `ferrix-sha256`, with identical digests. `certification-item.json`
   gains both crates. The traceability, fallible-allocation and unsafe
   baselines are updated, host coverage covers both, and after the final
   rebase carry-coverage then `gen-coverage-justification --check` run,
   with dropped anchors listed.
9. **Transcripts.** The new `sign` lines, and the recorded transcripts they
   change, are listed for all four boots (ARMv7-A at `--smp 2`). Under
   `off` with version 1, the lines match the merge base apart from
   `ferrix.slot.sig=none`.
10. **`present`.** No document claims signed slots for `present` until S6
    flips it. SECURITY-TARGET and CLAIM.md state the residual: an unsigned
    version-1 slot boots on `present`. S6's design writes signed slots
    before the loader goes `required`. The customer is told of this
    departure from §10.1 through the product owner.
11. **`bootctl`'s check is advisory.** The loader alone decides. A mismatch
    between the two at worst refuses a trial.
12. **Rows on the landing hash:** `check` and check-docs; `test-ab-boot`
    unsigned and `--signed` (x86-64, KVM); `test-boot` on all four boots;
    the scan and relink controls; the verify cost stated.

**The requirement ids** (reserved in
`tools/common/data/requirement-reservations.json`; each has one check):

| Id | Statement | Check |
|---|---|---|
| H.SIGN.7 | The loader starts a slot only when `ferrix-boot-policy` returned `Start` for it and every blob matched its signed digest on the copy it starts | boot: S1-S3, N1, N2 with controls |
| L.bootpolicy.1 | A policy block that is short, of an unknown version or mode, with a count above 8, non-zero padding or a non-slot kind refuses every slot, by its own refusal | host: one test and one control per case (condition 3): `a_short_block_refuses_every_slot`, `an_unknown_block_version_or_mode_refuses_every_slot`, `a_count_above_eight_refuses_every_slot`, `non_zero_block_padding_refuses_every_slot`, `a_non_slot_anchor_kind_refuses_every_slot` |
| L.bootpolicy.2 | A version-1 header is started on its hashes under `off`, with no anchor, and refused `Unsigned` under `required` | host |
| L.bootpolicy.3 | A version-2 header with a non-zero `generation` is refused | host |
| L.bootpolicy.4 | A version-2 header's manifest is verified against the block's anchors with the slot kind expected, and any refusal is returned | host |
| L.bootpolicy.5 | A version-2 header is never started unverified, under either mode | host |
| L.bootpolicy.6 | The manifest's payload is the header's seal and length, or the slot is refused `PayloadMismatch` | host |
| L.bootpolicy.7 | A manifest whose profile or arch differs from the build's is refused, by its own refusal | host |
| L.bootpolicy.8 | The family is the registry's rule over the SMBIOS strings; missing or bad strings give the generic family; a claimed profile accepts exactly its families | host |
| L.bootpolicy.9 | The floors passed are the caller's (all zero while `rollback = absent`) | host |
| L.bootpolicy.10 | Under `required`, a disk with no verified slot yields only the rescue slot image, never the unsigned files | host |
| L.bootpolicy.11 | `Start` carries the anchor id and security version the loader prints and writes as `ferrix.slot.sig` | host |
| L.bootpolicy.12 | Every input gives `Start` or one `Refusal`, with no panic | host: the fuzz target and the panic audit |
| L.uefisig.1 | The loader refuses a version-2 slot with a bad signature under `off` | boot: N1 |
| L.uefisig.2 | The loader refuses a version-1 slot under `required`, and a version-2 slot under an empty table | boot: N2, N3 |
| L.uefisig.3 | The loader refuses a wrong kind, a wrong family and a flipped kernel byte, and boots the other slot | boot: one case and one control each (condition 7): S1 (wrong kind, k-start), S2 (flipped byte, k-blob), S3 (wrong family, k-start) |
| L.uefisig.4 | The loader refuses a version-2 header with a non-zero `generation` | boot: S10 |
| L.uefisig.5 | With both slots refused, a signed rescue image boots, and without one the loader panics with each slot's reason | boot: R1 |

`L.bootpolicy.1-12` are `ferrix-boot-policy`'s, checked on the host.
`L.uefisig.1-5` are the loader's own behaviour (`src/boot/common/uefi`,
in no ring), checked by boots: they are §8.1's `sig-loader-*` rows. One
namespace holds one crate's units (ledger 4032 (1)).

`L.slots.1-20` hold `ferrix-slots`' behaviour as AB-BOOT §2-§4 states it
(the header, the record, the GPT scan, the plan, the words). They are
reserved without a list. The slice that moves `ferrix-slots` into the
item lists each id with its statement and one check before its code
review, and releases the ids it does not use (4032 (6)).

| Decision | Choice | Reason |
|---|---|---|
| Today's slots | Header version 1, the record and the boot info unchanged; `off` boots them | nazuna's slot B, and every install so far |
| `off` and version 2 | Verified, never downgraded | A signed slot cannot lose its signature by a profile setting |
| `present` | Stays `off` until a release key exists | An empty release table under `required` boots nothing |
| Policy | `ferrix-boot-policy`, `item` | §5.3 |
| Section | `.fxanch`, a 512-byte block, read through `black_box` | The PE 8-byte name limit; nothing folded outside the section |
| init and devmgr tables | S8 and S9 | No reader before then |

## 11. Sources

1. NIST SP 800-208, *Recommendation for Stateful Hash-Based Signature
   Schemes*, October 2020, https://doi.org/10.6028/NIST.SP.800-208. Read:
   §1.1-1.2, §4.2 (Tables 3, 4), §7, §7.1, §8.1 (and footnote 9), §8.2,
   §9.1-9.3.
2. RFC 8554, McGrew, Curcio, Fluhrer, *Leighton-Micali Hash-Based
   Signatures*, April 2019.
3. RFC 8391, Huelsing et al., *XMSS: eXtended Merkle Signature Scheme*, May
   2018.
4. RFC 9858, Fluhrer, Dang, *Additional Parameter Sets for HSS/LMS
   Hash-Based Signatures*, October 2025: §2.1, §5-§8, Table 3, Appendix A.
5. NSA, *Commercial National Security Algorithm Suite 2.0* (CSA, 2022, as
   updated) and *CNSA 2.0 FAQ*. These were read through quotations, because
   media.defense.gov refused this host (HTTP 403). The quotations: "LMS: all
   parameters approved; SHA-256/192 recommended", and, from the FAQ, "From
   NIST SP 800-208, NSA has only approved LMS and XMSS for use in NSS. The
   multitree algorithms HSS and XMSSMT are not allowed" (wolfSSL, "CNSA 2.0
   Update Part 3: LMS and XMSS", https://www.wolfssl.com/cnsa-2-0-update-part-3-lms-and-xmss-2/;
   SemiWiki, "NSA releases update to CNSA 2.0"). The primary text is to be
   read before relying on CNSA 2.0 (§12 Q1).
6. IANA, *Leighton-Micali Signatures (LMS)* registry,
   https://www.iana.org/assignments/leighton-micali-signatures/.
7. eBACS, hash function results for `armeabi-berry2` (Cortex-A7, BCM2836,
   supercop-20251222), https://bench.cr.yp.to/results-hash/armeabi-berry2.html:
   sha256, 28.24 cycles per byte (long), 380.53 (8 bytes).
8. `hbs-lms` 0.1.1, Fraunhofer AISEC, Apache-2.0, https://crates.io/crates/hbs-lms.
9. Utimaco, *Quantum Protect*, https://utimaco.com/products/firmware/quantum-protect.
10. Thales TCT, *Quantum-Resistant Stateful Hash-Based Signature Schemes*,
    https://www.thalestct.com/?p=4545, and its code-signing white paper (2024).
11. OASIS, *PKCS #11 Specification Version 3.1*, the HSS mechanism.
12. Utimaco, EC-Conference 2024, "Overcoming PQC Migration Challenges with
    Stateful Hash-Based Signatures and Secure Deployment in HSMs",
    https://brighttalk.com/webcast/20305/626435.
13. `lms-signature` 0.1.0-rc.2, RustCrypto, Apache-2.0 OR MIT,
    https://github.com/RustCrypto/signatures.
14. UEFI Specification 2.10, §32, *Secure Boot and Driver Signing*.
15. TCG, *TPM 2.0 Library Specification*, Part 1 (NV indices, counters), Part 2
    (`TPMA_NV`), Part 3 (`TPM2_NV_Increment`, `TPM2_NV_WriteLock`,
    `TPM2_NV_UndefineSpaceSpecial`, `TPM2_Clear`).
16. TCG TPM 2.0 reference implementation (Part 4), as mirrored at
    https://android.googlesource.com/platform/external/tpm2/+/716a46a/:
    `NV.c` (`NvInitialCounter`: "find the highest value in any active
    counter ... If there are no active counters, it will use the value of
    the previous largest counter"; `NvDeleteEntity` keeps a deleted
    counter's value as that maximum; `NvFlushHierarchy`, run by
    `TPM2_Clear`, deletes owner indices without doing so),
    `NV_DefineSpace.c` ("If TPMA_NV_POLICY_DELETE is SET, then the index
    must be defined by the platform") and `NV_UndefineSpace.c` ("The owner
    may only delete an index that was defined with ownerAuth"). It is an old
    mirror: S5 checks the current Part 3 text and the TPMs themselves.
17. TCG, *EFI Protocol Specification* (`EFI_TCG2_PROTOCOL`, `SubmitCommand`).
18. NIST ACVP, *ACVP LMS JSON Specification*,
    https://pages.nist.gov/ACVP/draft-celi-acvp-lms.html, and
    usnistgov/ACVP-Server `gen-val/json-files/LMS-sigVer-*`.
19. IEC 61508-3:2010, 7.4.4 (tool classes T1-T3, 7.4.4.10).
20. ISO 26262-8:2018, clause 11 (TI, TD, TCL).

Local copies of 1, 2, 4, 6 and 7, with the benchmark, are in
`~/.local/share/ferrix/signing-bench/`.

## 12. The customer's answers (2026-10-24)

The questions of version 1 were answered by the customer on 2026-10-24.
Every answer takes the consultant's recommendation (ledger lines 3719 and
3731).

| | Question | Answer | Where it acts |
|---|---|---|---|
| Q1 | HSS 10/15 or plain LMS? | **Plain LMS, `L = 1`, `LMS_SHA256_M24_H20`**, two independent keys per kind in two modules. Moving to HSS later costs a new key, not a format change. Read the primary NSA text before relying on CNSA 2.0 | §2.1, §4 |
| Q2 | Which HSM, how many? | **None until the first claimed release with signed updates.** Development uses software keys (§6.4). Then two FIPS 140-3 level 3 modules with LMS on their CMVP certificate (confirmed before purchase), at two sites. Each module added later gets new keys of its own | §3.3, §4.2, slice S6 |
| Q3 | Custodians | From then: 2 of 3, the customer and two external custodians under contract, with the independent assessor as witness. Until then, **single-person custody is a stated residual**, acceptable while no claimed release exists | §3.2 |
| Q4 | Secure Boot | Integrators **enrol Ferrix's own `db` certificate**, stated as an AoU. No shim and no Microsoft CA for now. The classical firmware link is a stated residual | §6.3, §10.3 |
| Q5 | Boards without storage | **Anti-rollback absent** on the DK1, the Pixel 7 and every QEMU target. No fuses, no relock | §7.3 |
| Q6 | The AArch64 claim board | A Lock-mode Cortex-A78AE platform (MICROKERNEL.md's: an AMD VEK385, an NXP S32N7, or the integrator's SoC), bought when a project funds it. Whether its security subsystem offers a usable monotonic counter or RPMB is confirmed with the vendor before purchase. To bring the mechanism up before then, any cheap board with eMMC RPMB and upstream OP-TEE, checked before buying; no claim rests on it | §7.3 |
| Q7 | The budget | **2^20 per key**, so 2^21 per kind, with an **alarm at 50%** of a key's leaves. CI, nightly and gate images never use a product key. No H25 | §2.2, §4.2 |
| Q8 | Config and driver floors | **Only with slot releases.** The residual window, in which a configuration or driver package can be rolled back to the floor the current slot carries until the next slot release, is stated. A ring-3 TPM path only if field data asks for it, under its own review | §7.2 |
| Q9 | Signed slot updates for `max` | **None until `max`'s claim is built (M7).** `max` is updated by a fresh install, so the verifier stays at `sc2` and `present`. An `sc4` verifier is an M7 work item, needed before any `max` product is fielded | §5.3, §10.1 |
| Q10 | Taking the TPM's owner and lockout authorisations | **Yes, but only on installs of a claimed profile on a dedicated machine**, chosen explicitly at install time. A dual-boot or desktop install does not take them, and its anti-rollback is stated absent. The clear-at-the-machine residual is accepted as the Security Target's physical-protection assumption and a SAFETY-MANUAL AoU | §7.2, §10.3 |

**Added by the customer:** during development, signing uses software keys
in a soft signer behind the same PKCS #11 interface the design uses for
HSMs, so that the later switch is configuration only. Its one-time-key
state lives in a locked file, committed with `fsync` before a signature is
released, and it can never use a product key (§6.4, ledger 3731
(D1)-(D5)).

## 13. Where it stands

2026-10-24: this design, on branch `docs/product-signing`. The
consultant's verdict is **OK IF (1)-(12)**, ledger
`~/.local/share/ferrix/cert-consultant/reviews.md` line 3667, against the
brief at line 3608. The conditions are folded in above:

1. a release-stable seal, with `generation` zero in header version 2 (§1.2);
2. the TPM floor rewritten, because `TPMA_NV_POLICY_DELETE` needs the
   platform hierarchy (§7.2, §7.3, §12 Q10);
3. kind 4's separation by the kind field alone (§1.1, §8.1);
4. the gate's development-anchor image defined exactly (§6.4);
5. classification through the item ring's level, and the enforcement
   points (§5.3);
6. the PKCS #11 composition of lower trees (§4), superseded in version 2
   by independent keys; it survives only in §4.3, for a later HSS key;
7. `sig-time` and `sig-stack` as two checks (§8.1);
8. the chain-call bound in place of "the same work" (§8.5);
9. the Pixel 7's missing authenticity claim, and no generic family in a
   claimed profile (§1.4, §5.2, §7.3);
10. the rescue image under the floor, and the raise held back for it
    (§9.2, §7.2 step 4);
11. the Secure Boot state on the boot line and in the audit record, with
    the claim conditional on it (§6.3);
12. this record.

The review **withdrew** (P4)'s alternative of an index range per HSM in one
tree (§4.3), and **moved** (P5)'s counting driver check from
`drvupdated` to `devmgr` (§1.3). Lesser points C1 (the "floor raise
pending" line) and C3 (revocations in the budget) are taken in. C2 (the
eBACS machine's clock) is as written: S2's measurement supersedes the
estimate.

**Version 2 (2026-10-24, the same day).** The customer answered §12, taking
every recommendation of ledger line 3719. The consultant recorded the
decisions and passed the soft signer **OK IF (D1)-(D5)** (ledger line
3731). Version 2 folds them in:

* `L = 1`, `LMS_SHA256_M24_H20`, with two keys per kind (§2.1, §2.2);
* §4 rewritten for independent keys, with the anchor count;
* the soft signer (§6.4);
* §12 answered.

The consultant's re-review of version 2 is **OK IF (1)-(5)**, and of the
id reservation **OK IF (6)-(8)**, ledger line 3738, folded in:

1. revocation between peer keys (§7.4 (a)-(d), §4.2's new row, §8.1, §8.3);
2. §4.2's first row names the kind's other key;
3. the text left from version 1 (§5.3's level stated as decided, §6.1, this
   section's item 6);
4. the soft signer's whole-directory restore stated as a development
   residual, with its state on one host (§6.4);
5. this record.

The reservation adds `L.sha256` for `ferrix-sha256`, and `H.SIGN`'s parent
is `ASR-7` alone (§10.4). Lesser points C1, what reads
`CKA_HSS_KEYS_REMAINING` (§2.2), and C2, the state on one host (§6.4), are
taken in.

No code and no requirement ids, and no document's claims change.
`AB-BOOT.md` and `BUILD-FLAGS.md` carry a pointer to this document saying
what will change. **Before code:** the reservation landing of §10.4 before
S1, and a design re-review of the rewritten §7.2 before any S5 code (3667
(2)). S1 may start beside §4's re-review (3731). Its code review checks it
against the re-reviewed design.

**Slice S1, as built (2026-10-24, branch `feature/sigverify`).** Three
crates were built, along with their evidence, the model's part 26 and
check wiring.

*`ferrix-sha256`* (`src/lib/crypto/sha256`) is a copy of `ferrix-slots`'
SHA-256 with SHA-256/192 added. Its message schedule was rewritten so that
every index is a constant. `ferrix-slots` moves onto it in S4.

*`ferrix-sigverify`* (`src/lib/crypto/sigverify`) holds the manifest and
LMS/HSS verification.

* The algorithms are written once for every SHA-256/192 set. The public
  entry points hold every key and signature to the product set of §2.1.
  The wider set (W1, W2, W8, H5) is compiled under `cfg(test)` only, so
  that RFC 9858 A.1 and NIST's ACVP samples run through the same code.
* Both crates are classified `item`. They are complete subsystems in the
  traceability report, so a new function no requirement names fails the
  gate. Both deny `clippy::arithmetic_side_effects`, with each bounded
  operation argued at its site. They contain no allocation, no recursion
  and no `unsafe`.
* Their requirements are `H.SIGN.1-6` (parent ASR-7), `L.sigverify.1-14`
  and `L.sha256.1-3`. The other reserved ids were released. `H.SIGN.6`,
  `L.sigverify.11` (time) and `L.sigverify.12` (stack) are in the baseline
  until S2 and S4 measure them.

*`ferrix-softsign`* (`tools/common/release/softsign`) is the soft signer of
§6.4, a host tool. It is in no ring and not a normal dependency of any item
crate (a dev-dependency of `ferrix-sigverify`'s tests). It reproduces RFC
9858 A.4's one-time signature from its seed, byte for byte.

**Evidence.**

* Vectors:
  * RFC 9858 A.4, which is the product set itself, with every single-bit
    flip refused;
  * RFC 9858 A.1, in the test-only wide set;
  * RFC 8554 Test Case 1, refused by its codes and level count only;
  * all 16 NIST ACVP LMS sigVer sample cases, in the wide set: the sample
    has no case of the product set, and those vectors are owed (§8.2);
  * NIST CAVP SHA256 ShortMsg, LongMsg and Monte;
  * a two-level HSS signature;
  * the `hbs-lms` 0.1.1 cross-check, with its type codes mapped.
* Manifests: every byte of a good block changed is refused, each field is
  refused by its name, and each bound field one off is refused, as is each
  case of §7.4 (a)-(b).
* Fuzzing: `sigverify_manifest`, seeded with a signed block, ran 300 s
  (6.9 million runs) with no crash. `cargo xtask check` does not build it;
  CI's fuzz job builds, replays and searches it with every other target,
  and a backlog row (`docs/backlog/p2.md`) owes building it in `check`.
* Coverage: under the host tests, every line and every branch outcome
  of both crates (`docs/certification/host-coverage.json`). `--check`
  runs in `check`. It refuses evidence that is stale against the product
  files or against any test input (the tests, the vectors, the soft
  signer). A measured control, an untested branch added, was refused by
  name. `check-no-target-cfg.py` holds that the product code has no
  target-dependent part, so the host measurement stands for every
  target.
* A configuration or driver manifest checked with no subject is refused
  (`NoSubject`).
* Soft-signer tests: a restored state is refused, a crash after the commit
  skips the leaf, an unreadable or exhausted state is refused, a release
  key is refused, and a key directory inside a git tree is refused. The
  PKCS #11 constants are pinned.
* Each comparison's negative control runs through `fleet/gate.sh control`.
  The logs are cited in the landing commit.

The consultant's code review of S1 is **OK IF (1)-(8)**, ledger line
3803, folded in before landing:

1. the subject fails closed for configuration and driver manifests;
2. a control for every comparison;
3. the coverage evidence tied to the test inputs, with a measured control;
4. the target-cfg check;
5. the fuzz build recorded as owed;
6. §8.2 and the requirements made true;
7. `L.sigverify.8` split into `.8`, `.13` and `.14`;
8. the rows and controls on the landing hash.

The review accepted three things as they stand: no reverse cross-check is
owed, the S3 deferrals stand while S3 comes before S4, and the time and
stack requirements stay in the baseline.

**Slice S3, as built (2026-10-24, branch `feature/signing-s3`).** S3
builds the rules that keep development keys and the verifier where they
belong, ahead of S4's loader, which will be the first program to carry an
anchor.

*The anchors file*, `tools/common/data/signing-anchors.toml`:

* `[release.*]` is empty: no product key exists until the HSMs (§12 Q2).
* `[development.*]` holds three soft-signer keys, `LMS_SHA256_M24_H15`
  with `LMOTS_SHA256_N24_W4`, `L = 1`, one per kind:
  * slot, anchor id 0x8000;
  * config, anchor id 0x8001;
  * driver, anchor id 0x8002.
* Their private state is in `~/.local/share/ferrix/signing-dev/` on
  nazuna (D5).

*The hardware-family registry*, `tools/common/data/hardware-families.toml`,
holds §1.4's six families. It gives each one's id and architecture, which
family is its architecture's generic one, and the rule the loader will
recognise it by (S4).

*`check-signing-anchors.py`*, in `cargo xtask check` with its self-test.

* It validates both files. Release ids are 0 or 1, development ids
  0x8000 and up. Every key is 52 bytes of the product set, and no key is
  given twice or in both sections. Every family id is in its
  architecture's range, with one generic family each.
* It refuses a package that depends on the soft signer as more than a
  dev-dependency, and a kernel package that depends on the verifier.
* `--scan` refuses any file holding a development key.

*xtask's refusals*, in `tools/common/xtask/src/signing.rs`:

* Every kernel xtask builds is refused if it holds the verifier's or the
  soft signer's name: their crate names, as symbols carry them, and their
  source paths, as debug information and panic locations carry them. That
  is §5.2's kernel check.
  * It was shown to fire on a real build: a kernel package given a real
    dependency on `ferrix-sigverify` and one call into it, built as the
    shipped x86-64 release kernel, was refused for holding
    `ferrix_sigverify` (`~/.local/share/ferrix/signing-bench/kernel-scan-integration-*.log`).
  * The countermeasure is still the dependency rule below. The scan is its
    backstop.
* The loader and every initramfs of a claimed profile's image are refused
  if they hold a development key's tree root `T`. This is D1, at the two
  points every image passes through (§6.4).
* No image carries an anchor until S4.

*`check-signing-anchors.py`* walks every `src/kernel` package's normal and
build dependency closure (`cargo metadata`, every platform). It refuses
any path to the verifier or the signer, naming it. Its self-test plants an
indirect path (a kernel package, then a library, then the verifier).

*Left to S4, by design:* the generated anchor tables in the loader, init
and `devmgr`, and the gate's relinked loader with its byte comparison
(§6.4). Development private keys exist only on nazuna, so a gate that
signs runs there. Keys for CI would be **a new class of development key**,
which owes a design review at S4, with D1-D5 applying.

S3 and the record of S2 had the consultant's review **OK IF (1)-(13)**,
ledger line 3843, folded in before landing.

* **S3**, conditions (1)-(8):
  * the kernel rule walks the whole dependency closure;
  * the development-key check runs where every image's loader and
    initramfs are final;
  * the match is by the root `T`;
  * integration controls: the kernel scan on a real release build, and
    a planted root in an image;
  * a control on the same-key-in-both-sections rule;
  * §6.4 and §10.5 made true.
* **S2**, conditions (9)-(13): the figures tied to their run, the worst
  case stated as a computed bound, AB-BOOT's ARMv7-A hashing cost, and
  `L.sigverify.11` left in the baseline, with a backlog row.
