//! The virtual machine a measurement ran in, as its guest sees it
//! (`docs/HOTPATHS.md` §5).
//!
//! A boot's processor is the gate's CPU model (`qemu::x86_cpu_for`,
//! `qemu::arm_cpu`) with whatever the accelerator could not give filtered
//! out: under KVM on nazuna `+pcid` is asked for by nobody and would be
//! dropped if it were, since the host has none. What the kernel finds is the
//! model after that filtering, and the model string alone does not say it.
//! So the fingerprint asks QEMU itself: it starts the same QEMU with the
//! same accelerator, machine and model, stopped before the first
//! instruction (`-S`), and reads each feature of the realised processor
//! over QMP (`qom-get`). No guest code runs and no image is needed.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use super::json::Value;
use crate::paths::Arch;

/// The x86 features the fingerprint reads from the realised processor:
/// `(name, QEMU property)`, named as [`super::host::X86_FEATURES`] names
/// the host's, so a host and a guest compare feature by feature.
const X86_PROPERTIES: &[(&str, &str)] = &[
    ("pcid", "pcid"),
    ("x2apic", "x2apic"),
    ("xsave", "xsave"),
    ("avx", "avx"),
    ("hypervisor", "hypervisor"),
    ("fsgsbase", "fsgsbase"),
    ("avx2", "avx2"),
    ("smep", "smep"),
    ("invpcid", "invpcid"),
    ("avx512f", "avx512f"),
    ("smap", "smap"),
    ("umip", "umip"),
    ("pku", "pku"),
    ("la57", "la57"),
    ("rdpid", "rdpid"),
    ("md_clear", "md-clear"),
    ("spec_ctrl", "spec-ctrl"),
    ("intel_stibp", "stibp"),
    ("arch_capabilities", "arch-capabilities"),
    ("spec_ctrl_ssbd", "ssbd"),
    ("fred", "fred"),
    ("xsaveopt", "xsaveopt"),
    ("xsavec", "xsavec"),
    ("xgetbv1", "xgetbv1"),
    ("xsaves", "xsaves"),
    ("tce", "tce"),
    ("pdpe1gb", "pdpe1gb"),
    ("rdtscp", "rdtscp"),
    ("constant_tsc", "invtsc"),
    ("ibpb", "ibpb"),
    ("ibrs", "ibrs"),
    ("stibp", "amd-stibp"),
    ("ssbd", "amd-ssbd"),
    ("virt_ssbd", "virt-ssbd"),
    ("lfence_serializing", "lfence-always-serializing"),
    ("null_sel_clr_base", "null-sel-clr-base"),
    ("auto_ibrs", "auto-ibrs"),
    ("eraps", "eraps"),
    ("rdctl_no", "rdctl-no"),
];

/// The identity properties of a realised x86 processor.
const X86_IDENTITY: &[&str] = &["vendor", "family", "model", "stepping", "model-id"];

/// The fingerprint's `vm` object for a boot of `arch` under `accelerator`
/// with the processor `model`, run by the QEMU at `binary`.
///
/// `cpu` is `null` when the probe could not run (no such QEMU, or one
/// without QMP's `qom-get`), and `probe` says which; a fingerprint without
/// the guest's processor is still a fingerprint, and a different one.
pub(crate) fn describe(arch: Arch, binary: Option<&Path>, accelerator: &str, model: &str) -> Value {
    let version = binary.and_then(version_line);
    let (qemu, package) = version
        .as_deref()
        .map_or((Value::Null, Value::Null), split_version);
    let cpu = binary.and_then(|binary| probe(arch, binary, accelerator, model));
    Value::object([
        ("arch", Value::str(arch.name())),
        ("accel", Value::str(accelerator)),
        ("qemu", qemu),
        ("qemu_package", package),
        ("cpu_model", Value::str(model)),
        (
            "probe",
            Value::str(if cpu.is_some() { "qom-get" } else { "none" }),
        ),
        ("cpu", cpu.unwrap_or(Value::Null)),
    ])
}

/// `binary --version`'s first line.
fn version_line(binary: &Path) -> Option<String> {
    let output = Command::new(binary).arg("--version").output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(super::host::squeeze)
}

/// "QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.2)" as the
/// version and the package in brackets.
fn split_version(line: &str) -> (Value, Value) {
    let rest = line.strip_prefix("QEMU emulator version ").unwrap_or(line);
    match rest.split_once(" (") {
        Some((version, package)) => (
            Value::str(version),
            Value::str(package.strip_suffix(')').unwrap_or(package)),
        ),
        None => (Value::str(rest), Value::Null),
    }
}

/// Start the machine stopped, read its processor over QMP, and quit.
fn probe(arch: Arch, binary: &Path, accelerator: &str, model: &str) -> Option<Value> {
    let machine = match arch {
        Arch::X86_64 => crate::qemu::x86_machine(accelerator),
        Arch::AArch64 | Arch::Armv7a => "virt".to_owned(),
    };
    let properties: Vec<&str> = match arch {
        Arch::X86_64 => X86_IDENTITY
            .iter()
            .copied()
            .chain(X86_PROPERTIES.iter().map(|&(_, property)| property))
            .collect(),
        Arch::AArch64 | Arch::Armv7a => vec!["midr"],
    };
    let mut script = String::from("{\"execute\":\"qmp_capabilities\"}\n");
    for property in &properties {
        let request = Value::object([
            ("execute", Value::str("qom-get")),
            ("id", Value::str(*property)),
            (
                "arguments",
                Value::object([
                    ("path", Value::str("/machine/unattached/device[0]")),
                    ("property", Value::str(*property)),
                ]),
            ),
        ]);
        script.push_str(&request.canonical());
        script.push('\n');
    }
    script.push_str("{\"execute\":\"quit\"}\n");
    let mut child = Command::new(binary)
        .args(["-machine", &machine, "-accel", accelerator, "-cpu", model])
        .args(["-S", "-nodefaults", "-display", "none", "-qmp", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
    }
    let output = child.wait_with_output().ok()?;
    let answers = answers(&String::from_utf8_lossy(&output.stdout));
    if answers.is_empty() {
        return None;
    }
    Some(match arch {
        Arch::X86_64 => x86_cpu(&answers),
        Arch::AArch64 | Arch::Armv7a => Value::object([(
            "midr",
            answers
                .get("midr")
                .and_then(Value::as_int)
                .map_or(Value::Null, |midr| Value::str(format!("{midr:#010x}"))),
        )]),
    })
}

/// Each QMP answer that carries an `id` and a `return`, by id; errors (a
/// property this QEMU does not have) are left out.
fn answers(text: &str) -> std::collections::BTreeMap<String, Value> {
    text.lines()
        .filter_map(|line| Value::parse(line).ok())
        .filter_map(|answer| {
            let id = answer.get("id")?.as_str()?.to_owned();
            Some((id, answer.get("return")?.clone()))
        })
        .collect()
}

/// The realised x86 processor: identity, then each feature it has.
fn x86_cpu(answers: &std::collections::BTreeMap<String, Value>) -> Value {
    let identity = |property: &str| match answers.get(property) {
        Some(Value::Str(text)) => Value::str(super::host::squeeze(text)),
        Some(other) => other.clone(),
        None => Value::Null,
    };
    let features =
        X86_PROPERTIES
            .iter()
            .filter_map(|&(name, property)| match answers.get(property) {
                Some(Value::Bool(present)) => Some((name, Value::Bool(*present))),
                _ => None,
            });
    Value::object([
        ("vendor", identity("vendor")),
        ("family", identity("family")),
        ("model", identity("model")),
        ("stepping", identity("stepping")),
        ("brand", identity("model-id")),
        ("features", Value::object(features)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_versions_split() {
        let (version, package) =
            split_version("QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.2)");
        assert_eq!(version, Value::str("10.2.1"));
        assert_eq!(package, Value::str("Debian 1:10.2.1+ds-1ubuntu3.2"));
        let (version, package) = split_version("QEMU emulator version 9.2.4");
        assert_eq!((version, package), (Value::str("9.2.4"), Value::Null));
    }

    #[test]
    fn answers_by_id_and_errors_dropped() {
        let transcript = concat!(
            "{\"QMP\": {\"version\": {}}}\n",
            "{\"return\": {}}\n",
            "{\"return\": false, \"id\": \"pcid\"}\n",
            "{\"return\": true, \"id\": \"eraps\"}\n",
            "{\"return\": 15, \"id\": \"family\"}\n",
            "{\"return\": \"QEMU Virtual CPU version 2.5+  \", \"id\": \"model-id\"}\n",
            "{\"id\": \"fred\", \"error\": {\"class\": \"GenericError\"}}\n",
            "{\"timestamp\": {\"seconds\": 1}, \"event\": \"SHUTDOWN\"}\n",
        );
        let cpu = x86_cpu(&answers(transcript));
        let features = cpu.get("features").unwrap();
        assert_eq!(features.get("pcid"), Some(&Value::Bool(false)));
        assert_eq!(features.get("eraps"), Some(&Value::Bool(true)));
        assert_eq!(
            features.get("fred"),
            None,
            "a property QEMU lacks is left out"
        );
        assert_eq!(cpu.get("family"), Some(&Value::Int(15)));
        assert_eq!(
            cpu.get("brand"),
            Some(&Value::str("QEMU Virtual CPU version 2.5+"))
        );
        assert_eq!(cpu.get("vendor"), Some(&Value::Null));
    }

    #[test]
    fn every_property_has_a_host_name() {
        for (name, _) in X86_PROPERTIES {
            let on_host = super::super::host::X86_FEATURES
                .iter()
                .any(|f| f.0 == *name);
            assert!(
                on_host || *name == "rdctl_no",
                "{name}: a guest feature the host table does not read"
            );
        }
    }
}
