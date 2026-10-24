//! journalctl's maintenance options (`docs/LOGGING.md` §9.1, §9.6):
//! `-F`/`-N`, `--disk-usage`, `--vacuum-size/-time/-files`, and
//! `--flush`, `--sync`, `--rotate`, which ask the service on its control
//! socket. Each prints what journalctl prints.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::format::{self, bytes_text};
use ferrix_lode_core::segment;

use crate::cli::{self, Known, Options, complain, out};
use crate::control::{self, Request};
use crate::{store, sys};

/// `--vacuum-*`: what to keep.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Vacuum {
    /// At most this many bytes in all.
    pub(crate) size: Option<u64>,
    /// Nothing whose newest record is older than this many seconds.
    pub(crate) time: Option<u64>,
    /// At most this many sealed segments.
    pub(crate) files: Option<usize>,
}

impl Vacuum {
    /// Whether any limit was given.
    pub(crate) fn any(&self) -> bool {
        self.size.is_some() || self.time.is_some() || self.files.is_some()
    }
}

/// `--flush`, `--sync`, `--rotate`: ask the service, and wait until it is
/// done. `--flush` first makes `/var/log/journal` for lode, as root, when
/// it is missing.
pub(crate) fn ask(options: &Options) -> i32 {
    let Some(request) = options.ask else {
        return 2;
    };
    if sys::getuid() != 0 {
        complain("lode: --flush, --sync and --rotate are root's");
        return 1;
    }
    if request == Request::Flush {
        let _ = sys::mkdirat(sys::AT_FDCWD, b"/var/log", 0o755);
        if sys::mkdirat(sys::AT_FDCWD, store::PERSISTENT, 0o2750).is_ok()
            && let Ok(fd) = sys::open(store::PERSISTENT, sys::O_RDONLY)
        {
            let _ = sys::fchown(fd, crate::serve::UID, crate::serve::GID);
            let _ = sys::fchmod(fd, 0o2750);
            sys::close(fd);
        }
    }
    match ask_lode(request) {
        Ok(said) => {
            if !said.is_empty() {
                out(alloc::format!("{said}\n").as_bytes());
            }
            0
        }
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            1
        }
    }
}

/// Send `request` on the control socket and wait for lode's answer.
pub(crate) fn ask_lode(request: Request) -> Result<String, String> {
    let fd = cli::connect(control::SOCKET).map_err(|error| {
        alloc::format!(
            "{}: errno {} (is lode running?)",
            String::from_utf8_lossy(control::SOCKET),
            error.0
        )
    })?;
    let sent = sys::write_all(fd, request.line());
    let mut answer = Vec::new();
    let mut buf = [0_u8; 256];
    while sent.is_ok() && !answer.contains(&b'\n') {
        match sys::read(fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(got) => answer.extend_from_slice(buf.get(..got).unwrap_or_default()),
        }
    }
    sys::close(fd);
    let text = String::from_utf8_lossy(&answer);
    match text.trim_end() {
        "OK" => Ok(String::new()),
        said if said.starts_with("OK ") => Ok(String::from(said.get(3..).unwrap_or_default())),
        other => Err(String::from(
            other.strip_prefix("ERR ").unwrap_or("lode did not answer"),
        )),
    }
}

/// The store's segments, and their summaries' and their own sizes.
fn sizes(segments: &[Known]) -> u64 {
    segments
        .iter()
        .map(|each| store::size(&each.segment) + summary_size(&each.segment))
        .sum()
}

/// A segment's summary's size, if it has one.
fn summary_size(segment: &store::Segment) -> u64 {
    match segment.passed {
        Some((_, Some(fd))) => sys::lseek(fd, 0, sys::SEEK_END).unwrap_or(0) as u64,
        Some(_) => 0,
        None => {
            let name = alloc::format!("{}.sum", segment.name);
            sys::openat(
                segment.dir,
                name.as_bytes(),
                sys::O_RDONLY | sys::O_CLOEXEC,
                0,
            )
            .map_or(0, |fd| {
                let size = sys::lseek(fd, 0, sys::SEEK_END).unwrap_or(0);
                sys::close(fd);
                size as u64
            })
        }
    }
}

/// `--disk-usage`: journalctl's sentence, for every segment the caller
/// may read.
pub(crate) fn disk_usage(options: &Options) -> i32 {
    let source = match cli::directories(options) {
        Ok(source) => source,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let segments = cli::known(&source);
    out(alloc::format!(
        "Archived and active journals take up {} in the file system.\n",
        bytes_text(sizes(&segments))
    )
    .as_bytes());
    cli::release(&segments);
    0
}

/// `-F FIELD` (each value once, as journalctl prints them) and `-N` (each
/// field name once), over the records the other options choose.
pub(crate) fn field_values(options: &Options) -> i32 {
    let source = match cli::directories(options) {
        Ok(source) => source,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let segments = cli::known(&source);
    let boot = match &options.boot {
        Some(word) => match cli::chosen_boot(word, &cli::boots(&segments)) {
            Ok(boot) => Some(boot),
            Err(why) => {
                complain(&alloc::format!("lode: {why}"));
                return 1;
            }
        },
        None => None,
    };
    let zone = crate::zone::reader();
    let window = match cli::window(options, &zone) {
        Ok(window) => window,
        Err(why) => return cli::usage(&why),
    };
    let mut seen: Vec<Vec<u8>> = Vec::new();
    for each in cli::candidates(&segments, boot, options, window) {
        for record in cli::records_of(each) {
            if !cli::passes(options, &record, window.0, window.1) {
                continue;
            }
            for (name, value) in format::shown_fields(&record) {
                let item = match &options.field {
                    Some(field) if *field == name => value,
                    Some(_) => continue,
                    None => name.into_bytes(),
                };
                if !seen.contains(&item) {
                    seen.push(item);
                }
            }
        }
    }
    let mut buffer = Vec::new();
    for item in seen {
        buffer.extend_from_slice(&item);
        buffer.push(b'\n');
    }
    out(&buffer);
    cli::release(&segments);
    0
}

/// `--vacuum-size`, `--vacuum-time`, `--vacuum-files` (root's): remove the
/// oldest sealed segments, never one being written, until what is left
/// keeps the limits; say each as journalctl does, and in the log.
pub(crate) fn vacuum(options: &Options) -> i32 {
    if sys::getuid() != 0 {
        complain("lode: vacuuming is root's");
        return 1;
    }
    let source = match cli::directories(options) {
        Ok(source) => source,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let segments = cli::known(&source);
    let sealed = sealed(&segments);
    let mut total = sizes(&segments);
    let now = sys::clock_us(sys::CLOCK_REALTIME);
    let mut left = sealed.len();
    let mut freed: Vec<(usize, u64)> = Vec::new();
    let mut said = Vec::new();
    for each in sealed {
        let size = store::size(&each.segment) + summary_size(&each.segment);
        let last = each.summary.as_ref().map_or(0, |summary| summary.times.1);
        let over = options.vacuum.size.is_some_and(|limit| total > limit)
            || options.vacuum.files.is_some_and(|limit| left > limit)
            || options
                .vacuum
                .time
                .is_some_and(|span| last < now.saturating_sub(span.saturating_mul(1_000_000)));
        if !over {
            continue;
        }
        let dir = each.segment.dir;
        if sys::unlinkat(dir, each.segment.name.as_bytes()).is_err() {
            continue;
        }
        let _ = sys::unlinkat(dir, alloc::format!("{}.sum", each.segment.name).as_bytes());
        total = total.saturating_sub(size);
        left -= 1;
        said.extend_from_slice(
            alloc::format!(
                "Deleted archived journal {}/{} ({}).\n",
                store_path(dir, &source),
                each.segment.name,
                bytes_text(size)
            )
            .as_bytes(),
        );
        match freed.iter_mut().find(|(seen, _)| *seen == dir) {
            Some((_, sum)) => *sum += size,
            None => freed.push((dir, size)),
        }
    }
    // journalctl says one line a directory, freed or not.
    for dir in &source.dirs {
        let sum = freed
            .iter()
            .find(|(seen, _)| *seen == dir.fd)
            .map_or(0, |(_, sum)| *sum);
        said.extend_from_slice(
            alloc::format!(
                "Vacuuming done, freed {} of archived journals from {}.\n",
                bytes_text(sum),
                store_path(dir.fd, &source)
            )
            .as_bytes(),
        );
    }
    // journalctl says these on standard error, as log lines.
    let _ = sys::write_all(2, &said);
    let removed = said
        .split(|&byte| byte == b'\n')
        .filter(|line| line.starts_with(b"Deleted"))
        .count();
    record_vacuum(removed, freed.iter().map(|(_, sum)| sum).sum());
    cli::release(&segments);
    0
}

/// The sealed segments of `segments`, oldest first: each with a summary,
/// and not the newest of its class and owner in its directory, which may
/// be being written.
fn sealed(segments: &[Known]) -> Vec<&Known> {
    let mut classes: Vec<(usize, &str)> = Vec::new();
    let mut newest: Vec<(usize, &str)> = Vec::new();
    for each in segments.iter().rev() {
        let key = (
            each.segment.dir,
            segment::class_of_name(&each.segment.name).unwrap_or_default(),
        );
        if !classes.contains(&key) {
            classes.push(key);
            newest.push((each.segment.dir, each.segment.name.as_str()));
        }
    }
    segments
        .iter()
        .filter(|each| {
            each.summary.is_some()
                && !newest.contains(&(each.segment.dir, each.segment.name.as_str()))
        })
        .collect()
}

/// Say in the log what a vacuum removed, and who asked: deletions are
/// recorded (`docs/LOGGING.md` §12.3).
fn record_vacuum(removed: usize, freed: u64) {
    let mut line = Vec::from(&b"<13>lode-vacuum: "[..]);
    line.extend_from_slice(
        alloc::format!(
            "removed {removed} sealed segment(s), {} freed, by uid {}",
            bytes_text(freed),
            sys::getuid()
        )
        .as_bytes(),
    );
    if let Ok(fd) = sys::open(b"/dev/kmsg", sys::O_WRONLY) {
        let _ = sys::write(fd, &line);
        sys::close(fd);
    }
}

/// The path of the store directory open as `dir`.
fn store_path(dir: usize, source: &cli::Source) -> String {
    source
        .dirs
        .iter()
        .position(|each| each.fd == dir)
        .and_then(|index| source.paths.get(index))
        .map_or_else(
            || String::from("the store"),
            |path| String::from_utf8_lossy(path).into_owned(),
        )
}
