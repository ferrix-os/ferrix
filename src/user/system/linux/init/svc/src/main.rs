//! `/bin/svc`: the init's control client (`docs/INIT.md` §10).
//!
//! systemctl's verbs, spoken to `/run/ferrix/control` as the records of
//! `src/lib/init/svc-proto`. One call per connection; init answers, the last
//! answer final. What `svc` prints is its own business: the records carry
//! the facts, so this output can change without breaking another client.
//!
//! ```text
//! svc status [unit]      svc start|stop|restart|reload unit...
//! svc list [--failed]    svc enable|disable|mask|unmask unit...
//! svc log unit [-n N]    svc daemon-reload   svc isolate target
//! svc poweroff|reboot    svc reset-failed [unit]
//! svc scope --unit NAME [--slice SLICE] PID...
//! svc set-property unit Key=value... [--persistent]
//! svc top
//! ```
//!
//! Under the names systemctl also answers to, it is that verb:
//! `/bin/poweroff` and `/bin/reboot` are links to it, and `/bin/shutdown`
//! is `poweroff`, or `reboot` with `-r`. Nothing here schedules, so
//! `shutdown` takes `now` or no time at all, and goes at once either way.
//!
//! Exit status as systemctl's: 0 for success, 1 for a failure or a
//! refusal, 3 from `status` for a unit that is not active, 4 for one that
//! is not loaded, and 2 for a command line `svc` does not take.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use ferrix_audit::{NO_UID, RECORD_BYTES, Record};
use ferrix_svc_proto::control::{Answer, Call, Framer, SOCKET, UnitStatus};

/// What went wrong, and the exit status for it.
#[derive(Debug)]
struct Failure {
    status: u8,
    message: String,
}

impl Failure {
    fn new(status: u8, message: impl Into<String>) -> Failure {
        Failure {
            status,
            message: message.into(),
        }
    }
}

/// The usage, for a command line `svc` does not take.
const USAGE: &str = "usage: svc status [unit] | list [--failed] | start|stop|restart|reload unit... \
| isolate target | reset-failed [unit] | poweroff | reboot | log unit [-n N] | daemon-reload \
| enable|disable|mask|unmask unit... | set-property unit Key=value... [--persistent] \
| scope --unit NAME [--slice SLICE] PID... | top | audit";

/// The names `svc` is linked under, each of which is a verb of its own.
const NAMES: [&str; 3] = ["poweroff", "reboot", "shutdown"];

/// The usage under those names.
const POWER_USAGE: &str = "usage: poweroff | reboot | shutdown [-h|-P|-r] [now]";

fn main() -> ExitCode {
    let mut args = std::env::args();
    let zero = args.next().unwrap_or_default();
    let args: Vec<String> = args.collect();
    let name = zero.rsplit('/').next().unwrap_or_default();
    let (name, result) = if NAMES.contains(&name) {
        (
            name,
            named(name, &args).and_then(|verb| run(&[verb.to_owned()])),
        )
    } else {
        ("svc", run(&args))
    };
    match result {
        Ok(status) => ExitCode::from(status),
        Err(failure) => {
            let _ = writeln!(io::stderr(), "{name}: {}", failure.message);
            ExitCode::from(failure.status)
        }
    }
}

/// The verb `svc` run as `name` with `args` stands for.
fn named(name: &str, args: &[String]) -> Result<&'static str, Failure> {
    if name != "shutdown" {
        return match args {
            [] if name == "reboot" => Ok("reboot"),
            [] => Ok("poweroff"),
            _ => Err(Failure::new(2, POWER_USAGE)),
        };
    }
    let mut verb = "poweroff";
    for arg in args {
        match arg.as_str() {
            "-h" | "-P" | "--poweroff" => verb = "poweroff",
            "-r" | "--reboot" => verb = "reboot",
            "now" | "+0" => {}
            time if time.starts_with('+') || time.contains(':') => {
                return Err(Failure::new(
                    1,
                    format!("{time}: nothing schedules a later shutdown; say now"),
                ));
            }
            _ => return Err(Failure::new(2, POWER_USAGE)),
        }
    }
    Ok(verb)
}

/// Ask init one thing and return every answer, the final one last.
fn ask(call: &Call) -> Result<Vec<Answer>, Failure> {
    exchange(connect()?, call)
}

/// A connection to init's control socket.
fn connect() -> Result<UnixStream, Failure> {
    UnixStream::connect(SOCKET)
        .map_err(|error| Failure::new(1, format!("{SOCKET}: {error}; is init running?")))
}

/// Send `call` over `stream` and return every answer, the final one last.
fn exchange(mut stream: UnixStream, call: &Call) -> Result<Vec<Answer>, Failure> {
    stream
        .write_all(&call.encode())
        .map_err(|error| Failure::new(1, format!("writing to {SOCKET}: {error}")))?;
    let mut framer = Framer::new();
    let mut answers = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        while let Some(record) = framer
            .next_record()
            .map_err(|error| Failure::new(1, format!("init's answer: {error:?}")))?
        {
            let answer = Answer::decode(&record)
                .map_err(|error| Failure::new(1, format!("init's answer: {error:?}")))?;
            let last = answer.is_final();
            answers.push(answer);
            if last {
                return Ok(answers);
            }
        }
        let count = stream
            .read(&mut buffer)
            .map_err(|error| Failure::new(1, format!("reading {SOCKET}: {error}")))?;
        if count == 0 {
            return Err(Failure::new(
                1,
                "init closed the connection without answering",
            ));
        }
        framer.push(buffer.get(..count).unwrap_or_default());
    }
}

/// Print each note, and judge the final answer of a call that changes
/// something: 0 for `done`, 1 otherwise.
fn changed(what: &str, answers: &[Answer]) -> Result<u8, Failure> {
    let mut out = io::stdout().lock();
    for answer in answers {
        match answer {
            Answer::Note(note) => {
                let _ = writeln!(out, "{note}");
            }
            Answer::Done(result) if result == "done" => return Ok(0),
            Answer::Done(result) => {
                return Err(Failure::new(1, format!("{what}: {result}")));
            }
            Answer::Refused(why) => return Err(Failure::new(1, format!("{what}: {why}"))),
            Answer::Units(_) | Answer::Lines(_) => {}
        }
    }
    Err(Failure::new(1, format!("{what}: no answer")))
}

/// The final answer's units, or its refusal.
fn units(answers: Vec<Answer>) -> Result<Vec<UnitStatus>, Failure> {
    match answers.into_iter().last() {
        Some(Answer::Units(units)) => Ok(units),
        Some(Answer::Refused(why)) => Err(Failure::new(1, why)),
        other => Err(Failure::new(
            1,
            format!("an answer that is not units: {other:?}"),
        )),
    }
}

fn run(args: &[String]) -> Result<u8, Failure> {
    let (verb, rest) = args.split_first().ok_or_else(|| Failure::new(2, USAGE))?;
    let one = |rest: &[String]| -> Result<String, Failure> {
        match rest {
            [unit] => Ok(unit.clone()),
            _ => Err(Failure::new(2, format!("svc {verb} takes one unit"))),
        }
    };
    let optional = |rest: &[String]| -> Result<Option<String>, Failure> {
        match rest {
            [] => Ok(None),
            [unit] => Ok(Some(unit.clone())),
            _ => Err(Failure::new(
                2,
                format!("svc {verb} takes at most one unit"),
            )),
        }
    };
    match verb.as_str() {
        "status" => match optional(rest)? {
            Some(unit) => status(&unit),
            None => list(false),
        },
        "list" | "list-units" => match rest {
            [] => list(false),
            [flag] if flag == "--failed" => list(true),
            _ => Err(Failure::new(2, "svc list takes --failed and nothing else")),
        },
        "start" | "stop" | "restart" | "reload" | "enable" | "disable" | "mask" | "unmask" => {
            if rest.is_empty() {
                return Err(Failure::new(2, format!("svc {verb} needs a unit")));
            }
            for unit in rest {
                let call = match verb.as_str() {
                    "start" => Call::Start(unit.clone()),
                    "stop" => Call::Stop(unit.clone()),
                    "restart" => Call::Restart(unit.clone()),
                    "reload" => Call::Reload(unit.clone()),
                    "enable" => Call::Enable(unit.clone()),
                    "disable" => Call::Disable(unit.clone()),
                    "mask" => Call::Mask(unit.clone()),
                    _ => Call::Unmask(unit.clone()),
                };
                let _ = changed(&format!("{verb} {unit}"), &ask(&call)?)?;
            }
            Ok(0)
        }
        "isolate" => changed("isolate", &ask(&Call::Isolate(one(rest)?))?),
        "reset-failed" => changed("reset-failed", &ask(&Call::ResetFailed(optional(rest)?))?),
        "poweroff" | "reboot" if rest.is_empty() => {
            let call = if verb == "poweroff" {
                Call::Poweroff
            } else {
                Call::Reboot
            };
            // No init, or a refusal, is said. Init may be gone before it
            // answers otherwise; asking was the point.
            match exchange(connect()?, &call) {
                Ok(answers) => match answers.last() {
                    Some(Answer::Refused(why)) => Err(Failure::new(1, format!("{verb}: {why}"))),
                    _ => Ok(0),
                },
                Err(_) => Ok(0),
            }
        }
        "daemon-reload" if rest.is_empty() => changed("daemon-reload", &ask(&Call::DaemonReload)?),
        "log" => log(rest),
        "set-property" => set_property(rest),
        "scope" => scope(rest),
        "top" if rest.is_empty() => top(),
        "audit" if rest.is_empty() => audit(),
        _ => Err(Failure::new(2, USAGE)),
    }
}

/// Where init keeps the audit record, one file a boot named by its id
/// (`docs/certification/AUDIT.md` §4).
const AUDIT_DIRECTORY: &str = "/var/log/audit";

/// `svc audit`: the newest boot's audit record, one line a record, in the
/// order init wrote them.
fn audit() -> Result<u8, Failure> {
    let newest = fs::read_dir(AUDIT_DIRECTORY)
        .map_err(|error| Failure::new(1, format!("{AUDIT_DIRECTORY}: {error}")))?
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bin"))
        .max_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok())
        .ok_or_else(|| Failure::new(1, format!("{AUDIT_DIRECTORY}: no record yet")))?;
    let bytes = fs::read(newest.path())
        .map_err(|error| Failure::new(1, format!("{}: {error}", newest.path().display())))?;
    let mut out = io::stdout().lock();
    for chunk in bytes.chunks_exact(RECORD_BYTES) {
        let Ok(chunk) = <&[u8; RECORD_BYTES]>::try_from(chunk) else {
            continue;
        };
        let _ = writeln!(out, "{}", describe(&Record::from_bytes(chunk)));
    }
    Ok(0)
}

/// One record as a line: its ring's number, its event, who, what about,
/// and the rest.
fn describe(record: &Record) -> String {
    let mut line = format!(
        "#{} {} {}.{:09} pid {} job {}",
        record.sequence,
        record.event_name(),
        record.time / 1_000_000_000,
        record.time % 1_000_000_000,
        record.pid,
        record.job,
    );
    if record.uid != NO_UID {
        line.push_str(&format!(" uid {}", record.uid));
    }
    if let Some((id, high, refusals)) = record.start_fields() {
        line.push_str(&format!(" id {id:032x} rings {high} {refusals}"));
        return line;
    }
    line.push_str(&format!(
        " outcome {} status {} target {}:{} detail {} {} {}",
        record.outcome,
        record.status,
        record.target_kind,
        record.target(),
        record.detail[0],
        record.detail[1],
        record.detail[2],
    ));
    line
}

/// `svc status unit`.
fn status(unit: &str) -> Result<u8, Failure> {
    let units = units(ask(&Call::Status(Some(unit.to_owned())))?)?;
    let Some(status) = units.first() else {
        return Err(Failure::new(4, format!("{unit}: not loaded")));
    };
    let mut out = io::stdout().lock();
    let marker = match status.active.as_str() {
        "active" => '*',
        "failed" => 'x',
        _ => 'o',
    };
    let description = status
        .description
        .as_deref()
        .map_or_else(String::new, |description| format!(" - {description}"));
    let _ = writeln!(out, "{marker} {}{description}", status.name);
    let _ = writeln!(out, "     Loaded: {}", status.load);
    let result = match status.result.as_deref() {
        Some(result) if result != "success" => format!(" (Result: {result})"),
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "     Active: {} ({}){result}",
        status.active, status.sub
    );
    if let Some(invocation) = &status.invocation {
        let _ = writeln!(out, " Invocation: {invocation}");
    }
    if let Some(main) = status.main {
        let _ = writeln!(out, "   Main PID: {main}");
    }
    if let Some(text) = &status.status {
        let _ = writeln!(out, "     Status: \"{text}\"");
    }
    if let Some(cgroup) = &status.cgroup {
        let _ = writeln!(out, "     CGroup: /{cgroup}");
        let tasks = read_number(cgroup, "pids.current");
        let memory = read_number(cgroup, "memory.current");
        if let Some(tasks) = tasks {
            let _ = writeln!(out, "      Tasks: {tasks}");
        }
        if let Some(memory) = memory {
            let _ = writeln!(out, "     Memory: {}", size(memory));
        }
    }
    // The run's last lines, as systemctl shows them: from the log, when lode
    // keeps one this caller may read (docs/LOGGING.md §9.2).
    if let Some(invocation) = &status.invocation
        && let Some(lines) = from_lode(&[
            "-u",
            &format!("{}@{invocation}", status.name),
            "-n",
            "10",
            "-o",
            "short",
        ])
        && !lines.is_empty()
    {
        let _ = writeln!(out);
        let _ = out.write_all(&lines);
    }
    Ok(match (status.load.as_str(), status.active.as_str()) {
        ("not-found", _) => 4,
        (_, "active") => 0,
        _ => 3,
    })
}

/// `svc list [--failed]`, and `svc status` with no unit.
fn list(failed: bool) -> Result<u8, Failure> {
    let mut units = units(ask(&Call::List { failed })?)?;
    units.sort_by(|a, b| a.name.cmp(&b.name));
    let width = units
        .iter()
        .map(|unit| unit.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let mut out = io::stdout().lock();
    let _ = writeln!(
        out,
        "{:width$}  {:9}  {:12}  {:12}  DESCRIPTION",
        "UNIT", "LOAD", "ACTIVE", "SUB"
    );
    for unit in &units {
        let _ = writeln!(
            out,
            "{:width$}  {:9}  {:12}  {:12}  {}",
            unit.name,
            unit.load,
            unit.active,
            unit.sub,
            unit.description.as_deref().unwrap_or("")
        );
    }
    let _ = writeln!(out, "\n{} units listed.", units.len());
    Ok(0)
}

/// What `/bin/lode` prints for `args`, if it ran and found what was asked
/// for: `None` when there is no lode, no log, or nothing of that run.
fn from_lode(args: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("/bin/lode")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// `svc log unit [-n N]`: the unit's current or last run from the log
/// (`lode -u UNIT@last`), or init's own ring of its last lines when lode
/// keeps none this caller may read.
fn log(rest: &[String]) -> Result<u8, Failure> {
    let (unit, lines) = match rest {
        [unit] => (unit, 50),
        [unit, flag, count] | [flag, count, unit] if flag == "-n" => (
            unit,
            count
                .parse()
                .map_err(|_| Failure::new(2, format!("-n takes a number, not {count:?}")))?,
        ),
        _ => return Err(Failure::new(2, "usage: svc log unit [-n N]")),
    };
    if let Some(text) = from_lode(&[
        "-u",
        &format!("{unit}@last"),
        "-n",
        &lines.to_string(),
        "-o",
        "short",
    ]) && !text.is_empty()
    {
        let _ = io::stdout().lock().write_all(&text);
        return Ok(0);
    }
    let answers = ask(&Call::Log {
        unit: unit.clone(),
        lines,
    })?;
    match answers.into_iter().last() {
        Some(Answer::Lines(lines)) => {
            let mut out = io::stdout().lock();
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            Ok(0)
        }
        Some(Answer::Refused(why)) => Err(Failure::new(1, why)),
        other => Err(Failure::new(
            1,
            format!("an answer that is not lines: {other:?}"),
        )),
    }
}

/// `svc set-property unit Key=value... [--persistent]`.
fn set_property(rest: &[String]) -> Result<u8, Failure> {
    let persistent = rest.iter().any(|arg| arg == "--persistent");
    let mut words = rest.iter().filter(|arg| *arg != "--persistent");
    let unit = words
        .next()
        .ok_or_else(|| Failure::new(2, "svc set-property needs a unit"))?;
    let assignments: Vec<String> = words.cloned().collect();
    if assignments.is_empty() {
        return Err(Failure::new(2, "svc set-property needs Key=value"));
    }
    changed(
        "set-property",
        &ask(&Call::SetProperty {
            unit: unit.clone(),
            assignments,
            persistent,
        })?,
    )
}

/// `svc scope --unit NAME [--slice SLICE] PID...`.
fn scope(rest: &[String]) -> Result<u8, Failure> {
    let mut unit = None;
    let mut slice = None;
    let mut pids = Vec::new();
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--unit" => unit = words.next().cloned(),
            "--slice" => slice = words.next().cloned(),
            "--pid" => {}
            pid => pids.push(
                pid.parse()
                    .map_err(|_| Failure::new(2, format!("{pid:?} is not a pid")))?,
            ),
        }
    }
    let unit = unit.ok_or_else(|| Failure::new(2, "svc scope needs --unit NAME"))?;
    changed(
        "scope",
        &ask(&Call::Scope {
            unit,
            slice,
            pids,
            seat: false,
        })?,
    )
}

/// `svc top`: every unit with a cgroup, by memory, from the controller
/// files.
fn top() -> Result<u8, Failure> {
    let units = units(ask(&Call::List { failed: false })?)?;
    let mut rows: Vec<(String, u64, u64, u64)> = units
        .iter()
        .filter_map(|unit| {
            let cgroup = unit.cgroup.as_deref()?;
            let memory = read_number(cgroup, "memory.current")?;
            let tasks = read_number(cgroup, "pids.current").unwrap_or(0);
            let cpu = fs::read_to_string(format!("/sys/fs/cgroup/{cgroup}/cpu.stat"))
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find_map(|line| line.strip_prefix("usage_usec "))
                        .and_then(|value| value.trim().parse().ok())
                })
                .unwrap_or(0);
            Some((unit.name.clone(), memory, tasks, cpu))
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let width = rows.iter().map(|row| row.0.len()).max().unwrap_or(4).max(4);
    let mut out = io::stdout().lock();
    let _ = writeln!(
        out,
        "{:width$}  {:>10}  {:>5}  {:>10}",
        "UNIT", "MEMORY", "TASKS", "CPU"
    );
    for (name, memory, tasks, cpu) in rows {
        let _ = writeln!(
            out,
            "{name:width$}  {:>10}  {tasks:>5}  {:>9.2}s",
            size(memory),
            cpu as f64 / 1_000_000.0
        );
    }
    Ok(0)
}

/// A number from a cgroup's file.
fn read_number(cgroup: &str, file: &str) -> Option<u64> {
    fs::read_to_string(format!("/sys/fs/cgroup/{cgroup}/{file}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Bytes, as a person reads them.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{bytes}{name}")
    } else {
        format!("{value:.1}{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_a_person_reads_them() {
        assert_eq!(size(512), "512B");
        assert_eq!(size(2048), "2.0K");
        assert_eq!(size(64 << 20), "64.0M");
    }

    #[test]
    fn a_command_line_svc_does_not_take_is_status_2() {
        assert_eq!(run(&[]).unwrap_err().status, 2);
        assert_eq!(run(&["frobnicate".to_owned()]).unwrap_err().status, 2);
        assert_eq!(run(&["start".to_owned()]).unwrap_err().status, 2);
        assert_eq!(run(&["log".to_owned()]).unwrap_err().status, 2);
    }

    #[test]
    fn each_name_is_its_verb() {
        let args = |words: &[&str]| {
            words
                .iter()
                .map(|&word| word.to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(named("poweroff", &[]).unwrap(), "poweroff");
        assert_eq!(named("reboot", &[]).unwrap(), "reboot");
        assert_eq!(named("shutdown", &[]).unwrap(), "poweroff");
        assert_eq!(
            named("shutdown", &args(&["-h", "now"])).unwrap(),
            "poweroff"
        );
        assert_eq!(named("shutdown", &args(&["-r", "now"])).unwrap(), "reboot");
        assert_eq!(named("shutdown", &args(&["now", "-r"])).unwrap(), "reboot");
        // A later time is refused rather than taken as now.
        assert_eq!(named("shutdown", &args(&["+5"])).unwrap_err().status, 1);
        assert_eq!(named("shutdown", &args(&["22:00"])).unwrap_err().status, 1);
        assert_eq!(named("poweroff", &args(&["now"])).unwrap_err().status, 2);
        assert_eq!(named("shutdown", &args(&["-k"])).unwrap_err().status, 2);
    }

    #[test]
    fn a_refusal_is_a_failure_with_its_reason() {
        let refused = [Answer::Refused("Permission denied".to_owned())];
        let failure = changed("stop a.service", &refused).unwrap_err();
        assert_eq!(failure.status, 1);
        assert!(failure.message.contains("Permission denied"));
        let done = [
            Answer::Note("made".to_owned()),
            Answer::Done("done".to_owned()),
        ];
        assert_eq!(changed("enable", &done).unwrap(), 0);
        assert!(changed("start", &[Answer::Done("failed".to_owned())]).is_err());
    }
}
