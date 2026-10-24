//! A lode store on a file system: its segments read (the uploader) and
//! written (the receiver), with `ferrix-lode-core`'s formats.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use ferrix_lode_core::blob;
use ferrix_lode_core::compress;
use ferrix_lode_core::record::Record;
use ferrix_lode_core::segment::{self, Header};
use ferrix_lode_core::summary::Summary;

/// The size a written segment is sealed at, as lode's.
pub const SEGMENT_MAX: u64 = 16 * 1024 * 1024;

/// One segment file.
#[derive(Debug, Clone)]
pub struct Segment {
    /// Its path.
    pub path: PathBuf,
    /// Its file name.
    pub name: String,
    /// The first sequence number its name says.
    pub first: u64,
}

impl Segment {
    /// Its header, compressed or not.
    #[must_use]
    pub fn header(&self) -> Option<Header> {
        let mut buf = [0_u8; 512];
        let got = File::open(&self.path)
            .and_then(|mut file| file.read(&mut buf))
            .ok()?;
        compress::header(buf.get(..got)?)
    }

    /// Its summary, if it has one that reads: a sealed segment's.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        let bytes = fs::read(summary_path(&self.path)).ok()?;
        Summary::decode(&bytes)
    }

    /// Its bytes from `offset` on as written: a compressed one unpacked.
    #[must_use]
    pub fn bytes(&self, offset: usize) -> Option<Vec<u8>> {
        let whole = compress::expand(fs::read(&self.path).ok()?);
        Some(whole.get(offset..).map(Vec::from).unwrap_or_default())
    }

    /// Its size on disk.
    #[must_use]
    pub fn size(&self) -> u64 {
        fs::metadata(&self.path).map_or(0, |meta| meta.len())
    }

    /// Its records.
    #[must_use]
    pub fn records(&self) -> Vec<Record> {
        self.bytes(0)
            .and_then(|bytes| segment::scan(&bytes, 0))
            .map(|scanned| {
                scanned
                    .records
                    .into_iter()
                    .map(|found| found.record)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// `<segment>.sum`.
fn summary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".sum");
    PathBuf::from(name)
}

/// Every segment in `dirs`, and in each user's directory in them
/// (`user-<uid>/`, `ferrix_lode_core::access`), by first sequence number.
#[must_use]
pub fn segments(dirs: &[PathBuf]) -> Vec<Segment> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if ferrix_lode_core::access::user_of_dir(&name).is_some() {
                found_in(&entry.path(), Some(&name), &mut out);
                continue;
            }
            push_segment(entry.path(), name, &mut out);
        }
    }
    out.sort_by_key(|segment| segment.first);
    out
}

/// The segments of the user's directory `dir`, named `user/<file>`.
fn found_in(dir: &Path, user: Option<&str>, out: &mut Vec<Segment>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if segment::is_segment_name(&name) {
            let shown = user.map_or_else(|| name.clone(), |user| format!("{user}/{name}"));
            let first = segment::seqnum_of_name(&name).unwrap_or(0);
            out.push(Segment {
                path: entry.path(),
                name: shown,
                first,
            });
        }
    }
}

/// `path`, named `name`, if it is a segment.
fn push_segment(path: PathBuf, name: String, out: &mut Vec<Segment>) {
    if !segment::is_segment_name(&name) {
        return;
    }
    let first = segment::seqnum_of_name(&name).unwrap_or(0);
    out.push(Segment { path, name, first });
}

/// A store being written by the receiver: one segment open at a time,
/// sealed at [`SEGMENT_MAX`] or a new boot -- summarised and compressed, as
/// lode seals its own.
#[derive(Debug)]
pub struct Writer {
    /// The store's directory.
    dir: PathBuf,
    /// The open segment, if there is one.
    open: Option<Open>,
    /// The machine's id, for new segments' headers.
    machine_id: [u8; 16],
}

/// A segment being appended to.
#[derive(Debug)]
struct Open {
    /// Its path.
    path: PathBuf,
    /// The file.
    file: File,
    /// Its length.
    size: u64,
    /// Its boot.
    boot_id: [u8; 16],
    /// Its last sequence number.
    last: u64,
}

impl Writer {
    /// Begin on the store at `dir`, made if it is not there, going on in its
    /// newest unsealed segment.
    ///
    /// # Errors
    ///
    /// A directory that cannot be made or read.
    pub fn open(dir: &Path, machine_id: [u8; 16]) -> std::io::Result<Writer> {
        fs::create_dir_all(dir)?;
        let mut writer = Writer {
            dir: dir.to_owned(),
            open: None,
            machine_id,
        };
        if let Some(newest) = segments(&[dir.to_owned()]).pop()
            && newest.summary().is_none()
            && let Some(bytes) = newest.bytes(0)
            && !compress::is_compressed(&fs::read(&newest.path)?)
            && let Some(scanned) = segment::scan(&bytes, 0)
        {
            let file = OpenOptions::new().append(true).open(&newest.path)?;
            // A torn tail from a crash: cut, as lode cuts its own.
            file.set_len(scanned.good as u64)?;
            writer.open = Some(Open {
                path: newest.path.clone(),
                file,
                size: scanned.good as u64,
                boot_id: scanned.header.boot_id,
                last: scanned
                    .records
                    .last()
                    .map_or(0, |found| found.record.seqnum),
            });
        }
        Ok(writer)
    }

    /// The highest sequence number stored, and the boots the store holds:
    /// what makes a retried upload's records known.
    #[must_use]
    pub fn known(&self) -> (u64, Vec<[u8; 16]>) {
        let mut highest = 0;
        let mut boots: Vec<[u8; 16]> = Vec::new();
        for each in segments(std::slice::from_ref(&self.dir)) {
            let (last, boot) = match (each.summary(), each.header()) {
                (Some(summary), Some(header)) => (summary.seqs.1, header.boot_id),
                (_, Some(header)) => (
                    each.records().last().map_or(0, |record| record.seqnum),
                    header.boot_id,
                ),
                _ => continue,
            };
            highest = highest.max(last);
            if !boots.contains(&boot) {
                boots.push(boot);
            }
        }
        (highest, boots)
    }

    /// Append `records`, synced before this returns, in segments of their
    /// boots, the host named `host`.
    ///
    /// # Errors
    ///
    /// A write or sync that failed: nothing after it is taken as stored.
    pub fn append(&mut self, records: &[Record], host: &str) -> std::io::Result<()> {
        for record in records {
            let stale = self.open.as_ref().is_some_and(|open| {
                open.boot_id != record.boot_id
                    || open.size >= SEGMENT_MAX
                    || record.seqnum <= open.last
            });
            if stale {
                self.seal()?;
            }
            if self.open.is_none() {
                self.begin(record, host)?;
            }
            let Some(open) = self.open.as_mut() else {
                continue;
            };
            let mut bytes = Vec::new();
            segment::encode_record(record, &mut bytes);
            open.file.write_all(&bytes)?;
            open.size += bytes.len() as u64;
            open.last = record.seqnum;
        }
        if let Some(open) = &self.open {
            open.file.sync_data()?;
        }
        Ok(())
    }

    /// A new segment for `record`'s boot, beginning at its sequence number.
    fn begin(&mut self, record: &Record, host: &str) -> std::io::Result<()> {
        let header = Header {
            boot_id: record.boot_id,
            machine_id: self.machine_id,
            first_seqnum: record.seqnum,
            created_us: now_us(),
            host: host.to_owned(),
        };
        let path = self
            .dir
            .join(segment::file_name("system", &record.boot_id, record.seqnum));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)?;
        let bytes = header.encode();
        file.write_all(&bytes)?;
        self.open = Some(Open {
            path,
            file,
            size: bytes.len() as u64,
            boot_id: record.boot_id,
            last: 0,
        });
        Ok(())
    }

    /// Seal the open segment: synced, summarised, compressed in its place.
    ///
    /// # Errors
    ///
    /// A sync, read or write that failed.
    pub fn seal(&mut self) -> std::io::Result<()> {
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        open.file.sync_data()?;
        drop(open.file);
        let raw = fs::read(&open.path)?;
        if let Some(scanned) = segment::scan(&raw, 0) {
            let records: Vec<Record> = scanned
                .records
                .into_iter()
                .map(|found| found.record)
                .collect();
            replace(&summary_path(&open.path), &Summary::of(&records).encode())?;
        }
        if let Some(packed) = compress::compress(raw.clone())
            && packed.len() < raw.len()
        {
            replace(&open.path, &packed)?;
        }
        Ok(())
    }
}

/// Put `bytes` at `path`: a new file beside it, synced, renamed over it.
///
/// # Errors
///
/// A write, sync or rename that failed.
pub fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut fresh = path.as_os_str().to_owned();
    fresh.push(".new");
    let fresh = PathBuf::from(fresh);
    let mut file = File::create(&fresh)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    drop(file);
    fs::rename(&fresh, path)
}

/// The wall clock, in microseconds since 1970.
#[must_use]
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        })
}

/// The blob container's directory under a store directory, as lode's.
pub const BLOBS: &str = "blobs";

/// The packs of the blob containers beside `dirs`, newest name last.
fn packs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir.join(BLOBS)) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("pack-") && name.ends_with(".fxb"))
            })
            .collect();
        found.sort();
        out.extend(found);
    }
    out
}

/// The blob `id` from the containers beside `dirs`: its type and bytes,
/// checked against the id; `None` when no pack holds it whole.
#[must_use]
pub fn blob(dirs: &[PathBuf], id: &blob::BlobId) -> Option<(String, Vec<u8>)> {
    for pack in packs(dirs) {
        let index = fs::read(pack.with_extension("idx"))
            .ok()
            .and_then(|bytes| blob::decode_index(&bytes));
        let offset = match index {
            Some(index) => index
                .iter()
                .find(|(each, _)| each == id)
                .map(|(_, offset)| *offset),
            None => None,
        };
        let Ok(bytes) = fs::read(&pack) else {
            continue;
        };
        let offset = offset.or_else(|| {
            blob::scan(&bytes)
                .into_iter()
                .find(|entry| entry.id == *id)
                .map(|entry| entry.offset)
        });
        if let Some(found) = offset.and_then(|offset| blob::read(&bytes, offset)) {
            return Some(found);
        }
    }
    None
}

/// A machine's blob container, as the receiver keeps it: packs appended to
/// and synced a blob at a time, a new one past [`blob::PACK_MAX`]. A
/// reader without an index scans a pack, so none is written.
#[derive(Debug)]
pub struct BlobWriter {
    /// The container's directory.
    dir: PathBuf,
    /// The ids it holds.
    known: std::collections::BTreeSet<blob::BlobId>,
    /// The pack being appended to, and its length.
    open: Option<(File, u64)>,
}

impl BlobWriter {
    /// The container of the store at `store`, its ids read from its packs.
    ///
    /// # Errors
    ///
    /// A directory that cannot be made.
    pub fn open(store: &Path) -> std::io::Result<BlobWriter> {
        let dir = store.join(BLOBS);
        fs::create_dir_all(&dir)?;
        let mut known = std::collections::BTreeSet::new();
        for pack in packs(&[store.to_owned()]) {
            if let Ok(bytes) = fs::read(&pack) {
                known.extend(blob::scan(&bytes).into_iter().map(|entry| entry.id));
            }
        }
        Ok(BlobWriter {
            dir,
            known,
            open: None,
        })
    }

    /// Whether it holds `id`.
    #[must_use]
    pub fn has(&self, id: &blob::BlobId) -> bool {
        self.known.contains(id)
    }

    /// Keep `bytes`, of type `kind`, as `id` (checked by the caller), synced
    /// before it returns; a blob held already is not written again.
    ///
    /// # Errors
    ///
    /// A pack that cannot be made, written or synced.
    pub fn keep(&mut self, id: &blob::BlobId, kind: &str, bytes: &[u8]) -> std::io::Result<()> {
        if self.known.contains(id) {
            return Ok(());
        }
        let entry = blob::encode_entry(id, kind, bytes);
        let full = self.open.as_ref().is_some_and(|(_, size)| {
            size.saturating_add(entry.len() as u64) > blob::PACK_MAX as u64
        });
        if full || self.open.is_none() {
            let name = format!("pack-{}.fxb", blob::hex(id).get(..16).unwrap_or_default());
            let mut file = OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(self.dir.join(name))?;
            file.write_all(&blob::PACK_MAGIC)?;
            self.open = Some((file, blob::PACK_MAGIC.len() as u64));
        }
        let Some((file, size)) = self.open.as_mut() else {
            return Err(std::io::Error::other("no pack"));
        };
        file.write_all(&entry)?;
        file.sync_data()?;
        *size = size.saturating_add(entry.len() as u64);
        let _ = self.known.insert(*id);
        Ok(())
    }
}
