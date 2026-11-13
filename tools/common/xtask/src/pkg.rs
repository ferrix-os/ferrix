//! The package manager, `/bin/pkg` (`src/user/system/linux/pkg`,
//! `docs/APPS.md` §7): built for an architecture, carried by the images a
//! person runs, and `cargo xtask test-pkg`, the boot that has it install,
//! run and remove an app.
//!
//! The boot runs zinc as init with `pkg`, the stat service's package as
//! `build-apps` makes it, and three packages made here: `pkg-base`, the
//! `pkg-user` that depends on it, and `pkg-base` with one byte of a file
//! changed after its record was written. It requires the stat service
//! installed, run and removed again, and each refusal said: the changed
//! package, `pkg-user` without what it depends on, and `pkg-base`'s removal
//! while `pkg-user` needs it. Each refusal is a negative control of the check
//! behind it, and the installs that go through show the same checks pass a
//! good package.
//!
//! Then the repositories of `docs/UPDATES.md` §2.4, directories in the
//! boot's root: `pkg update` accepts an index, `pkg newer` names the
//! security fix waiting with its advisory, `pkg upgrade` puts it in place
//! with the package the upgrade needs, and an older index, another index of
//! the accepted sequence, a file whose digest is not the index's, an index
//! signed by a key `pkg` does not trust, a package signed as another, an
//! unsigned index and package, a dependency no one has, and an index one
//! byte changed under its good signature are refused,
//! each beside the run that passes. A replacement with something in the
//! way of a file being set aside is refused with nothing changed, and a
//! transaction left half done (its journal written by the script) is undone
//! by the next `pkg`. The repositories are signed with two
//! throwaway development keys the run makes and deletes ([`Signer`]), and
//! the boot's `pkg` is built trusting one of them.

use std::path::PathBuf;

use ferrix_pkg::index::{self, Index};
use ferrix_pkg::manifest;
use ferrix_pkg::record::{self, Installed, Record};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, shell, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-pkg",
        help_at: 230,
        help: &[
            "test-pkg      Boot with pkg, the package manager, and require it to install, run and remove the stat",
            "              service, to replace a package with its next version and back (`pkg install --replace`),",
            "              and to refuse a changed package or replacement, a missing dependency and a needed removal;",
            "              then to upgrade from a signed repository it serves itself, with a security fix, and refuse an",
            "              older index, a wrong digest, a bad or missing signature, a dependency nobody has and an index",
            "              changed under its signature",
            "              (docs/UPDATES.md)",
        ],
        run,
    },
    Task {
        name: "install-app",
        help_at: 240,
        help: &[
            "install-app   Build the app --app NAME for this machine and put it in the running system in the place of",
            "              the version installed, with `pkg install --replace` (on Ferrix, as root; --to DIR installs",
            "              into the root at DIR instead, which is the only way it runs on another system)",
        ],
        run,
    },
];

/// The workspace.
const DIR: &str = "src/user/system/linux/pkg";
/// Where the program goes.
const PROGRAM: &str = "bin/pkg";

/// Build `pkg` for `arch`, or `None` where there is no musl target.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn build(arch: Arch) -> Result<Option<PathBuf>> {
    build_trusting(arch, None)
}

/// [`build`], trusting the package anchors `anchors` names
/// (`FERRIX_PKG_ANCHORS`, `src/user/system/linux/pkg/build.rs`) in a target
/// directory of its own; [`build`]'s `pkg`, the one every image carries,
/// trusts none.
fn build_trusting(arch: Arch, anchors: Option<&str>) -> Result<Option<PathBuf>> {
    let Some(target) = zinc::target(arch) else {
        println!("  pkg is not built for {arch} yet");
        return Ok(None);
    };
    let target_dir = paths::target_dir().join(if anchors.is_some() {
        "pkg-test-anchors"
    } else {
        "pkg"
    });
    let program = target_dir.join(target).join("release").join("pkg");
    crate::builds::Build::cargo(
        format!("cargo build (pkg) --target {target}"),
        paths::workspace_root().join(DIR),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    // As an app's: RUSTFLAGS replaces the flags every config file up the
    // tree would merge, and rust-lld is the whole toolchain with the musl
    // targets' own C runtime.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .env(&crate::apps::linker_variable(target), "rust-lld")
    .env("FERRIX_PKG_ANCHORS", anchors.unwrap_or_default())
    .output(&program)
    .run()?;
    Ok(Some(program))
}

/// What an image carries for the package manager on `arch`: `/bin/pkg`.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn carried(arch: Arch) -> Result<Vec<File>> {
    let Some(program) = build(arch)? else {
        return Ok(Vec::new());
    };
    Ok(vec![File {
        path: PROGRAM.to_owned(),
        mode: 0o755,
        content: Content::Bytes(read(&program)?),
    }])
}

fn read(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// Where the boot's packages are.
const PACKAGES: &str = "var/lib/pkg-test";

/// A package of `name`, built for `arch`, depending on `depends`, holding
/// one line of text at `bin/<name>`: the boot reads it rather than runs it,
/// since its image has no `/bin/sh` for a script.
fn made(name: &str, depends: &[&str], arch: Arch) -> Result<Vec<u8>> {
    made_at(name, "1.0", "here", None, depends, arch)
}

/// [`made`], of `version`, its line saying `says`, and with `extra` a
/// second file, `bin/<extra>`, saying the same.
fn made_at(
    name: &str,
    version: &str,
    says: &str,
    extra: Option<&str>,
    depends: &[&str],
    arch: Arch,
) -> Result<Vec<u8>> {
    Ok(made_with(name, version, says, extra, depends, arch)?.1)
}

/// [`made_at`], and the package's record.
fn made_with(
    name: &str,
    version: &str,
    says: &str,
    extra: Option<&str>,
    depends: &[&str],
    arch: Arch,
) -> Result<(Record, Vec<u8>)> {
    let depends: Vec<String> = depends.iter().map(|name| format!("\"{name}\"")).collect();
    let text = format!(
        "[package]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"test-pkg's {name}\"\nlicense = \"MIT\"\n\
         abi = \"linux\"\narches = [\"{arch}\"]\ndepends = [{}]\n\n\
         [[package.files]]\nfrom = \"{name}\"\nto = \"bin/{name}\"\nmode = \"755\"\n",
        depends.join(", "),
        arch = arch.name()
    );
    let mut package = manifest::recipe(&text)
        .map_err(|error| Error::new(format!("test-pkg's {name}: {error}")))?
        .package;
    package.arches = vec![arch.name().to_owned()];
    let script = format!("{name}: {says}\n").into_bytes();
    let mut files: Vec<File> = std::iter::once(name)
        .chain(extra)
        .map(|file| File {
            path: format!("bin/{file}"),
            mode: 0o755,
            content: Content::Bytes(script.clone()),
        })
        .collect();
    let record = Record {
        package,
        files: files
            .iter()
            .map(|file| Installed::of(&file.path, 0o755, &script))
            .collect(),
    };
    files.push(File {
        path: record::path(name),
        mode: 0o644,
        content: Content::Bytes(record::render(&record).into_bytes()),
    });
    let bytes = initramfs::package(&files)?;
    Ok((record, bytes))
}

/// Where the boot's repositories are (`docs/UPDATES.md` §2.4): one
/// directory each, `<arch>/index.toml` and the package files it names.
const REPOSITORIES: &str = "srv/pkg-repo";

/// The advisory of the security fix in the boot's repository.
const ADVISORY: &str = "FXSA-2026-0001: test-pkg's pkg-base 1.0 fixed in 1.1";

/// The anchor id of `test-pkg`'s throwaway package keys: a development id
/// (`docs/SIGNING.md` §6.4) that `signing-anchors.toml` gives no key.
const ANCHOR: &str = "0x8f00";

/// `test-pkg`'s throwaway development keys (`docs/UPDATES.md` §2.2): two
/// H10 package keys, `good`, whose anchor the boot's `pkg` is built with,
/// and `other`, which no `pkg` trusts, made by the soft signer in a
/// directory of their own outside any git tree, for one run, and deleted
/// with it. The manifests they sign are kept by what they cover, since the
/// repositories share most of their files.
struct Signer {
    dir: PathBuf,
    good: String,
    signed: std::collections::HashMap<(String, String, u32, Vec<u8>), Vec<u8>>,
}

impl Signer {
    fn new() -> Result<Signer> {
        let dir = std::env::temp_dir().join(format!("ferrix-test-pkg-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut signer = Signer {
            dir,
            good: String::new(),
            signed: std::collections::HashMap::new(),
        };
        signer.good = signer.softsign(&["keygen", "good", "--height", "10"])?;
        let _ = signer.softsign(&["keygen", "other", "--height", "10"])?;
        Ok(signer)
    }

    /// The soft signer's answer to `args`, on this run's keys.
    fn softsign(&self, args: &[&str]) -> Result<String> {
        let out = std::process::Command::new(cargo::cargo())
            .current_dir(paths::workspace_root())
            .args(["run", "-q", "--release", "-p", "ferrix-softsign", "--"])
            .args(args)
            .arg("--dir")
            .arg(&self.dir)
            .output()
            .map_err(|error| Error::new(format!("running the soft signer: {error}")))?;
        if !out.status.success() {
            return Err(Error::new(format!(
                "test-pkg: the soft signer refused {args:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// `FERRIX_PKG_ANCHORS` for the boot's `pkg`: `good`'s anchor.
    fn anchors(&self) -> String {
        format!("{ANCHOR}:{}", self.good)
    }

    /// The package manifest of `payload` as `subject`'s, of
    /// `security_version`, for `arch`, signed by `key`.
    fn sign(
        &mut self,
        key: &str,
        subject: &str,
        security_version: u32,
        arch: Arch,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        let id = (
            key.to_owned(),
            subject.to_owned(),
            security_version,
            index::sha256(payload).to_vec(),
        );
        if let Some(block) = self.signed.get(&id) {
            return Ok(block.clone());
        }
        let (arch_byte, family) = match arch {
            Arch::X86_64 => ("1", "0x0101"),
            Arch::AArch64 => ("2", "0x0201"),
            Arch::Armv7a => ("3", "0x0301"),
        };
        let input = self.dir.join("payload");
        let output = self.dir.join("manifest");
        std::fs::write(&input, payload)?;
        let version = security_version.to_string();
        let _ = self.softsign(&[
            "sign",
            key,
            "--kind",
            "package",
            "--anchor-id",
            ANCHOR,
            "--profile",
            "255",
            "--arch",
            arch_byte,
            "--family",
            family,
            "--security-version",
            &version,
            "--subject",
            subject,
            "--label",
            "test-pkg",
            "--payload",
            &input.to_string_lossy(),
            "--out",
            &output.to_string_lossy(),
        ])?;
        let block = read(&output)?;
        let _ = self.signed.insert(id, block.clone());
        Ok(block)
    }
}

impl Drop for Signer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// How one of the boot's repositories is signed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Signed {
    /// Every file by `good`, as it should be.
    Good,
    /// The index by `other`, and one package (`name version`) signed by
    /// `good` as another package's.
    Badly(&'static str),
    /// Nothing.
    Not,
}

/// One of the boot's repositories: its directory, its index's sequence,
/// its packages, the one version (`name version`) marked a security fix,
/// a file whose one byte is changed after the index named it, and how it
/// is signed.
struct Repository<'a> {
    dir: &'a str,
    sequence: u64,
    packages: Vec<&'a (Record, Vec<u8>)>,
    security: Option<&'a str>,
    tamper: Option<&'a str>,
    signed: Signed,
}

/// `repository`'s files, under [`REPOSITORIES`], each with its signature.
fn repository_files(
    arch: Arch,
    repository: &Repository<'_>,
    signer: &mut Signer,
) -> Result<Vec<File>> {
    let at = format!("{REPOSITORIES}/{}/{}", repository.dir, arch.name());
    let mut files = Vec::new();
    let file = |path: String, bytes: Vec<u8>| File {
        path,
        mode: 0o644,
        content: Content::Bytes(bytes),
    };
    let mut entries = Vec::new();
    for (record, bytes) in &repository.packages {
        let mut entry = index::entry_of(record, bytes);
        let named = format!("{} {}", entry.name, entry.version);
        if repository.security == Some(named.as_str()) {
            entry.security = true;
            entry.advisory = Some(ADVISORY.to_owned());
        }
        let subject = match repository.signed {
            Signed::Badly(misnamed) if misnamed == named => "pkg-other",
            _ => entry.name.as_str(),
        };
        if repository.signed != Signed::Not {
            let block = signer.sign("good", subject, 0, arch, bytes)?;
            files.push(file(format!("{at}/{}.fxsig", entry.file), block));
        }
        let mut bytes = bytes.clone();
        if repository.tamper == Some(named.as_str()) {
            // One byte of its program changed, the length kept: only the
            // digest can tell.
            bytes = initramfs::with_files_changed(&bytes, |name, data| {
                let mut data = data.to_vec();
                if let Some(first) = data.first_mut().filter(|_| name.starts_with("bin/")) {
                    *first ^= 0x20;
                }
                Ok(Some(data))
            })?;
        }
        files.push(file(format!("{at}/{}", entry.file), bytes));
        entries.push(entry);
    }
    let text = index::render(&Index {
        arch: arch.name().to_owned(),
        sequence: repository.sequence,
        entries,
    })
    .into_bytes();
    let sequence = u32::try_from(repository.sequence).unwrap_or(u32::MAX);
    let key = match repository.signed {
        Signed::Good => Some("good"),
        Signed::Badly(_) => Some("other"),
        Signed::Not => None,
    };
    if let Some(key) = key {
        let block = signer.sign(key, ":index", sequence, arch, &text)?;
        files.push(file(format!("{at}/index.toml.fxsig"), block));
    }
    files.push(file(format!("{at}/index.toml"), text));
    Ok(files)
}

/// The boot's repositories: `good`, index 2, whose pkg-base 1.1 is a
/// security fix and whose pkg-user 2.0 needs it and pkg-lib, which is not
/// installed; `older`, index 1; `same-sequence`, another index 2;
/// `bad-digest`, `good` with pkg-base 1.1's program changed after its
/// index was written; `bad-signature`, `good` with the index signed by a
/// key no `pkg` trusts and pkg-base 1.1 signed as another package;
/// `unsigned`, `good` with no signatures; `missing`, index 3, whose
/// pkg-user 3.0 needs a package no one has; and `changed-index`, `missing`
/// as index 4 with one byte of its index changed under its signature.
fn repositories(arch: Arch, signer: &mut Signer) -> Result<Vec<File>> {
    let base = made_with("pkg-base", "1.0", "here", None, &[], arch)?;
    let fixed = made_with("pkg-base", "1.1", "fixed", None, &[], arch)?;
    let user = made_with("pkg-user", "1.0", "here", None, &["pkg-base"], arch)?;
    let user2 = made_with(
        "pkg-user",
        "2.0",
        "two",
        None,
        &["pkg-base >= 1.1", "pkg-lib"],
        arch,
    )?;
    let user3 = made_with("pkg-user", "3.0", "three", None, &["pkg-gone"], arch)?;
    let lib = made_with("pkg-lib", "1.0", "here", None, &[], arch)?;
    let good = || vec![&base, &fixed, &user, &user2, &lib];
    let like_good = |dir, tamper, signed| Repository {
        dir,
        sequence: 2,
        packages: good(),
        security: Some("pkg-base 1.1"),
        tamper,
        signed,
    };
    let mut files = Vec::new();
    for repository in [
        like_good("good", None, Signed::Good),
        Repository {
            dir: "older",
            sequence: 1,
            packages: vec![&base, &user],
            security: None,
            tamper: None,
            signed: Signed::Good,
        },
        Repository {
            dir: "same-sequence",
            sequence: 2,
            packages: vec![&base, &user],
            security: None,
            tamper: None,
            signed: Signed::Good,
        },
        like_good("bad-digest", Some("pkg-base 1.1"), Signed::Good),
        like_good("bad-signature", None, Signed::Badly("pkg-base 1.1")),
        like_good("unsigned", None, Signed::Not),
        Repository {
            dir: "missing",
            sequence: 3,
            packages: vec![&base, &fixed, &user2, &user3, &lib],
            security: Some("pkg-base 1.1"),
            tamper: None,
            signed: Signed::Good,
        },
    ] {
        files.extend(repository_files(arch, &repository, signer)?);
    }
    // `changed-index`: `missing` as index 4, signed, then one byte of its
    // advisory changed. The length, the sequence and the syntax are kept,
    // so only the signature's digest of the index can tell.
    let changed = Repository {
        dir: "changed-index",
        sequence: 4,
        packages: vec![&base, &fixed, &user2, &user3, &lib],
        security: Some("pkg-base 1.1"),
        tamper: None,
        signed: Signed::Good,
    };
    for mut file in repository_files(arch, &changed, signer)? {
        if file.path.ends_with("/index.toml")
            && let Content::Bytes(bytes) = &mut file.content
        {
            let text = String::from_utf8_lossy(bytes).replace("FXSA-2026-0001", "FXSA-2026-0002");
            *bytes = text.into_bytes();
        }
        files.push(file);
    }
    Ok(files)
}

/// What the boot's shell runs. Each check echoes `pkg-test: ok <what>`.
const SCRIPT: &str = r#"PATH=/bin:/sbin:/usr/bin
export PATH
P=/var/lib/pkg-test
echo "pkg-test: started"
pkg install $P/statd.fxpkg && echo "pkg-test: ok installed"
pkg list | while read -r line; do case "$line" in statd*) echo "pkg-test: ok listed";; esac; done
pkg info statd | while read -r line; do case "$line" in '/sbin/ferrix-statd '*) echo "pkg-test: ok info";; esac; done
ferrix-statd --seconds 1 | while read -r line; do case "$line" in FERRIX-STAT-END*) echo "pkg-test: ok ran";; esac; done
pkg remove statd && echo "pkg-test: ok removed"
ferrix-statd --seconds 1 || echo "pkg-test: ok gone"
pkg list | while read -r line; do case "$line" in statd*) echo "pkg-test: still listed";; esac; done
pkg install $P/changed.fxpkg || echo "pkg-test: ok refused a changed file"
read -r line < /bin/pkg-base || echo "pkg-test: ok nothing of it went in"
pkg install $P/user.fxpkg || echo "pkg-test: ok refused a missing dependency"
pkg install $P/user.fxpkg $P/base.fxpkg && echo "pkg-test: ok installed together"
read -r line < /bin/pkg-user && case "$line" in 'pkg-user: here') echo "pkg-test: ok user is there";; esac
pkg install $P/base2.fxpkg || echo "pkg-test: ok refused an installed package without --replace"
pkg install --replace $P/changed2.fxpkg || echo "pkg-test: ok refused a changed replacement"
read -r line < /bin/pkg-base && case "$line" in 'pkg-base: here') echo "pkg-test: ok the old version stayed";; esac
pkg install --replace $P/base2.fxpkg && echo "pkg-test: ok replaced"
read -r line < /bin/pkg-base && case "$line" in 'pkg-base: two') echo "pkg-test: ok the new version is there";; esac
read -r line < /bin/pkg-base-two && echo "pkg-test: ok its new file is there"
pkg list | while read -r line; do case "$line" in 'pkg-base '*' 2.0 '*) echo "pkg-test: ok listed as 2.0";; esac; done
read -r line < /bin/pkg-user && echo "pkg-test: ok what depends on it stayed"
pkg install --replace $P/base.fxpkg && echo "pkg-test: ok replaced back"
read -r line < /bin/pkg-base-two || echo "pkg-test: ok the newer version's own file went with it"
pkg remove pkg-base || echo "pkg-test: ok refused removing a dependency"
pkg remove pkg-user && pkg remove pkg-base && echo "pkg-test: ok removed both"
R=/srv/pkg-repo
pkg install $P/base.fxpkg $P/user.fxpkg && echo "pkg-test: ok installed for the repository"
pkg newer || echo "pkg-test: ok newer wants an accepted index"
pkg update --repo $R/good && echo "pkg-test: ok accepted index 2"
pkg newer | while read -r line; do case "$line" in 'pkg-base 1.0 -> 1.1  (security)') echo "pkg-test: ok newer named the security fix";; 'security fix in 1.1: FXSA-2026-0001: '*) echo "pkg-test: ok newer gave its advisory";; 'pkg-user 1.0 -> 2.0') echo "pkg-test: ok newer named pkg-user 2.0";; esac; done
pkg update --repo $R/older 2>&1 | while read -r line; do case "$line" in *'index: it is older than the one accepted (BelowFloor): refused') echo "pkg-test: ok refused an older index";; esac; done
pkg update --repo $R/same-sequence 2>&1 | while read -r line; do case "$line" in *'index 2 is not the index 2 accepted before: refused') echo "pkg-test: ok refused another index 2";; esac; done
pkg upgrade --repo $R/bad-digest 2>&1 | while read -r line; do case "$line" in *'its SHA-256 differs): refused') echo "pkg-test: ok refused a wrong digest";; esac; done
pkg update --repo $R/bad-signature 2>&1 | while read -r line; do case "$line" in *'index: its signature does not verify (BadSignature): refused') echo "pkg-test: ok refused an index signed by another key";; esac; done
pkg update --repo $R/unsigned 2>&1 | while read -r line; do case "$line" in *'index has no signature, index.toml.fxsig: refused') echo "pkg-test: ok refused an unsigned index";; esac; done
pkg upgrade --repo $R/bad-signature 2>&1 | while read -r line; do case "$line" in *'pkg-base 1.1: it is signed for something else (WrongSubject): refused') echo "pkg-test: ok refused another package's signature";; esac; done
pkg upgrade --repo $R/unsigned 2>&1 | while read -r line; do case "$line" in *'pkg-base 1.1 has no signature, '*) echo "pkg-test: ok refused an unsigned package";; esac; done
read -r line < /bin/pkg-base && case "$line" in 'pkg-base: here') echo "pkg-test: ok the refused upgrade changed nothing";; esac
read -r line < /bin/pkg-lib || echo "pkg-test: ok nothing it needed went in"
pkg upgrade --repo $R/good && echo "pkg-test: ok upgraded"
read -r line < /bin/pkg-base && case "$line" in 'pkg-base: fixed') echo "pkg-test: ok the security fix is in";; esac
read -r line < /bin/pkg-user && case "$line" in 'pkg-user: two') echo "pkg-test: ok pkg-user is 2.0";; esac
read -r line < /bin/pkg-lib && echo "pkg-test: ok what it needs came with it"
pkg newer | while read -r line; do case "$line" in 'pkg: every package is the newest of index 2') echo "pkg-test: ok up to date";; esac; done
pkg update --repo $R/missing && echo "pkg-test: ok accepted index 3"
pkg upgrade --repo $R/missing 2>&1 | while read -r line; do case "$line" in *'pkg-user 3.0 needs pkg-gone, which neither this root nor the repository has') echo "pkg-test: ok refused a dependency nobody has";; esac; done
read -r line < /bin/pkg-user && case "$line" in 'pkg-user: two') echo "pkg-test: ok pkg-user stayed 2.0";; esac
echo "pkg-user: in the way" > /bin/pkg-user.pkg-old
pkg install --replace $P/user.fxpkg 2>&1 | while read -r line; do case "$line" in *'/bin/pkg-user.pkg-old is in the way of /bin/pkg-user being set aside: refused, nothing changed') echo "pkg-test: ok refused a replacement with something in the way";; esac; done
read -r line < /bin/pkg-user && case "$line" in 'pkg-user: two') echo "pkg-test: ok the refused replacement changed nothing";; esac
echo "pkg transaction 1" > /var/lib/pkg/transaction
echo "swapping" >> /var/lib/pkg/transaction
echo "n bin/pkg-stray" >> /var/lib/pkg/transaction
echo "pkg-stray: half put down" > /bin/pkg-stray
pkg update --repo $R/missing | while read -r line; do case "$line" in 'pkg: undid a transaction that stopped before it was done') echo "pkg-test: ok pkg undid a stopped transaction";; esac; done
read -r line < /bin/pkg-stray || echo "pkg-test: ok what the stopped transaction put down is gone"
pkg update --repo $R/changed-index 2>&1 | while read -r line; do case "$line" in *'index: it is not the file its signature names (PayloadMismatch): refused') echo "pkg-test: ok refused an index changed under its signature";; esac; done
pkg newer | while read -r line; do case "$line" in *'(index 3)') echo "pkg-test: ok the changed index left index 3 accepted";; esac; done
"#;

/// The lines the boot must print, in any order.
const WANTED: [&str; 53] = [
    "pkg-test: started",
    "pkg-test: ok installed",
    "pkg-test: ok listed",
    "pkg-test: ok info",
    "pkg-test: ok ran",
    "pkg-test: ok removed",
    "pkg-test: ok gone",
    "pkg-test: ok refused a changed file",
    "pkg-test: ok nothing of it went in",
    "pkg-test: ok refused a missing dependency",
    "pkg-test: ok installed together",
    "pkg-test: ok user is there",
    "pkg-test: ok refused an installed package without --replace",
    "pkg-test: ok refused a changed replacement",
    "pkg-test: ok the old version stayed",
    "pkg-test: ok replaced",
    "pkg-test: ok the new version is there",
    "pkg-test: ok its new file is there",
    "pkg-test: ok listed as 2.0",
    "pkg-test: ok what depends on it stayed",
    "pkg-test: ok replaced back",
    "pkg-test: ok the newer version's own file went with it",
    "pkg-test: ok refused removing a dependency",
    "pkg-test: ok removed both",
    "pkg-test: ok installed for the repository",
    "pkg-test: ok newer wants an accepted index",
    "pkg-test: ok accepted index 2",
    "pkg-test: ok newer named the security fix",
    "pkg-test: ok newer gave its advisory",
    "pkg-test: ok newer named pkg-user 2.0",
    "pkg-test: ok refused an older index",
    "pkg-test: ok refused another index 2",
    "pkg-test: ok refused a wrong digest",
    "pkg-test: ok refused an index signed by another key",
    "pkg-test: ok refused an unsigned index",
    "pkg-test: ok refused another package's signature",
    "pkg-test: ok refused an unsigned package",
    "pkg-test: ok the refused upgrade changed nothing",
    "pkg-test: ok nothing it needed went in",
    "pkg-test: ok upgraded",
    "pkg-test: ok the security fix is in",
    "pkg-test: ok pkg-user is 2.0",
    "pkg-test: ok what it needs came with it",
    "pkg-test: ok up to date",
    "pkg-test: ok accepted index 3",
    "pkg-test: ok refused a dependency nobody has",
    "pkg-test: ok pkg-user stayed 2.0",
    "pkg-test: ok refused a replacement with something in the way",
    "pkg-test: ok the refused replacement changed nothing",
    "pkg-test: ok pkg undid a stopped transaction",
    "pkg-test: ok what the stopped transaction put down is gone",
    "pkg-test: ok refused an index changed under its signature",
    "pkg-test: ok the changed index left index 3 accepted",
];

/// The lines the boot must not print.
const UNWANTED: [&str; 1] = ["pkg-test: still listed"];

/// `cargo xtask test-pkg`: on each architecture, `pkg` installs, runs and
/// removes the stat service, and refuses what it must.
///
/// # Errors
///
/// A build that fails, or a line the boot did not print.
pub(crate) fn test_pkg(args: &Args) -> Result<()> {
    let mut signer = Signer::new()?;
    for arch in args.arches()? {
        let Some(program) = build_trusting(arch, Some(&signer.anchors()))? else {
            continue;
        };
        let statd = crate::apps::built_package("statd", arch, args.release)?
            .ok_or_else(|| Error::new(format!("the statd app is not built for {arch}")))?;
        let base = made("pkg-base", &[], arch)?;
        let changed = initramfs::with_files_changed(&base, |name, data| {
            Ok((name == "bin/pkg-base").then(|| [data, b"changed\n".as_slice()].concat()))
        })?;
        // pkg-base 2.0: its line changed and a file of its own added, and
        // a copy of it with one byte changed after its record was written.
        let base2 = made_at("pkg-base", "2.0", "two", Some("pkg-base-two"), &[], arch)?;
        let changed2 = initramfs::with_files_changed(&base2, |name, data| {
            Ok((name == "bin/pkg-base").then(|| [data, b"changed\n".as_slice()].concat()))
        })?;
        let package = |name: &str, bytes: Vec<u8>| File {
            path: format!("{PACKAGES}/{name}.fxpkg"),
            mode: 0o644,
            content: Content::Bytes(bytes),
        };
        let mut files = vec![
            File {
                path: PROGRAM.to_owned(),
                mode: 0o755,
                content: Content::Bytes(read(&program)?),
            },
            package("statd", read(&statd)?),
            package("base", base),
            package("changed", changed),
            package("user", made("pkg-user", &["pkg-base"], arch)?),
            package("base2", base2),
            package("changed2", changed2),
        ];
        files.extend(repositories(arch, &mut signer)?);
        let shell_program = zinc::built(arch)?.ok_or_else(|| {
            Error::new(format!(
                "zinc is not built for {arch}, and test-pkg runs its checks in it"
            ))
        })?;
        let script = format!("{SCRIPT}exit {}\n", shell::STATUS);
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &shell_program, &script)?;
        let natives = native::build(arch, args.release)?;
        let initramfs = initramfs::build(None, &natives, None, &files)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
        let printed = |want: &str| lines.iter().any(|line| line.trim_end().ends_with(want));
        let missing: Vec<&str> = WANTED.into_iter().filter(|want| !printed(want)).collect();
        let unwanted: Vec<&str> = UNWANTED.into_iter().filter(|line| printed(line)).collect();
        if !missing.is_empty() || !unwanted.is_empty() {
            let tail: Vec<&str> = lines
                .iter()
                .rev()
                .take(40)
                .rev()
                .map(String::as_str)
                .collect();
            return Err(Error::new(format!(
                "{arch}: test-pkg is missing [{}] and printed [{}]\n  The boot's last lines:\n    {}",
                missing.join(", "),
                unwanted.join(", "),
                tail.join("\n    ")
            )));
        }
        println!(
            "{arch}: pkg installed, ran and removed the stat service, replaced a package and back, \
             upgraded from a signed repository with its security fix, refused all sixteen, and undid a \
             stopped transaction"
        );
    }
    Ok(())
}

/// `test-pkg`, or `install-app`: the package manager's two commands.
///
/// # Errors
///
/// As [`test_pkg`] and [`install_app`].
pub(crate) fn run(command: &str, args: &Args) -> Result<()> {
    if command == "install-app" {
        install_app(args)
    } else {
        test_pkg(args)
    }
}

/// `cargo xtask install-app --app NAME [--to DIR]`: build each app named
/// for this machine and put it in the running system, each in the place of
/// the version installed (`docs/DEV-ON-FERRIX.md` §4.4, D5): what a developer
/// on Ferrix types between an edit and running the result.
///
/// It is `build-apps` and then `pkg install --replace`, with the `pkg` this
/// tree builds, so that the tree being worked on decides what a replacement
/// is. On Ferrix the root is `/`, and writing it is root's, as `pkg`'s is;
/// anywhere else the root has to be named with `--to`, since `/` there is
/// not a Ferrix system and an app's files have no business in it.
///
/// # Errors
///
/// No `--app`, a root that is not named off Ferrix, a build that fails, or a
/// refusal of `pkg`'s, which it prints.
pub(crate) fn install_app(args: &Args) -> Result<()> {
    if args.apps.is_empty() {
        return Err(Error::new("install-app needs the app: --app NAME"));
    }
    let arch = Arch::host();
    let root = install_root(args.to.as_deref(), on_ferrix())?;
    let program = build(arch)?.ok_or_else(|| Error::new(format!("pkg is not built for {arch}")))?;
    let mut packages = Vec::new();
    for name in &args.apps {
        let package = crate::apps::built_package(name, arch, args.release)?
            .ok_or_else(|| Error::new(format!("the app {name} is not built for {arch}")))?;
        packages.push(package);
    }
    let status = std::process::Command::new(&program)
        .args(pkg_arguments(&root, &packages))
        .status()
        .map_err(|error| Error::new(format!("running {}: {error}", program.display())))?;
    if !status.success() {
        return Err(Error::new(format!(
            "pkg did not replace {}: it said why above{}",
            args.apps.join(", "),
            if on_ferrix() && root == "/" {
                " (writing / is root's: `su -c 'cargo xtask install-app ...'`)"
            } else {
                ""
            }
        )));
    }
    Ok(())
}

/// The root [`install_app`] writes: `--to`'s, or `/` on Ferrix alone.
fn install_root(to: Option<&str>, on_ferrix: bool) -> Result<String> {
    match to {
        Some(dir) => Ok(dir.to_owned()),
        None if on_ferrix => Ok("/".to_owned()),
        None => Err(Error::new(
            "install-app puts an app into a running Ferrix; on another system name the root to \
             install into with --to DIR",
        )),
    }
}

/// `pkg`'s command line for [`install_app`].
fn pkg_arguments(root: &str, packages: &[PathBuf]) -> Vec<std::ffi::OsString> {
    let mut arguments: Vec<std::ffi::OsString> = vec![
        "--root".into(),
        root.into(),
        "install".into(),
        "--replace".into(),
    ];
    arguments.extend(packages.iter().map(|path| path.as_os_str().to_owned()));
    arguments
}

/// Whether this process runs on Ferrix, whose `uname -s` says so.
fn on_ferrix() -> bool {
    std::process::Command::new("uname")
        .arg("-s")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "Ferrix")
}

#[cfg(test)]
mod tests {
    use super::{SCRIPT, WANTED, install_root, pkg_arguments};

    #[test]
    fn install_app_writes_slash_on_ferrix_alone() {
        assert_eq!(install_root(None, true).unwrap(), "/");
        assert!(install_root(None, false).is_err());
        assert_eq!(install_root(Some("/tmp/r"), false).unwrap(), "/tmp/r");
    }

    #[test]
    fn install_app_asks_pkg_to_replace() {
        let arguments = pkg_arguments("/", &[std::path::PathBuf::from("/b/x.fxpkg")]);
        let words: Vec<&str> = arguments.iter().filter_map(|a| a.to_str()).collect();
        assert_eq!(words, ["--root", "/", "install", "--replace", "/b/x.fxpkg"]);
    }

    #[test]
    fn every_wanted_line_is_one_the_script_echoes() {
        for want in WANTED {
            assert!(SCRIPT.contains(&format!("\"{want}\"")), "{want}");
        }
    }
}
