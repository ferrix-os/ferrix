//! `lode snapshot PID`: what a process that hangs is doing, into the log
//! (`docs/LOGGING.md` §18 "Hang snapshots"; the customer, 2026-10-24: what
//! was gathered by hand on the night of the Endfield start failures).
//!
//! One record set, all with the same `SNAPSHOT` id: a record of the
//! process -- its name, path and command line, its `status`, how many
//! descriptors it holds, and its last lines in the log -- and one record a
//! thread: its name, state, the system call it is in, where it waits
//! (`wchan`) and its signal lines (`SigPnd`, `ShdPnd`, `SigBlk`, `SigIgn`,
//! `SigCgt`). Read from `/proc` as the person running it, so a person
//! snapshots their own processes and root anyone's; written to the log
//! through systemd's native protocol, and printed.

use alloc::string::String;
use alloc::vec::Vec;

use crate::cli::{self, complain, out, usage};
use crate::sys;

/// How many of the process's last lines a snapshot carries by default.
const TAIL: usize = 20;

/// One thread, as `/proc/<pid>/task/<tid>/` shows it.
struct Thread {
    tid: String,
    comm: Vec<u8>,
    state: Vec<u8>,
    syscall: Vec<u8>,
    wchan: Vec<u8>,
    signals: Vec<u8>,
}

/// A small `/proc` file, its trailing newline gone; empty when it cannot
/// be read.
fn read(path: &str) -> Vec<u8> {
    let mut bytes = sys::read_small(path.as_bytes()).unwrap_or_default();
    while bytes.last() == Some(&b'\n') {
        let _ = bytes.pop();
    }
    bytes
}

/// The lines of `status` whose names are given, joined by `; `.
fn lines_of(status: &[u8], names: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in status.split(|&byte| byte == b'\n') {
        if names.iter().any(|name| line.starts_with(name.as_bytes())) {
            if !out.is_empty() {
                out.extend_from_slice(b"; ");
            }
            out.extend(
                line.iter()
                    .map(|&byte| if byte == b'\t' { b' ' } else { byte }),
            );
        }
    }
    out
}

/// What a snapshot read of a process.
struct Taken {
    pid: String,
    comm: Vec<u8>,
    exe: Vec<u8>,
    cmdline: Vec<u8>,
    status: Vec<u8>,
    fds: Option<usize>,
    threads: Vec<Thread>,
    tail: Vec<u8>,
}

/// One record's fields, in order.
pub(crate) type Fields = Vec<(String, Vec<u8>)>;

/// `lode snapshot PID [-n LINES] [--reason TEXT]`.
pub(crate) fn snapshot(words: &[String]) -> i32 {
    let Some(pid) = words.first().filter(|word| word.parse::<u32>().is_ok()) else {
        return usage("snapshot needs a pid");
    };
    let mut tail = TAIL;
    let mut reason = String::from("asked");
    let mut at = 1;
    while let Some(word) = words.get(at) {
        at += 1;
        match (word.as_str(), words.get(at)) {
            ("-n", Some(count)) => {
                tail = count.parse().unwrap_or(TAIL);
                at += 1;
            }
            ("--reason", Some(text)) => {
                reason.clone_from(text);
                at += 1;
            }
            _ => return usage("snapshot PID [-n LINES] [--reason TEXT]"),
        }
    }
    let Some(taken) = take(pid, tail) else {
        complain(&alloc::format!("lode: snapshot: no process {pid}"));
        return 1;
    };
    let mut id = [0_u8; 8];
    let _ = sys::random(&mut id);
    let mut id_text = String::new();
    ferrix_lode_core::segment::push_hex(&mut id_text, &id);
    let (records, printed) = records_of(&taken, &id_text, &reason);
    out(printed.as_bytes());
    match cli::connect_datagram(b"/run/systemd/journal/socket") {
        Ok(fd) => {
            for record in &records {
                let _ = sys::write(fd, &datagram(record));
            }
            sys::close(fd);
            0
        }
        Err(error) => {
            complain(&alloc::format!(
                "lode: snapshot: the log is not there (errno {}); printed only",
                error.0
            ));
            1
        }
    }
}

/// Read the process `pid` and its last `tail` lines; `None` when there is
/// no such process.
fn take(pid: &str, tail: usize) -> Option<Taken> {
    let base = alloc::format!("/proc/{pid}");
    let status = read(&alloc::format!("{base}/status"));
    if status.is_empty() {
        return None;
    }
    Some(Taken {
        pid: String::from(pid),
        comm: read(&alloc::format!("{base}/comm")),
        exe: sys::read_link(alloc::format!("{base}/exe").as_bytes()).unwrap_or_default(),
        cmdline: read(&alloc::format!("{base}/cmdline"))
            .iter()
            .map(|&byte| if byte == 0 { b' ' } else { byte })
            .collect(),
        status,
        fds: sys::open(alloc::format!("{base}/fd").as_bytes(), sys::O_RDONLY)
            .ok()
            .map(|fd| {
                let count = sys::list_dir(fd).map_or(0, |names| names.len());
                sys::close(fd);
                count
            }),
        threads: threads_of(&base),
        tail: last_lines(pid, tail),
    })
}

/// The snapshot's records -- the process's, then one a thread -- and what
/// is printed.
fn records_of(taken: &Taken, id: &str, reason: &str) -> (Vec<Fields>, String) {
    let summary = alloc::format!(
        "snapshot {id} of {} ({}): {} thread(s), {} descriptor(s); {reason}",
        taken.pid,
        String::from_utf8_lossy(&taken.comm),
        taken.threads.len(),
        taken
            .fds
            .map_or_else(|| String::from("?"), |count| alloc::format!("{count}"))
    );
    let common = |message: String| -> Fields {
        alloc::vec![
            (String::from("MESSAGE"), message.into_bytes()),
            (String::from("PRIORITY"), Vec::from(&b"5"[..])),
            (
                String::from("SYSLOG_IDENTIFIER"),
                Vec::from(&b"lode-snapshot"[..])
            ),
            (String::from("FERRIX_EVENT"), Vec::from(&b"snapshot"[..])),
            (String::from("SNAPSHOT"), Vec::from(id.as_bytes())),
            (
                String::from("SNAPSHOT_PID"),
                Vec::from(taken.pid.as_bytes())
            ),
        ]
    };
    let mut first = common(summary.clone());
    let fds = taken
        .fds
        .map_or_else(Vec::new, |count| alloc::format!("{count}").into_bytes());
    for (name, value) in [
        ("SNAPSHOT_COMM", &taken.comm),
        ("SNAPSHOT_EXE", &taken.exe),
        ("SNAPSHOT_CMDLINE", &taken.cmdline),
        ("SNAPSHOT_FDS", &fds),
        ("SNAPSHOT_STATUS", &taken.status),
        ("SNAPSHOT_TAIL", &taken.tail),
    ] {
        if !value.is_empty() {
            first.push((String::from(name), value.clone()));
        }
    }
    first.push((
        String::from("SNAPSHOT_REASON"),
        Vec::from(reason.as_bytes()),
    ));
    let mut records = alloc::vec![first];
    let mut printed = alloc::format!("{summary}\n");
    let shown = |bytes: &[u8]| {
        if bytes.is_empty() {
            String::from("?")
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    };
    for thread in &taken.threads {
        let line = alloc::format!(
            "snapshot {id}: thread {} ({}) state {} in {} waiting at {}; {}",
            thread.tid,
            String::from_utf8_lossy(&thread.comm),
            shown(&thread.state),
            shown(&thread.syscall),
            shown(&thread.wchan),
            String::from_utf8_lossy(&thread.signals)
        );
        printed.push_str(&line);
        printed.push('\n');
        let mut record = common(line);
        for (name, value) in [
            ("THREAD_TID", thread.tid.as_bytes()),
            ("THREAD_COMM", &thread.comm),
            ("THREAD_STATE", &thread.state),
            ("THREAD_SYSCALL", &thread.syscall),
            ("THREAD_WCHAN", &thread.wchan),
            ("THREAD_SIGNALS", &thread.signals),
        ] {
            if !value.is_empty() {
                record.push((String::from(name), Vec::from(value)));
            }
        }
        records.push(record);
    }
    if !taken.tail.is_empty() {
        printed.push_str("last lines:\n");
        printed.push_str(&String::from_utf8_lossy(&taken.tail));
        printed.push('\n');
    }
    (records, printed)
}

/// Every thread of the process at `base`, a line each, as `lode snapshot`
/// prints them: what `lode crashd` keeps as a crash's `BLOB_THREADS`.
pub(crate) fn threads_text(base: &str) -> Vec<u8> {
    let shown = |bytes: &[u8]| {
        if bytes.is_empty() {
            String::from("?")
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    };
    let mut text = Vec::new();
    for thread in threads_of(base) {
        text.extend_from_slice(
            alloc::format!(
                "thread {} ({}) state {} in {} waiting at {}; {}\n",
                thread.tid,
                String::from_utf8_lossy(&thread.comm),
                shown(&thread.state),
                shown(&thread.syscall),
                shown(&thread.wchan),
                String::from_utf8_lossy(&thread.signals)
            )
            .as_bytes(),
        );
    }
    text
}

/// Every thread of the process at `base`.
fn threads_of(base: &str) -> Vec<Thread> {
    let Ok(fd) = sys::open(alloc::format!("{base}/task").as_bytes(), sys::O_RDONLY) else {
        return Vec::new();
    };
    let mut tids: Vec<String> = sys::list_dir(fd)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| String::from_utf8(name).ok())
        .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    sys::close(fd);
    tids.sort_by_key(|tid| tid.parse::<u32>().unwrap_or(0));
    tids.into_iter()
        .map(|tid| {
            let dir = alloc::format!("{base}/task/{tid}");
            let status = read(&alloc::format!("{dir}/status"));
            Thread {
                comm: read(&alloc::format!("{dir}/comm")),
                state: lines_of(&status, &["State:"])
                    .strip_prefix(b"State: ")
                    .map(Vec::from)
                    .unwrap_or_default(),
                syscall: read(&alloc::format!("{dir}/syscall")),
                wchan: read(&alloc::format!("{dir}/wchan")),
                signals: lines_of(
                    &status,
                    &["SigQ", "SigPnd", "ShdPnd", "SigBlk", "SigIgn", "SigCgt"],
                ),
                tid,
            }
        })
        .collect()
}

/// The last `count` lines the process wrote to the log, as `short` shows
/// them, that the caller may read.
fn last_lines(pid: &str, count: usize) -> Vec<u8> {
    if count == 0 {
        return Vec::new();
    }
    let Ok(options) = cli::parse(&[alloc::format!("_PID={pid}"), String::from("-b")]) else {
        return Vec::new();
    };
    let Ok(source) = cli::directories(&options) else {
        return Vec::new();
    };
    let segments = cli::known(&source);
    let zone = crate::zone::reader();
    let boot = cli::chosen_boot("", &cli::boots(&segments)).ok();
    let mut found: Vec<ferrix_lode_core::record::Record> = Vec::new();
    for each in cli::candidates(&segments, boot, &options, (None, None)) {
        found.extend(
            cli::records_of(each)
                .into_iter()
                .filter(|record| cli::passes(&options, record, None, None)),
        );
    }
    cli::release(&segments);
    found.sort_by_key(|record| record.seqnum);
    let skip = found.len().saturating_sub(count);
    let mut text = Vec::new();
    for record in found.iter().skip(skip) {
        ferrix_lode_core::format::write(
            record,
            ferrix_lode_core::format::Output::Short,
            &zone,
            &mut text,
        );
    }
    while text.last() == Some(&b'\n') {
        let _ = text.pop();
    }
    text
}

/// `fields` as one datagram of systemd's native protocol.
pub(crate) fn datagram(fields: &Fields) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(name.as_bytes());
        if value.contains(&b'\n') {
            out.push(b'\n');
            out.extend_from_slice(&u64::try_from(value.len()).unwrap_or(0).to_le_bytes());
        } else {
            out.push(b'=');
        }
        out.extend_from_slice(value);
        out.push(b'\n');
    }
    out
}

/// `lode follow FILE [-t ID] [--from-start] [--until-pid PID]`: a file
/// some program writes, into the log a line a record under ID (the file's
/// name by default) -- the game's `Player.log`, the crash reporter's, DXVK's
/// (`docs/LOGGING.md` §5, file sources). It reads on from the file's end
/// (from its start with `--from-start`) every quarter second, starts again
/// at the beginning when the file is cut short or replaced, and ends once
/// the process `--until-pid` names is gone and the file is read to its end.
pub(crate) fn follow(words: &[String]) -> i32 {
    let Some(path) = words.first() else {
        return usage("follow FILE [-t ID] [--from-start] [--until-pid PID]");
    };
    let mut identifier = String::from(path.rsplit('/').next().unwrap_or(path));
    let mut from_start = false;
    let mut until: Option<String> = None;
    let mut at = 1;
    while let Some(word) = words.get(at) {
        at += 1;
        match (word.as_str(), words.get(at)) {
            ("-t", Some(id)) => {
                identifier.clone_from(id);
                at += 1;
            }
            ("--until-pid", Some(pid)) => {
                until = Some(pid.clone());
                at += 1;
            }
            ("--from-start", _) => from_start = true,
            _ => return usage("follow FILE [-t ID] [--from-start] [--until-pid PID]"),
        }
    }
    let mut lode = cli::open_stream(&identifier);
    let mut offset: Option<usize> = None;
    let mut identity: Option<u64> = None;
    let mut held: Vec<u8> = Vec::new();
    loop {
        let gone = until.as_ref().is_some_and(|pid| {
            sys::read_small(alloc::format!("/proc/{pid}/status").as_bytes()).is_none()
        });
        if let Ok(fd) = sys::open(path.as_bytes(), sys::O_RDONLY | sys::O_CLOEXEC) {
            let size = sys::lseek(fd, 0, sys::SEEK_END).unwrap_or(0);
            let now_identity = sys::identity(fd);
            let start = match offset {
                None if from_start => 0,
                None => size,
                // Cut short, or a new file under the name: from its start.
                Some(at) if size < at || now_identity != identity => 0,
                Some(at) => at,
            };
            identity = now_identity;
            if let Ok(bytes) = sys::read_fd_from(fd, start) {
                offset = Some(start + bytes.len());
                held.extend_from_slice(&bytes);
            }
            sys::close(fd);
        }
        let take = held
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |end| end + 1);
        let lines: Vec<u8> = held.drain(..take).collect();
        if !lines.is_empty() {
            match lode {
                Some(fd) if sys::send_all(fd, &lines).is_ok() => {}
                _ => {
                    // No lode: try again for the next lines; these go out.
                    out(&lines);
                    lode = cli::open_stream(&identifier);
                }
            }
        }
        if gone {
            return 0;
        }
        sys::sleep_ms(250);
    }
}
