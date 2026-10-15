//! What a netbooted machine sends the server: its loader's boot log
//! (`docs/NETBOOT.md` §6), kept in a directory per machine and printed.
//!
//! The loader can only fetch -- iPXE's download protocol is the one network
//! it has -- so a report comes as GETs, the bytes hex in the query:
//!
//! ```text
//! GET /ferrix/report/box/<mac>/<FILE>?part=<i>&of=<n>&data=<hex>
//! ```
//!
//! parts in order from 0, each answered `ok`; the last completes the file.
//! Everything in a request is checked before it is used -- the machine's
//! name, the file's, the numbers, the hex -- and a request that fails any
//! check is answered 400 and changes nothing. No path is ever taken from a
//! request: a file's path is built from the checked name and an allow-list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The files a machine may send.
pub(super) const FILES: &[&str] = &["BOOTLOG.TXT"];

/// The most parts one report may come in.
const MAX_PARTS: usize = 64;

/// The most bytes one report may hold, decoded.
const MAX_BYTES: usize = 64 * 1024;

/// One request of a report, checked.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Part {
    /// The machine: its MAC address, six lowercase hex pairs joined by `-`.
    pub(super) machine: String,
    /// The file, one of [`FILES`].
    pub(super) file: &'static str,
    pub(super) index: usize,
    pub(super) of: usize,
    pub(super) data: Vec<u8>,
}

/// A machine's name as `${netX/mac:hexhyp}` gives it, lowercased; `None`
/// for anything else.
pub(super) fn machine(raw: &str) -> Option<String> {
    let pairs: Vec<&str> = raw.split('-').collect();
    (pairs.len() == 6
        && pairs
            .iter()
            .all(|pair| pair.len() == 2 && pair.bytes().all(|byte| byte.is_ascii_hexdigit())))
    .then(|| raw.to_ascii_lowercase())
}

/// Read one report request: `rest` is the path after `report/box/`, `query`
/// what followed the `?`.
pub(super) fn part(rest: &str, query: &str) -> Result<Part, &'static str> {
    let (raw_machine, raw_file) = rest.split_once('/').ok_or("no file named")?;
    let machine = machine(raw_machine).ok_or("the machine is not a MAC address")?;
    let file = FILES
        .iter()
        .copied()
        .find(|&file| file == raw_file)
        .ok_or("not a file a machine may send")?;
    let mut index = None;
    let mut of = None;
    let mut data = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').ok_or("a query word without a value")?;
        let slot = match name {
            "part" => &mut index,
            "of" => &mut of,
            "data" => &mut data,
            _ => return Err("an unknown query word"),
        };
        if slot.replace(value).is_some() {
            return Err("a query word given twice");
        }
    }
    let number = |value: Option<&str>| -> Result<usize, &'static str> {
        let value = value.ok_or("part, of or data missing")?;
        if value.is_empty() || value.len() > 3 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("part or of is not a small number");
        }
        value
            .parse()
            .map_err(|_| "part or of is not a small number")
    };
    let index = number(index)?;
    let of = number(of)?;
    if of == 0 || of > MAX_PARTS || index >= of {
        return Err("part is not one of 0..of, or of is over 64");
    }
    let data = hex(data.ok_or("data missing")?)?;
    Ok(Part {
        machine,
        file,
        index,
        of,
        data,
    })
}

/// Decode lowercase or uppercase hex.
fn hex(text: &str) -> Result<Vec<u8>, &'static str> {
    if !text.len().is_multiple_of(2) || text.len() / 2 > MAX_BYTES {
        return Err("data is not whole bytes of hex, or too long");
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |byte: u8| (byte as char).to_digit(16).ok_or("data is not hex");
            match pair {
                [high, low] => Ok((digit(*high)? << 4 | digit(*low)?) as u8),
                _ => Err("data is not whole bytes of hex"),
            }
        })
        .collect()
}

/// A report that arrived whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) machine: String,
    pub(crate) file: &'static str,
    pub(crate) bytes: Vec<u8>,
    /// Where it was kept, when the server keeps reports.
    pub(crate) path: Option<PathBuf>,
}

/// A report under way.
#[derive(Debug)]
struct Pending {
    /// How many parts it comes in.
    of: usize,
    /// The part expected next.
    next: usize,
    bytes: Vec<u8>,
}

/// The reports coming in, and those that came.
#[derive(Debug, Default)]
pub(super) struct Reports {
    /// Where each machine's directory goes; `None` keeps them in memory only.
    directory: Option<PathBuf>,
    /// Reports under way, by machine and file.
    pending: BTreeMap<(String, &'static str), Pending>,
    /// Reports that arrived whole, in order.
    pub(super) done: Vec<Report>,
}

impl Reports {
    pub(super) fn new(directory: Option<PathBuf>) -> Self {
        Reports {
            directory,
            ..Reports::default()
        }
    }

    /// Take one part. Part 0 starts the report afresh; any other must be the
    /// next of the same count, or the report is dropped. Answers the report
    /// when this part completed it, kept and printed.
    pub(super) fn take(&mut self, part: Part) -> Result<Option<Report>, &'static str> {
        let key = (part.machine.clone(), part.file);
        if part.index == 0 {
            let _ = self.pending.insert(
                key.clone(),
                Pending {
                    of: part.of,
                    next: 0,
                    bytes: Vec::new(),
                },
            );
        }
        let Some(pending) = self.pending.get_mut(&key) else {
            return Err("a part with no part 0 before it");
        };
        if pending.of != part.of
            || pending.next != part.index
            || pending.bytes.len() + part.data.len() > MAX_BYTES
        {
            let _ = self.pending.remove(&key);
            return Err("a part out of order, of another count, or past 64 KiB");
        }
        pending.bytes.extend_from_slice(&part.data);
        pending.next += 1;
        if pending.next < pending.of {
            return Ok(None);
        }
        let Some(whole) = self.pending.remove(&key) else {
            return Ok(None);
        };
        let path = self
            .directory
            .as_deref()
            .and_then(|directory| keep(directory, &part.machine, part.file, &whole.bytes));
        let report = Report {
            machine: part.machine,
            file: part.file,
            bytes: whole.bytes,
            path,
        };
        print(&report);
        self.done.push(report.clone());
        Ok(Some(report))
    }
}

/// Write a report into its machine's directory, as `<seconds>-<FILE>` and
/// as `<FILE>`, the latest; where the first went, or `None` (said) when it
/// could not be written.
fn keep(directory: &Path, machine: &str, file: &str, bytes: &[u8]) -> Option<PathBuf> {
    let directory = directory.join(machine);
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let path = directory.join(format!("{seconds}-{file}"));
    let written = std::fs::create_dir_all(&directory)
        .and_then(|()| std::fs::write(&path, bytes))
        .and_then(|()| std::fs::write(directory.join(file), bytes));
    match written {
        Ok(()) => Some(path),
        Err(error) => {
            println!(
                "  netboot: {machine} {file} not kept in {}: {error}",
                directory.display()
            );
            None
        }
    }
}

/// Print a report on the server's console, a line at a time.
fn print(report: &Report) {
    let kept = report
        .path
        .as_ref()
        .map(|path| format!(", kept in {}", path.display()))
        .unwrap_or_default();
    println!(
        "  netboot: machine {} sent {}, {} bytes{kept}:",
        report.machine,
        report.file,
        report.bytes.len()
    );
    for line in String::from_utf8_lossy(&report.bytes).lines() {
        println!("    | {}", line.trim_end_matches('\r'));
    }
}
