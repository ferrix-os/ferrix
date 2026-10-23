//! `cargo xtask box-log --host <DEST> [-- <lode's options>]`: follow a
//! machine's log from here, over ssh, without losing or repeating a line
//! (`docs/LOGGING.md` §10.1, step 4).
//!
//! It runs `lode -f -o json` on the machine, with whatever lode options
//! follow `--` (`--where 'id = "intel*"' -p info`), writes every record to
//! `~/.local/share/ferrix/logs/box/<UTC>-<host>.jsonl` and a line of it to
//! the terminal, and keeps the last record's cursor. A connection that
//! drops -- a reboot of the machine, a network that blinked -- is made again
//! every two seconds and goes on `--after-cursor` the last record kept, so a
//! test's evidence across a reboot is whole, and the store's own sequence
//! numbers make a repeat impossible. The file begins with a line naming the
//! host, this tree's commit and when it began, so the evidence can be cited.
//!
//! Its own options, taken out of what follows `--` before lode sees it:
//! `--until-match TEXT` ends it, status 0, at the first record whose
//! message holds TEXT; `--out FILE` writes there instead; `--timeout`
//! (xtask's own) bounds the whole follow, status 1 when it runs out with
//! no match asked for unmet.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "box-log",
    help_at: 1136,
    help: &[
        "box-log --host <DEST> [-- <lode options>] [--until-match TEXT] [--out FILE]",
        "              Follow a machine's lode over ssh into ~/.local/share/ferrix/logs/box, resuming",
        "              from the last record after a dropped connection or a reboot",
    ],
    run: |_, args| run(args),
}];

/// What `box-log` was asked.
#[derive(Debug, Default, PartialEq, Eq)]
struct Asked {
    /// lode's own options.
    lode: Vec<String>,
    /// `--until-match`.
    until: Option<String>,
    /// `--out`.
    out: Option<PathBuf>,
}

/// Take box-log's own options out of what followed `--`.
fn asked(passthrough: &[String]) -> Result<Asked> {
    let mut asked = Asked::default();
    let mut words = passthrough.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--until-match" => {
                asked.until = Some(
                    words
                        .next()
                        .ok_or_else(|| Error::new("--until-match needs a text"))?
                        .clone(),
                );
            }
            "--out" => {
                asked.out = Some(PathBuf::from(
                    words
                        .next()
                        .ok_or_else(|| Error::new("--out needs a file"))?,
                ));
            }
            _ => asked.lode.push(word.clone()),
        }
    }
    Ok(asked)
}

/// A JSON record's string field `name`, as lode writes one: `"NAME":"..."`,
/// its escapes left as they are but `\"`, which is enough for a cursor and
/// for a message to be matched.
fn field(line: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\":\"");
    let start = line.find(&key)? + key.len();
    let mut value = String::new();
    let mut chars = line.get(start..)?.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(value),
            '\\' => {
                if let Some(next) = chars.next() {
                    value.push(if next == 'n' { '\n' } else { next });
                }
            }
            _ => value.push(c),
        }
    }
    None
}

/// The command line run on the machine: lode, following, from `cursor`
/// when there is one and from the boot's start when not.
fn remote(lode: &[String], cursor: Option<&str>) -> String {
    let quote = |word: &str| format!("'{}'", word.replace('\'', "'\\''"));
    let mut command = String::from("/bin/lode -f -o json -n 100000000");
    match cursor {
        Some(cursor) => command.push_str(&format!(" --after-cursor={}", quote(cursor))),
        None => command.push_str(" -b"),
    }
    for word in lode {
        command.push(' ');
        command.push_str(&quote(word));
    }
    command
}

/// `cargo xtask box-log`.
fn run(args: &Args) -> Result<()> {
    let host = args
        .host
        .clone()
        .ok_or_else(|| Error::new("box-log needs --host <DEST>, as ssh names the machine"))?;
    let asked = asked(&args.passthrough)?;
    let (path, mut file) = evidence(&host, asked.out.as_ref())?;
    println!("  box-log: following {host}'s lode into {}", path.display());
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    let mut cursor: Option<String> = None;
    while Instant::now() < deadline {
        let mut child = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=5",
            ])
            .arg(&host)
            .arg(remote(&asked.lode, cursor.as_deref()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| Error::new(format!("ssh: {error}")))?;
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::result::Result::ok)
            {
                let _ = writeln!(file, "{line}");
                if let Some(next) = field(&line, "__CURSOR") {
                    cursor = Some(next);
                }
                let message = field(&line, "MESSAGE").unwrap_or_default();
                let who = field(&line, "SYSLOG_IDENTIFIER").unwrap_or_default();
                println!("{who}: {message}");
                if asked
                    .until
                    .as_deref()
                    .is_some_and(|until| message.contains(until))
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    println!("  box-log: matched; the records are in {}", path.display());
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        std::thread::sleep(Duration::from_secs(2));
    }
    match asked.until {
        Some(until) => Err(Error::new(format!(
            "box-log: `{until}` never came within {}s; the records are in {}",
            args.timeout,
            path.display()
        ))),
        None => Ok(()),
    }
}

/// The file the records go in, opened for appending, its first line naming
/// the host, this tree's commit and the time.
fn evidence(host: &str, out: Option<&PathBuf>) -> Result<(PathBuf, std::fs::File)> {
    let started = chrono_free_stamp();
    let path = match out {
        Some(path) => path.clone(),
        None => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            let dir = home.join(".local/share/ferrix/logs/box");
            std::fs::create_dir_all(&dir)
                .map_err(|error| Error::new(format!("{}: {error}", dir.display())))?;
            let safe: String = host
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            dir.join(format!("{started}-{safe}.jsonl"))
        }
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(crate::paths::workspace_root())
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "{{\"box_log\":\"begun\",\"host\":\"{host}\",\"tree_commit\":\"{commit}\",\"at\":\"{started}\"}}"
    );
    Ok((path, file))
}

/// Now as `YYYYMMDDTHHMMSSZ`, from the system clock, with no date library.
fn chrono_free_stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let of_day = seconds % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::{asked, field, remote};

    #[test]
    fn its_own_options_come_out_and_the_rest_go_to_lode() {
        let words: Vec<String> = [
            "--where",
            "id = \"intel*\"",
            "--until-match",
            "is replaced",
            "-p",
            "info",
        ]
        .iter()
        .map(|word| (*word).to_owned())
        .collect();
        let asked = asked(&words).unwrap();
        assert_eq!(asked.until.as_deref(), Some("is replaced"));
        assert_eq!(asked.lode, ["--where", "id = \"intel*\"", "-p", "info"]);
    }

    #[test]
    fn a_cursor_resumes_and_none_starts_at_the_boot() {
        assert_eq!(
            remote(&["-p".into(), "it's".into()], None),
            "/bin/lode -f -o json -n 100000000 -b '-p' 'it'\\''s'"
        );
        assert!(remote(&[], Some("i=2a;b=00")).contains("--after-cursor='i=2a;b=00'"));
    }

    #[test]
    fn fields_read_out_of_a_record() {
        let line = r#"{"__CURSOR":"i=9;b=ab","MESSAGE":"say \"hi\"","SYSLOG_IDENTIFIER":"x"}"#;
        assert_eq!(field(line, "__CURSOR").as_deref(), Some("i=9;b=ab"));
        assert_eq!(field(line, "MESSAGE").as_deref(), Some("say \"hi\""));
        assert_eq!(field(line, "NONE"), None);
    }
}
