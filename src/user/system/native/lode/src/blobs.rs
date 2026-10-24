//! The blob container on disk (`ferrix_lode_core::blob`, `docs/LOGGING.md`
//! §21.4): `<store>/blobs/pack-<16 hex>.fxb`, written by the service only.
//!
//! The service queues a blob ([`Blobs::submit`]) while it makes the record
//! that names it, and writes the queue to the open pack and syncs it
//! ([`Blobs::flush`]) before that record's batch is appended: the blob is
//! on disk first. The queue is bounded; a blob that does not fit, or a
//! pack that cannot be written, is said in the record instead
//! (`BLOB_DROPPED`), and record ingest never waits for it. A sealed pack
//! gets its index; a sweep at vacuum time removes or rewrites the packs
//! whose blobs no record names any more.
//!
//! Readers find a blob by id through [`find`], from any store directory.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::blob::{self, BlobId, Entry, Sweep};

use crate::sys;

/// The blobs' directory under a store directory.
pub(crate) const DIR: &[u8] = b"blobs";

/// The most bytes queued for writing.
const QUEUE_MAX: usize = 16 * 1024 * 1024;

/// The pack being written.
struct Open {
    /// Its descriptor.
    fd: usize,
    /// Its name.
    name: String,
    /// Its length.
    length: u64,
    /// Its entries.
    entries: Vec<Entry>,
}

/// The container a service writes.
pub(crate) struct Blobs {
    /// The `blobs` directory, once made, and the store directory it is in.
    dir: Option<(usize, usize)>,
    /// The pack being written.
    open: Option<Open>,
    /// Blobs queued: id, type, bytes.
    queue: VecDeque<(BlobId, String, Vec<u8>)>,
    /// What the queue holds.
    queued: usize,
    /// Every id on disk or queued, so a blob is written once.
    known: BTreeSet<BlobId>,
    /// Bytes written to packs since the service started, for the write
    /// budget's row.
    pub(crate) written: u64,
}

impl Blobs {
    /// No container yet: it is made in the store directory the first blob
    /// is written to.
    pub(crate) fn new() -> Blobs {
        Blobs {
            dir: None,
            open: None,
            queue: VecDeque::new(),
            queued: 0,
            known: BTreeSet::new(),
            written: 0,
        }
    }

    /// Queue `bytes` as a blob of `kind`; `Err` with why when it cannot be,
    /// for the record to say.
    pub(crate) fn submit(&mut self, id: BlobId, kind: &str, bytes: Vec<u8>) -> Result<(), String> {
        if self.known.contains(&id) {
            return Ok(());
        }
        if bytes.len() > blob::BLOB_MAX {
            return Err(String::from("larger than a blob may be"));
        }
        if self.queued + bytes.len() > QUEUE_MAX {
            return Err(String::from("the blob queue is full"));
        }
        self.queued += bytes.len();
        let _ = self.known.insert(id);
        self.queue.push_back((id, String::from(kind), bytes));
        Ok(())
    }

    /// Write what is queued to the pack in `store`, and sync it once.
    /// Gives the ids that could not be written.
    pub(crate) fn flush(&mut self, store: usize) -> Vec<BlobId> {
        if self.queue.is_empty() {
            return Vec::new();
        }
        let mut failed = Vec::new();
        let mut wrote = false;
        while let Some((id, kind, bytes)) = self.queue.pop_front() {
            self.queued = self.queued.saturating_sub(bytes.len());
            if self.append(store, &id, &kind, &bytes).is_err() {
                let _ = self.known.remove(&id);
                failed.push(id);
            } else {
                wrote = true;
            }
        }
        if wrote && let Some(open) = &self.open {
            let _ = sys::fdatasync(open.fd);
        }
        failed
    }

    /// The `blobs` directory in `store`, made if need be.
    fn dir_in(&mut self, store: usize) -> Result<usize, sys::Errno> {
        if let Some((dir, of)) = self.dir {
            if of == store {
                return Ok(dir);
            }
            self.seal();
            sys::close(dir);
            self.dir = None;
        }
        sys::mkdirat(store, DIR, 0o2750)?;
        let dir = sys::openat(store, DIR, sys::O_RDONLY | sys::O_CLOEXEC, 0)?;
        for name in sys::list_dir(dir).unwrap_or_default() {
            for (id, _) in pack_entries(dir, &name) {
                let _ = self.known.insert(id);
            }
        }
        self.dir = Some((dir, store));
        Ok(dir)
    }

    /// One blob onto the open pack, a new one begun when there is none or
    /// it is full.
    fn append(
        &mut self,
        store: usize,
        id: &BlobId,
        kind: &str,
        bytes: &[u8],
    ) -> Result<(), sys::Errno> {
        let dir = self.dir_in(store)?;
        if self
            .open
            .as_ref()
            .is_some_and(|open| open.length >= blob::PACK_MAX as u64)
        {
            self.seal();
        }
        if self.open.is_none() {
            let mut name = String::from("pack-");
            ferrix_lode_core::segment::push_hex(&mut name, id.get(..8).unwrap_or_default());
            name.push_str(".fxb");
            let fd = sys::openat(
                dir,
                name.as_bytes(),
                sys::O_WRONLY | sys::O_CREAT | sys::O_EXCL | sys::O_CLOEXEC,
                0o640,
            )?;
            sys::write_all(fd, &blob::PACK_MAGIC)?;
            self.open = Some(Open {
                fd,
                name,
                length: blob::PACK_MAGIC.len() as u64,
                entries: Vec::new(),
            });
        }
        let Some(open) = self.open.as_mut() else {
            return Err(sys::Errno(sys::EINVAL));
        };
        let entry = blob::encode_entry(id, kind, bytes);
        sys::write_all(open.fd, &entry)?;
        open.entries.push(Entry {
            id: *id,
            offset: open.length,
            length: entry.len() as u64,
            size: bytes.len() as u64,
            kind: String::from(kind),
        });
        open.length += entry.len() as u64;
        self.written += entry.len() as u64;
        Ok(())
    }

    /// Close the open pack and write its index.
    pub(crate) fn seal(&mut self) {
        let (Some(open), Some((dir, _))) = (self.open.take(), self.dir) else {
            return;
        };
        let _ = sys::fdatasync(open.fd);
        sys::close(open.fd);
        let index = blob::encode_index(&open.entries);
        let name = open.name.replace(".fxb", ".idx");
        if let Ok(fd) = sys::openat(
            dir,
            name.as_bytes(),
            sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
            0o640,
        ) {
            let _ = sys::write_all(fd, &index).and_then(|()| sys::fdatasync(fd));
            sys::close(fd);
        }
    }

    /// Mark and sweep (at vacuum time): `live` every id the remaining
    /// records name. A sealed pack older than the grace period with nothing
    /// named is removed; with less than half named, rewritten. Gives the
    /// packs removed.
    pub(crate) fn sweep(&mut self, live: &BTreeSet<BlobId>, now_s: u64) -> usize {
        let Some((dir, _)) = self.dir else {
            return 0;
        };
        let open = self.open.as_ref().map(|open| open.name.clone());
        let mut removed = 0;
        for name in sys::list_dir(dir).unwrap_or_default() {
            let Ok(text) = String::from_utf8(name.clone()) else {
                continue;
            };
            if !text.ends_with(".fxb") || Some(&text) == open.as_ref() {
                continue;
            }
            let young = sys::mtime_at(dir, &name)
                .is_none_or(|mtime| now_s.saturating_sub(mtime) < blob::GRACE_US / 1_000_000);
            let Ok(pack) = sys::read_file_from(dir, &name, 0) else {
                continue;
            };
            let entries = blob::scan(&pack);
            match blob::sweep(&entries, live, young) {
                Sweep::Keep => continue,
                Sweep::Rewrite(kept) => {
                    if !self.rewrite(&pack, kept) {
                        // Not all of it moved: keep the old pack.
                        continue;
                    }
                }
                Sweep::Remove => {}
            }
            for entry in &entries {
                if !live.contains(&entry.id) {
                    let _ = self.known.remove(&entry.id);
                }
            }
            let _ = sys::unlinkat(dir, &name);
            let _ = sys::unlinkat(dir, text.replace(".fxb", ".idx").as_bytes());
            removed += 1;
        }
        removed
    }
}

impl Blobs {
    /// The `kept` entries of `pack` written again, to the open pack;
    /// whether all of them were.
    fn rewrite(&mut self, pack: &[u8], kept: Vec<Entry>) -> bool {
        for entry in kept {
            let Some((kind, bytes)) = blob::read(pack, entry.offset) else {
                continue;
            };
            let _ = self.known.remove(&entry.id);
            let _ = self.submit(entry.id, &kind, bytes);
        }
        let store = self.dir.map_or(0, |(_, store)| store);
        self.flush(store).is_empty()
    }
}

/// The ids and offsets of the pack `name` in `dir`, from its index or a
/// scan; nothing for what is not a pack.
fn pack_entries(dir: usize, name: &[u8]) -> Vec<(BlobId, u64)> {
    if !name.ends_with(b".fxb") {
        return Vec::new();
    }
    let mut index = Vec::from(name);
    index.truncate(index.len() - 4);
    index.extend_from_slice(b".idx");
    if let Ok(bytes) = sys::read_file_from(dir, &index, 0)
        && let Some(entries) = blob::decode_index(&bytes)
    {
        return entries;
    }
    sys::read_file_from(dir, name, 0)
        .map(|pack| {
            blob::scan(&pack)
                .into_iter()
                .map(|entry| (entry.id, entry.offset))
                .collect()
        })
        .unwrap_or_default()
}

/// The blob `id`, from the containers in `stores` (store directories):
/// its type and bytes, checked against its id.
pub(crate) fn find(stores: &[usize], id: &BlobId) -> Option<(String, Vec<u8>)> {
    for &store in stores {
        let Ok(dir) = sys::openat(store, DIR, sys::O_RDONLY | sys::O_CLOEXEC, 0) else {
            continue;
        };
        let names = sys::list_dir(dir).unwrap_or_default();
        let found = names.iter().find_map(|name| {
            let (_, offset) = pack_entries(dir, name)
                .into_iter()
                .find(|(each, _)| each == id)?;
            let pack = sys::read_file_from(dir, name, 0).ok()?;
            blob::read(&pack, offset)
        });
        sys::close(dir);
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Every blob in the containers of `stores`: id, pack, entry.
pub(crate) fn all(stores: &[usize]) -> Vec<(BlobId, String, Entry)> {
    let mut out = Vec::new();
    for &store in stores {
        let Ok(dir) = sys::openat(store, DIR, sys::O_RDONLY | sys::O_CLOEXEC, 0) else {
            continue;
        };
        for name in sys::list_dir(dir).unwrap_or_default() {
            if !name.ends_with(b".fxb") {
                continue;
            }
            if let Ok(pack) = sys::read_file_from(dir, &name, 0) {
                let shown = String::from_utf8_lossy(&name).into_owned();
                for entry in blob::scan(&pack) {
                    out.push((entry.id, shown.clone(), entry));
                }
            }
        }
        sys::close(dir);
    }
    out
}
