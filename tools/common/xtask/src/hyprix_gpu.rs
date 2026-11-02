//! hyprix-gpu in an image (`docs/GPU-COMPOSITOR.md` §10): the compositor's
//! Vulkan helper, and the Mesa it draws with, under a prefix of their own.
//!
//! The helper (`src/user/system/linux/hyprix-gpu`) is a glibc program. It is
//! built against the yserver volume's Debian sysroot, as tole's GPU demos
//! are (`compositor::tole_demo`), and linked to find its own loader and
//! libraries under [`PREFIX`] -- `PT_INTERP` and `RUNPATH` -- so nothing
//! else in the image sees them and the desktop needs nothing of the Steam
//! volume's `/data/usr`. What it loads is the intel-uapi volume's pinned
//! Debian 13 Mesa (`tools/common/fetch/fetch-intel-uapi.sh`, the builds
//! yserver pins): anv, the Vulkan loader, and the libraries they need,
//! [`LIBRARIES`]. anv's ICD manifest names it by absolute path under the
//! prefix, and the helper is pointed at that manifest alone. Each package's
//! copyright file goes beside them, under `usr/share/doc/hyprix-gpu/`.
//!
//! Opt-in (the product owner, 2026-10-28): an image carries it only when
//! `FERRIX_GPU_HELPER=1` is set as it is built -- the N150 box's image and
//! the GPU helper's own boot -- so the default image and the ISO stay as
//! they are until hyprix's `gpu` option defaults to `auto`. x86-64 only:
//! the volume and the sysroot are x86-64.

use std::path::{Path, PathBuf};

use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result};

/// Where the helper's loader, libraries and ICD manifest go.
pub(crate) const PREFIX: &str = "usr/lib/hyprix-gpu";

/// Where the helper goes: what hyprix starts.
pub(crate) const PROGRAM: &str = "usr/libexec/hyprix-gpu";

/// The variable that asks for it.
const WANTED: &str = "FERRIX_GPU_HELPER";

/// The helper's workspace.
const DIR: &str = "src/user/system/linux/hyprix-gpu";

/// The libraries, by the name they are loaded by, and the Debian package
/// each comes from: anv's and the loader's closure on Debian 13, which
/// `readelf -d` over the volume's tree gives. A library missing from the
/// volume stops the build and is named.
pub(crate) const LIBRARIES: &[(&str, &str)] = &[
    ("ld-linux-x86-64.so.2", "libc6"),
    ("libc.so.6", "libc6"),
    ("libm.so.6", "libc6"),
    ("libgcc_s.so.1", "libgcc-s1"),
    ("libstdc++.so.6", "libstdc++6"),
    ("libvulkan.so.1", "libvulkan1"),
    ("libvulkan_intel.so", "mesa-vulkan-drivers"),
    ("libdrm.so.2", "libdrm2"),
    ("libz.so.1", "zlib1g"),
    ("libzstd.so.1", "libzstd1"),
    ("libexpat.so.1", "libexpat1"),
    ("libffi.so.8", "libffi8"),
    ("libwayland-client.so.0", "libwayland-client0"),
    ("libxcb.so.1", "libxcb1"),
    ("libxcb-dri3.so.0", "libxcb-dri3-0"),
    ("libxcb-present.so.0", "libxcb-present0"),
    ("libxcb-xfixes.so.0", "libxcb-xfixes0"),
    ("libxcb-sync.so.1", "libxcb-sync1"),
    ("libxcb-randr.so.0", "libxcb-randr0"),
    ("libxcb-shm.so.0", "libxcb-shm0"),
    ("libX11-xcb.so.1", "libx11-xcb1"),
    ("libxshmfence.so.1", "libxshmfence1"),
    ("libXau.so.6", "libxau6"),
    ("libXdmcp.so.6", "libxdmcp6"),
];

/// Whether an image for `arch` carries the helper.
pub(crate) fn wanted(arch: Arch) -> bool {
    arch == Arch::X86_64 && std::env::var_os(WANTED).is_some_and(|value| value == "1")
}

/// The yserver volume's Debian sysroot, which the helper is built against.
fn sysroot() -> Result<PathBuf> {
    let sysroot = paths::volume_directory("yserver")?.join("sysroot");
    if !sysroot.join("usr/lib/x86_64-linux-gnu/libc.so.6").is_file() {
        return Err(Error::new(format!(
            "{} has no glibc: tools/common/fetch/fetch-yserver.sh makes the sysroot",
            sysroot.display()
        )));
    }
    Ok(sysroot)
}

/// Build the helper, linked to load from [`PREFIX`]; its path.
///
/// # Errors
///
/// No sysroot, or a build that fails.
pub(crate) fn build() -> Result<PathBuf> {
    let sysroot = sysroot()?;
    let target = "x86_64-unknown-linux-gnu";
    let target_dir = paths::target_dir().join("hyprix-gpu");
    let program = target_dir.join(target).join("release").join("hyprix-gpu");
    println!(
        "  building hyprix-gpu for {target} against {}",
        sysroot.display()
    );
    // The sysroot's libraries first (the C compiler would add the host's,
    // whose glibc is newer than the volume's); then the image's own loader
    // and library directory, as RUNPATH so a test that starts it through
    // another loader with `--library-path` is still obeyed.
    let flags = format!(
        "-C link-arg=--sysroot={0} -L native={0}/usr/lib/x86_64-linux-gnu \
         -C link-arg=-Wl,--dynamic-linker=/{PREFIX}/ld-linux-x86-64.so.2 \
         -C link-arg=-Wl,-rpath,/{PREFIX} -C link-arg=-Wl,--enable-new-dtags",
        sysroot.display()
    );
    crate::builds::Build::cargo(
        format!("cargo build (hyprix-gpu) --target {target}"),
        paths::workspace_root().join(DIR),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS", flags)
    .output(&program)
    .run()?;
    Ok(program)
}

/// `name` in the volume's tree, followed to the file it names.
fn library(tree: &Path, name: &str) -> Result<PathBuf> {
    for dir in [
        "usr/lib/x86_64-linux-gnu",
        "lib/x86_64-linux-gnu",
        "usr/lib64",
    ] {
        let path = tree.join(dir).join(name);
        if let Ok(real) = std::fs::canonicalize(&path) {
            return Ok(real);
        }
    }
    Err(Error::new(format!(
        "{name} is not in {}: run tools/common/fetch/fetch-intel-uapi.sh",
        tree.display()
    )))
}

/// The package whose docs `package`'s docs are: GCC's runtime libraries
/// link theirs to `gcc-14-base`'s.
fn docs_of(package: &str) -> &str {
    match package {
        "libgcc-s1" | "libstdc++6" => "gcc-14-base",
        other => other,
    }
}

/// The copyright file of Debian package `package`, from its `.deb` in the
/// volume's pool (the tree has the docs stripped).
fn copyright(pool: &Path, package: &str, scratch: &Path) -> Result<Vec<u8>> {
    let package = docs_of(package);
    let prefix = format!("{package}_");
    let deb = std::fs::read_dir(pool)
        .map_err(|error| Error::new(format!("reading {}: {error}", pool.display())))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".deb"))
        })
        .ok_or_else(|| Error::new(format!("no {package} package in {}", pool.display())))?;
    let into = scratch.join(package);
    let _ = std::fs::remove_dir_all(&into);
    std::fs::create_dir_all(&into)
        .map_err(|error| Error::new(format!("making {}: {error}", into.display())))?;
    let wanted = format!("./usr/share/doc/{package}/copyright");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("dpkg-deb --fsys-tarfile \"$1\" | tar -x -C \"$2\" \"$3\"")
        .arg("sh")
        .arg(&deb)
        .arg(&into)
        .arg(&wanted)
        .status()
        .map_err(|error| Error::new(format!("dpkg-deb: {error}")))?;
    let path = into.join(&wanted);
    if !status.success() {
        return Err(Error::new(format!(
            "no copyright file in {}: dpkg-deb and tar are needed",
            deb.display()
        )));
    }
    std::fs::read(&path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// anv's ICD manifest, naming it under [`PREFIX`].
fn manifest(api: &str) -> String {
    format!(
        "{{\n    \"ICD\": {{\n        \"api_version\": \"{api}\",\n        \"library_path\": \"/{PREFIX}/libvulkan_intel.so\"\n    }},\n    \"file_format_version\": \"1.0.0\"\n}}\n"
    )
}

/// What an image carries for the helper: the program, its libraries, anv's
/// manifest and the copyright files.
///
/// # Errors
///
/// No sysroot or intel-uapi volume, a library missing from it, or a build
/// that fails.
pub(crate) fn carried() -> Result<Vec<File>> {
    let program = build()?;
    let root = paths::volume_directory("intel-uapi")?;
    let tree = root.join("tree");
    let pool = root.join("pool");
    let read = |path: &Path| {
        std::fs::read(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    let mut files = vec![File {
        path: PROGRAM.to_owned(),
        mode: 0o755,
        content: Content::Bytes(read(&program)?),
    }];
    for (name, _) in LIBRARIES {
        files.push(File {
            path: format!("{PREFIX}/{name}"),
            mode: 0o755,
            content: Content::Bytes(read(&library(&tree, name)?)?),
        });
    }
    let icd = tree.join("usr/share/vulkan/icd.d/intel_icd.json");
    let said = String::from_utf8_lossy(&read(&icd)?).into_owned();
    let api = said
        .split("\"api_version\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .unwrap_or("1.4.305")
        .to_owned();
    files.push(File {
        path: format!("{PREFIX}/icd.d/intel_icd.json"),
        mode: 0o644,
        content: Content::Bytes(manifest(&api).into_bytes()),
    });
    let scratch = paths::target_dir().join("hyprix-gpu-copyright");
    let mut packages: Vec<&str> = LIBRARIES
        .iter()
        .map(|(_, package)| docs_of(package))
        .collect();
    packages.sort_unstable();
    packages.dedup();
    for package in packages {
        files.push(File {
            path: format!("usr/share/doc/hyprix-gpu/{package}/copyright"),
            mode: 0o644,
            content: Content::Bytes(copyright(&pool, package, &scratch)?),
        });
    }
    let bytes: usize = files
        .iter()
        .map(|file| match &file.content {
            Content::Bytes(bytes) => bytes.len(),
            Content::Link(_) | Content::Directory => 0,
        })
        .sum();
    println!(
        "  hyprix-gpu: carried at /{PROGRAM} with anv and {} libraries under /{PREFIX}, {} MiB",
        LIBRARIES.len(),
        bytes / (1024 * 1024)
    );
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest names anv by its place in the image, nowhere else.
    #[test]
    fn the_manifest_names_anv_under_the_prefix() {
        let text = manifest("1.4.305");
        assert!(text.contains("\"library_path\": \"/usr/lib/hyprix-gpu/libvulkan_intel.so\""));
        assert!(text.contains("\"api_version\": \"1.4.305\""));
    }

    /// The loader the helper is linked against is among what is carried.
    #[test]
    fn the_loader_is_carried() {
        assert!(
            LIBRARIES
                .iter()
                .any(|(name, _)| *name == "ld-linux-x86-64.so.2")
        );
        assert!(
            LIBRARIES
                .iter()
                .any(|(name, _)| *name == "libvulkan_intel.so")
        );
    }
}
