//! `ferrix-eventlog`: read the Ferrix event log, or append a pstore record
//! to it, from Linux (`docs/POSTMORTEM.md` §4).
//!
//! ```text
//! ferrix-eventlog list   DEVICE [--text] [--block BYTES]
//! ferrix-eventlog append DEVICE (--mem ADDRESS | --region FILE) [--block BYTES]
//! ```
//!
//! `DEVICE` is the event-log partition (`/dev/disk/by-partlabel/ferrix-events`
//! on the N150 box) or a file holding exactly it. `list` opens it read-only
//! and prints every record the header counts, oldest first, with `--text`
//! the whole text. `append` is what the inspection Linux runs at boot when
//! the boot manager started it instead of Ferrix: it reads the pstore region
//! at physical `ADDRESS` from `/dev/mem` (reserved with `memmap=`), and
//! appends its panic record exactly as the loader would, skipping one the
//! log holds already. It never writes the region, which the next Ferrix
//! loader finds as it was and skips as already kept. `--region FILE` reads
//! the region from a file instead (a saved copy, `test-postmortem`'s).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

use ferrix_bootinfo::pstore::{self, HEADER_BYTES};
use ferrix_eventlog::{self as eventlog, Geometry, HEAD_LEN, Outcome, RECORD_LEN};
use ferrix_slots::gpt::Part;
use ferrix_slots::record::{ControlDisk, DeviceError};

/// A line on standard output; a closed output is not this tool's failure.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// A line on standard error.
macro_rules! tell {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

/// A partition (or a file of one) as the log's block device.
struct Device {
    file: File,
    block: u32,
}

impl ControlDisk for Device {
    fn block_size(&self) -> u32 {
        self.block
    }

    fn write_block(&mut self, lba: u64, bytes: &[u8]) -> Result<(), DeviceError> {
        let at = lba.checked_mul(u64::from(self.block)).ok_or(DeviceError)?;
        // Seek and write rather than `FileExt::write_all_at`, which is
        // Unix's only: the tool builds and is tested on Windows too.
        let _ = self
            .file
            .seek(SeekFrom::Start(at))
            .map_err(|_| DeviceError)?;
        self.file.write_all(bytes).map_err(|_| DeviceError)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        self.file.sync_data().map_err(|_| DeviceError)
    }

    fn read_block(&mut self, lba: u64, bytes: &mut [u8]) -> Result<(), DeviceError> {
        let at = lba.checked_mul(u64::from(self.block)).ok_or(DeviceError)?;
        let _ = self
            .file
            .seek(SeekFrom::Start(at))
            .map_err(|_| DeviceError)?;
        self.file.read_exact(bytes).map_err(|_| DeviceError)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            tell!("ferrix-eventlog: {why}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let usage = || {
        "usage: ferrix-eventlog list DEVICE [--text] [--block BYTES] | append DEVICE --mem ADDRESS [--block BYTES]"
            .to_owned()
    };
    let (command, device) = match args {
        [command, device, ..] => (command.as_str(), device.as_str()),
        _ => return Err(usage()),
    };
    let rest = args.get(2..).unwrap_or_default();
    let block = value(rest, "--block")
        .map(|text| {
            text.parse::<u32>()
                .map_err(|why| format!("--block {text}: {why}"))
        })
        .transpose()?
        .unwrap_or(512);
    match command {
        "list" => list(device, block, rest.iter().any(|arg| arg == "--text")),
        "append" => {
            let source = match (value(rest, "--mem"), value(rest, "--region")) {
                (Some(address), None) => Source::Memory(parse_address(address)?),
                (None, Some(file)) => Source::File(file),
                _ => return Err(usage()),
            };
            append(device, block, &source)
        }
        _ => Err(usage()),
    }
}

/// The word after `name` in `args`.
fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at.checked_add(1)?).map(String::as_str)
}

/// A physical address, decimal or `0x` hex.
fn parse_address(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|why| format!("--mem {text}: {why}"))
}

/// Open `path` as the log: its geometry from its length.
fn open(path: &str, block: u32, write: bool) -> Result<(Device, Geometry), String> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .open(path)
        .map_err(|why| format!("{path}: {why}"))?;
    let bytes = device_bytes(&file).map_err(|why| format!("{path}: {why}"))?;
    let blocks = bytes / u64::from(block.max(1));
    if blocks == 0 {
        return Err(format!("{path} is empty"));
    }
    let part = Part {
        index: 0,
        unique: [0; 16],
        first_lba: 0,
        last_lba: blocks - 1,
    };
    let geometry = Geometry::of(&part, block).map_err(|why| format!("{path}: {why:?}"))?;
    Ok((Device { file, block }, geometry))
}

/// A file's or a block device's length.
fn device_bytes(file: &File) -> std::io::Result<u64> {
    let mut file = file;
    let end = file.seek(SeekFrom::End(0))?;
    let _ = file.seek(SeekFrom::Start(0))?;
    Ok(end)
}

/// `list`.
fn list(path: &str, block: u32, text: bool) -> Result<(), String> {
    let (mut device, geometry) = open(path, block, false)?;
    let header =
        eventlog::read_header(&mut device, &geometry).map_err(|why| format!("{path}: {why:?}"))?;
    let Some(header) = header else {
        say!(
            "{path}: no valid header: an empty event log ({} records of room)",
            geometry.records()
        );
        return Ok(());
    };
    let mut buffer = vec![0u8; RECORD_LEN];
    let mut found = Vec::new();
    for slot in 0..geometry.records() {
        if let Ok(Some(meta)) = eventlog::read_record(&mut device, &geometry, slot, &mut buffer)
            && eventlog::counted(&header, &meta)
        {
            let body = eventlog::decode_record(&buffer)
                .map(|(_, body)| body.to_vec())
                .unwrap_or_default();
            found.push((meta, body));
        }
    }
    found.sort_by_key(|(meta, _)| meta.index);
    say!(
        "{path}: {} record(s) held, {} ever appended, room for {}",
        found.len(),
        header.appended,
        geometry.records()
    );
    for (meta, body) in &found {
        let line = String::from_utf8_lossy(eventlog::first_line(body, 200)).into_owned();
        let time = i64::from_le_bytes(meta.last_time);
        let flags = [
            (meta.flags & eventlog::flags::TRUNCATED != 0).then_some("truncated"),
            (meta.flags & eventlog::flags::UNCHECKED != 0).then_some("unchecked"),
        ];
        let flags: Vec<&str> = flags.into_iter().flatten().collect();
        say!(
            "#{} {} {} time(s), boots {}..{}, unix time {time}, {} bytes ({}): {line}",
            meta.index,
            meta.kind.word(),
            meta.count,
            meta.first_boot,
            meta.last_boot,
            meta.text_len,
            flags.join(", ")
        );
        if text {
            say!("{}", String::from_utf8_lossy(body));
        }
    }
    Ok(())
}

/// Where `append` reads the pstore region from.
enum Source<'a> {
    /// `/dev/mem` at this physical address.
    Memory(u64),
    /// A file holding a copy of it.
    File(&'a str),
}

/// The region's bytes.
fn read_region(source: &Source<'_>) -> Result<Vec<u8>, String> {
    let mut region = vec![0u8; usize::try_from(pstore::LEN).map_err(|why| why.to_string())?];
    let (path, at) = match *source {
        Source::Memory(address) => ("/dev/mem", address),
        Source::File(path) => (path, 0),
    };
    let mut file = File::open(path).map_err(|why| format!("{path}: {why}"))?;
    let _ = file
        .seek(SeekFrom::Start(at))
        .map_err(|why| format!("{path} at {at:#x}: {why}"))?;
    file.read_exact(&mut region)
        .map_err(|why| format!("{path} at {at:#x}: {why}"))?;
    Ok(region)
}

/// `append`.
fn append(path: &str, block: u32, source: &Source<'_>) -> Result<(), String> {
    let region = read_region(source)?;
    let header: [u8; HEADER_BYTES] = region
        .get(..HEADER_BYTES)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("the region is too short")?;
    let ring = region
        .get(usize::try_from(pstore::HEADER).map_err(|why| why.to_string())?..)
        .ok_or("the region is too short")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(0));
    let mut record = vec![0u8; RECORD_LEN];
    let text = record.get_mut(HEAD_LEN..).ok_or("no record buffer")?;
    let Some(incoming) = eventlog::compose(&header, ring, now.to_le_bytes(), text) else {
        say!("the pstore region holds no panic record to keep");
        return Ok(());
    };
    let (mut device, geometry) = open(path, block, true)?;
    let mut scratch = vec![0u8; RECORD_LEN];
    let outcome = eventlog::append(&mut device, &geometry, &incoming, &mut record, &mut scratch)
        .map_err(|why| format!("{path}: the append failed: {why:?}"))?;
    match outcome {
        Outcome::Appended { index, slot } => {
            say!(
                "boot {}'s panic kept as record #{index} (slot {slot})",
                incoming.boot
            );
        }
        Outcome::Collapsed { index, count } => {
            say!(
                "boot {}'s panic is record #{index}'s again: {count} times",
                incoming.boot
            );
        }
        Outcome::Skipped { index } => {
            say!("boot {}'s panic is already record #{index}", incoming.boot);
        }
    }
    Ok(())
}
