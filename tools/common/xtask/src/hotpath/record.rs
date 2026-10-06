//! The record of one measurement of a hot path (`docs/HOTPATHS.md` §6).
//!
//! A record is keyed by four things, and its file name and directory carry
//! all four: the hot path, the hardware's fingerprint hash, the Ferrix
//! commit, and the configuration's hash:
//!
//! ```text
//! docs/hotpaths/results/<path>/<hw hash, 12>/<UTC time>-<commit, 12>-<configuration hash, 8>.json
//! ```
//!
//! It holds the fingerprint whole, so a reader can check the directory's
//! hash against it, each run's summary statistics as the benchmark printed
//! them (not the raw samples: `ipc-bench` prints n, min, p50, p90, p99 and
//! mean of its 20,000), the host's load and SMT sibling for each run, the
//! kernel's own account of the processor it found, and the log's path.

use std::path::{Path, PathBuf};

use super::Fingerprint;
use super::json::Value;
use crate::args::{Args, Mitigations};
use crate::{Error, Result};

/// What a record's text says it is.
pub(crate) const RESULT_SCHEMA: &str = "ferrix-hotpath-result/1";

/// Where records live, under the workspace.
pub(crate) const RESULTS: &str = "docs/hotpaths/results";

/// One boot of a benchmark, as its output read.
#[derive(Debug, Clone, Default)]
pub(crate) struct Boot {
    /// Every line of the boot's output.
    pub(crate) lines: Vec<String>,
}

impl Boot {
    /// The statistics of each `<prefix> <name> n=.. min=.. p50=..` line,
    /// by name; the first line of a name wins, as `ipc.rs`'s `field` reads.
    pub(crate) fn stats(&self, prefix: &str) -> Value {
        let mut found = std::collections::BTreeMap::new();
        for line in &self.lines {
            let Some((_, rest)) = line.split_once(&format!("{prefix} ")) else {
                continue;
            };
            let mut words = rest.split_whitespace();
            let Some(name) = words.next() else { continue };
            let numbers: Vec<(String, Value)> = words
                .filter_map(|word| {
                    let (key, value) = word.split_once('=')?;
                    Some((key.to_owned(), Value::int(value.parse::<u64>().ok()?)))
                })
                .collect();
            if !numbers.is_empty() && !found.contains_key(name) {
                let _ = found.insert(name.to_owned(), Value::object(numbers));
            }
        }
        Value::Object(found)
    }

    /// The host line: load before and after, the SMT sibling's share.
    pub(crate) fn host(&self) -> Value {
        self.lines
            .iter()
            .find_map(|line| line.split_once("host load ").map(|(_, rest)| rest.trim()))
            .map_or(Value::Null, |rest| Value::str(format!("host load {rest}")))
    }

    /// The kernel's lines about the processor it found and what it turned
    /// on: each boot-log `cpu` line that states something (`a: b`), without
    /// the counts that differ run to run.
    pub(crate) fn kernel_view(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for line in &self.lines {
            let text = line
                .split_once("| ")
                .map_or(line.as_str(), |(_, text)| text)
                .trim();
            let Some(rest) = text.strip_prefix("cpu ") else {
                continue;
            };
            let statement = format!("cpu {}", rest.trim());
            if statement.contains(':') && !seen.contains(&statement) {
                seen.push(statement);
            }
        }
        seen
    }
}

/// What a benchmark is compared against within its own run, if anything.
#[derive(Debug, Clone)]
pub(crate) enum Reference {
    /// Nothing: one tree, measured alone.
    None,
    /// Another Ferrix tree, turn about (`--alternate`).
    Tree {
        /// The ref as given.
        name: String,
        /// Its commit.
        commit: String,
        /// Its boots, in round order.
        boots: Vec<Boot>,
    },
    /// Another kernel's figure, turn about (`--against-sel4`).
    Kernel {
        /// What it is.
        name: String,
        /// Its p50 in ns, a round each.
        p50_ns: Vec<u64>,
    },
}

/// A measurement ready to be written.
#[derive(Debug, Clone)]
pub(crate) struct Measurement<'a> {
    /// The hot path's id: a directory under `docs/hotpaths/`.
    pub(crate) path: &'a str,
    /// The benchmark's line prefix (`ipc-bench`).
    pub(crate) prefix: &'a str,
    /// The figure of merit's line (`domain-call`), whose p50 is the figure.
    pub(crate) figure: &'a str,
    /// This tree's boots, in round order.
    pub(crate) boots: Vec<Boot>,
    /// What they were compared with.
    pub(crate) reference: Reference,
}

/// Build the record's value. `taken` is the UTC time, `commit` this tree's
/// and whether it had changes outside `docs/hotpaths/results`.
pub(crate) fn value(
    measurement: &Measurement<'_>,
    fingerprint: &Fingerprint,
    configuration: Value,
    ferrix: Value,
    taken: &str,
    log: Option<&str>,
) -> Value {
    let runs: Vec<Value> = measurement
        .boots
        .iter()
        .map(|boot| run(boot, measurement.prefix))
        .collect();
    let here = p50s(&measurement.boots, measurement.prefix, measurement.figure);
    let (reference, theirs) = match &measurement.reference {
        Reference::None => (Value::Null, Vec::new()),
        Reference::Tree {
            name,
            commit,
            boots,
        } => (
            Value::object([
                ("kind", Value::str("ferrix tree")),
                ("ref", Value::str(name)),
                ("commit", Value::str(commit)),
                (
                    "runs",
                    Value::List(boots.iter().map(|b| run(b, measurement.prefix)).collect()),
                ),
            ]),
            p50s(boots, measurement.prefix, measurement.figure),
        ),
        Reference::Kernel { name, p50_ns } => (
            Value::object([
                ("kind", Value::str("other kernel")),
                ("name", Value::str(name)),
                (
                    "p50_ns",
                    Value::List(p50_ns.iter().map(|&n| Value::int(n)).collect()),
                ),
            ]),
            p50_ns.iter().map(|&n| Some(n)).collect(),
        ),
    };
    let kernel_view = measurement
        .boots
        .first()
        .map(Boot::kernel_view)
        .unwrap_or_default();
    Value::object([
        ("schema", Value::str(RESULT_SCHEMA)),
        ("path", Value::str(measurement.path)),
        (
            "figure",
            Value::object([
                ("line", Value::str(measurement.figure)),
                ("statistic", Value::str("p50")),
                ("unit", Value::str("ns")),
            ]),
        ),
        ("hw_hash", Value::str(&fingerprint.hw_hash)),
        ("host_hash", Value::str(&fingerprint.host_hash)),
        ("cpu_hash", Value::str(&fingerprint.cpu_hash)),
        ("hardware", fingerprint.value.clone()),
        ("ferrix", ferrix),
        (
            "configuration_hash",
            Value::str(super::hash(&configuration)),
        ),
        ("configuration", configuration),
        ("runs", Value::List(runs)),
        ("reference", reference),
        ("summary", summary(&here, &theirs)),
        (
            "kernel_view",
            Value::List(kernel_view.into_iter().map(Value::Str).collect()),
        ),
        ("log", log.map_or(Value::Null, Value::str)),
        ("taken", Value::str(taken)),
    ])
}

/// One run: its statistics and its host line.
fn run(boot: &Boot, prefix: &str) -> Value {
    Value::object([("stats", boot.stats(prefix)), ("host", boot.host())])
}

/// Each boot's p50 of `figure`.
fn p50s(boots: &[Boot], prefix: &str, figure: &str) -> Vec<Option<u64>> {
    boots
        .iter()
        .map(|boot| {
            boot.stats(prefix)
                .get(figure)
                .and_then(|stats| stats.get("p50"))
                .and_then(Value::as_int)
                .and_then(|n| u64::try_from(n).ok())
        })
        .collect()
}

/// The median of the figure over the rounds, and, against a reference, the
/// median, least and most of the per-round ratios in thousandths, as
/// `ipc.rs`'s `ratio` prints them.
fn summary(here: &[Option<u64>], there: &[Option<u64>]) -> Value {
    let mut mine: Vec<u64> = here.iter().flatten().copied().collect();
    mine.sort_unstable();
    let mut ratios: Vec<u64> = here
        .iter()
        .zip(there)
        .filter_map(|(here, there)| Some(((*here)?, (*there)?)))
        .filter(|&(_, there)| there > 0)
        .map(|(here, there)| (here * 1000 + there / 2) / there)
        .collect();
    ratios.sort_unstable();
    let mut theirs: Vec<u64> = there.iter().flatten().copied().collect();
    theirs.sort_unstable();
    let median = |sorted: &[u64]| {
        sorted
            .get(sorted.len() / 2)
            .map_or(Value::Null, |&n| Value::int(n))
    };
    Value::object([
        ("rounds", Value::int(here.len())),
        ("p50_ns_median", median(&mine)),
        ("reference_p50_ns_median", median(&theirs)),
        (
            "ratio_milli",
            if ratios.is_empty() {
                Value::Null
            } else {
                Value::object([
                    ("median", median(&ratios)),
                    (
                        "least",
                        ratios.first().map_or(Value::Null, |&n| Value::int(n)),
                    ),
                    (
                        "most",
                        ratios.last().map_or(Value::Null, |&n| Value::int(n)),
                    ),
                ])
            },
        ),
    ])
}

/// The configuration of a `bench-ipc` run: everything on its command line
/// that changes what is measured. `--init`'s program by file name only: its
/// directory is a user's home.
pub(crate) fn ipc_configuration(args: &Args, rounds: u32, alternate: Option<&str>) -> Value {
    let init = args
        .init
        .as_deref()
        .map(|init| init.rsplit(['/', '\\']).next().unwrap_or(init).to_owned());
    Value::object([
        ("command", Value::str("bench-ipc")),
        ("release", Value::Bool(args.release)),
        (
            "accel",
            args.accel.as_deref().map_or(Value::Null, Value::str),
        ),
        ("smp", Value::int(args.smp)),
        ("memory_mib", Value::int(args.memory)),
        ("pin", Value::str(args.pin.as_deref().unwrap_or("none"))),
        (
            "mitigations",
            Value::str(if args.mitigations == Mitigations::Off {
                "off"
            } else {
                "on"
            }),
        ),
        (
            "kernel_options",
            Value::List(args.kernel_options.iter().map(Value::str).collect()),
        ),
        ("init", init.map_or(Value::Null, Value::Str)),
        ("rounds", Value::int(rounds)),
        ("alternate", alternate.map_or(Value::Null, Value::str)),
        ("against_sel4", Value::Bool(args.against_sel4)),
    ])
}

/// This tree's commit, and whether anything outside the results directory
/// differs from it.
pub(crate) fn ferrix_commit(root: &Path) -> Result<Value> {
    let git = |arguments: &[&str]| -> Result<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .output()
            .map_err(|error| Error::new(format!("could not run git: {error}")))?;
        if !output.status.success() {
            return Err(Error::new(format!("git {} failed", arguments.join(" "))));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "HEAD"])?;
    let changed = git(&["status", "--porcelain", "--", ".", &format!(":!{RESULTS}")])?;
    Ok(Value::object([
        ("commit", Value::str(commit)),
        ("dirty", Value::Bool(!changed.is_empty())),
    ]))
}

/// Write `record` under the workspace root `root`, and say where.
pub(crate) fn write(root: &Path, record: &Value) -> Result<PathBuf> {
    let field = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or_default();
    let commit = record
        .get("ferrix")
        .and_then(|ferrix| ferrix.get("commit"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let directory = root
        .join(RESULTS)
        .join(field("path"))
        .join(short(field("hw_hash"), super::SHORT));
    let name = format!(
        "{}-{}-{}.json",
        field("taken").replace([':', '-'], ""),
        short(commit, 12),
        short(field("configuration_hash"), 8)
    );
    std::fs::create_dir_all(&directory)?;
    let file = directory.join(name);
    std::fs::write(&file, record.pretty())?;
    Ok(file)
}

/// `path` with the home directory `home` written `~`, so that no user name
/// reaches a record (`docs/HOTPATHS.md` §5, privacy).
pub(crate) fn without_home(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_owned(),
    }
}

/// The first `count` characters of a hex hash.
fn short(text: &str, count: usize) -> &str {
    text.get(..count).unwrap_or(text)
}

/// Now, in UTC, as `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn utc_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    utc(seconds)
}

/// `seconds` since 1970 as `YYYY-MM-DDTHH:MM:SSZ` (the civil calendar from
/// a day count, Howard Hinnant's `civil_from_days`).
fn utc(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0) + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Check every record under `root`'s results directory, from the unit test
/// that holds the tree's own records to it: it parses, names its schema, sits in the directory its path and hash name, and its
/// hardware hashes to the hash it states. The problems, one line each.
#[cfg(test)]
pub(crate) fn check_results(root: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let mut files = Vec::new();
    collect_json(&root.join(RESULTS), &mut files);
    for file in files {
        let shown = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let record = match Value::parse(&text) {
            Ok(record) => record,
            Err(why) => {
                problems.push(format!("{shown}: not JSON in the canonical limits: {why}"));
                continue;
            }
        };
        problems.extend(
            check_record(&record, &file)
                .into_iter()
                .map(|p| format!("{shown}: {p}")),
        );
    }
    problems
}

/// One record's problems.
#[cfg(test)]
fn check_record(record: &Value, file: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let text = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or_default();
    if text("schema") != RESULT_SCHEMA {
        problems.push(format!("schema is not {RESULT_SCHEMA}"));
    }
    let hardware = record.get("hardware").cloned().unwrap_or(Value::Null);
    if super::hash(&hardware) != text("hw_hash") {
        problems.push("hw_hash is not the SHA-256 of its hardware".to_owned());
    }
    let configuration = record.get("configuration").cloned().unwrap_or(Value::Null);
    if super::hash(&configuration) != text("configuration_hash") {
        problems.push("configuration_hash is not the SHA-256 of its configuration".to_owned());
    }
    let mut parents = file.ancestors().skip(1);
    let directory = parents
        .next()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str());
    let path = parents
        .next()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str());
    if directory != Some(short(text("hw_hash"), super::SHORT)) {
        problems.push("filed under another hardware hash than its own".to_owned());
    }
    if path != Some(text("path")) {
        problems.push("filed under another hot path than its own".to_owned());
    }
    for key in ["ferrix", "runs", "summary", "taken", "figure"] {
        if record.get(key).is_none() {
            problems.push(format!("no `{key}`"));
        }
    }
    problems
}

/// Every `.json` file below `directory`.
#[cfg(test)]
fn collect_json(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_json(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::host;
    use super::*;

    /// A boot's output as `bench-ipc` prints it (po7-ipcM's eraps-alt-1).
    fn boot(domain_p50: u64) -> Boot {
        let lines = [
            "    0.75 |   cpu      ring 0 kept out of user pages: SMEP on, SMAP on".to_owned(),
            "    1.07 |   cpu      speculation defences: AutoIBRS, STIBP, SSBD".to_owned(),
            "    4.46 |   cpu      speculation defences read back on 1 processors, 392 switch barriers".to_owned(),
            "    6.71 | ipc-bench floor n=20000 min=389 p50=399 p90=399 p99=399 mean=407".to_owned(),
            format!("    7.55 | ipc-bench domain-call n=20000 min=2736 p50={domain_p50} p90=18907 p99=20055 mean=5805"),
            "  x86_64: host load 1.30 before, 1.27 after, SMT sibling cpu23 0% busy".to_owned(),
            format!("  x86_64: ipc-bench domain-call n=20000 min=1 p50={domain_p50} p90=1 p99=1 mean=1"),
            "  x86_64: ipc-bench: exit 0".to_owned(),
        ];
        Boot {
            lines: lines.to_vec(),
        }
    }

    #[test]
    fn a_boot_reads_into_its_parts() {
        let boot = boot(3046);
        let stats = boot.stats("ipc-bench");
        let domain = stats.get("domain-call").unwrap();
        assert_eq!(domain.get("p50"), Some(&Value::Int(3046)));
        assert_eq!(
            domain.get("min"),
            Some(&Value::Int(2736)),
            "the first line of a name wins"
        );
        assert_eq!(
            stats.get("floor").unwrap().get("mean"),
            Some(&Value::Int(407))
        );
        assert_eq!(
            stats.get("exit"),
            None,
            "`ipc-bench: exit 0` is no statistic"
        );
        assert_eq!(
            boot.host(),
            Value::str("host load 1.30 before, 1.27 after, SMT sibling cpu23 0% busy")
        );
        assert_eq!(
            boot.kernel_view(),
            vec![
                "cpu ring 0 kept out of user pages: SMEP on, SMAP on".to_owned(),
                "cpu speculation defences: AutoIBRS, STIBP, SSBD".to_owned(),
            ],
            "only statements, counts left out"
        );
    }

    #[test]
    fn ratios_are_thousandths_median_least_most() {
        let here = [Some(2287), Some(2716), Some(2300)];
        let there = [Some(2427), Some(2427), Some(2400)];
        let both = summary(&here, &there);
        assert_eq!(both.get("p50_ns_median"), Some(&Value::Int(2300)));
        let ratio = both.get("ratio_milli").unwrap();
        assert_eq!(ratio.get("least"), Some(&Value::Int(942)), "2287/2427");
        assert_eq!(ratio.get("median"), Some(&Value::Int(958)), "2300/2400");
        assert_eq!(ratio.get("most"), Some(&Value::Int(1119)), "2716/2427");
        assert_eq!(summary(&here, &[]).get("ratio_milli"), Some(&Value::Null));
    }

    #[test]
    fn a_log_under_home_loses_the_user_name() {
        assert_eq!(
            without_home("/home/ann/logs/a.log", "/home/ann"),
            "~/logs/a.log"
        );
        assert_eq!(
            without_home("/home/ann/logs/a.log", "/home/ann/"),
            "~/logs/a.log"
        );
        assert_eq!(
            without_home("/home/annie/a.log", "/home/ann"),
            "/home/annie/a.log"
        );
        assert_eq!(without_home("/srv/a.log", ""), "/srv/a.log");
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_791_283_454), "2026-10-06T10:44:14Z");
    }

    #[test]
    fn a_written_record_checks_clean_and_a_moved_one_does_not() {
        let root = std::env::temp_dir().join(format!("ferrix-hotpath-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fingerprint = Fingerprint::new(host::tests::nazuna().describe(), Value::Null);
        let measurement = Measurement {
            path: "ipc-round-trip",
            prefix: "ipc-bench",
            figure: "domain-call",
            boots: vec![boot(2287), boot(2300)],
            reference: Reference::Tree {
                name: "main".to_owned(),
                commit: "de0eb7b32".to_owned(),
                boots: vec![boot(2427), boot(2400)],
            },
        };
        let ferrix = Value::object([
            ("commit", Value::str("a1d456820abc")),
            ("dirty", Value::Bool(false)),
        ]);
        let configuration = ipc_configuration(&Args::default(), 2, Some("main"));
        let record = value(
            &measurement,
            &fingerprint,
            configuration,
            ferrix,
            "2026-10-06T10:44:14Z",
            None,
        );
        let file = write(&root, &record).unwrap();
        let name = file.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("20261006T104414Z-a1d456820abc-"), "{name}");
        assert_eq!(
            file.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            fingerprint.short()
        );
        assert_eq!(check_results(&root), Vec::<String>::new());
        assert_eq!(
            Value::parse(&std::fs::read_to_string(&file).unwrap()).unwrap(),
            record,
            "the file reads back as the record"
        );

        let moved = root.join(RESULTS).join("ipc-round-trip/000000000000");
        std::fs::create_dir_all(&moved).unwrap();
        let tampered = std::fs::read_to_string(&file)
            .unwrap()
            .replace("\"p50\": 2287", "\"p50\": 2286");
        std::fs::write(
            moved.join("x.json"),
            tampered.replace("\"smp\": 0", "\"smp\": 1"),
        )
        .unwrap();
        let problems = check_results(&root);
        assert!(
            problems.iter().any(|p| p.contains("another hardware hash")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("configuration_hash")),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The records in the tree itself (`docs/HOTPATHS.md` §6).
    #[test]
    fn the_trees_records_check_clean() {
        let root = crate::paths::workspace_root();
        assert_eq!(check_results(&root), Vec::<String>::new());
    }
}
