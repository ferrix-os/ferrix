//! The audit record's reader (`docs/certification/AUDIT.md` §4).
//!
//! The kernel gives pid 1 the one handle that reads the audit record, with
//! `READ` alone. Init keeps it here and, once `/` is settled, copies every
//! record into `/var/log/audit/<id>.bin` -- the boot's audit id in hex, so
//! two boots' records can never be mixed in one file -- 64 bytes each, as
//! `src/lib/proto/audit` lays them out. It asks once a second: the kernel
//! signals nothing, since a record is made where no port may be woken, and
//! whatever a ring overwrote between two reads shows in the file as a gap in
//! its numbers and is counted here. And it reads a last time just before it
//! asks for power-off, so that everything up to the power action itself is
//! on the volume.
//!
//! This is the interim the design allows until authentication gives
//! services uids of their own: then the reader becomes `audit.service`,
//! running as a user of its own, and init hands it the handle.
//!
//! The three rings are read in turn: the boot's own records first -- the
//! start-up record and the configuration, pinned where nothing overwrites
//! them -- then the high-value ring, passing over the numbers the boot
//! records already wrote, then the refusals.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::time::{Duration, Instant};

use ferrix_audit::{RECORD_BYTES, Record};
use ferrix_native::OwnedHandle;
use ferrix_native::pending;
use ferrix_native_abi::nr::AUDIT_READ_MAX;
use ferrix_native_abi::types::{AUDIT_BOOT, AUDIT_HIGH, AUDIT_REFUSALS};

use crate::sys::Native;

/// How often the rings are read.
pub(crate) const EVERY: Duration = Duration::from_secs(1);

/// Where the records go.
pub(crate) const DIRECTORY: &str = "/var/log/audit";

/// Records read at a time.
const BATCH: usize = AUDIT_READ_MAX as usize;

/// The reader.
#[derive(Debug)]
pub(crate) struct Reader {
    handle: OwnedHandle<Native>,
    path: String,
    file: File,
    /// Where each ring is read from next: the boot records, the high-value
    /// ring and the refusals.
    boot: u64,
    high: u64,
    refusals: u64,
    /// The high-value ring's numbers the boot records already wrote.
    pinned: BTreeSet<u64>,
    /// Records written, and records the rings overwrote before a read.
    written: u64,
    lost: u64,
    next: Instant,
}

/// A native call's refusal, as an I/O error.
fn refused(error: ferrix_native::Error) -> io::Error {
    io::Error::other(format!("audit_read: {error:?}"))
}

impl Reader {
    /// Open this boot's file under [`DIRECTORY`] on whatever `/` is now, and
    /// read every record there is into it.
    ///
    /// # Errors
    ///
    /// When the file cannot be made or written, or the kernel refuses a read.
    pub(crate) fn open(handle: OwnedHandle<Native>) -> io::Result<Reader> {
        let id = pending::audit_read(Native, &handle, AUDIT_BOOT, 0, &mut [])
            .map_err(refused)?
            .id;
        fs::create_dir_all(DIRECTORY)?;
        let path = format!("{DIRECTORY}/{id:032x}.bin");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut reader = Reader {
            handle,
            path,
            file,
            boot: 0,
            high: 0,
            refusals: 0,
            pinned: BTreeSet::new(),
            written: 0,
            lost: 0,
            next: Instant::now(),
        };
        reader.read()?;
        Ok(reader)
    }

    /// When the rings are to be read next.
    pub(crate) fn next_read(&self) -> Instant {
        self.next
    }

    /// Copy every record not yet copied, and read again in [`EVERY`].
    ///
    /// # Errors
    ///
    /// When the file cannot be written or the kernel refuses a read.
    pub(crate) fn read(&mut self) -> io::Result<()> {
        self.drain(AUDIT_BOOT)?;
        self.drain(AUDIT_HIGH)?;
        self.drain(AUDIT_REFUSALS)?;
        self.file.flush()?;
        self.next = Instant::now() + EVERY;
        Ok(())
    }

    /// Copy what ring `which` holds past its cursor.
    fn drain(&mut self, which: u64) -> io::Result<()> {
        let mut buffer = [0_u8; BATCH * RECORD_BYTES];
        loop {
            let from = match which {
                AUDIT_BOOT => self.boot,
                AUDIT_HIGH => self.high,
                _ => self.refusals,
            };
            let read = pending::audit_read(Native, &self.handle, which, from, &mut buffer)
                .map_err(refused)?;
            self.lost += read.lost;
            for chunk in buffer
                .as_chunks::<RECORD_BYTES>()
                .0
                .iter()
                .take(read.copied)
            {
                let sequence = Record::from_bytes(chunk).sequence;
                match which {
                    AUDIT_BOOT => {
                        let _ = self.pinned.insert(sequence);
                    }
                    AUDIT_HIGH if self.pinned.contains(&sequence) => continue,
                    _ => {}
                }
                self.file.write_all(chunk)?;
                self.written += 1;
            }
            match which {
                AUDIT_BOOT => self.boot = read.next,
                AUDIT_HIGH => self.high = read.next,
                _ => self.refusals = read.next,
            }
            if read.copied < BATCH {
                return Ok(());
            }
        }
    }

    /// One line for the console: where the records are, how many, and the
    /// numbers each ring is read from next.
    pub(crate) fn said(&self) -> String {
        format!(
            "audit: {} records in {}, {} lost; high-value ring next {}, refusals next {}",
            self.written, self.path, self.lost, self.high, self.refusals
        )
    }
}
