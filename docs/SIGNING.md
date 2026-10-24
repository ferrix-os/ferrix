# Product signing: one LMS/HSS scheme for slots, configuration and drivers

Version 1, written 2026-10-24 by product-signing. **A design for review, before
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
its decisions. §12 lists the open questions for the customer.

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
| *Revocation* | 4 | A list of revoked lower trees of one kind (§7.4) | The verifier of the kind it names | A file next to that kind's anchors (§7.4) |

**One format, one verifier.** The four kinds share the manifest below and one
verifier crate (§5). They differ only in four things: the anchor set that
verifies them, the kind code the caller expects, the meaning of `subject`, and
what `payload_sha256` covers.

**A key per kind.** Kinds 1, 2 and 3 each have their own HSS root keys, each
with its own root public key (§3.3). An anchor names the kind it verifies.
The verifier takes its anchor set from the caller's expected kind, never
from the kind field in the manifest. So a signature of kind 1, 2 or 3 fails
under another of those kinds' keys before any field is read. That holds
even if a manifest of another kind carried the right kind code, which it
cannot, because the code is signed. The kind field in the signed bytes is a
second, independent separation.

**Kind 4 is the exception.** A revocation manifest is signed under its
target kind's roots (§3.3, §7.4), so a revocation of kind k and a manifest
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
176  revoked           [[u8; 16]; 20]       revocation kind only: the lower trees' identifiers I
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
* no lower tree's `I` in the signature is revoked for this kind (§7.4);
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
| Domain separation | A separate HSS root per kind, chosen by the caller's expected kind; the kind is also signed | A signature of one kind cannot verify as another under any field value |
| Bound fields | kind, security version, payload digest and length, profile, arch, hardware family, subject, anchor | (P1)'s list, plus `subject` so that a driver image cannot be put on another driver, and `anchor_id` so that the anchor lookup is signed |
| Slot binding | `payload_sha256` = the slot header's seal | Authenticates the three blobs through hashes the loader already checks |
| Driver check | `devmgr` on its own copy | The only copy with no time-of-check-to-time-of-use window |

## 2. (P2) Parameter sets

### 2.1 The choice

| Item | Choice | From |
|---|---|---|
| Hash | SHA-256/192 (SHA-256 truncated to 192 bits, `n = m = 24`) | SP 800-208 §4.2 [1]; code points RFC 9858 [4]; CNSA 2.0 recommends SHA-256/192 for LMS [5] |
| LM-OTS | `LMOTS_SHA256_N24_W4`, type 0x00000007: `n = 24`, `w = 4`, `p = 51` | SP 800-208 Table 3; RFC 9858 Table 1 |
| LMS, top level | `LMS_SHA256_M24_H10`, type 0x0000000B: 1,024 leaves | SP 800-208 Table 4; RFC 9858 Table 2 |
| LMS, bottom level | `LMS_SHA256_M24_H15`, type 0x0000000C: 32,768 leaves | as above |
| HSS levels | `L = 2` | RFC 8554 §6; SP 800-208 §7.1 (distributed lower trees, §4) |
| Signature | 3,172 bytes | RFC 9858 Table 3, row "15/10, W=4"; recomputed below |
| Allowed in the verifier | LM-OTS: N24_W4 only; LMS: M24_H10, M24_H15, M24_H20; `L` 1 or 2 | The smallest set that carries this choice and §12 Q1's alternative (`L = 1`, H20). Every other code is refused |

The SHA-256/192 code points were "TBD" in SP 800-208's Tables 3 and 4. They
were assigned by RFC 9858 (October 2025) and are in IANA's LMS registry [6].
The one SHA-256 implementation the verifier uses serves both the truncated
hash and the slot hashes (§5.2).

### 2.2 Tree height against the signature count

**Budget per kind.** The lifetime ceiling is **2^20 = 1,048,576 signatures**
per kind and root, over a key life of 25 years: 115 a day. A release signs
one slot manifest per (profile, arch, family) it ships, which is at most 4 ×
6 = 24 today. It also signs one manifest per driver image and configuration
package it ships. Even daily releases of every slot combination use 24 of
the 115. And product releases are not daily: development builds use
development anchors that are never in a claimed image (§6.4).

| Configuration | Capacity per root | Margin over 2^20 | Lower trees | Signatures per lower tree |
|---|---|---|---|---|
| **HSS 10/15 (chosen)** | 2^25 = 33,554,432 | 32× per root; 64× with both roots of the kind | 1,024 | 32,768 |
| LMS H20 (`L = 1`, §12 Q1) | 2^20 | 1× per key; n× with n keys | none | 1,048,576 |
| HSS 20/20 (largest allowed) | 2^40 | far above | 2^20 | 2^20 |

The top tree's 1,024 leaves sign only lower-tree public keys (SP 800-208
§7). At 115 signatures a day, a lower tree of 32,768 lasts nine months. 1,024
of them outlast the key by orders of magnitude. The binding limit is
therefore not the top tree but the HSMs' lifetime, which §4 covers.
Generating one lower tree costs the HSM about 2^15 × 767 ≈ 25 million hash
calls, once, at its ceremony.

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
| **2, 10/15, W4, n=24 (chosen)** | **3,172** | 1,530 | 4.85 M cycles = **7.5 ms** | **22 ms** | 35 µs |
| 2, 10/15, W8, n=24 | 1,972 | 13,260 | 40.5 M = 62 ms | 187 ms | 258 µs |
| 2, 10/15, W4, n=32 | 5,236 | 2,010 | 6.4 M = 9.8 ms | 29 ms | 46 µs |
| 1, 20, W4, n=24 | 1,744 | 765 | 2.5 M = 3.8 ms | 11 ms | 18 µs (H5) |
| 2, 20/20, W4, n=24 (largest allowed) | 3,532 | 1,530 | 4.9 M = 7.5 ms | 23 ms | — |

The signature sizes are RFC 9858 Table 3's, recomputed from RFC 8554 §4-§6.
The host column measures `hbs-lms` 0.1.1 [8] verifying 24 signatures of
200-byte messages, 20 times each, built `--release`. It uses the same
parameters as the row, but with H5 trees, so that key generation stays
short. A taller tree adds only h tree hashes, which is under 3% of the total
here. On that host one 47-byte SHA-256 call takes 36 ns, so the chosen row's
35 µs is about 980 calls: the analytic mean (two levels of about 51 × 7.5
chain calls, plus K, Q and the tree) within 10%. The program and its output
are in `~/.local/share/ferrix/signing-bench/`, outside the tree. No
dependency was added.

**W4 rather than W8.** W8 saves 1,200 bytes per signature, which no kind
needs: the block has 4,096 bytes. It costs about eight times the verify time
and the HSM's signing time. W4 also keeps the DK1's bound under 25 ms, a
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

## 3. (P3) State custody in the HSM

### 3.1 What the HSM must be

| Requirement | Source | How it is shown |
|---|---|---|
| Key generation and signing only in a hardware module validated to FIPS 140-2 or 140-3 **level 3 or higher** physical security, in an approved mode with no bypass | SP 800-208 §8.1 | The CMVP certificate, which must list LMS key generation and signature generation (§12 Q2) |
| The private key is never exported, and the entropy source is inside the module | SP 800-208 §8.1 | Certificate and security policy; the module's LMS key is created non-extractable, and its backup feature is off for it (§4) |
| The leaf index `q` is incremented and **stored in non-volatile memory before a signature is released** or another request accepted | SP 800-208 §8.1, §9.1 | The security policy's statement. A power-cut test at acceptance: cut power during a signing loop, power on, and check that no `q` repeats and at most one is skipped |
| Each tree's root is signed by its parent **once**, and the signature is stored | SP 800-208 §8.1, footnote 9 | The ceremony record (§3.4) keeps each lower tree's signed public key. The pipeline never asks for it again |

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
the HSM's own authentication (smart cards or PINs). The custodians, their
roles and their changes are a configuration item under configuration
management, as 3564 (S4) asks. Key ceremonies need the quorum plus a witness
who is not a custodian. Who the custodians are is §12 Q3.

### 3.3 Roots and anchors

Each kind has **two roots**, `anchor_id` 0 and 1. Each is an HSS key whose
top tree is in its own HSM, at its own site. Both roots of each kind are
anchors in every image (§6). Three kinds and the revocation kind's three
targets share these: revocations of kind k are signed under kind k's roots.
That makes **six HSS roots**.

### 3.4 The signature log

Each signature appends one line to the **signature log**, a git repository
of its own (`ferrix-os/signing-log`, private, signed commits, under
configuration management). A line holds:

`time, kind, anchor_id, lower tree I, q, manifest SHA-256 (bytes 0..512), payload SHA-256, release, custodians`

The pipeline (§8.6) checks before it publishes:

* the `(I, q)` pair is new in the log. A **repeat is an incident**. It is
  reported to the customer, the lower tree is revoked (§7.4), and nothing
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

Nothing in this scheme copies private state. SP 800-208 §7 forbids the
export it would need, and a restored backup is exactly the reuse §1.2 of
that document warns of. Resilience comes from **more keys**, never from
copies:

1. **Lower trees in several HSMs** (SP 800-208 §7.1). At the root's ceremony
   its HSM, T, makes the top tree. Each signing HSM, S1 and S2 at site A and
   S3 at site B, generates its own lower trees. T signs each lower tree's
   public key once. The signed public keys are not secret: they are kept in
   the pipeline's store and in the ceremony record. Each S signs ordinary
   manifests with its own lower trees. The pipeline joins an LMS signature
   with its lower tree's signed public key into the HSS signature (§7.1 of
   SP 800-208 describes the joining device as non-cryptographic). The first
   ceremony certifies **64 lower trees per root**, about 2 million
   signatures, 16 to 32 per signing HSM. The rest of T's 1,024 leaves stay
   for later ceremonies.

   **How the composition is made with PKCS #11 3.1 [11].** Every module holds
   only **single-level** keys, `CKK_HSS` with `CKA_HSS_LEVELS = 1`, and signs
   with `CKM_HSS`:

   * T's key uses `LMS_SHA256_M24_H10`.
   * Each lower key in S uses `LMS_SHA256_M24_H15`.
   * Both use `LMOTS_SHA256_N24_W4`.

   The pipeline builds the two-level key and its signatures in four steps.

   1. **The lower tree's public key.** S's HSS public key is
      `u32str(1) || pub[1]`. The pipeline strips the leading `u32str(1)`,
      leaving S's LMS public key
      `pub[1] = u32str(lms type) || u32str(ots type) || I || T[1]`
      (RFC 8554 §5.3).
   2. **Certifying it.** At the ceremony, T signs those 48 bytes as its
      message, once (SP 800-208 footnote 9). T returns the HSS signature
      `u32str(0) || sig[0]`. The pipeline strips the `u32str(0)` (Nspk = 0),
      leaving the LMS signature `sig[0]`. The pair `sig[0] || pub[1]` is
      exactly RFC 8554 §6.2's `signed_pub_key[0]`.
   3. **Signing a manifest.** S signs bytes 0..512 and returns
      `u32str(0) || sig[1]`. The pipeline strips it to `sig[1]` and joins
      `u32str(1) || sig[0] || pub[1] || sig[1]`. That is RFC 8554 §6.2's HSS
      signature with Nspk = 1.
   4. **The anchor.** The anchor is `u32str(2) || pub[0]`, where `pub[0]` is
      T's LMS public key, likewise stripped of its `u32str(1)`. That is the
      HSS public key of a two-level key.

   RFC 8554 §6.3's verification checks `sig[0]` over `pub[1]` under
   `pub[0]`, then `sig[1]` over the message under `pub[1]`, which is
   exactly what steps 1 to 3 made. So the joined key is an ordinary HSS key
   to the verifier, and no module ever holds a level it did not generate.
   The pipeline is non-cryptographic: it only strips and concatenates, as
   SP 800-208 §7.1 describes. Slice S6's acceptance test does all of this
   on the purchased modules, verifies the joined signatures with
   `ferrix-sigverify` (host build), and checks them against RFC 8554's
   encoding byte by byte. A module that cannot export the bare public key
   or sign a 48-byte message with a single-level key cannot be used for
   this, and §12 Q2 asks for that capability.
2. **Two roots per kind** (SP 800-208 §7, last paragraph). The top-tree HSM
   T is the single point of failure for new lower trees, so each kind has a
   second root at the other site. Losing T loses no signing ability until
   the lower trees it already certified are spent.
3. **No shared index range of one tree.** The consultant's alternative, an
   index range of one top tree per HSM, would need that tree's seed in
   several HSMs. That is the copy §8.1 forbids. So this design does not use
   it.

**Procedures** (written as a runbook with slice S6; every step under the
quorum, logged):

| Event | Procedure | Lost | What devices need |
|---|---|---|---|
| A signing HSM fails or is lost | Retire its lower trees in the store. Revoke them (§7.4) only if loss of custody is suspected | Its unspent leaves | Nothing: other lower trees verify under the same root |
| A lower tree is exhausted | Switch to the next certified lower tree. A ceremony certifies more on T once fewer than 16 remain unused | Nothing | Nothing |
| The top-tree HSM T of a root fails | Keep signing with that root's certified lower trees. Plan the root's replacement | No new lower trees for that root | Nothing until its lower trees run out; the other root still serves |
| A root's top tree passes 75% of its leaves, or its HSM is gone and its lower trees run low | Generate a new root at a ceremony and ship it as an anchor (§6.2) a release ahead of need | Nothing | An update that carries the new anchor |
| Suspected compromise or a repeated `(I, q)` | Revoke the lower tree (§7.4). For a root: ship anchors without it (§6.2) and raise the floor (§7) | The tree or root | A revocation file, or a new loader (slot kind) or slot (other kinds) |
| A vendor's state-aware backup | Not used until assessed against this section. A scheme that splits leaves between modules without copying a seed (Utimaco's "OTS-preserving framework" is offered as one [12]) may replace step 1, by a change to this document | — | — |

| Decision | Choice | Reason |
|---|---|---|
| Resilience | Lower trees in separate HSMs at two sites, each certified once by its root's top tree; two roots per kind | SP 800-208 §7 and §7.1; no private state is copied |
| Index ranges in one tree | Not used | They need the seed in several modules, which §8.1 forbids |
| Loss and exhaustion | The runbook above; new roots reach devices as anchors | (P4) |

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
`max`'s levels are built. Which level the crate must finally reach is §12
Q9's answer:

* If `max` takes signed slot updates, the level is **`sc4`**: DAL A
  objectives, MC/DC coverage, SC 4 / ASIL D.
* If `max` is updated only by a fresh install, and the verifier ships in
  `fast` and `present`, it needs both `sc2` and `present`, which are
  incomparable (BUILD-FLAGS §4).

If the crate ever needs a level the ring does not carry, the `"level"` key
becomes a new schema field, with its change to `check-item-boundary.py`, a
self-test, and its own review. It is not assumed here. The design
recommends the second answer until `max`'s claim is built (M7), with `sc4`
as the crate's target from its first line: MC/DC-ready structure, no
recursion and bounded loops. The crate depends on nothing but
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

* **Slot anchors** (two roots of kind 1, plus the slot revocation floor
  rule) are compiled into the UEFI loader, from
  `tools/common/data/signing-anchors.toml` through a generated `const`. An
  anchor is an HSS public key, RFC 8554 §6.1: `L`, the top tree's types, `I`
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
signed under a key the platform already trusts (§12 Q4). Its key lives in
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

### 6.4 Development builds

Gates and development images need signatures without an HSM. So **development
anchors** exist: software LMS keys generated by a host tool,
`tools/common/release/dev-sign` (new, slice S3). They are listed by their
public keys in `signing-anchors.toml`'s `[development]` table. A development
key's state lives in a file and may be reused, which is exactly why no
shipped image trusts one.

There are three kinds of image, and they never mix:

| Image | Its anchor table | Its manifests' profile byte | Its build line |
|---|---|---|---|
| Shipped (release) image of a named profile | release anchors only | the profile's own | `profile=<name> level=<level>` |
| Development image (no profile) | development anchors | 0xFF | `profile=none level=none`, as today |
| **The signing gate's image** (§8.4) | development anchors in place of the release table | the named profile under test, so that the profile check is exercised as shipped | `profile=<name> level=none anchors=development`, and **no claim** |

* **How the gate's image is made.** xtask builds the named profile, then
  links the loader again with only its anchor table replaced. The table
  sits in a section of its own, `.ferrix_anchors`, of fixed size. The gate
  image's build line says `anchors=development` and `level=none`, and the
  boot check compares that line with the profile file under BUILD-FLAGS rule
  6 a. So the image cannot pass as the profile's.
* **The two loaders differ only in the table.** The gate compares the gate
  loader's PE with the shipped loader built from the same commit, byte for
  byte, outside `.ferrix_anchors` and the PE checksum and Authenticode
  fields. A difference anywhere else fails the gate. Its control is a
  one-byte change outside the section, which must read FIRED.
* **No shipped loader holds a development anchor.** A check over every
  release image's loader, `init` and `devmgr` (`check-signing-anchors.py`,
  slice S3) searches them for every development public key's 52 bytes. A
  hit fails the release. Its control is a release build with one
  development anchor planted, which must read FIRED.
* xtask refuses to put a development anchor into a named profile's shipped
  image at all. Only the gate's relink produces one, and only under the
  gate's output directory.

| Decision | Choice | Reason |
|---|---|---|
| Slot anchors | Compiled into the UEFI loader; two roots | The loader is the first Ferrix code that runs, and the one Secure Boot verifies |
| Config and driver anchors | Compiled into init's verifier and `devmgr`, inside the signed initramfs | `devmgr` reads no files; the anchors are as authentic as the slot |
| Handover | Secure Boot's Authenticode verifies the loader; the loader verifies LMS | UEFI has no LMS; no artifact is signed twice |
| Residual | The firmware's classical chain is outside this scheme; with Secure Boot off there is no root | Stated in A.FIRMWARE and VULNERABILITY-ANALYSIS (§10.3) |
| Firmware state | `SecureBoot` and `SetupMode` on the loader's line, in its words and in the start-up audit record; the authenticity claim needs `on` and `user` | FAU_GEN.1 b; the claim's condition is visible per boot |
| Development | Development anchors only in development images (profile 0xFF) and the gate's relinked loader (`level=none anchors=development`); a byte comparison outside the anchor section, and a search of shipped images, each with a control | Gates can sign; a shipped image never trusts a reusable key |

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
reviewed (§12 Q10).

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
configuration's own good trial. A ring-3 TPM path that would raise it
earlier is §12 Q8.

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

* **A lower tree** is revoked by a **revocation manifest** (kind 4). Its
  `target_kind` names the kind, and its `revoked` field lists up to 20 lower
  trees' `I`. It is signed under that kind's roots, and its
  `security_version` is the revocation epoch.
  * For **kind 1**, the loader reads it from `FERRIX/REVOKED.SIG` on the ESP.
    Its floor is the kind-4 counter (§7.2). A floor above zero with no file
    at or above it fails closed (§9.1), so deleting the file does not undo
    a revocation. The loader raises the revocation floor to the file's epoch
    only when the good slot still verifies with the list applied. That way
    a revocation cannot strand a machine on its own. Until then, the list
    applies in memory on every boot.
  * For **kinds 2 and 3**, the revocation manifests are part of the slot
    image's initramfs, covered by its signature and its floor.
* **A root** is retired by anchors without it (§6.2) and a raised floor.
* A revoked tree's past signatures fail at once wherever the list is
  applied. The release process re-signs whatever must keep booting with a
  tree that is not revoked before it publishes the revocation.

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
| sig-revoke | A signature through a revoked lower tree is refused | host |
| sig-payload | The caller's payload digest equals `payload_sha256`, and `payload_len` matches | host and boot: AB-BOOT case 4's flipped byte |
| sig-time | One verify is at most 50 ms on the DK1 at 650 MHz | DK1 measurement of the worst-case signature (S2); `MEMORY-AND-TIMING.md` |
| sig-stack | One verify uses at most S bytes of stack and no heap, S from the build's stack report | the stack report of the loader's and `devmgr`'s builds; `MEMORY-AND-TIMING.md` |
| sig-loader-* | The loader's slot path: refuse unsigned or wrong-kind slots in a signed profile, the floors, the raise, the lock (§7.2 steps 1-6) | `test-ab-boot` cases (§8.4) |
| sig-cfg-*, sig-drv-* | init's and `devmgr`'s refusals (§9) | boot sub-cases in their own slices |

### 8.2 Test vectors

* RFC 8554 Appendix F, test cases 1 and 2 (SHA-256/256, `L = 2`). They show
  the HSS and LMS structure. They are refused by type code in the product
  set, and run through a test-only widening of the allowed codes that is
  compiled under `cfg(test)` alone.
* RFC 9858 Appendix A.1 (SHA-256/192, W8) and **A.4 (SHA-256/192, W4)**:
  A.4 is in the product set.
* NIST ACVP LMS vectors [18], `LMS-sigVer-1.0` and `LMS-sigVer-SP800-208` in
  usnistgov/ACVP-Server's `gen-val/json-files`: every SHA-256/192 W4 case
  with M24_H10, H15 or H20, valid and invalid.
* Signatures the development signer makes, cross-checked against `hbs-lms`
  with its type codes mapped (§5.1). The cross-check is a second, independent
  implementation, not a vector source.

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
PKCS #11, joins the HSS signature, writes the block and appends the
signature log. It is a tool whose output becomes product data:

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
| `slot_signing` (image, new) | `off`, `required` | `required` if §12 Q9 says signed slot updates, else `off` with no slot path | `required` | `required` once S4 lands, a dated change | `off` | Says whether the loader refuses header version 1 and the unsigned rescue files |
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
reserves **`H.SIGN.1-6`** and **`L.sigverify.1-24`** in
`tools/common/data/requirement-reservations.json`, under the lock and with
the consultant's review (CONVENTIONS, "Requirement ids are reserved before
they are written"). `H.SIGN` is the hazard-level family: authenticity,
binding, anti-rollback, revocation, fail-closed and audit. `L.sigverify`
holds §8.1's rows. Neither prefix is used on `develop` at 6a60444a6. Each
later slice (loader, init, `devmgr`) reserves its own `L.*` ids the same
way.

### 10.5 Slices

| | Slice | Needs first | Points |
|---|---|---|---|
| S0 | This design; the consultant's verdict; the reservation landing | — | 2 |
| S1 | `ferrix-sha256` moved out of `ferrix-slots`; `ferrix-sigverify` (format, LM-OTS, LMS, HSS, policy), its host tests, vectors, controls and fuzz target; the hardware-family registry; the kernel-symbol and no-signing-dependency checks | S0 | 8 |
| S2 | The DK1 measurement of §2.3 and `MEMORY-AND-TIMING.md`'s bound | S1; the product owner's OK for the board | 2 |
| S3 | `dev-sign` and development anchors; xtask's refusal of development anchors in claimed images | S1 | 3 |
| S4 | The loader: header version 2, the slot and rescue paths, refusal lines; `bootctl try`'s check; `test-ab-boot` S1-S3 | S1, S3 | 5 |
| S5 | TPM floors: the installer's provisioning (owner and lockout authorisations, `ownerPolicy`), the loader's `TPMA_PERMANENT` check, read, raise and lock through TCG2, the words; `bootctl sync`; `test-ab-boot` S4-S11 with `swtpm`; the check on the N150's PTT | S4; §12 Q10; the consultant's re-review of §7.2; the product owner's OK for the box | 8 |
| S6 | The signing pipeline, the signature log, the runbook of §4, the HSM acceptance test | the HSM (§12 Q2) | 5 |
| S7 | Revocation manifests (kind 4) in the loader | S5 | 3 |
| S8 | init's configuration verifier | 3564 (S1)-(S6) | theirs |
| S9 | `devmgr`'s check (D3) | S1, S3 | 3 |

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
   read before §12 Q1 is answered.
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

## 12. Open questions for the customer

1. **HSS (`L = 2`) or plain LMS (`L = 1`)?** Your decision says LMS/HSS, and
   this design uses HSS 10/15 for its backup story (SP 800-208 §7.1). But
   the CNSA 2.0 FAQ, as quoted (source 5), allows LMS and XMSS but **not
   HSS or XMSS^MT** for US national security systems. If Ferrix is to meet
   CNSA 2.0, the alternative is `L = 1`, `LMS_SHA256_M24_H20`: one key per
   HSM, each an anchor, with 1,048,576 signatures per key, 1,744-byte
   signatures and 11 ms on the DK1. Backup then comes from several
   independent keys (SP 800-208 §7, first option). The verifier accepts
   both, so the format does not change. Which: HSS 10/15 (recommended
   unless CNSA 2.0 matters), or LMS H20?
2. **Which HSM, and how many?** At least three signing modules and two
   top-tree modules over two sites (§4), FIPS 140 level 3 or higher with LMS
   on the CMVP certificate. The modules must be able to sign a 48-byte
   message with a single-level key and export its bare LMS public key (§4,
   step 1's composition). Candidates: Utimaco u.trust Se-Series with
   Quantum Protect, Thales Luna. Who buys and hosts them?
3. **Who are the custodians** (at least 2 of 3) and the ceremony witness?
4. **Secure Boot.** Should integrators enrol Ferrix's own `db` certificate,
   or should Ferrix get its loader signed under a key platforms already
   trust? And do you accept the residual of §6.3, that the firmware's link
   is classical?
5. **Boards without storage.** Do you accept "anti-rollback absent" on the
   DK1, the Pixel 7 and every QEMU target? Or do you want either of the
   persistent, irreversible paths: fusing a counter into the DK1's OTP, or
   re-locking the Pixel 7 with an AVB custom key? Each is a write to the
   device that only you can allow.
6. **Which AArch64 board carries `max`'s and `fast`'s claim?** It needs
   rollback storage: a TPM 2.0, or RPMB through OP-TEE. None of the AArch64
   boards Ferrix runs on today has storage it can use.
7. **The budget.** Is 2^20 signatures per kind over 25 years (115 a day)
   right for Ferrix's release cadence? It counts every signature, including
   revocation manifests and the re-signing a tree revocation forces (§7.4).
8. **Config and driver floors at slot cadence** (§7.2), or a ring-3 TPM path
   later that raises them at their own good trial? The design recommends
   slot cadence: no TPM driver, and the loader as the only writer.
9. **Does `max` take signed slot updates**, which puts the verifier at `sc4`?
   Or is `max` updated only by a fresh install until its claim is built,
   which puts the verifier at `sc2` and `present` (§5.3)?
10. **May the installer take the TPM's owner and lockout authorisations?**
    The rollback claim of §7.2 needs it: the installer sets both to random
    values it does not keep, restricts the owner hierarchy by policy, and
    disables the OS-side clear. Another operating system on the same machine
    could then not reclaim the TPM's owner hierarchy without a clear at the
    machine. And the claim holds only against an attacker without physical
    access: a clear confirmed at the machine and root's re-provisioning is a
    rollback. If you decline, or if S5 shows a TPM does not keep these
    protections, anti-rollback is **absent on every family** until another
    mechanism is designed.

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
6. the PKCS #11 composition of lower trees (§4);
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
tree (§4, step 3), and **moved** (P5)'s counting driver check from
`drvupdated` to `devmgr` (§1.3). Lesser points C1 (the "floor raise
pending" line) and C3 (revocations in the budget) are taken in. C2 (the
eBACS machine's clock) is as written: S2's measurement supersedes the
estimate.

No code and no requirement ids, and no document's claims change.
`AB-BOOT.md` and `BUILD-FLAGS.md` carry a pointer to this document saying
what will change. **Before code:** the answers to §12; a design re-review
of the rewritten §7.2 before any S5 code (3667 (2)); and the reservation
landing of §10.4 before S1.
