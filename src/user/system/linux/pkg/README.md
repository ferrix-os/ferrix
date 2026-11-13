# pkg

Ferrix's package manager, `/bin/pkg` (`docs/APPS.md` §7). An app is built
into a package, a newc archive of its files and its record,
`lib/ferrix/packages/<name>.toml`; an image is packages installed into a
root. `pkg` does the same on a running Ferrix:

```
pkg [--root DIR] list               the packages installed
pkg [--root DIR] info NAME          one package: what it is and its files
pkg [--root DIR] install FILE...    install packages (.fxpkg), together
pkg [--root DIR] install --replace FILE...
                                    the same, each in its installed version's place
pkg [--root DIR] remove NAME        remove a package
pkg [--root DIR] update [--repo DIR]
                                    accept the repository's index
pkg [--root DIR] newer              what the accepted index has newer, and
                                    the security fixes waiting
pkg [--root DIR] upgrade [--repo DIR] [NAME...]
                                    fetch, check and put in place the newest
                                    of each, with what it needs
```

A repository is a directory per architecture with an index of every version
it has, each with its file's size and SHA-256 and, for a security fix, an
advisory; `pkg update` refuses an index older than the one it accepted
before, and `pkg upgrade` a file that is not the one the index names
(`docs/UPDATES.md`). The index and every package carry a signature beside
them, `docs/SIGNING.md`'s manifest of kind 5, verified with the anchors
compiled in from `FERRIX_PKG_ANCHORS` (`build.rs`): none in the `pkg` an
image carries until a package key exists, so it refuses every repository.

There is no database: the records are what is installed. An install,
replacement or upgrade of any number of packages is one transaction, with a
journal in `/var/lib/pkg/transaction`: all of it, or the root as it was. What a package is
-- the manifest, the record, the plan that orders a set and refuses a missing
dependency or a path owned twice -- is `src/lib/proto/pkg`, which xtask
builds images with, and the archive reader is the kernel's
`src/lib/fs/common/cpio`, so the image build and `pkg` agree on both.

`cargo test` here runs it against a directory as the root;
`cargo xtask test-pkg` boots it, installs, runs and removes the stat
service, replaces a package and back, upgrades from repositories it serves
itself, and requires its fifteen refusals and a stopped transaction undone.
