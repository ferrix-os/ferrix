//! Hot paths described for agents, the machine a figure was measured on,
//! and the record of a measurement (`docs/HOTPATHS.md`).
//!
//! `cargo xtask hw-fingerprint` prints the canonical description of this
//! host and of the virtual machine a boot of `--arch` would run in, and the
//! SHA-256 of each. A measurement taken with `bench-ipc --record` is filed
//! under `docs/hotpaths/results/<path>/<hash>/` by the first twelve hex
//! digits of that hash, so an agent on another machine finds the results of
//! its own hardware by computing its own fingerprint.

pub(crate) mod guest;
pub(crate) mod host;
pub(crate) mod json;
pub(crate) mod record;

use crate::args::Args;
use crate::paths::Arch;
use crate::{Result, qemu};
use json::Value;

/// What the fingerprint's text says it is, so a later layout is a new name.
pub(crate) const FINGERPRINT_SCHEMA: &str = "ferrix-hw-fingerprint/1";

/// How many hex digits of a hash name a directory.
pub(crate) const SHORT: usize = 12;

/// A machine's fingerprint and its hashes.
#[derive(Debug, Clone)]
pub(crate) struct Fingerprint {
    /// `{"schema", "host", "vm"}`.
    pub(crate) value: Value,
    /// SHA-256 of the whole canonical text: the key results are filed by.
    pub(crate) hw_hash: String,
    /// SHA-256 of `host` alone: the machine, whatever QEMU ran on it.
    pub(crate) host_hash: String,
    /// SHA-256 of the processor alone, without microcode, kernel or
    /// hypervisor: the silicon, for finding results of the same part on
    /// another machine.
    pub(crate) cpu_hash: String,
}

impl Fingerprint {
    /// Hash `host` and `vm` together.
    pub(crate) fn new(host: Value, vm: Value) -> Fingerprint {
        let silicon = |key: &str| host.get(key).cloned().unwrap_or(Value::Null);
        let cpu_only = match host.get("cpu") {
            Some(Value::Object(fields)) => Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| key.as_str() != "microcode")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
            other => other.cloned().unwrap_or(Value::Null),
        };
        let cpu = Value::object([
            ("cpu", cpu_only),
            ("features", silicon("features")),
            ("caches", silicon("caches")),
            ("tlb", silicon("tlb")),
            ("topology", silicon("topology")),
        ]);
        let host_hash = hash(&host);
        let value = Value::object([
            ("schema", Value::str(FINGERPRINT_SCHEMA)),
            ("host", host),
            ("vm", vm),
        ]);
        Fingerprint {
            hw_hash: hash(&value),
            host_hash,
            cpu_hash: hash(&cpu),
            value,
        }
    }

    /// This host, and a boot of `arch` under `args`' accelerator.
    pub(crate) fn of_this_host(arch: Arch, args: &Args) -> Result<Fingerprint> {
        let binary = crate::window::qemu_for(arch.qemu_binary(), false);
        let accelerator = match binary.as_deref() {
            Some(binary) => qemu::accelerator(arch, binary, args.accel.as_deref())?,
            None => args.accel.clone().unwrap_or_else(|| "tcg".to_owned()),
        };
        let model = match (arch, binary.as_deref()) {
            (Arch::X86_64, Some(binary)) => qemu::x86_cpu_for(binary, &accelerator),
            (Arch::X86_64, None) => qemu::x86_cpu(&accelerator),
            (Arch::AArch64 | Arch::Armv7a, _) => qemu::arm_cpu(arch),
        };
        let vm = guest::describe(arch, binary.as_deref(), &accelerator, &model);
        Ok(Fingerprint::new(host::Facts::read().describe(), vm))
    }

    /// The directory name results on this machine are filed under.
    pub(crate) fn short(&self) -> &str {
        self.hw_hash.get(..SHORT).unwrap_or(&self.hw_hash)
    }
}

/// The SHA-256 of `value`'s canonical text, in hex.
pub(crate) fn hash(value: &Value) -> String {
    crate::sha256::hex(&crate::sha256::digest(value.canonical().as_bytes()))
}

/// `cargo xtask hw-fingerprint [--arch A] [--accel X]`: print each
/// architecture's fingerprint and its hashes.
pub(crate) fn hw_fingerprint(args: &Args) -> Result<()> {
    let arches = if args.arch.is_none() {
        vec![Arch::X86_64]
    } else {
        args.arches()?
    };
    for arch in arches {
        let fingerprint = Fingerprint::of_this_host(arch, args)?;
        print!("{}", fingerprint.value.pretty());
        println!(
            "hw-fingerprint {arch}: {} (directory {}); host {}; cpu {}",
            fingerprint.hw_hash,
            fingerprint.short(),
            fingerprint.host_hash,
            fingerprint.cpu_hash
        );
    }
    Ok(())
}

/// What a hot path's data file says it is.
#[cfg(test)]
const PATH_SCHEMA: &str = "ferrix-hotpath/1";

/// Check every hot path's data file, `docs/hotpaths/<id>.json`, from the
/// unit test that holds the tree's own to it: it parses, names its schema
/// and its own id, its skill and every file it names exist, and every
/// attempt says whether its effect was measured, guessed or argued. The
/// problems, one line each.
#[cfg(test)]
pub(crate) fn check_paths(root: &std::path::Path) -> Vec<String> {
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("docs/hotpaths")) else {
        return vec!["no docs/hotpaths".to_owned()];
    };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    if files.is_empty() {
        problems.push("docs/hotpaths holds no hot path".to_owned());
    }
    for file in files {
        let id = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let mut say = |what: String| problems.push(format!("docs/hotpaths/{id}.json: {what}"));
        let data = match Value::parse(&std::fs::read_to_string(&file).unwrap_or_default()) {
            Ok(data) => data,
            Err(why) => {
                say(format!("not JSON in the canonical limits: {why}"));
                continue;
            }
        };
        let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
        if text("schema") != PATH_SCHEMA {
            say(format!("schema is not {PATH_SCHEMA}"));
        }
        if text("id") != id {
            say("its id is not its file name".to_owned());
        }
        let mut named: Vec<String> = vec![text("skill").to_owned(), text("spec").to_owned()];
        if let Some(Value::List(files)) = data.get("definition").and_then(|d| d.get("files")) {
            named.extend(files.iter().filter_map(Value::as_str).map(str::to_owned));
        } else {
            say("no definition.files".to_owned());
        }
        for path in named {
            if path.is_empty() || !root.join(&path).exists() {
                say(format!("names `{path}`, which the tree does not have"));
            }
        }
        for key in ["figure", "budget", "never_traded", "requirements"] {
            if data.get(key).is_none() {
                say(format!("no `{key}`"));
            }
        }
        let Some(Value::List(tried)) = data.get("tried") else {
            say("no `tried`".to_owned());
            continue;
        };
        for attempt in tried {
            let field = |key: &str| attempt.get(key).and_then(Value::as_str).unwrap_or_default();
            if !matches!(field("evidence"), "measured" | "guessed" | "argued") {
                say(format!(
                    "attempt `{}`: evidence is not measured, guessed or argued",
                    field("id")
                ));
            }
            for key in ["id", "arch", "what", "status", "effect", "source"] {
                if field(key).is_empty() {
                    say(format!("attempt `{}`: no `{key}`", field("id")));
                }
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guest as `guest::describe` writes one, without running QEMU.
    fn vm(model: &str) -> Value {
        Value::object([
            ("arch", Value::str("x86_64")),
            ("accel", Value::str("kvm")),
            ("qemu", Value::str("10.2.1")),
            ("cpu_model", Value::str(model)),
        ])
    }

    #[test]
    fn the_hash_is_of_the_canonical_text() {
        let fingerprint = Fingerprint::new(host::tests::nazuna().describe(), vm("qemu64"));
        let text = fingerprint.value.canonical();
        let reread = Value::parse(&fingerprint.value.pretty()).unwrap();
        assert_eq!(
            hash(&reread),
            fingerprint.hw_hash,
            "pretty text hashes as canonical"
        );
        assert_eq!(
            fingerprint.hw_hash,
            crate::sha256::hex(&crate::sha256::digest(text.as_bytes()))
        );
        assert_eq!(fingerprint.short().len(), SHORT);
        assert!(text.starts_with("{\"host\":"), "keys in byte order: {text}");
    }

    #[test]
    fn the_same_facts_give_the_same_hash() {
        let one = Fingerprint::new(host::tests::nazuna().describe(), vm("qemu64"));
        let two = Fingerprint::new(host::tests::nazuna().describe(), vm("qemu64"));
        assert_eq!(one.hw_hash, two.hw_hash);
    }

    #[test]
    fn each_hash_moves_with_what_it_covers() {
        let base = Fingerprint::new(host::tests::nazuna().describe(), vm("qemu64"));
        let other_model = Fingerprint::new(host::tests::nazuna().describe(), vm("qemu64,+eraps"));
        assert_ne!(base.hw_hash, other_model.hw_hash, "the guest is in the key");
        assert_eq!(
            base.host_hash, other_model.host_hash,
            "the host is not the guest"
        );

        let mut facts = host::tests::nazuna();
        facts.cpuinfo = facts.cpuinfo.replace("0xb404035", "0xb404036");
        let microcode = Fingerprint::new(facts.describe(), vm("qemu64"));
        assert_ne!(
            base.hw_hash, microcode.hw_hash,
            "microcode moves the full hash"
        );
        assert_eq!(base.cpu_hash, microcode.cpu_hash, "but not the silicon's");

        let mut facts = host::tests::nazuna();
        let _ = facts
            .cpuid
            .insert((1, 0), [0x00B4_0F40, 0, 0x7ED8_320B | 1 << 17, 0]);
        let pcid = Fingerprint::new(facts.describe(), vm("qemu64"));
        assert_ne!(base.cpu_hash, pcid.cpu_hash, "a feature bit is the silicon");
    }

    /// The hot paths in the tree itself (`docs/HOTPATHS.md` §3).
    #[test]
    fn the_trees_hot_paths_check_clean() {
        let root = crate::paths::workspace_root();
        assert_eq!(check_paths(&root), Vec::<String>::new());
    }
}
