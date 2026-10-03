//! `cargo xtask new-app --app NAME [--abi native|linux]`: a new app's folder,
//! with everything the gates ask of one, and nothing written anywhere else.
//!
//! The program it writes says hello and its lib target has one test, so the
//! folder passes `check` and `test-apps` as it is: the first thing its author
//! sees is green, and everything after is their change.

use std::fs;
use std::path::Path;

use ferrix_pkg::manifest::{self, Abi};

use super::PLACE;
use crate::args::Args;
use crate::{Error, Result, paths};

/// Write the app `--app` names, for `--abi`'s ABI.
///
/// # Errors
///
/// Not exactly one `--app`, a name that cannot name a package, an ABI that
/// is neither, or a folder that is already there.
pub(crate) fn new_app(args: &Args) -> Result<()> {
    let [name] = args.apps.as_slice() else {
        return Err(Error::new("new-app wants exactly one --app NAME"));
    };
    if !manifest::is_name(name) {
        return Err(Error::new(format!(
            "`{name}` cannot name an app: lower-case letters, digits and `-`, from a letter"
        )));
    }
    let abi = match args.abi.as_deref() {
        None | Some("native") => Abi::Native,
        Some("linux") => Abi::Linux,
        Some(other) => {
            return Err(Error::new(format!("--abi {other}: native or linux")));
        }
    };
    let dir = paths::workspace_root().join(PLACE).join(name);
    if dir.exists() {
        return Err(Error::new(format!("{} is already there", dir.display())));
    }
    for (path, text) in files(name, abi) {
        write(&dir.join(path), &text)?;
    }
    println!(
        "{}: a {} app; `cargo xtask check` and `cargo xtask test-apps` take it as it is",
        dir.display(),
        abi.as_str()
    );
    Ok(())
}

/// The folder's files, by path within it.
fn files(name: &str, abi: Abi) -> Vec<(&'static str, String)> {
    let krate = name.replace('-', "_");
    let fill = |template: &str| template.replace("{name}", name).replace("{crate}", &krate);
    let mut files = vec![
        ("app.toml", fill(APP_TOML).replace("{abi}", abi.as_str())),
        ("README.md", fill(README)),
        ("src/lib.rs", fill(LIB)),
    ];
    match abi {
        Abi::Native => {
            files.push(("Cargo.toml", fill(NATIVE_CARGO)));
            files.push(("build.rs", fill(NATIVE_BUILD)));
            files.push(("src/main.rs", fill(NATIVE_MAIN)));
        }
        Abi::Linux => {
            files.push(("Cargo.toml", fill(LINUX_CARGO)));
            files.push(("src/main.rs", fill(LINUX_MAIN)));
        }
    }
    files
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| Error::new(format!("{}: {error}", parent.display())))?;
    }
    fs::write(path, text)
        .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))
}

const APP_TOML: &str = r#"# {name}, an app (docs/APPS.md).

[package]
name = "{name}"
version = "0.1.0"
description = "What {name} is, in one line."
license = "MIT"
abi = "{abi}"
arches = ["x86_64", "aarch64", "armv7a"]
depends = []

[[package.files]]
from = "{name}"
to = "bin/{name}"
mode = "755"

[build]
kind = "cargo"

[image]
default = false

[check]
host-tests = true

[[smoke]]
run = "{name}"
expect = "hello from {name}"
"#;

const README: &str = "# {name}

What {name} is. An app (`docs/APPS.md`): everything it is lives in this
folder, and xtask finds it by its `app.toml`.

```
cargo xtask check                     # its fmt, clippy and tests
cargo xtask test-apps --arch x86_64   # a boot that runs its [[smoke]] lines
cargo xtask run --app {name}          # an image with it at /bin/{name}
```
";

const LIB: &str = r#"//! What {name} does, where the host's `cargo test --lib` reaches it.

#![no_std]

/// The line the program prints.
#[must_use]
pub const fn greeting() -> &'static str {
    "hello from {name}\n"
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_greets_by_name() {
        assert_eq!(super::greeting(), "hello from {name}\n");
    }
}
"#;

const NATIVE_CARGO: &str = r#"[package]
name = "{name}"
description = "What {name} is, in one line."
version = "0.1.0"
edition = "2024"
rust-version = "1.97"
license = "MIT"
publish = false

# An app: a workspace of its own (docs/APPS.md).
[workspace]

[lib]
path = "src/lib.rs"

[[bin]]
name = "{name}"
path = "src/main.rs"
test = false
bench = false

# The runtime, on the kernel's targets only: the host builds the lib alone.
[target.'cfg(target_os = "none")'.dependencies]
ferrix-rt = { path = "../../system/native/rt" }

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"
opt-level = "s"
lto = true
codegen-units = 1
"#;

const NATIVE_BUILD: &str = r#"//! Link the program with the runtime's layout, on the kernel's targets.

// A build script talks to cargo over stdout; that is the whole interface.
#![allow(
    clippy::print_stdout,
    reason = "stdout is how a build script communicates with cargo"
)]

fn main() {
    println!("cargo::rerun-if-env-changed=DEP_FERRIX_RT_LINKER_SCRIPT");
    if let Ok(script) = std::env::var("DEP_FERRIX_RT_LINKER_SCRIPT") {
        println!("cargo::rustc-link-arg-bins=-T{script}");
        println!("cargo::rerun-if-changed={script}");
    }
}
"#;

const NATIVE_MAIN: &str = r#"//! `{name}`: a native program, started from a shell by `execve`.

#![no_std]
#![no_main]

use ferrix_rt::{Bootstrap, linux};

ferrix_rt::entry!(main);

/// Standard output.
const STDOUT: usize = 1;

fn main(_: Bootstrap) -> i32 {
    match linux::write(STDOUT, {crate}::greeting().as_bytes()) {
        Ok(_) => 0,
        Err(_) => 1,
    }
}
"#;

const LINUX_CARGO: &str = r#"[package]
name = "{name}"
description = "What {name} is, in one line."
version = "0.1.0"
edition = "2024"
rust-version = "1.97"
license = "MIT"
publish = false

# An app: a workspace of its own (docs/APPS.md), a static program against
# the target's musl.
[workspace]

[lib]
path = "src/lib.rs"

[[bin]]
name = "{name}"
path = "src/main.rs"
test = false
bench = false

[profile.release]
panic = "abort"
opt-level = "s"
lto = true
strip = true
"#;

const LINUX_MAIN: &str = r#"//! `{name}`: a Linux program, on Ferrix's Linux ABI.

use std::io::Write as _;

fn main() {
    let _written = std::io::stdout().write_all({crate}::greeting().as_bytes());
}
"#;

#[cfg(test)]
mod tests {
    use super::files;
    use ferrix_pkg::manifest::{self, Abi};

    #[test]
    fn a_new_apps_manifest_reads_and_names_its_program() {
        for abi in [Abi::Native, Abi::Linux] {
            let files = files("my-app", abi);
            let manifest = files
                .iter()
                .find(|(path, _)| *path == "app.toml")
                .map(|(_, text)| text)
                .expect("an app.toml");
            let recipe = manifest::recipe(manifest).expect("it reads");
            assert_eq!(recipe.package.name, "my-app");
            assert_eq!(recipe.package.abi, abi);
            assert_eq!(recipe.files[0].to, "bin/my-app");
            assert_eq!(recipe.smoke[0].expect, "hello from my-app");
            let main = files
                .iter()
                .find(|(path, _)| *path == "src/main.rs")
                .map(|(_, text)| text)
                .expect("a main.rs");
            assert!(main.contains("my_app::greeting()"), "{main}");
            assert!(
                !files
                    .iter()
                    .any(|(_, text)| text.contains("{name}") || text.contains("{crate}"))
            );
        }
    }
}
