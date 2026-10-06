//! The measuring machine, described canonically (`docs/HOTPATHS.md` §5).
//!
//! What is read is split from what is written: [`Facts::read`] gathers the
//! raw inputs from this host (CPUID, `/proc/cpuinfo`, sysfs), and
//! [`Facts::describe`] turns them into the fingerprint's `host` object
//! without touching the machine, so the tests hold it to fixtures.
//!
//! Only named fields are copied, never a whole file: `/proc/cpuinfo` on some
//! Arm boards carries a `Serial`, and no serial number, MAC address, host
//! name or user name may reach a fingerprint that is meant to be published.

use std::collections::{BTreeMap, BTreeSet};

use super::json::Value;

/// One CPUID leaf's four registers.
pub(crate) type Registers = [u32; 4];

/// EAX, EBX, ECX, EDX: the positions in [`Registers`].
const EAX: usize = 0;
/// See [`EAX`].
const EBX: usize = 1;
/// See [`EAX`].
const ECX: usize = 2;
/// See [`EAX`].
const EDX: usize = 3;

/// The x86 features a hot path's knobs depend on: `(name, leaf, subleaf,
/// register, bit)`. A feature that is absent is written `false`, because
/// "no PCID" is as much a fact about a machine as "PCID" (OPAQUE-KERNEL.md
/// §9.6). Names are Linux's `/proc/cpuinfo` spellings where it has one.
pub(crate) const X86_FEATURES: &[(&str, u32, u32, usize, u32)] = &[
    ("pcid", 1, 0, ECX, 17),
    ("x2apic", 1, 0, ECX, 21),
    ("xsave", 1, 0, ECX, 26),
    ("avx", 1, 0, ECX, 28),
    ("hypervisor", 1, 0, ECX, 31),
    ("fsgsbase", 7, 0, EBX, 0),
    ("avx2", 7, 0, EBX, 5),
    ("smep", 7, 0, EBX, 7),
    ("invpcid", 7, 0, EBX, 10),
    ("avx512f", 7, 0, EBX, 16),
    ("smap", 7, 0, EBX, 20),
    ("umip", 7, 0, ECX, 2),
    ("pku", 7, 0, ECX, 3),
    ("la57", 7, 0, ECX, 16),
    ("rdpid", 7, 0, ECX, 22),
    ("md_clear", 7, 0, EDX, 10),
    ("spec_ctrl", 7, 0, EDX, 26),
    ("intel_stibp", 7, 0, EDX, 27),
    ("arch_capabilities", 7, 0, EDX, 29),
    ("spec_ctrl_ssbd", 7, 0, EDX, 31),
    ("fred", 7, 1, EAX, 17),
    ("xsaveopt", 0xD, 1, EAX, 0),
    ("xsavec", 0xD, 1, EAX, 1),
    ("xgetbv1", 0xD, 1, EAX, 2),
    ("xsaves", 0xD, 1, EAX, 3),
    ("tce", 0x8000_0001, 0, ECX, 17),
    ("pdpe1gb", 0x8000_0001, 0, EDX, 26),
    ("rdtscp", 0x8000_0001, 0, EDX, 27),
    ("constant_tsc", 0x8000_0007, 0, EDX, 8),
    ("invlpgb", 0x8000_0008, 0, EBX, 3),
    ("ibpb", 0x8000_0008, 0, EBX, 12),
    ("ibrs", 0x8000_0008, 0, EBX, 14),
    ("stibp", 0x8000_0008, 0, EBX, 15),
    ("ssbd", 0x8000_0008, 0, EBX, 24),
    ("virt_ssbd", 0x8000_0008, 0, EBX, 25),
    ("lfence_serializing", 0x8000_0021, 0, EAX, 2),
    ("null_sel_clr_base", 0x8000_0021, 0, EAX, 6),
    ("auto_ibrs", 0x8000_0021, 0, EAX, 8),
    ("eraps", 0x8000_0021, 0, EAX, 24),
];

/// The raw inputs, as read from a host or written by a test.
#[derive(Debug, Default, Clone)]
pub(crate) struct Facts {
    /// `uname -m`'s word: `x86_64`, `aarch64`, ...
    pub(crate) arch: String,
    /// Every CPUID leaf read, by `(leaf, subleaf)`; empty off x86.
    pub(crate) cpuid: BTreeMap<(u32, u32), Registers>,
    /// `/proc/cpuinfo`'s text.
    pub(crate) cpuinfo: String,
    /// Processor 0's caches, one map of sysfs file to contents an index.
    pub(crate) caches: Vec<BTreeMap<String, String>>,
    /// `/sys/devices/system/cpu/online`.
    pub(crate) online: String,
    /// Processor 0's `thread_siblings_list`.
    pub(crate) siblings: String,
    /// Each online processor's `physical_package_id`.
    pub(crate) packages: Vec<String>,
    /// `/proc/sys/kernel/osrelease`.
    pub(crate) kernel: String,
    /// `/sys/devices/system/cpu/vulnerabilities`, by file.
    pub(crate) vulnerabilities: BTreeMap<String, String>,
}

impl Facts {
    /// Read this host.
    pub(crate) fn read() -> Facts {
        let cpu = std::path::Path::new("/sys/devices/system/cpu");
        let text = |path: &std::path::Path| {
            std::fs::read_to_string(path)
                .map(|text| text.trim().to_owned())
                .unwrap_or_default()
        };
        let online = text(&cpu.join("online"));
        let packages = list(&online)
            .iter()
            .map(|n| text(&cpu.join(format!("cpu{n}/topology/physical_package_id"))))
            .collect();
        let mut caches = Vec::new();
        for index in 0..16 {
            let dir = cpu.join(format!("cpu0/cache/index{index}"));
            if !dir.is_dir() {
                break;
            }
            let files = [
                "level",
                "type",
                "size",
                "ways_of_associativity",
                "coherency_line_size",
                "number_of_sets",
                "shared_cpu_list",
            ];
            caches.push(
                files
                    .iter()
                    .map(|file| ((*file).to_owned(), text(&dir.join(file))))
                    .collect(),
            );
        }
        let vulnerabilities = std::fs::read_dir(cpu.join("vulnerabilities"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        (name, text(&entry.path()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Facts {
            arch: std::env::consts::ARCH.to_owned(),
            cpuid: read_cpuid(),
            cpuinfo: std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default(),
            caches,
            online,
            siblings: text(&cpu.join("cpu0/topology/thread_siblings_list")),
            packages,
            kernel: text(std::path::Path::new("/proc/sys/kernel/osrelease")),
            vulnerabilities,
        }
    }

    /// The fingerprint's `host` object.
    pub(crate) fn describe(&self) -> Value {
        let (cpu, features, tlb, virtualised) = if self.cpuid.is_empty() {
            (
                self.cpu_from_cpuinfo(),
                self.features_from_cpuinfo(),
                Value::Null,
                self.hypervisor_from_cpuinfo(),
            )
        } else {
            (
                self.cpu_from_cpuid(),
                self.features_from_cpuid(),
                self.tlb(),
                self.hypervisor_from_cpuid(),
            )
        };
        Value::object([
            ("arch", Value::str(&self.arch)),
            ("cpu", cpu),
            ("features", features),
            ("caches", self.cache_list()),
            ("tlb", tlb),
            ("topology", self.topology()),
            ("hypervisor", virtualised),
            ("kernel", Value::str(squeeze(&self.kernel))),
            (
                "vulnerabilities",
                Value::object(
                    self.vulnerabilities
                        .iter()
                        .map(|(name, text)| (name.clone(), Value::str(squeeze(text)))),
                ),
            ),
        ])
    }

    /// One leaf's registers, zero where it was not read.
    fn leaf(&self, leaf: u32, subleaf: u32) -> Registers {
        self.cpuid
            .get(&(leaf, subleaf))
            .copied()
            .unwrap_or_default()
    }

    /// Vendor, family, model and stepping by CPUID, the brand string, and
    /// the microcode revision Linux reports.
    fn cpu_from_cpuid(&self) -> Value {
        let zero = self.leaf(0, 0);
        let vendor = text_of(&[zero[EBX], zero[EDX], zero[ECX]]);
        let signature = self.leaf(1, 0)[EAX];
        let base_family = (signature >> 8) & 0xF;
        let base_model = (signature >> 4) & 0xF;
        let family = if base_family == 0xF {
            base_family + ((signature >> 20) & 0xFF)
        } else {
            base_family
        };
        let model = if base_family == 0x6 || base_family == 0xF {
            base_model + (((signature >> 16) & 0xF) << 4)
        } else {
            base_model
        };
        let brand: Vec<u32> = (0x8000_0002..=0x8000_0004)
            .flat_map(|leaf| self.leaf(leaf, 0))
            .collect();
        Value::object([
            ("vendor", Value::str(vendor)),
            ("family", Value::int(family)),
            ("model", Value::int(model)),
            ("stepping", Value::int(signature & 0xF)),
            ("brand", Value::str(squeeze(&text_of(&brand)))),
            ("microcode", self.microcode()),
        ])
    }

    /// The first processor's `microcode`, as lower-case hex without leading
    /// zeros, or `null` where the kernel does not say.
    fn microcode(&self) -> Value {
        cpuinfo_field(&self.cpuinfo, "microcode")
            .and_then(|raw| {
                let digits = raw.trim_start_matches("0x").trim_start_matches("0X");
                u64::from_str_radix(digits, 16).ok()
            })
            .map_or(Value::Null, |revision| Value::str(format!("{revision:#x}")))
    }

    /// Each feature of [`X86_FEATURES`], present or not.
    fn features_from_cpuid(&self) -> Value {
        let max_basic = self.leaf(0, 0)[EAX];
        let max_extended = self.leaf(0x8000_0000, 0)[EAX];
        Value::object(
            X86_FEATURES
                .iter()
                .map(|&(name, leaf, subleaf, register, bit)| {
                    let in_range = if leaf >= 0x8000_0000 {
                        leaf <= max_extended
                    } else {
                        leaf <= max_basic
                    };
                    let word = self.leaf(leaf, subleaf).get(register).copied().unwrap_or(0);
                    (name, Value::Bool(in_range && word >> bit & 1 == 1))
                }),
        )
    }

    /// The TLBs: AMD's leaves decoded (4 KiB and 2 MiB pages, each level),
    /// Intel's leaf 0x18 kept as its raw words, since its encoding is a
    /// list of descriptors rather than fixed fields.
    fn tlb(&self) -> Value {
        let max_extended = self.leaf(0x8000_0000, 0)[EAX];
        if max_extended >= 0x8000_0006 && self.leaf(0x8000_0005, 0) != [0; 4] {
            let l1 = self.leaf(0x8000_0005, 0);
            let l2 = self.leaf(0x8000_0006, 0);
            let first = |word: u32| {
                Value::object([
                    ("data_entries", Value::int((word >> 16) & 0xFF)),
                    ("data_ways", Value::int(word >> 24)),
                    ("code_entries", Value::int(word & 0xFF)),
                    ("code_ways", Value::int((word >> 8) & 0xFF)),
                ])
            };
            let second = |word: u32| {
                Value::object([
                    ("data_entries", Value::int((word >> 16) & 0xFFF)),
                    ("data_ways_code", Value::int(word >> 28)),
                    ("code_entries", Value::int(word & 0xFFF)),
                    ("code_ways_code", Value::int((word >> 12) & 0xF)),
                ])
            };
            return Value::object([
                ("l1_4k", first(l1[EBX])),
                ("l1_2m", first(l1[EAX])),
                ("l2_4k", second(l2[EBX])),
                ("l2_2m", second(l2[EAX])),
            ]);
        }
        let raw: Vec<Value> = self
            .cpuid
            .iter()
            .filter(|((leaf, _), _)| *leaf == 0x18)
            .map(|((_, subleaf), words)| {
                Value::object([
                    ("subleaf", Value::int(*subleaf)),
                    (
                        "words",
                        Value::List(
                            words
                                .iter()
                                .map(|w| Value::str(format!("{w:#010x}")))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect();
        if raw.is_empty() {
            Value::Null
        } else {
            Value::object([("intel_leaf_18", Value::List(raw))])
        }
    }

    /// Whether this host is itself a virtual machine, and whose.
    fn hypervisor_from_cpuid(&self) -> Value {
        if self.leaf(1, 0)[ECX] >> 31 & 1 == 0 {
            return Value::Null;
        }
        let leaf = self.leaf(0x4000_0000, 0);
        Value::str(squeeze(&text_of(&[leaf[EBX], leaf[ECX], leaf[EDX]])))
    }

    /// An Arm (or other) host's processor, from the fields Linux names.
    fn cpu_from_cpuinfo(&self) -> Value {
        let field = |name: &str| {
            cpuinfo_field(&self.cpuinfo, name).map_or(Value::Null, |v| Value::str(squeeze(v)))
        };
        Value::object([
            ("implementer", field("CPU implementer")),
            ("architecture", field("CPU architecture")),
            ("variant", field("CPU variant")),
            ("part", field("CPU part")),
            ("revision", field("CPU revision")),
            ("vendor", field("vendor_id")),
            ("brand", field("model name")),
            ("microcode", self.microcode()),
            ("distinct_parts", self.distinct_parts()),
        ])
    }

    /// A big.LITTLE host's every `(implementer, part)` pair, which processor
    /// 0's block alone would hide.
    fn distinct_parts(&self) -> Value {
        let mut parts = BTreeSet::new();
        let mut implementer = String::new();
        for line in self.cpuinfo.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            match key.trim() {
                "CPU implementer" => value.trim().clone_into(&mut implementer),
                "CPU part" => {
                    let _ = parts.insert(format!("{implementer}:{}", value.trim()));
                }
                _ => {}
            }
        }
        Value::List(parts.into_iter().map(Value::Str).collect())
    }

    /// `Features` (Arm) or `flags` (anything else), sorted, as `true`.
    fn features_from_cpuinfo(&self) -> Value {
        let words = cpuinfo_field(&self.cpuinfo, "Features")
            .or_else(|| cpuinfo_field(&self.cpuinfo, "flags"))
            .unwrap_or_default();
        let names: BTreeSet<&str> = words.split_whitespace().collect();
        Value::object(names.into_iter().map(|name| (name, Value::Bool(true))))
    }

    /// The hypervisor a non-x86 host says it runs under, if it says.
    fn hypervisor_from_cpuinfo(&self) -> Value {
        let path = std::path::Path::new("/sys/hypervisor/type");
        std::fs::read_to_string(path)
            .ok()
            .filter(|_| self.cpuid.is_empty() && !cfg!(test))
            .map_or(Value::Null, |text| Value::str(squeeze(&text)))
    }

    /// Processor 0's caches, by level and type.
    fn cache_list(&self) -> Value {
        let mut caches: Vec<Value> = self
            .caches
            .iter()
            .map(|cache| {
                let number = |file: &str| {
                    cache
                        .get(file)
                        .and_then(|text| text.trim().parse::<u64>().ok())
                        .map_or(Value::Null, Value::int)
                };
                Value::object([
                    ("level", number("level")),
                    (
                        "type",
                        Value::str(cache.get("type").map_or("", |t| t.trim())),
                    ),
                    (
                        "size_kib",
                        cache.get("size").map_or(Value::Null, |size| kib(size)),
                    ),
                    ("ways", number("ways_of_associativity")),
                    ("line", number("coherency_line_size")),
                    ("sets", number("number_of_sets")),
                    (
                        "shared_by",
                        Value::int(cache.get("shared_cpu_list").map_or(0, |l| list(l).len())),
                    ),
                ])
            })
            .collect();
        caches.sort_by_key(Value::canonical);
        Value::List(caches)
    }

    /// Processors, cores, threads a core and packages.
    fn topology(&self) -> Value {
        let logical = list(&self.online).len();
        let threads = list(&self.siblings).len().max(1);
        let packages: BTreeSet<&str> = self.packages.iter().map(String::as_str).collect();
        Value::object([
            ("logical", Value::int(logical)),
            ("threads_per_core", Value::int(threads)),
            ("cores", Value::int(logical / threads)),
            ("packages", Value::int(packages.len().max(1))),
        ])
    }
}

/// Every CPUID leaf a fingerprint reads, on an x86-64 host.
#[cfg(target_arch = "x86_64")]
fn read_cpuid() -> BTreeMap<(u32, u32), Registers> {
    use std::arch::x86_64::__cpuid_count;
    let mut leaves = BTreeMap::new();
    let mut read = |leaf: u32, subleaf: u32| {
        let r = __cpuid_count(leaf, subleaf);
        let words = [r.eax, r.ebx, r.ecx, r.edx];
        let _ = leaves.insert((leaf, subleaf), words);
        words
    };
    let max_basic = read(0, 0)[EAX];
    for (leaf, subleaf) in [(1, 0), (7, 0), (7, 1), (0xD, 1)] {
        if leaf <= max_basic {
            let _ = read(leaf, subleaf);
        }
    }
    if max_basic >= 0x18 {
        let subleaves = read(0x18, 0)[EAX];
        for subleaf in 1..=subleaves.min(16) {
            let _ = read(0x18, subleaf);
        }
    }
    if read(1, 0)[ECX] >> 31 & 1 == 1 {
        let _ = read(0x4000_0000, 0);
    }
    let max_extended = read(0x8000_0000, 0)[EAX];
    for leaf in [
        0x8000_0001,
        0x8000_0002,
        0x8000_0003,
        0x8000_0004,
        0x8000_0005,
        0x8000_0006,
        0x8000_0007,
        0x8000_0008,
        0x8000_0021,
    ] {
        if leaf <= max_extended {
            let _ = read(leaf, 0);
        }
    }
    leaves
}

/// No CPUID off x86-64: [`Facts::describe`] reads `/proc/cpuinfo` instead.
#[cfg(not(target_arch = "x86_64"))]
fn read_cpuid() -> BTreeMap<(u32, u32), Registers> {
    BTreeMap::new()
}

/// The ASCII text of little-endian register words, NULs dropped.
fn text_of(words: &[u32]) -> String {
    words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .filter(|&byte| byte != 0)
        .map(char::from)
        .collect()
}

/// Runs of white space as one space, none at either end.
pub(crate) fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first `name : value` line of `/proc/cpuinfo`'s text.
fn cpuinfo_field<'a>(cpuinfo: &'a str, name: &str) -> Option<&'a str> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == name).then_some(value.trim())
    })
}

/// A sysfs list such as `0-3,8,10-11`, expanded.
fn list(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        let (from, to) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(from), Ok(to)) = (from.trim().parse::<u32>(), to.trim().parse::<u32>()) {
            out.extend(from..=to.min(from.saturating_add(4096)));
        }
    }
    out
}

/// A sysfs size such as `48K` or `1M`, in KiB.
fn kib(size: &str) -> Value {
    let size = size.trim();
    let (digits, scale) = match size.strip_suffix('K') {
        Some(digits) => (digits, 1),
        None => match size.strip_suffix('M') {
            Some(digits) => (digits, 1024),
            None => (size, 0),
        },
    };
    match digits.parse::<u64>() {
        Ok(number) if scale > 0 => Value::int(number * scale),
        Ok(bytes) => Value::int(bytes / 1024),
        Err(_) => Value::Null,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// nazuna's CPUID as read on 2026-10-06 (Ryzen 9 9900X, Zen 5, microcode
    /// 0xb404035), the leaves a fingerprint reads; the brand string is in
    /// `0x8000_0002..=0x8000_0004`, and the caches are its sysfs.
    pub(crate) fn nazuna() -> Facts {
        let mut cpuid = BTreeMap::new();
        let brand = *b"AMD Ryzen 9 9900X 12-Core Processor            \0";
        let word = |at: usize| {
            u32::from_le_bytes([brand[at], brand[at + 1], brand[at + 2], brand[at + 3]])
        };
        for (index, leaf) in (0x8000_0002..=0x8000_0004u32).enumerate() {
            let at = index * 16;
            let _ = cpuid.insert(
                (leaf, 0),
                [word(at), word(at + 4), word(at + 8), word(at + 12)],
            );
        }
        for (key, words) in [
            ((0, 0), [0x10, 0x6874_7541, 0x444D_4163, 0x6974_6E65]),
            ((1, 0), [0x00B4_0F40, 0x1818_0800, 0x7ED8_320B, 0x178B_FBFF]),
            ((7, 0), [0x1, 0xF1BF_97AB, 0x1940_5FDE, 0x1000_0110]),
            ((7, 1), [0x30, 0, 0, 0]),
            ((0xD, 1), [0xF, 0x9B0, 0x1800, 0]),
            (
                (0x8000_0000, 0),
                [0x8000_0028, 0x6874_7541, 0x444D_4163, 0x6974_6E65],
            ),
            ((0x8000_0001, 0), [0x00B4_0F40, 0, 0x75C2_37FF, 0x2FD3_FBFF]),
            (
                (0x8000_0005, 0),
                [0xFF60_FF40, 0xFF60_FF40, 0x300C_0140, 0x2008_0140],
            ),
            (
                (0x8000_0006, 0),
                [0x4080_2040, 0x6080_4040, 0x0400_8140, 0x0200_9140],
            ),
            ((0x8000_0007, 0), [0, 0x3B, 0, 0x6799]),
            ((0x8000_0008, 0), [0x3030, 0x791E_F257, 0x5017, 0x0001_0000]),
            ((0x8000_0021, 0), [0x593F_FFCF, 0x0008_0382, 0, 0]),
        ] {
            let _ = cpuid.insert(key, words);
        }
        let cache = |level: &str, kind: &str, size: &str, ways: &str, sets: &str, shared: &str| {
            [
                ("level", level),
                ("type", kind),
                ("size", size),
                ("ways_of_associativity", ways),
                ("coherency_line_size", "64"),
                ("number_of_sets", sets),
                ("shared_cpu_list", shared),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
        };
        Facts {
            arch: "x86_64".to_owned(),
            cpuid,
            cpuinfo: "processor\t: 0\nvendor_id\t: AuthenticAMD\nmicrocode\t: 0xb404035\n\
                      model name\t: AMD Ryzen 9 9900X 12-Core Processor\n"
                .to_owned(),
            caches: vec![
                cache("1", "Data", "48K", "12", "64", "0,12"),
                cache("1", "Instruction", "32K", "8", "64", "0,12"),
                cache("2", "Unified", "1024K", "16", "1024", "0,12"),
                cache("3", "Unified", "32768K", "16", "32768", "0-5,12-17"),
            ],
            online: "0-23".to_owned(),
            siblings: "0,12".to_owned(),
            packages: vec!["0".to_owned(); 24],
            kernel: "7.0.0-29-generic".to_owned(),
            vulnerabilities: [
                ("meltdown", "Not affected"),
                ("spectre_v2", "Mitigation: Enhanced / Automatic IBRS"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        }
    }

    #[test]
    fn nazuna_reads_as_zen_5_without_pcid() {
        let host = nazuna().describe();
        let cpu = host.get("cpu").unwrap();
        assert_eq!(cpu.get("vendor").unwrap().as_str(), Some("AuthenticAMD"));
        assert_eq!(cpu.get("family").unwrap().as_int(), Some(26), "0xF + 0xB");
        assert_eq!(cpu.get("model").unwrap().as_int(), Some(68), "0x44");
        assert_eq!(cpu.get("stepping").unwrap().as_int(), Some(0));
        assert_eq!(
            cpu.get("brand").unwrap().as_str(),
            Some("AMD Ryzen 9 9900X 12-Core Processor"),
            "trailing spaces squeezed"
        );
        assert_eq!(cpu.get("microcode").unwrap().as_str(), Some("0xb404035"));
        let features = host.get("features").unwrap();
        for (name, present) in [
            ("pcid", false),
            ("invpcid", true),
            ("eraps", true),
            ("auto_ibrs", true),
            ("fsgsbase", true),
            ("xgetbv1", true),
            ("tce", true),
            ("pku", true),
            ("hypervisor", false),
        ] {
            assert_eq!(
                features.get(name),
                Some(&Value::Bool(present)),
                "{name} should read {present}"
            );
        }
        let topology = host.get("topology").unwrap();
        assert_eq!(topology.get("logical").unwrap().as_int(), Some(24));
        assert_eq!(topology.get("cores").unwrap().as_int(), Some(12));
        assert_eq!(topology.get("threads_per_core").unwrap().as_int(), Some(2));
        let l1 = host.get("tlb").unwrap().get("l1_4k").unwrap();
        assert_eq!(l1.get("data_entries").unwrap().as_int(), Some(0x60));
        assert_eq!(host.get("hypervisor"), Some(&Value::Null));
    }

    #[test]
    fn a_leaf_past_the_maximum_reads_absent() {
        let mut facts = nazuna();
        let _ = facts.cpuid.insert((0x8000_0000, 0), [0x8000_0008, 0, 0, 0]);
        let host = facts.describe();
        assert_eq!(
            host.get("features").unwrap().get("eraps"),
            Some(&Value::Bool(false)),
            "0x8000_0021 is past a maximum of 0x8000_0008, whatever was read there"
        );
    }

    #[test]
    fn the_order_inputs_arrive_in_does_not_change_the_text() {
        let one = nazuna();
        let mut two = nazuna();
        two.caches.reverse();
        two.packages.reverse();
        "0,12 ".clone_into(&mut two.siblings);
        two.cpuinfo = two.cpuinfo.replace("\t: ", "   :   ");
        assert_eq!(one.describe().canonical(), two.describe().canonical());
    }

    #[test]
    fn nothing_identifying_reaches_the_text() {
        let facts = Facts {
            arch: "aarch64".to_owned(),
            cpuinfo: "processor\t: 0\nBogoMIPS\t: 48.00\nFeatures\t: fp asimd aes pmull\n\
                      CPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x1\n\
                      CPU part\t: 0xd44\nCPU revision\t: 0\n\nprocessor\t: 1\n\
                      CPU implementer\t: 0x41\nCPU part\t: 0xd05\n\
                      Hardware\t: BCM2835\nRevision\t: c03111\nSerial\t\t: 10000000deadbeef\n\
                      Model\t\t: Raspberry Pi 4 Model B\n"
                .to_owned(),
            online: "0-1".to_owned(),
            siblings: "0".to_owned(),
            kernel: "6.1.0-rpi7".to_owned(),
            ..Facts::default()
        };
        let text = facts.describe().canonical();
        for secret in ["deadbeef", "BCM2835", "c03111", "Raspberry"] {
            assert!(!text.contains(secret), "{secret} leaked into {text}");
        }
        assert!(
            text.contains("\"part\":\"0xd44\""),
            "the part is kept: {text}"
        );
        assert!(text.contains("0x41:0xd05"), "every distinct part: {text}");
        assert!(text.contains("\"asimd\":true"), "features kept: {text}");
    }

    #[test]
    fn lists_and_sizes_parse() {
        assert_eq!(list("0-3,8,10-11"), vec![0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(list(""), Vec::<u32>::new());
        assert_eq!(kib("48K"), Value::Int(48));
        assert_eq!(kib("32M"), Value::Int(32768));
        assert_eq!(kib("x"), Value::Null);
    }
}
