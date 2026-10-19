# Licences in a release

Ferrix is MIT licensed, and so are ferrousli, zinc and the apps written for
it. A release's images also carry other people's programs: busybox, git and
alsa-utils under the GPL, alsa-lib under the LGPL, and many permissive C
libraries and Rust crates. Two things go with every release so that it can
be shipped:

- **A licence inventory**, `THIRD-PARTY-LICENSES.md`: every part of the
  image, the licence it is shipped under, and every licence text. It is
  inside the x86-64 desktop's zip, and an asset of its own for each image
  (`ferrix-<tag>-THIRD-PARTY-LICENSES.md`,
  `ferrix-pixel7-<edition>-THIRD-PARTY-LICENSES.md`).
- **The corresponding sources**, `ferrix-<tag>-sources.tar.gz`: for each GPL
  and LGPL program, and what is linked statically into one, the exact
  upstream tarball its build used, its build script with every patch, and
  for busybox its config; ferrousli at its pinned commit; and `SOURCES.md`,
  which says how each is rebuilt.

The release workflow makes both (`.github/workflows/release.yml`). Nothing
in either is written by hand.

## How the inventory is made

`tools/common/release/licences.py` reads the image itself: the initramfs
`cargo xtask release-image` writes beside the disk image, or the one in
`flash --stage`'s directory for the Pixel 7. Every regular file in it is
claimed by one of:

1. **a package record** (`lib/ferrix/packages/<app>.toml`), which names the
   app, whose licence is its `app.toml`'s `license`;
2. **an app's program** (`bin/<app>`), for the apps built with cargo;
3. **a rule** in `tools/common/data/release-licences.toml`'s
   `[components]`: busybox, zinc, oh-my-zsh, zsh's functions, and Ferrix's
   own programs and configuration.

For each C program it lists the upstream tarballs its build script pins
(`tools/common/release/pinned.py` reads them out of the scripts) and copies
the licence texts out of those tarballs. For Rust it reads every
`Cargo.lock` of a workspace that builds programs for Ferrix -- the main
workspace, each one under `src/user/system/linux/`, each app's, and the
lockfile inside a crate an app builds from crates.io -- and takes each
crate's licence and licence files from its own sources in cargo's cache.
That list is a superset of what one image links: it is every crate any
Ferrix image can carry. Where a part offers a choice, `A OR B`, the
inventory takes the first alternative that is permissive and says which it
took: curl's and Mbed TLS's `Apache-2.0`, FreeType's `FTL`.

## What the check refuses

`licences.py` exits non-zero, which fails the release job and stops
`package-desktop.py` before it packs anything, when:

- a file of the image is claimed by nothing;
- an app, component or crate has no licence, or one outside
  `release-licences.toml`'s `permissive` and `copyleft` lists;
- a component under a copyleft licence is not in that file's `[sources]`,
  the list `package-sources.py` packs the source of.

So adding a GPL program to an image means adding it to `[sources]`, and the
release then carries its source. A new licence means adding it to the list
after reading it.

The check's negative control: change one app's `license` to one outside the
lists, or add a file to the image no rule claims, and run `licences.py`;
it prints `licences: REFUSED: ...` for each and exits 1.

## The sources

`tools/common/release/package-sources.py` packs, for each `[sources]` entry,
the tarballs its build script pins -- from the build's own download cache,
checked against the pin, or fetched and checked with `--fetch` -- and the
script's directory. Why each is there:

| Part | Licence | Why |
|---|---|---|
| busybox 1.37.0 | GPL-2.0-only | every image's shell utilities |
| git 2.55.0 | GPL-2.0-only | the x86-64 desktop |
| alsa-utils 1.2.16 | GPL-2.0-or-later | `aplay` and `speaker-test` |
| alsa-lib 1.2.16.1 | LGPL-2.1-or-later | linked statically into alsa-utils only |
| curl 8.22.0, Mbed TLS 3.6.7 | curl, Apache-2.0 | linked statically into git |
| zlib 1.3.2 | Zlib | linked statically into git |
| ferrousli | MIT | the C library all of them link statically |

alsa-lib is linked statically into alsa-utils' two programs, both GPL, and
into nothing else: the alsa-lib package itself puts only its configuration
files on the image, and Ferrix's own audio code (`src/user/system/linux/media`)
is a Rust implementation of the parts it needs, not a link. So LGPL-2.1's
relinking terms (§6) are met by the GPL programs' own source.

MPL-2.0 is owed file by file: the sources asset carries the `.crate` of each
MPL-2.0 crate the inventory lists, and curl's MPL CA bundle is its own
source.

## Firmware

None is bundled. The x86-64 desktop's run scripts boot the OVMF that QEMU
or the distribution installed, under its own licence (EDK2's
BSD-2-Clause-Patent).

## The Pixel 7's Chromium volume

The `full` edition's `ferrix-pixel7-chromium.img.gz` is Debian 13's
Chromium for arm64 and its libraries, unpacked unchanged from Debian's
packages by `tools/common/fetch/fetch-chromium-arm64.sh`. The sources asset
lists each package, its version and hash, its source package, and the
snapshot.debian.org address of that source (`pixel7-debian/packages.tsv`),
and the volume keeps each package's `usr/share/doc/<package>/copyright`.
The release points to Debian's sources rather than carrying them: Chromium's
alone is over a gigabyte. For the GPL packages in that list a pointer to a
third party's archive is weaker than carrying the source, and attaching
the source packages of the GPL and LGPL ones is the open step.

## Not covered yet

- **oh-my-zsh's plugins and themes** are listed as the tree's MIT licence;
  the few that carry a licence of their own inside the tree are not read
  out one by one.
- **zsh's functions** are under zsh's licence, a permissive one SPDX has no
  identifier for (`LicenseRef-Zsh`); its text is in the inventory.
- **A crate with no licence file** in its package is listed with its SPDX
  identifier, and the inventory says the identifier's standard text applies.
