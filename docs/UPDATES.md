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
accepted index is `var/lib/pkg/index.toml` (with its signature beside it
from step b). An index is refused when it is for another architecture, does
not parse, or is **older than the accepted one** (a lower sequence), or has
the accepted one's sequence and other bytes. An upgrade refuses a file whose
size or SHA-256 is not the index's, a package whose record is not the
index's name, version and architecture, and a dependency that neither the
root nor the index has in a version that will do. A dependency the index
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
* `profile`: `0xFF` (development) while only development keys exist; a
  release package key and `pkg` reading the build's profile come together.
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
of its own, for the consultant's review before it lands.

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

### 2.4 (d) The gate

`cargo xtask test-pkg` serves a repository from a directory in its boot's
root and requires: an index read and accepted; `pkg newer` naming a newer
version and a security fix with its advisory; the upgrade applied, the
dependency it pulls with it; and each refusal next to the control that
passes: a wrong digest, a bad signature, an index older than the accepted
one, a missing dependency. No build of today refuses a development key at
`pkg`'s level (S3's refusal is by image, at build time), so that refusal has
no row until a claimed profile carries `pkg`.

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

## 4. Left, in order

1. A development package key (anchor 0x8003, kind `package`) in
   `signing-anchors.toml`, made by whoever holds `signing-dev`, and `pkg`'s
   generated table reading it, with `check-signing-anchors.py` taking the
   kind: until then a development image's `pkg` refuses every repository.
2. Fetching over HTTP with the curl app.
3. CI building the index and signing it with the CI development key
   (SIGNING.md §6.5), served statically; never a release key until S6.
4. btrfs snapshots in the writer, then the snapshot as the transaction.
5. D3 (S9, `devmgr`'s kind-3 check), which this does not touch.
6. Building on Ferrix and the system as packages (APPS.md §7).

## 5. Where it stands

2026-11-13: this design, on branch `pkg/updates`.
