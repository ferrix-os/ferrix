//! Apps: optional programs, each a folder of its own under `src/user/apps/`
//! (`docs/APPS.md`), which is the ferrix-os/apps repository checked out there
//! (`components.toml`).
//!
//! Nothing here names an app. Every folder in [`PLACES`] is one, described by
//! its `app.toml`, which `ferrix-pkg` reads; this module builds each into a
//! package, installs packages into the images a person runs, gates each in
//! `check`, and boots them all once in `test-apps`. Adding an app is adding a
//! folder, and the check that nothing outside the folder names it is here
//! too (`docs/APPS.md` §4, rule 1).

mod new;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) use new::new_app;

use ferrix_cpio::{Archive, FileType};
use ferrix_pkg::manifest::{self, Abi, Build, FileSpec, Recipe, Source};
use ferrix_pkg::plan::plan;
use ferrix_pkg::record::{self, Installed, Record};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, initramfs, native, ports, qemu, shell, zinc};

/// Where apps are, and where a new app goes, from the workspace's root: the
/// ferrix-os/apps repository's checkout (`components.toml`).
pub(crate) const PLACE: &str = "src/user/apps";

/// Every place apps are found, for a message and for [`discover`].
pub(crate) const PLACES: &[&str] = &[PLACE];

/// [`PLACES`], for a message.
fn places() -> String {
    PLACES.join(" or ")
}

/// An app's manifest, in its folder.
const MANIFEST: &str = "app.toml";

/// The lints every app is held to, passed to its clippy: an app's manifest
/// carries no table of its own (`docs/APPS.md` §4, rule 3). The workspace's
/// rules that matter in a program nobody supervises -- no panics by
/// `unwrap`, `expect`, indexing or `panic!`, every `unsafe` argued and
/// alone -- and documentation. The root's `.clippy.toml` still applies, so
/// tests may `expect`.
const LINTS: &[&str] = &[
    "-D",
    "warnings",
    "-D",
    "missing_docs",
    "-D",
    "unsafe_op_in_unsafe_fn",
    "-D",
    "clippy::unwrap_used",
    "-D",
    "clippy::expect_used",
    "-D",
    "clippy::panic",
    "-D",
    "clippy::indexing_slicing",
    "-D",
    "clippy::undocumented_unsafe_blocks",
    "-D",
    "clippy::missing_safety_doc",
    "-D",
    "clippy::multiple_unsafe_ops_per_block",
];

/// An app: its folder and what its manifest says.
#[derive(Debug, Clone)]
pub(crate) struct App {
    /// Its folder.
    pub(crate) dir: PathBuf,
    /// Its `app.toml`.
    pub(crate) recipe: Recipe,
}

impl App {
    fn name(&self) -> &str {
        &self.recipe.package.name
    }

    fn builds_for(&self, arch: Arch) -> bool {
        self.recipe
            .package
            .arches
            .iter()
            .any(|name| name == arch.name())
    }
}

/// Every app, by name.
///
/// # Errors
///
/// A folder in [`PLACES`] without an `app.toml`, a manifest that does not
/// read, or one whose name is not its folder's, or two apps of one name.
pub(crate) fn discover() -> Result<Vec<App>> {
    let mut apps: Vec<App> = Vec::new();
    for place in PLACES {
        let place = paths::workspace_root().join(place);
        let Ok(entries) = fs::read_dir(&place) else {
            continue;
        };
        apps.extend(discover_in(&place, entries)?);
    }
    if let Some(twice) = apps
        .iter()
        .enumerate()
        .find(|(index, app)| {
            apps.iter()
                .take(*index)
                .any(|other| other.name() == app.name())
        })
        .map(|(_, app)| app)
    {
        return Err(Error::new(format!(
            "two apps are named `{}`, in {}: one name is one app",
            twice.name(),
            places()
        )));
    }
    apps.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(in_build_order(apps))
}

/// The apps in one of [`PLACES`].
fn discover_in(place: &Path, entries: fs::ReadDir) -> Result<Vec<App>> {
    let mut apps = Vec::new();
    for entry in entries {
        let dir = entry
            .map_err(|error| Error::new(format!("{}: {error}", place.display())))?
            .path();
        // A hidden folder is the checkout's own -- `.git`, `.github` -- not an
        // app: the place is a repository of its own (`components.toml`).
        let hidden = dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if !dir.is_dir() || hidden {
            continue;
        }
        let path = dir.join(MANIFEST);
        let text = fs::read_to_string(&path).map_err(|error| {
            Error::new(format!(
                "{}: {error}; every folder in {} is an app, described by its {MANIFEST}",
                path.display(),
                places()
            ))
        })?;
        let recipe = manifest::recipe(&text)
            .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
        let folder = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if recipe.package.name != folder {
            return Err(Error::new(format!(
                "{}: the app is named `{}`, and its folder `{folder}`; they are one name",
                path.display(),
                recipe.package.name
            )));
        }
        apps.push(App { dir, recipe });
    }
    Ok(apps)
}

/// `apps` with each after the apps it depends on, and otherwise in the
/// order given: an app is built after what it links, as git after curl's
/// library. A dependency that is not an app, or a circle, is left for
/// `ferrix_pkg::plan` to refuse at install.
fn in_build_order(mut apps: Vec<App>) -> Vec<App> {
    let mut ordered: Vec<App> = Vec::with_capacity(apps.len());
    while !apps.is_empty() {
        let ready =
            apps.iter()
                .position(|app| {
                    app.recipe.package.depends.iter().all(|dependency| {
                        !apps.iter().any(|waiting| waiting.name() == dependency.name)
                    })
                })
                .unwrap_or(0);
        ordered.push(apps.remove(ready));
    }
    ordered
}

/// The folder of the app named `name`: for a system gate that builds an app
/// its own way (`test-badapple`'s negative control), and finds it by its
/// name, never its path (rule 1).
///
/// # Errors
///
/// No app of that name, or a manifest that does not read.
pub(crate) fn folder(name: &str) -> Result<PathBuf> {
    discover()?
        .into_iter()
        .find(|app| app.name() == name)
        .map(|app| app.dir)
        .ok_or_else(|| Error::new(format!("there is no app `{name}` in {}", places())))
}

/// The package of the app `name` for `arch`, built now, or `None` for an
/// architecture it is not built for: for a system test that installs a
/// package itself, as test-pkg installs the stat service's.
///
/// # Errors
///
/// No app of that name, or a build that fails.
pub(crate) fn built_package(name: &str, arch: Arch, release: bool) -> Result<Option<PathBuf>> {
    let app = discover()?
        .into_iter()
        .find(|app| app.name() == name)
        .ok_or_else(|| Error::new(format!("there is no app `{name}` in {}", places())))?;
    package(&app, arch, release, Fresh::Always)
}

/// `cargo xtask apps`: every app, and whether its manifest reads.
pub(crate) fn list() -> Result<()> {
    let apps = discover()?;
    for app in &apps {
        let package = &app.recipe.package;
        let image = if app.recipe.default {
            "default"
        } else {
            "opt-in"
        };
        println!(
            "{:<16} {:<10} {:<6} {:<7} {}  {}\n{:<16} licence: {}",
            package.name,
            package.version,
            package.abi.as_str(),
            image,
            package.arches.join(","),
            package.description,
            "",
            package.license
        );
    }
    println!("{} apps in {}", apps.len(), places());
    Ok(())
}

/// Every `--app` names a folder in [`PLACES`].
fn every_app_named_is_one(apps: &[App], args: &Args) -> Result<()> {
    match args
        .apps
        .iter()
        .find(|name| !apps.iter().any(|app| app.name() == name.as_str()))
    {
        Some(unknown) => Err(Error::new(format!(
            "--app {unknown}: there is no app of that name in {}",
            places()
        ))),
        None => Ok(()),
    }
}

/// The apps an image a person runs carries: the `default` ones and every
/// `--app`, or none with `--no-apps`; under `--everything`, every app; with
/// `defaults` false, only the `--app` ones.
fn selected(args: &Args, defaults: bool) -> Result<Vec<App>> {
    let apps = discover()?;
    every_app_named_is_one(&apps, args)?;
    if args.no_apps {
        return Ok(Vec::new());
    }
    Ok(apps
        .into_iter()
        .filter(|app| {
            (defaults && (app.recipe.default || args.everything))
                || args.apps.iter().any(|name| name == app.name())
        })
        .collect())
}

/// The files of the apps `args` select, built for `arch`, as an image a
/// person runs carries them: each app's package, installed.
///
/// # Errors
///
/// A build that fails, or a set of packages that does not install.
pub(crate) fn installed(arch: Arch, args: &Args) -> Result<Vec<ports::File>> {
    install_selected(arch, args, true)
}

/// [`installed`], but only the apps `--app` names: for an image a test
/// boots too, which carries no app it was not asked for.
///
/// # Errors
///
/// As [`installed`].
pub(crate) fn named(arch: Arch, args: &Args) -> Result<Vec<ports::File>> {
    install_selected(arch, args, false)
}

/// The script apps built for `arch` -- the ported programs, curl and git
/// and the rest -- from the packages built last, less any `--app` names:
/// what a test's image carries beside its program, as it carried every port
/// that was built. None is built here; one that is not built, or whose
/// dependencies are not, is left out with a line saying so.
///
/// # Errors
///
/// A package that does not read, or a set that does not install.
pub(crate) fn ported(arch: Arch, args: &Args) -> Result<Vec<ports::File>> {
    let apps: Vec<App> = discover()?
        .into_iter()
        .filter(|app| {
            app.recipe.build == Build::Script
                && app.builds_for(arch)
                && !args.apps.iter().any(|name| name == app.name())
        })
        .collect();
    let mut built: Vec<&App> = Vec::new();
    let mut packages = Vec::new();
    for app in &apps {
        let path = package_path(app, arch);
        let missing = app
            .recipe
            .package
            .depends
            .iter()
            .find(|dependency| !built.iter().any(|done| done.name() == dependency.name));
        match (path.is_file(), missing) {
            (true, None) => {
                packages.push(read(&path)?);
                built.push(app);
            }
            (true, Some(dependency)) => println!(
                "  {} is left out: it needs {}, which is not built for {arch}",
                app.name(),
                dependency.name
            ),
            (false, _) => println!(
                "  {} is not built for {arch}: `cargo xtask build-apps --arch {arch} --app {}`",
                app.name(),
                app.name()
            ),
        }
    }
    install(&packages)
}

/// The apps `names`, from the packages built last for `arch`, installed
/// together: for a test that boots a program an app is, as test-audio boots
/// ALSA's, given every app it needs. One that is not built is said and left
/// out, and the test finds it missing.
///
/// # Errors
///
/// A name that is no app, a package that does not read, or a set that does
/// not install.
pub(crate) fn taken(arch: Arch, names: &[&str]) -> Result<Vec<ports::File>> {
    let apps = discover()?;
    let mut packages = Vec::new();
    for name in names {
        let app = apps
            .iter()
            .find(|app| app.name() == *name)
            .ok_or_else(|| Error::new(format!("there is no app `{name}` in {}", places())))?;
        if let Some(path) = package(app, arch, false, Fresh::UnlessScript)? {
            packages.push(read(&path)?);
        }
    }
    install(&packages)
}

fn install_selected(arch: Arch, args: &Args, defaults: bool) -> Result<Vec<ports::File>> {
    // `--everything` is everything: a script app with no package is built,
    // and a build that fails stops the run.
    let fresh = if args.everything {
        Fresh::UnlessBuilt
    } else {
        Fresh::UnlessScript
    };
    let mut packages = Vec::new();
    for app in selected(args, defaults)? {
        if let Some(path) = package(&app, arch, args.release, fresh)? {
            packages.push(read(&path)?);
        }
    }
    let files = install(&packages)?;
    if !packages.is_empty() {
        println!("  {} apps installed, {} files", packages.len(), files.len());
    }
    Ok(files)
}

/// Whether a package is built for the asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fresh {
    /// Always: `build-apps`.
    Always,
    /// Unless the app is built by a script, which is a download and minutes
    /// of C that no image starts on its own, as no image starts a port: the
    /// last package built is taken, and its absence said.
    UnlessScript,
    /// As [`Fresh::UnlessScript`], but a script app with no package yet is
    /// built: `--everything`, which leaves nothing out.
    UnlessBuilt,
}

/// Where `app`'s package for `arch` is written.
fn package_path(app: &App, arch: Arch) -> PathBuf {
    let name = app.name();
    paths::target_dir()
        .join("apps")
        .join(name)
        .join(arch.name())
        .join(format!(
            "{name}-{}-{}.fxpkg",
            app.recipe.package.version,
            arch.name()
        ))
}

/// Build `app` for `arch` and write its package, or `None` for an
/// architecture it is not built for or a build this host cannot make.
///
/// # Errors
///
/// A build that fails, a file its manifest names that the build did not
/// make, or a native program the kernel could not start.
pub(crate) fn package(
    app: &App,
    arch: Arch,
    release: bool,
    fresh: Fresh,
) -> Result<Option<PathBuf>> {
    let name = app.name();
    if !app.builds_for(arch) {
        println!("  {name} is not built for {arch}");
        return Ok(None);
    }
    let out = package_path(app, arch);
    if fresh != Fresh::Always && app.recipe.build == Build::Script {
        if out.is_file() {
            println!("  {name}: the package built last, {}", out.display());
            restamp(app, arch, &out)?;
            return Ok(Some(out));
        }
        if fresh == Fresh::UnlessScript {
            println!(
                "  {name} is not built for {arch}: `cargo xtask build-apps --arch {arch} --app {name}`"
            );
            return Ok(None);
        }
    }
    let Some(built) = build(app, arch, release)? else {
        return Ok(None);
    };
    write(&out, &packed(app, arch, built)?)?;
    Ok(Some(out))
}

/// What a record says of an entry: a directory, which it does not list,
/// `None`.
fn recorded(entry: &ports::File) -> Option<Installed> {
    match &entry.content {
        ports::Content::Bytes(bytes) => Some(Installed::of(&entry.path, entry.mode, bytes)),
        ports::Content::Link(target) => Some(Installed::link(&entry.path, target)),
        ports::Content::Directory => None,
    }
}

/// The target a Linux app is built for on `arch`: the target's own musl,
/// and on ARMv7-A its hard-float ABI, as the compositor and its clients are,
/// so a program the desktop carries and the same app installed are one
/// binary.
pub(crate) fn linux_target(arch: Arch) -> Option<&'static str> {
    crate::display::target(arch)
}

/// The flags a Linux app is built with on `arch`: zinc's static, fixed
/// address executable, and on ARMv7-A the Cortex-A7 the boards have, with
/// its NEON.
fn linux_rustflags(arch: Arch) -> String {
    if arch == Arch::Armv7a {
        format!("{} -C target-cpu=cortex-a7", zinc::RUSTFLAGS)
    } else {
        zinc::RUSTFLAGS.to_owned()
    }
}

/// `cargo build --release` of a Linux app for `arch` with `selection`
/// (`--bins`, or `--bin` and a name), into its own target directory,
/// requiring `outputs` of it; where they are, or `None` on an architecture
/// with no target.
fn cargo_linux(
    app: &App,
    arch: Arch,
    outputs: &[String],
    selection: &[&str],
) -> Result<Option<PathBuf>> {
    let name = app.name();
    let Some(target) = linux_target(arch) else {
        println!("  {name} is not built for {arch} yet: there is no musl target");
        return Ok(None);
    };
    let target_dir = paths::target_dir().join("apps").join(name);
    let out = target_dir.join(target).join("release");
    let mut build =
        crate::builds::Build::cargo(format!("cargo build ({name}) --target {target}"), &app.dir)
            .args(["build", "--release", "--target", target])
            .args(selection)
            .env("CARGO_TARGET_DIR", &target_dir)
            // For zinc's reason: RUSTFLAGS replaces the flags every config file up
            // the tree would otherwise merge in.
            .env("RUSTFLAGS", linux_rustflags(arch))
            // And the linker, so an app needs no `.cargo/config.toml` of its own:
            // the musl targets carry their C runtime, so rust-lld is the whole
            // toolchain on any host.
            .env(&linker_variable(target), "rust-lld");
    for output in outputs {
        build = build.output(out.join(output));
    }
    println!("  building {name} for {target}");
    build.run()?;
    Ok(Some(out))
}

/// One program of the Linux app `name`, built for `arch` from its folder,
/// and where it is: for what carries a program the system needs whether or
/// not the app is installed -- the desktop's clients, the gates that boot
/// them.
///
/// # Errors
///
/// No app of that name, an architecture it has no target on, or a build
/// that fails.
pub(crate) fn program(arch: Arch, name: &str, binary: &str) -> Result<PathBuf> {
    let app = discover()?
        .into_iter()
        .find(|app| app.name() == name)
        .ok_or_else(|| Error::new(format!("there is no app `{name}` in {}", places())))?;
    let out = cargo_linux(&app, arch, &[binary.to_owned()], &["--bin", binary])?
        .ok_or_else(|| Error::new(format!("{name} has no build for {arch}")))?;
    Ok(out.join(binary))
}

/// `built`, the files of `app`'s package for `arch`, with its record made
/// from the `[package]` its `app.toml` has now, as a package archive.
fn packed(app: &App, arch: Arch, built: Vec<ports::File>) -> Result<Vec<u8>> {
    let mut package = app.recipe.package.clone();
    package.arches = vec![arch.name().to_owned()];
    let record = Record {
        package,
        files: built.iter().filter_map(recorded).collect(),
    };
    let mut entries = built;
    entries.push(ports::File {
        path: record::path(app.name()),
        mode: 0o644,
        content: ports::Content::Bytes(record::render(&record).into_bytes()),
    });
    initramfs::package(&entries)
}

/// Bring the record in a script app's last package to its `app.toml` as it
/// is now: that package is taken without building again, so a change to
/// `[package]` -- a licence, a description -- would otherwise never reach
/// it, and a record a newer reader refuses would stop every image that
/// installs it. The files are the package's own; only the record is made
/// again, and only when it differs.
fn restamp(app: &App, arch: Arch, out: &Path) -> Result<()> {
    let bytes = read(out)?;
    let mut wanted = app.recipe.package.clone();
    wanted.arches = vec![arch.name().to_owned()];
    if package_record(&bytes).is_ok_and(|record| record.package == wanted) {
        return Ok(());
    }
    let record_path = record::path(app.name());
    let mut files = Vec::new();
    for entry in Archive::new(&bytes).entries() {
        let entry = entry.map_err(|error| Error::new(format!("{}: {error:?}", out.display())))?;
        if entry.name == record_path {
            continue;
        }
        let content = match entry.file_type() {
            FileType::Regular => ports::Content::Bytes(entry.data.to_vec()),
            FileType::Symlink => ports::Content::Link(
                entry
                    .symlink_target()
                    .ok_or_else(|| Error::new(format!("{}: a link is not text", entry.name)))?
                    .to_owned(),
            ),
            FileType::Directory => ports::Content::Directory,
            _ => continue,
        };
        files.push(ports::File {
            path: entry.name.to_owned(),
            mode: entry.mode & 0o7777,
            content,
        });
    }
    println!("  {}: its record made again from app.toml", app.name());
    write(out, &packed(app, arch, files)?)
}

/// `cargo xtask build-apps`: every app's package, or `--app`'s, built for
/// each `--arch`, scripts too.
///
/// # Errors
///
/// A build that fails, or an `--app` there is no folder for.
pub(crate) fn build_apps(args: &Args) -> Result<()> {
    let apps = discover()?;
    every_app_named_is_one(&apps, args)?;
    let wanted: Vec<&App> = apps
        .iter()
        .filter(|app| args.apps.is_empty() || args.apps.iter().any(|name| name == app.name()))
        .collect();
    for arch in args.arches()? {
        for app in &wanted {
            if let Some(path) = package(app, arch, args.release, Fresh::Always)? {
                println!("{}", path.display());
            }
        }
    }
    Ok(())
}

/// What a build put where its manifest says: files, links and a tree's
/// directories, each at its path from the root.
type Built = Vec<ports::File>;

/// Each file `app`'s manifest installs, with its bytes, built for `arch`.
fn build(app: &App, arch: Arch, release: bool) -> Result<Option<Built>> {
    let name = app.name();
    let target_dir = paths::target_dir().join("apps").join(name);
    let out = match (app.recipe.build, app.recipe.package.abi) {
        (Build::Cargo, Abi::Native) => {
            let target = arch.kernel_target();
            let profile = if release { "release" } else { "debug" };
            let out = target_dir.join(target).join(profile);
            let mut build = crate::builds::Build::cargo(
                format!("cargo build ({name}) --target {target}"),
                &app.dir,
            )
            .args(["build", "--bins", "--target", target])
            .env("CARGO_TARGET_DIR", &target_dir);
            if release {
                build = build.args(["--release"]);
            }
            for spec in built_by_it(app) {
                build = build.output(out.join(&spec.from));
            }
            println!("  building {name} for {target}");
            build.run()?;
            out
        }
        (Build::Cargo, Abi::Linux) => {
            let outputs: Vec<String> = built_by_it(app).map(|spec| spec.from.clone()).collect();
            match cargo_linux(app, arch, &outputs, &["--bins"])? {
                Some(out) => out,
                None => return Ok(None),
            }
        }
        (Build::Script, _) => {
            let out = target_dir.join(arch.name()).join("out");
            fs::create_dir_all(&out)
                .map_err(|error| Error::new(format!("{}: {error}", out.display())))?;
            if cfg!(windows) {
                // In WSL, as ferrousli's shared library is built, writing
                // into this checkout's target directory through `/mnt`, and
                // building ferrousli into the target directory `cargo xtask
                // ports` builds it into, one for every app.
                crate::wsl::require_toolchain(&format!(
                    "{name} is built by its build.sh with a Linux host's tools"
                ))?;
                let status = crate::wsl::bash_building(
                    &app.dir,
                    &paths::workspace_root().join("src/user/system/linux/ferrousli"),
                    "exec bash build.sh \"$1\" \"$(wslpath -u \"$2\")\"",
                    &[arch.name(), &out.to_string_lossy()],
                )
                .stdin(std::process::Stdio::null())
                .status()
                .map_err(|error| Error::new(format!("could not run wsl.exe: {error}")))?;
                if !status.success() {
                    return Err(Error::new(format!(
                        "{name}'s build.sh {arch} in WSL: {status}"
                    )));
                }
            } else {
                let mut command = Command::new("bash");
                let _ = command
                    .current_dir(&app.dir)
                    .arg("build.sh")
                    .arg(arch.name())
                    .arg(&out);
                cargo::run(command, &format!("{name}'s build.sh"))?;
            }
            out
        }
    };
    gathered(app, arch, &out).map(Some)
}

/// What `app`'s manifest installs, from the build's output `out` and the
/// app's folder, each native program held to what the kernel starts.
fn gathered(app: &App, arch: Arch, out: &Path) -> Result<Built> {
    let name = app.name();
    let mut files = Vec::new();
    for spec in &app.recipe.files {
        let from = match spec.source {
            Source::Build => out.join(&spec.from),
            Source::Folder => app.dir.join(&spec.from),
        };
        // A file is taken as it is, a symbolic link as a link (git's
        // `bin/git`); a tree, with everything in it.
        ports::read_entry(&from, &spec.to, spec.mode, spec.tree, &mut files)?;
    }
    if app.recipe.package.abi == Abi::Native {
        for file in &files {
            if let ports::Content::Bytes(bytes) = &file.content
                && bytes.starts_with(b"\x7fELF")
            {
                native::verify(arch, bytes).map_err(|why| {
                    Error::new(format!(
                        "{name}'s {} for {arch} is not a program the kernel can start: {why}",
                        file.path
                    ))
                })?;
            }
        }
    }
    Ok(files)
}

/// The files of `app`'s manifest its build makes: not the ones kept in its
/// folder.
fn built_by_it(app: &App) -> impl Iterator<Item = &FileSpec> {
    app.recipe
        .files
        .iter()
        .filter(|spec| spec.source == Source::Build)
}

/// Install `packages` into a root: what an image carries, as `ports` files.
///
/// Read back out of each archive with the kernel's own cpio reader, as the
/// package manager will read them: every file must be its record's, with
/// the size and digest the record says, and the set must install whole
/// (`ferrix_pkg::plan`) before one file is taken.
fn install(packages: &[Vec<u8>]) -> Result<Vec<ports::File>> {
    let mut records = Vec::new();
    for bytes in packages {
        records.push(package_record(bytes)?);
    }
    let order = plan(&records).map_err(|error| Error::new(format!("apps: {error}")))?;
    let mut files = Vec::new();
    let mut directories = Vec::new();
    for at in order {
        let (Some(bytes), Some(record)) = (packages.get(at), records.get(at)) else {
            continue;
        };
        let name = &record.package.name;
        let record_path = record::path(name);
        for entry in Archive::new(bytes).entries() {
            let entry =
                entry.map_err(|error| Error::new(format!("{name}'s package: {error:?}")))?;
            let mode = entry.mode & 0o7777;
            let (found, content) = match entry.file_type() {
                FileType::Regular => (
                    Installed::of(entry.name, mode, entry.data),
                    ports::Content::Bytes(entry.data.to_vec()),
                ),
                FileType::Symlink => {
                    let target = entry.symlink_target().ok_or_else(|| {
                        Error::new(format!("{name}'s link {} is not text", entry.name))
                    })?;
                    (
                        Installed::link(entry.name, target),
                        ports::Content::Link(target.to_owned()),
                    )
                }
                FileType::Directory => {
                    directories.push(entry.name.to_owned());
                    continue;
                }
                _ => continue,
            };
            if entry.name != record_path {
                let listed = record
                    .files
                    .iter()
                    .find(|file| file.path == entry.name)
                    .ok_or_else(|| {
                        Error::new(format!(
                            "{name}'s package holds {}, which its record does not list",
                            entry.name
                        ))
                    })?;
                if *listed != found {
                    return Err(Error::new(format!(
                        "{name}'s package holds {} unlike its record says",
                        entry.name
                    )));
                }
            }
            files.push(ports::File {
                path: entry.name.to_owned(),
                mode,
                content,
            });
        }
    }
    // A directory nothing is in, such as one of git's templates, is made;
    // the others come with what is in them.
    for directory in directories {
        let inside = format!("{directory}/");
        if !files.iter().any(|file| file.path.starts_with(&inside)) {
            files.push(ports::File {
                path: directory,
                mode: manifest::TREE_MODE,
                content: ports::Content::Directory,
            });
        }
    }
    Ok(files)
}

/// The record inside a package.
fn package_record(bytes: &[u8]) -> Result<Record> {
    let prefix = format!("{}/", record::RECORDS);
    for entry in Archive::new(bytes).entries() {
        let entry = entry.map_err(|error| Error::new(format!("a package: {error:?}")))?;
        if entry.file_type() == FileType::Regular && entry.name.starts_with(&prefix) {
            let text = std::str::from_utf8(entry.data)
                .map_err(|_| Error::new(format!("{} is not text", entry.name)))?;
            return record::parse(text)
                .map_err(|error| Error::new(format!("{}: {error}", entry.name)));
        }
    }
    Err(Error::new("a package without a record"))
}

/// Cargo's variable for `target`'s linker: `CARGO_TARGET_<TRIPLE>_LINKER`.
pub(crate) fn linker_variable(target: &str) -> String {
    format!(
        "CARGO_TARGET_{}_LINKER",
        target.to_ascii_uppercase().replace('-', "_")
    )
}

/// `cargo` in `app`'s folder, into the app's own target directory under
/// the tree's, where its builds go and CI's cache finds it: on Windows, a
/// Linux app's through WSL, which keeps its own.
fn cargo_in(app: &App, arguments: &[&str]) -> Command {
    if cfg!(windows) && app.recipe.package.abi == Abi::Linux {
        return crate::wsl::cargo(&app.dir, arguments);
    }
    let mut command = Command::new(cargo::cargo());
    let _ = command.current_dir(&app.dir).args(arguments).env(
        "CARGO_TARGET_DIR",
        paths::target_dir().join("apps").join(app.name()),
    );
    command
}

/// Rule 1: nothing outside `app`'s folder names it -- in this tree, and in
/// the repository the app lives in when that is a component of its own.
pub(crate) fn stays_in_its_folder(app: &App) -> Result<()> {
    let root = paths::workspace_root();
    let relative = |base: &Path| {
        app.dir
            .strip_prefix(base)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .map_err(|_| {
                Error::new(format!(
                    "{} is outside {}",
                    app.dir.display(),
                    base.display()
                ))
            })
    };
    let in_tree = relative(&root)?;
    names_only_itself(&root, &[in_tree.as_str()], &in_tree)?;
    let toplevel = Command::new("git")
        .current_dir(&app.dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| Error::new(format!("could not run git rev-parse: {error}")))?;
    let toplevel = PathBuf::from(String::from_utf8_lossy(&toplevel.stdout).trim());
    if toplevel.as_os_str().is_empty() || same_dir(&toplevel, &root) {
        return Ok(());
    }
    // In the component's own repository the folder's path is its bare name,
    // `curl` or `git`, which ordinary words match; what names the folder
    // there is the tree's path to it, or a sibling's `../<name>/`.
    let folder = relative(&toplevel)?;
    let sibling = format!("../{folder}/");
    names_only_itself(&toplevel, &[in_tree.as_str(), sibling.as_str()], &folder)
}

/// Whether two paths are one directory, however each is spelled.
fn same_dir(one: &Path, other: &Path) -> bool {
    match (one.canonicalize(), other.canonicalize()) {
        (Ok(one), Ok(other)) => one == other,
        _ => one == other,
    }
}

/// `git grep` in `repository` for any of `needles`, outside `folder`.
fn names_only_itself(repository: &Path, needles: &[&str], folder: &str) -> Result<()> {
    let mut grep = Command::new("git");
    let _ = grep
        .current_dir(repository)
        .args(["grep", "--untracked", "-l", "-F"]);
    for needle in needles {
        let _ = grep.args(["-e", needle]);
    }
    let output = grep
        .args(["--", "."])
        .arg(format!(":(exclude){folder}"))
        .output()
        .map_err(|error| Error::new(format!("could not run git grep: {error}")))?;
    match output.status.code() {
        Some(1) => Ok(()),
        Some(0) => Err(Error::new(format!(
            "these files name {folder}, and an app changes nothing outside its folder \
             (docs/APPS.md §4):\n{}",
            String::from_utf8_lossy(&output.stdout)
        ))),
        _ => Err(Error::new(format!(
            "git grep failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))),
    }
}

/// `cargo fmt --check` in `app`'s folder; for an app built by a script,
/// `bash -n` over the script, which is what can be said of it without
/// running it.
pub(crate) fn formatting(app: &App) -> Result<()> {
    if app.recipe.build == Build::Script {
        let mut command = Command::new("bash");
        let _ = command.current_dir(&app.dir).args(["-n", "build.sh"]);
        return cargo::run(command, &format!("{}: bash -n build.sh", app.name()));
    }
    cargo::run(
        cargo_in(app, &["fmt", "--check"]),
        &format!("{}: cargo fmt", app.name()),
    )
}

/// The host's clippy and tests over `app`'s lib target, when its manifest
/// asks for them.
pub(crate) fn host(app: &App) -> Result<()> {
    if !app.recipe.host_tests {
        println!("  {} has no host tests", app.name());
        return Ok(());
    }
    let mut clippy = vec!["clippy", "--lib", "--tests", "--"];
    clippy.extend(LINTS);
    cargo::run(
        cargo_in(app, &clippy),
        &format!("{}: cargo clippy", app.name()),
    )?;
    // The lib's tests and the integration tests beside it in `tests/`.
    cargo::run(
        cargo_in(app, &["test", "--lib", "--tests"]),
        &format!("{}: cargo test", app.name()),
    )
}

/// Clippy over `app`'s programs for each target it is built for: none for
/// an app built by a script, which has no cargo workspace.
pub(crate) fn targets(app: &App) -> Result<()> {
    if app.recipe.build == Build::Script {
        println!(
            "  {} is built by its build.sh: nothing for clippy",
            app.name()
        );
        return Ok(());
    }
    for arch in Arch::ALL {
        if !app.builds_for(arch) {
            continue;
        }
        let target = match app.recipe.package.abi {
            Abi::Native => arch.kernel_target(),
            Abi::Linux => match linux_target(arch) {
                Some(target) => target,
                None => continue,
            },
        };
        let mut clippy = vec!["clippy", "--bins", "--target", target, "--"];
        clippy.extend(LINTS);
        let mut command = cargo_in(app, &clippy);
        if app.recipe.package.abi == Abi::Linux {
            let _ = command.env("RUSTFLAGS", linux_rustflags(arch));
        }
        cargo::run(
            command,
            &format!("{}: cargo clippy --target {target}", app.name()),
        )?;
    }
    Ok(())
}

/// What `test-apps` prints before the apps run.
const STARTED: &str = "apps: started";

/// The line an app's `index`th smoke check prints when it passes.
fn passed(name: &str, index: usize) -> String {
    format!("apps: ok {name} {index}")
}

/// `text` quoted for the shell, whole.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The script `test-apps`' shell runs: each smoke command, its output read
/// line by line for the one it expects.
fn script(apps: &[App]) -> String {
    let mut script = format!("PATH=/bin:/sbin:/usr/bin\nexport PATH\necho \"{STARTED}\"\n");
    for app in apps {
        for (index, smoke) in app.recipe.smoke.iter().enumerate() {
            script.push_str(&format!(
                "{} | while read -r line; do case \"$line\" in {}*) echo \"{}\";; esac; done\n",
                smoke.run,
                quoted(&smoke.expect),
                passed(app.name(), index)
            ));
        }
    }
    script.push_str(&format!("exit {}\n", shell::STATUS));
    script
}

/// The apps of `apps` that `wanted` picks, and every app those depend on,
/// in `apps`' order.
fn with_dependencies(apps: Vec<App>, wanted: impl Fn(&App) -> bool) -> Vec<App> {
    let mut names: Vec<String> = apps
        .iter()
        .filter(|app| wanted(app))
        .map(|app| app.name().to_owned())
        .collect();
    let mut at = 0;
    while let Some(name) = names.get(at).cloned() {
        at += 1;
        let depends = apps
            .iter()
            .filter(|app| app.name() == name)
            .flat_map(|app| app.recipe.package.depends.iter());
        for dependency in depends {
            if !names.contains(&dependency.name) {
                names.push(dependency.name.clone());
            }
        }
    }
    apps.into_iter()
        .filter(|app| names.iter().any(|name| name == app.name()))
        .collect()
}

/// `cargo xtask test-apps`: one boot that runs every app's smoke checks.
///
/// # Errors
///
/// A build that fails, or a check whose line did not come.
pub(crate) fn test_apps(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let apps = with_dependencies(
            discover()?
                .into_iter()
                .filter(|app| app.builds_for(arch))
                .collect(),
            |app| !app.recipe.smoke.is_empty(),
        );
        let mut packages = Vec::new();
        let mut tested = Vec::new();
        for app in apps {
            if let Some(path) = package(&app, arch, args.release, Fresh::UnlessBuilt)? {
                packages.push(read(&path)?);
                tested.push(app);
            }
        }
        if tested.is_empty() {
            println!("{arch}: no app has a smoke check");
            continue;
        }
        let files = install(&packages)?;
        let program = zinc::built(arch)?.ok_or_else(|| {
            Error::new(format!(
                "zinc is not built for {arch}, and test-apps runs its checks in it"
            ))
        })?;
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &program, &script(&tested))?;
        let natives = native::build(arch, args.release)?;
        let initramfs = initramfs::build(None, &natives, None, &files)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
        let mut missing: Vec<String> = Vec::new();
        let mut wanted = vec![STARTED.to_owned()];
        for app in &tested {
            wanted.extend((0..app.recipe.smoke.len()).map(|index| passed(app.name(), index)));
        }
        for want in wanted {
            if !lines.iter().any(|line| line.trim_end().ends_with(&want)) {
                missing.push(want);
            }
        }
        if !missing.is_empty() {
            let tail: Vec<&str> = lines
                .iter()
                .rev()
                .take(30)
                .rev()
                .map(String::as_str)
                .collect();
            return Err(Error::new(format!(
                "{arch}: test-apps is missing {}\n  The boot's last lines:\n    {}",
                missing.join(", "),
                tail.join("\n    ")
            )));
        }
        println!("{arch}: {} apps' smoke checks passed", tested.len());
    }
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| Error::new(format!("{}: {error}", parent.display())))?;
    }
    fs::write(path, bytes)
        .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::{App, install, quoted, script};
    use crate::ports;
    use ferrix_pkg::manifest;
    use ferrix_pkg::record::{self, Installed, Record};

    fn app(name: &str, smoke: &str) -> App {
        let text = format!(
            "[package]\nname = \"{name}\"\nversion = \"1.0\"\ndescription = \"\"\nlicense = \"MIT\"\nabi = \"native\"\n\
             arches = [\"x86_64\"]\n\n[[package.files]]\nfrom = \"{name}\"\nto = \"bin/{name}\"\nmode = \"755\"\n{smoke}"
        );
        App {
            dir: std::path::PathBuf::new(),
            recipe: manifest::recipe(&text).expect("a manifest"),
        }
    }

    /// [`app`], depending on `depends`.
    fn depending(name: &str, depends: &[&str]) -> App {
        let mut app = app(name, "");
        app.recipe.package.depends = depends
            .iter()
            .map(|name| manifest::Dependency::parse(name).expect("a dependency"))
            .collect();
        app
    }

    #[test]
    fn apps_are_built_after_what_they_depend_on() {
        let apps = vec![
            depending("alsa-utils", &["alsa-lib"]),
            depending("alsa-lib", &[]),
            depending("btop", &[]),
            depending("git", &["curl", "zlib"]),
            depending("curl", &[]),
        ];
        let order: Vec<String> = super::in_build_order(apps)
            .iter()
            .map(|app| app.name().to_owned())
            .collect();
        assert_eq!(order, ["alsa-lib", "alsa-utils", "btop", "curl", "git"]);
        // A circle is left as it is, for the install's plan to refuse.
        let circle = vec![depending("a", &["b"]), depending("b", &["a"])];
        assert_eq!(super::in_build_order(circle).len(), 2);
    }

    #[test]
    fn the_real_apps_read_and_stay_in_their_folders() {
        for app in super::discover().expect("every app's manifest reads") {
            super::stays_in_its_folder(&app).expect("nothing outside the folder names it");
        }
    }

    #[test]
    fn every_launcher_entry_starts_what_its_package_installs() {
        let mut entries = 0;
        for app in super::discover().expect("every app's manifest reads") {
            for spec in app
                .recipe
                .files
                .iter()
                .filter(|spec| spec.to.starts_with("usr/share/applications/"))
            {
                entries += 1;
                entry_is_its_package(&app, &spec.from);
            }
        }
        assert!(entries > 0, "an app ships an entry");
    }

    /// Every program the entry at `from` names is `app`'s, or the shell
    /// that holds a terminal open, and its icon is one `app` carries.
    fn entry_is_its_package(app: &App, from: &str) {
        let installs = |path: &str| app.recipe.files.iter().any(|spec| spec.to == path);
        let text =
            std::fs::read_to_string(app.dir.join(from)).expect("an entry its manifest names");
        let key = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .unwrap_or_else(|| panic!("{from}: no {name}"))
        };
        for program in key("Exec")
            .split([' ', '"', ';'])
            .filter_map(|word| word.strip_prefix('/'))
        {
            assert!(
                installs(program) || program == "bin/zinc",
                "{from}: {program} is not installed by {}",
                app.recipe.package.name
            );
        }
        let icon = format!("usr/share/icons/hicolor/scalable/apps/{}.svg", key("Icon"));
        assert!(installs(&icon), "{from}: no {icon}");
    }

    #[test]
    fn everything_selects_every_app() {
        let every = super::discover().expect("every app's manifest reads").len();
        let mut args = crate::args::Args::default();
        let defaults = super::selected(&args, true).expect("the defaults").len();
        assert!(defaults < every, "an opt-in app to leave out");
        args.everything = true;
        assert_eq!(
            super::selected(&args, true).expect("every app").len(),
            every
        );
        assert!(super::selected(&args, false).expect("none").is_empty());
    }

    #[test]
    fn the_smoke_script_quotes_what_it_expects() {
        assert_eq!(quoted("it's"), "'it'\\''s'");
        let smoke = "\n[[smoke]]\nrun = \"a --version\"\nexpect = \"a 1.0 'x'\"\n";
        let text = script(&[app("a", smoke)]);
        assert!(text.contains(
            "a --version | while read -r line; do case \"$line\" in 'a 1.0 '\\''x'\\'''*) \
             echo \"apps: ok a 0\";; esac; done"
        ));
        assert!(text.starts_with("PATH=/bin:/sbin:/usr/bin\n"));
        assert!(text.ends_with("exit 7\n"));
    }

    /// A package of `name` holding `files`, with `listed` as its record's.
    fn package(name: &str, files: &[(&str, &[u8])], listed: &[(&str, &[u8])]) -> Vec<u8> {
        let mut package = app(name, "").recipe.package;
        package.arches = vec!["x86_64".to_owned()];
        let record = Record {
            package,
            files: listed
                .iter()
                .map(|(path, bytes)| Installed::of(path, 0o755, bytes))
                .collect(),
        };
        let text = record::render(&record);
        let path = record::path(name);
        let mut entries: Vec<(&str, u32, &[u8])> = files
            .iter()
            .map(|(path, bytes)| (*path, 0o755, *bytes))
            .collect();
        entries.push((&path, 0o644, text.as_bytes()));
        crate::initramfs::plain(
            &["bin", "lib", "lib/ferrix", "lib/ferrix/packages"],
            &entries,
        )
        .expect("an archive")
    }

    #[test]
    fn a_reused_package_gets_the_record_its_manifest_has_now() {
        // The package as an older xtask made it: a record with no licence,
        // which today's reader refuses.
        let mut package = app("a", "").recipe.package;
        package.arches = vec!["x86_64".to_owned()];
        let record = Record {
            package,
            files: vec![Installed::of("bin/a", 0o755, b"elf")],
        };
        let old = record::render(&record).replace("license = \"MIT\"\n", "");
        let path = record::path("a");
        let stale = crate::initramfs::plain(
            &["bin", "lib", "lib/ferrix", "lib/ferrix/packages"],
            &[("bin/a", 0o755, b"elf"), (&path, 0o644, old.as_bytes())],
        )
        .expect("an archive");
        let dir = std::env::temp_dir().join(format!("xtask-restamp-{}", std::process::id()));
        let out = dir.join("a.fxpkg");
        super::write(&out, &stale).expect("written");
        assert!(
            super::package_record(&stale).is_err(),
            "the stale record reads"
        );

        let app = app("a", "");
        super::restamp(&app, crate::paths::Arch::X86_64, &out).expect("restamped");
        let bytes = super::read(&out).expect("read back");
        let record = super::package_record(&bytes).expect("the new record reads");
        assert_eq!(record.package.license, "MIT");
        assert_eq!(
            install(&[bytes]).expect("installs").len(),
            2,
            "bin/a and the record"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn install_takes_what_the_record_vouches_for() {
        let good = package("a", &[("bin/a", b"one")], &[("bin/a", b"one")]);
        let files = install(&[good]).expect("it installs");
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["bin/a", "lib/ferrix/packages/a.toml"]);
        let changed = package("a", &[("bin/a", b"two")], &[("bin/a", b"one")]);
        assert!(install(&[changed]).is_err(), "a file unlike its record");
        let unlisted = package(
            "a",
            &[("bin/a", b"one"), ("bin/b", b"")],
            &[("bin/a", b"one")],
        );
        assert!(
            install(&[unlisted]).is_err(),
            "a file its record does not list"
        );
        let both = [
            package("a", &[("bin/x", b"")], &[("bin/x", b"")]),
            package("b", &[("bin/x", b"")], &[("bin/x", b"")]),
        ];
        assert!(install(&both).is_err(), "two packages owning one path");
    }

    /// A package as [`super::package`] writes one, of `entries`, its record
    /// made from `listed`.
    fn tree_package(entries: &[ports::File], listed: &[ports::File]) -> Vec<u8> {
        let mut package = app("git", "").recipe.package;
        package.arches = vec!["x86_64".to_owned()];
        let record = Record {
            package,
            files: listed.iter().filter_map(super::recorded).collect(),
        };
        let mut entries = entries.to_vec();
        entries.push(ports::File {
            path: record::path("git"),
            mode: 0o644,
            content: ports::Content::Bytes(record::render(&record).into_bytes()),
        });
        crate::initramfs::package(&entries).expect("an archive")
    }

    #[test]
    fn links_and_empty_directories_install_as_their_record_says() {
        let entry = |path: &str, content| ports::File {
            path: path.to_owned(),
            mode: 0o755,
            content,
        };
        let tree = [
            entry("bin/git", ports::Content::Bytes(b"\x7fELF".to_vec())),
            entry("usr/libexec/git-core", ports::Content::Directory),
            entry(
                "usr/libexec/git-core/git-upload-pack",
                ports::Content::Link("../../bin/git".to_owned()),
            ),
            entry(
                "usr/share/git-core/templates/branches",
                ports::Content::Directory,
            ),
        ];
        let files = install(&[tree_package(&tree, &tree)]).expect("it installs");
        let found = |path: &str| {
            files
                .iter()
                .find(|file| file.path == path)
                .map(|file| file.content.clone())
        };
        assert_eq!(
            found("usr/libexec/git-core/git-upload-pack"),
            Some(ports::Content::Link("../../bin/git".to_owned()))
        );
        assert_eq!(
            found("usr/share/git-core/templates/branches"),
            Some(ports::Content::Directory),
            "an empty directory is made"
        );
        assert_eq!(
            found("usr/libexec/git-core"),
            None,
            "a directory with something in it comes with that"
        );

        let mut elsewhere = tree.clone();
        elsewhere[2].content = ports::Content::Link("/bin/sh".to_owned());
        assert!(
            install(&[tree_package(&elsewhere, &tree)]).is_err(),
            "a link to somewhere its record does not say"
        );
    }
}
