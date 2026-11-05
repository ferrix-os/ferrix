//! The guest half of `cargo xtask test-msync`, run as init on two boots of one disk.
//!
//! On a disk with no [`PATH`] (boot 1): a file of [`PAGES`] pages is written, synced, mapped
//! shared, written through the mapping and `msync(MS_SYNC)`ed, and `msync: synced` is printed, at
//! which the host cuts the power. On a disk with one (boot 2): every word is read back:
//! `msync: survived`, or `msync: lost <page>` at the first page the mapping's write did not reach
//! the disk for.

use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::os::unix::io::AsRawFd as _;
use std::process::ExitCode;

/// The file's pages.
const PAGES: usize = 64;
/// A page.
const PAGE: usize = 4096;
/// Every word after the mapping's write: its page's index over this.
const MAGIC: u64 = 0x4d53_594e;
/// What the file holds before it.
const BEFORE: u8 = 0x5a;
/// The file, on the writable disk the kernel mounts at stage 12.
const PATH: &str = "/mnt-rw/msync-test";

fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
    let _ = std::io::stdout().flush();
}

/// Write it, map it, msync it.
fn first_boot() -> Result<(), &'static str> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(PATH)
        .map_err(|error| {
            say(&format!("msync: create {error}"));
            "create"
        })?;
    file.write_all(&[BEFORE; PAGES * PAGE])
        .map_err(|_| "write")?;
    file.sync_all().map_err(|_| "fsync")?;
    // SAFETY: no arguments; it commits what the volume holds.
    unsafe { libc::sync() };
    // SAFETY: a fresh shared mapping of a file of this length, at no fixed address.
    let at = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            PAGES * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err("mmap");
    }
    let words = at.cast::<u64>();
    for page in 0..PAGES {
        for word in 0..PAGE / 8 {
            // SAFETY: inside the mapping just made, which is writable and aligned to a page.
            unsafe {
                words
                    .add(page * (PAGE / 8) + word)
                    .write_volatile(MAGIC | ((page as u64) << 32))
            };
        }
    }
    // SAFETY: the whole of the mapping.
    if unsafe { libc::msync(at, PAGES * PAGE, libc::MS_SYNC) } != 0 {
        return Err("msync");
    }
    say("msync: synced");
    Ok(())
}

/// Read it back.
fn second_boot() -> Result<(), &'static str> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(PATH)
        .map_err(|_| "open")?;
    let mut bytes = vec![0_u8; PAGES * PAGE];
    file.read_exact(&mut bytes).map_err(|_| "read")?;
    for (page, chunk) in bytes.chunks_exact(PAGE).enumerate() {
        let want = (MAGIC | ((page as u64) << 32)).to_ne_bytes();
        if chunk.chunks_exact(8).any(|word| word != want) {
            say(&format!("msync: lost {page}"));
            return Ok(());
        }
    }
    say("msync: survived");
    Ok(())
}

fn main() -> ExitCode {
    say("msync: started");
    let seen = std::fs::metadata(PATH);
    say(&format!(
        "msync: {PATH} {:?}",
        seen.as_ref().map(std::fs::Metadata::len)
    ));
    let done = if seen.is_ok() {
        second_boot()
    } else {
        first_boot()
    };
    if let Err(what) = done {
        say(&format!("msync: FAILED {what}"));
        return ExitCode::FAILURE;
    }
    // Held here: init ending ends the machine, and boot 1 is ended by its power.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
