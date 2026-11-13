# Updates: how a fix reaches an installed Ferrix

Written 2026-11-13 by pkg-updates, for the customer's "an agent that
handles security updates and package management". It continues
[APPS.md](APPS.md) §7, whose list of what was left is this document's
subject, and it uses the signing design of [SIGNING.md](SIGNING.md) as it
stands. Section 1 is what exists, section 2 the plan, section 3 the
decisions taken where the documents said nothing, section 5 where it stands.

## 1. What exists (2026-11-13)

| Piece | State | Where |
|---|---|---|
| Packages, records with a BLAKE2b-256 digest per file | built | `src/lib/proto/pkg` (`record.rs`), APPS.md §6 |
| `pkg list`, `info`, `install`, `install --replace`, `remove` | built, booted by `test-pkg` on every architecture | `src/user/system/linux/pkg`, `tools/common/xtask/src/pkg.rs` |
| Minimum versions in `depends` (`name >= 1.2`), checked by the plan | built: `Dependency::accepts`, `plan` refuses a dependency missing or too old | `src/lib/proto/pkg/src/manifest.rs`, `plan.rs` |
| Upgrades from a repository | missing: `install --replace` replaces the file it is given, nothing finds a newer one | APPS.md §7's list |
| A repository and its index | missing: no index format, nothing builds or serves one | — |
| Security marks and advisories per version | missing | — |
| Signatures over the index and packages | missing. The scheme exists: `ferrix-sigverify` verifies the 4 KiB manifest of SIGNING.md §1.2 for kinds 1 to 4 (slot, configuration, driver, revocation), and the soft signer `ferrix-softsign` signs them. **No kind is a package or an index**, so neither crate can sign or verify one as it is | `src/lib/crypto/sigverify`, `tools/common/release/softsign` |
| Package anchors | missing: `signing-anchors.toml` holds development keys of kinds slot, config and driver only (0x8000-0x8002), their private state on nazuna | `tools/common/data/signing-anchors.toml` |
| Atomic upgrades by a btrfs snapshot | missing, and **cannot be built in ring 3 today**: `ferrix-btrfs-write` refuses any volume with a subvolume or snapshot (`Error::Subvolumes`, its crate comment: a snapshot shares tree blocks, and copying one needs back-reference bookkeeping it does not do), and the Linux personality has no snapshot ioctl | `src/lib/fs/btrfs/write/src/lib.rs`, `src/kernel/extensions/personality/linux/src/syscall/fsctl.rs` |
| `install --replace` as a transaction | half: per package, every file staged beside its place, then renamed, the record last; a stop part way is finished by running it again, not undone. Several packages replaced together are several transactions | `src/user/system/linux/pkg/src/lib.rs` |
| Driver updates, `drvupdated` and `drvupdate` | built without D3: root's word is the only check. SIGNING.md moved D3 to `devmgr` (§1.3) as slice S9; S9 is not built, and `devmgr` has no anchor table | `src/user/system/native/drvupdated`, `src/user/system/linux/init/drvupdate`, DEVMGR.md §4.1 |
| Slot updates (A/B) | S1, S3 and S4 built: the UEFI loader verifies a slot manifest; the gate signs with nazuna's development slot key. S5 (TPM floors, `bootctl sync`), S6 (HSM), S7 (revocations), S8 (init's configuration verifier) are not built | SIGNING.md §13 |
| Release updates | whole images only: release assets on GitHub; the Pixel 7 app replaces the VM's desktop from a release's assets by SHA-256 and size (`tools/vendor/google/pixel7/README.md`, "Updates") | RELEASES.md |
| A security advisory process | a private reporting address only; no advisory ids, no feed | `SECURITY.md` |

## 2. The plan

Each step stands on its own and ends in a gated state.

### 2.1 (a) A repository index, and `pkg update`, `newer`, `upgrade`

A repository is a directory per architecture:

```
<repository>/<arch>/index.toml
<repository>/<arch>/index.toml.fxsig                      (step b)
<repository>/<arch>/<name>-<version>-<arch>.fxpkg
<repository>/<arch>/<name>-<version>-<arch>.fxpkg.fxsig   (step b)
```

The index is in the TOML subset records are written in
(`ferrix_pkg::toml`), so numbers are strings:

```toml
[index]
arch = "x86_64"
sequence = "7"             # rises with every index the repository publishes

[[packages]]
name = "statd"
version = "1.1"
file = "statd-1.1-x86_64.fxpkg"
size = "40960"
sha256 = "<64 lower-case hex digits>"
depends = ["ferrix-base >= 1.0"]
security = true            # optional; false when absent
advisory = "FXSA-2026-0001: the stat service read past a buffer"   # required with security
```

An index may list several versions of one package: the security mark is a
version's, so an installed 1.0 with 1.1 (security) and 1.2 (not) in the index
has a security fix waiting, which 1.2 carries too.

`pkg` gains three commands, all against `--root`:

```
pkg update [--repo DIR]     read the repository's index for this architecture,
                            check it, and keep it as the accepted index
pkg newer                   each installed package the accepted index has a
                            newer version of, and each waiting security fix
                            with its advisory
pkg upgrade [--repo DIR] [NAME...]
                            fetch the newest version of each (all installed
                            packages when none is named), check each against
                            the index, and replace them together
```

The repository is `--repo`, or the one line of `etc/pkg/repository`. The
accepted index is `var/lib/pkg/index.toml`: the bytes `pkg update`
verified, kept without their signature, since the index is verified once,
as it is accepted (decision 10). An index is refused when it is for
another architecture, does not parse, or is **older than the accepted one**
(a lower sequence), or has the accepted one's sequence and other bytes. An
upgrade refuses a file whose size or SHA-256 is not the index's, a package
whose record is not the index's name, version and architecture, and a
dependency that neither the root nor the index has in a version that will
do. A dependency the index
has, newer than the installed one or not installed at all, comes with the
upgrade: minimum versions, no solver, as APPS.md §7 says.

### 2.2 (b) Signatures: a package kind in the one manifest

SIGNING.md signs everything with one 4 KiB manifest and one verifier, with a
key per kind. Packages are not one of its kinds, so step b adds the fifth:

| Kind | Code | The artifact | Who verifies | Where the manifest travels |
|---|---|---|---|---|
| **Package** | 5 | A repository's index for one architecture, and each package in it | `pkg`, on its own copy in memory, before it keeps an index or writes a file | A file beside it, `<file>.fxsig`: the 4 KiB block alone |

* `subject`: a package's name; for the index, `:index`, which no package can
  be named (`ferrix_pkg::manifest::is_name`). A configuration or driver
  manifest's rule holds: a package manifest checked with no subject is
  refused.
* `payload_sha256` and `payload_len`: the file's bytes. The index's digest of
  a package and its manifest's are the same SHA-256.
* `security_version`: the index's sequence for the index, so that the
  verifier's floor check is the "older than accepted" refusal (the floor is
  the accepted sequence); zero for a package, which the index binds by
  digest.
* `profile`: `0xFF` (development), the only profile a package key exists
  for; `pkg` reading the build's profile byte comes with the development
  package key (§4 item 1).
* `arch`: the package's; `family`: the generic family of its architecture
  (0x0101, 0x0201, 0x0301), since a package is built per architecture, not
  per board.
* Anchors: compiled into `pkg` (SIGNING.md §6.1's rule for every verifier in
  the image), from a generated table. The `pkg` an image carries has no
  package anchor until a development package key exists in
  `signing-anchors.toml`, so it refuses every signed repository, failing
  closed. `test-pkg` makes a throwaway key per run, outside any git tree,
  and builds its own `pkg` with that key's anchor.
* Revocations of package keys (kind 4 with target 5) are verified by the
  same code; `pkg` holds no revoked list until S7 gives the slot one.

This is a change to `ferrix-sigverify`, which is in the certified item, and
to the soft signer, which its evidence is measured with. It is one commit
of its own, reviewed by the consultant (ledger line 9932, §5). Kind 5 is
for development only: no product key, no slot or TPM floor, and no anchor
in any image (SIGNING.md §1.1).

**What the signatures do not show.**

* **The floor is a file root writes.** The floor is the sequence of the
  accepted index in `var/lib/pkg/`, not a TPM index or a slot's floors
  (SIGNING.md §7). The refusal of an older index holds against a
  repository and whoever stands between it and the machine. It does not
  hold against root, or anyone who writes the root's disk while the
  system is off: deleting or replacing the accepted index lowers the floor.
* **A first update has floor 0.** A machine with no accepted index takes
  any index the key signed, the oldest included.
* **Nothing bounds an index's age.** An index carries no expiry, so a
  repository, or a path to it, that withholds a newer index is not
  noticed: the machine stays on the last index it accepted and says it is
  up to date.
* **`pkg install FILE` checks no signature.** A package file named on the
  command line is installed on root's word, as `drvupdated` takes a driver
  image without D3 (§1).
* **No revoked list before S7.** `pkg` verifies with an empty list of
  revoked package keys, so a revoked key's signatures are accepted until
  S7 gives the slot a list and `pkg` reads it.

### 2.3 (c) The upgrade as one transaction

APPS.md §7 names a btrfs snapshot before a transaction as the rollback.
Ferrix's btrfs writer refuses a volume with any snapshot (§1), so that waits
on snapshot support in `ferrix-btrfs-write` and an ioctl or a native call to
take one. Without it, step c makes a multi-package upgrade one transaction in
`pkg` itself: every package's files staged first; each file it replaces moved
aside, not deleted, until the last record is in place; and a journal,
`var/lib/pkg/transaction`, written before the first rename and removed after
the last, so that a failure, or a `pkg` run after a stop, puts back what was
there.

### 2.4 (d) The gate (built with each step)

`cargo xtask test-pkg` serves a repository from a directory in its boot's
root and requires: an index read and accepted; `pkg newer` naming a newer
version and a security fix with its advisory; the upgrade applied, the
dependency it pulls with it; and each refusal next to the control that
passes: a wrong digest, a bad signature, an index older than the accepted
one, an index changed under its good signature, a missing dependency. No
build of today refuses a development key at `pkg`'s level (S3's refusal is
by image, at build time), so that refusal has no row until a claimed
profile carries `pkg`.

## 3. Decisions taken where the documents were silent

1. **One kind, 5, for the index and the packages**, separated by the signed
   subject, rather than two kinds and two keys: the repository's one
   publisher signs both, as apk's and Debian's single repository key does.
2. **Detached manifests** (`<file>.fxsig`) rather than a block in front of
   the file: a package stays the cpio archive `pkg install` and every image
   build read, and an unsigned local package still installs.
3. **SHA-256 for the index's digests**, not the records' BLAKE2b-256, so that
   the index and the signature name one digest.
4. **The index's sequence is the manifest's security version**, so the
   verifier's floor is the rollback check.
5. **The generic family of the architecture** in a package manifest.
6. **A repository is a directory** in this step. Fetching over HTTP (with the
   curl app, APPS.md §7) is the next step after the gate, and changes only
   where the bytes come from: every check runs on the copy in memory.
7. **The accepted index in `var/lib/pkg/`, the repository in
   `etc/pkg/repository`.**
8. **Advisories are one line in the index**, with an id of the form
   `FXSA-<year>-<n>` by convention; no advisory feed of its own yet.
9. **No unsigned mode.** A missing signature is refused as a bad one is,
   and the `pkg` an image carries, trusting no package key yet, refuses every
   repository rather than accept an unsigned one.
10. **The index is verified once, when `pkg update` accepts it**, before one
    byte of it is parsed; `newer` and `upgrade` trust the accepted copy in
    `var/lib/pkg/`, which only root writes. `upgrade` fetches only what that
    copy names, so `update` comes first.
11. **An upgrade installs a dependency the root lacks**, the newest version
    the index has that meets the minimum, rather than refuse it.
12. **`test-pkg`'s throwaway keys** take anchor id 0x8f00, a development id
    `signing-anchors.toml` gives no key; they live for one run in a
    directory the run makes for itself, outside any git tree (D5), and the
    boot's `pkg` is built trusting one of them, through
    `FERRIX_PKG_ANCHORS`, in a target directory of its own, so the `pkg`
    every image carries is unchanged.
13. **The transaction is a journal and renames**, not a snapshot (§2.3); a
    `.pkg-old` already in the way is refused before anything changes. A
    removal stays as it was: record first, then files.
14. **No development-key refusal row.** SIGNING.md §6.4's refusal of a
    development key is by image, at build time, for a claimed profile, and no
    claimed profile carries `pkg` with a package anchor, so `test-pkg` has no
    build that must refuse one.

## 4. Left, in order

1. A development package key, its own landing with a one-line OK from the
   consultant before its take (ledger line 9932 (8)): (a) made by whoever
   holds `signing-dev` with `ferrix-softsign keygen`, H15, anchor id 0x8003,
   kind `package`, its private state in `signing-dev` only (D5), with a
   `made` line; (b) `check-signing-anchors.py` taking the kind under
   `[development.*]` and refusing it under `[release.*]` until the
   customer answers (item 3), with a self-test row for each; (c) xtask
   giving the anchor to an image's `pkg` from `signing-anchors.toml`, never
   from the caller's environment, and only outside claimed profiles, its
   claimed-image scan refusing a claimed image whose `pkg` holds the key,
   shown by a planted control that fires (decision 14's missing row comes
   then); (d) `pkg` reading the build's profile byte, or keeping 0xFF with
   the text saying a claimed profile's `pkg` trusts nothing; (e) `test-pkg`
   keeping its throwaway keys, so the gate never needs `signing-dev`. With
   it come VULNERABILITY-ANALYSIS's rows for a repository attacker: replay,
   freeze and the first update (§2.2). Until then a development image's
   `pkg` refuses every repository.
2. Fetching over HTTP with the curl app.
3. **The customer's questions** (ledger line 9932 (7), SIGNING.md §12
   Q11, filed and not waited for): is there a product package key at all
   (eight product keys, its leaf budget, its custody)? Does any claimed
   profile carry `pkg` trusting a package anchor? Do released images trust
   a CI-signed repository fetched over the network? Whose repositories may
   an image trust? Until they are answered: no `[release.*]` anchor of kind
   `package`, `pkg`'s `build.rs` keeps refusing an anchor id below 0x8000,
   and item 4 does not start.
4. CI building the index and signing it with a CI development key
   (SIGNING.md §6.5), served statically; never a release key until S6.
   It waits on item 3's answers, and takes the review CI's slot key took
   (ledger 4005, 4188) and the customer's word.
5. A removal as a transaction.
6. btrfs snapshots in the writer, then the snapshot as the transaction.
7. D3 (S9, `devmgr`'s kind-3 check), which this does not touch.
8. Building on Ferrix and the system as packages (APPS.md §7).

## 5. Where it stands

2026-11-13: this design, and steps a to d, on branch `pkg/updates`. Nothing
of it has landed. The consultant's design and code review is **OK IF
(1)-(6)**, ledger `~/.local/share/ferrix/cert-consultant/reviews.md` line
9932, with (7), the customer's questions, filed beside it (§4 item 3) and
(8), the development package key, owed by that key's own landing (§4
item 1). The conditions, met on the branch on top of the reviewed commits
(the code at 26805f1e0):

1. the item control on the package kind's subject, `control: FIRED
   (host)` (tag `pkgc-ctl-item`, SIGNING.md §13);
2. host coverage measured again, `manifest.rs` as committed (SIGNING.md
   §13);
3. SIGNING.md true to five kinds (§1.1, §1.2, §6.4, §12, §13);
4. this document: §2.1's accepted index, §2.2's limits, §4 and this
   section;
5. the refusal of a good signature beside other bytes (`PayloadMismatch`,
   below), with its controls;
6. `check` and `test-pkg --arch all` `run: PASSED` on 26805f1e0 (tags
   `pkgc-check`, log sha256 `48c948a1...`, and `pkgc-pkg`, `8e30d3c2...`,
   aarch64 and armv7a under TCG, x86_64 under KVM), and `check-docs` on the
   tip.

Two of the review's lesser points are taken in on the branch: `test-pkg`'s
key directory is made by the run alone, mode 0700, never one already there
(C3), and `repo.rs`'s own comparison of sequences says it is the second
check behind the floor (C4). C1, C2 and C5 are BACKLOG rows.

**Step a, as built.** `ferrix_pkg::index` (`src/lib/proto/pkg/src/index.rs`)
reads and writes the index, says what is newer and which versions are
security fixes (`Index::newer`), and chooses an upgrade by minimum versions
(`Index::upgrade`); its host tests are in `src/lib/proto/pkg/src/tests.rs`.
`pkg update`, `newer` and `upgrade` are `src/user/system/linux/pkg/src/repo.rs`,
with host tests against a directory as both root and repository: the
upgrade with its security fix and a dependency it pulls in, an older index
and another index of the accepted sequence refused, a file one byte
changed, one byte longer and another package in its place refused with
nothing written, and a dependency nobody has. `test-pkg` serves five
repositories from its boot's root (`tools/common/xtask/src/pkg.rs`,
`repositories`) and requires each of these in the guest.

**Step b, as built.** Kind 5 in `ferrix-sigverify` and the soft signer is
commit "sigverify: a fifth manifest kind", alone, reviewed by the
consultant (above). `pkg` verifies with it in `src/user/system/linux/pkg/src/sig.rs`:
`pkg update` verifies the index's `index.toml.fxsig` before it reads the
index, with the accepted sequence as the floor, and requires the sequence
the index says to be the one signed; `pkg upgrade` verifies each package's
`.fxsig` after its size and digest. The anchors are compiled in from
`FERRIX_PKG_ANCHORS` (`build.rs`); unset, as for every image, `pkg` trusts
no package key and refuses every repository. Host tests sign with the soft
signer as a dev-dependency: a key not the anchor's, no anchor at all, the
index signed as a package, a sequence other than the signed one, a missing
signature, a package signed by another key or as another package, each
refused beside its control. `test-pkg` makes two throwaway H10 keys per
run in a directory outside any git tree (`Signer`), builds its `pkg` with
one of them, and adds an index signed by the other, a package signed as
another, and an unsigned repository to its refusals.

**The payload check.** `sig::check` hands the verified manifest's length
and digest to `check_payload` against the bytes `pkg` holds: the one check
that ties a file to its signature. A host test,
`a_good_signature_beside_other_bytes_is_refused`, changes one byte of a
signed index, its length, sequence and syntax kept, and requires
"(PayloadMismatch): refused" with the accepted index as it was; then a
package the index names beside a signature, under its own name, of other
bytes of the same length, refused with nothing written; each beside the
control that passes.
`test-pkg` serves `changed-index`, index 4 with one byte of its advisory
changed under its signature, and requires the same refusal on every
architecture with index 3 still accepted after it. With `check_payload`'s
result dropped in `sig.rs`, both fire: `control: FIRED (line, no panic):
xtask: x86_64: test-pkg is missing [pkg-test: ok refused an index changed
under its signature, pkg-test: ok the changed index left index 3 accepted]
and printed []` (tag `pkgc-ctl-payload`, log sha256 `a82d3ca0...`), and
`control: FIRED (host): test
tests::repository::a_good_signature_beside_other_bytes_is_refused ...
FAILED` (tag `pkgc-ctl-payload-host`, `c79b8ddd...`).

**Step c, as built.** `src/user/system/linux/pkg/src/transaction.rs`. Every
install, `--replace` and upgrade of any number of packages is one
transaction: every new file staged as `<name>.pkg-new`; the journal,
`var/lib/pkg/transaction`, written and synced with each path and whether
something was there, under `swapping`; each path's old file renamed aside
as `<name>.pkg-old` and the new one into its place, the records last; the
journal turned `committed`; only then the old files deleted. A failed
rename undoes everything from the journal, and a `pkg` that changes the
root first undoes a transaction that stopped before its commit, or
finishes one that stopped after it. A `.pkg-old` already there is refused
before anything changes, so undoing puts back only what the transaction
moved. Host tests stop the swap dead, and fail it, after every one of its
renames in turn, and require the root byte for byte as it was; `test-pkg`
refuses a replacement with something in the way, and undoes a journal its
script leaves half done. A removal is not a transaction yet: it deletes the
record first, as before. The btrfs snapshot stays the aim (§2.3); this
needs nothing of the file system but rename.
