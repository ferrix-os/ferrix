//! The package anchors `pkg` trusts (`docs/UPDATES.md` §2.2), compiled in
//! as `docs/SIGNING.md` §6.1 has every verifier in an image take its own.
//!
//! `FERRIX_PKG_ANCHORS` names them: entries `<anchor id>:<HSS public key in
//! hex>` separated by white space, every id a development key's (`0x8000`
//! and up, §6.4). Unset, as for every image today, the table is empty and
//! `pkg` refuses every repository: no package key exists in
//! `tools/common/data/signing-anchors.toml` yet. `cargo xtask test-pkg` sets
//! it to the throwaway key it signs its repositories with.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic,
    reason = "a build script: cargo reads its output, and a malformed anchor stops the build"
)]

use std::fmt::Write as _;

/// An HSS public key's length (RFC 8554 §6.1, n = 24).
const KEY_LEN: usize = 52;

fn main() {
    println!("cargo::rerun-if-env-changed=FERRIX_PKG_ANCHORS");
    let given = std::env::var("FERRIX_PKG_ANCHORS").unwrap_or_default();
    let mut table = String::from("/// The package anchors this build trusts (`build.rs`).\n");
    table.push_str("pub const ANCHORS: &[Anchor] = &[\n");
    for entry in given.split_whitespace() {
        let Some((id, key)) = entry.split_once(':') else {
            panic!("FERRIX_PKG_ANCHORS: `{entry}` is not <id>:<hex key>");
        };
        let id = match id.strip_prefix("0x") {
            Some(hex) => u16::from_str_radix(hex, 16),
            None => id.parse(),
        }
        .unwrap_or_else(|_| panic!("FERRIX_PKG_ANCHORS: `{id}` is not an anchor id"));
        assert!(
            id >= 0x8000,
            "FERRIX_PKG_ANCHORS: {id:#x} is not a development anchor id (0x8000 and up)"
        );
        let bytes: Vec<u8> = (0..key.len() / 2)
            .map(|n| {
                key.get(2 * n..2 * n + 2)
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .unwrap_or_else(|| panic!("FERRIX_PKG_ANCHORS: `{key}` is not hex"))
            })
            .collect();
        assert!(
            bytes.len() == KEY_LEN && key.len() == 2 * KEY_LEN,
            "FERRIX_PKG_ANCHORS: a key is {KEY_LEN} bytes, and {id:#x}'s is not"
        );
        let mut list = String::new();
        for byte in &bytes {
            let _ = write!(list, "{byte:#04x}, ");
        }
        let _ = writeln!(
            table,
            "    Anchor {{ kind: Kind::Package, id: {id:#06x}, \
             public_key: [{list}] }},"
        );
    }
    table.push_str("];\n");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    if let Err(error) = std::fs::write(out.join("anchors.rs"), table) {
        panic!("writing anchors.rs: {error}");
    }
}
