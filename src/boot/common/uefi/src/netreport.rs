//! The boot log of a netbooted run, sent to the server it came from.
//!
//! A machine booted by iPXE over the network (`docs/NETBOOT.md`) has no
//! volume the loader can write: iPXE's file system is read-only, and the
//! machine's own disk is never touched. So the loader sends what it would
//! have written (`bootlog.rs`) to `cargo xtask netboot`, which keeps it in a
//! directory for the machine and prints it.
//!
//! Only when both hold: the loader's volume has `FERRIX/REPORT.TXT` -- the
//! URL to send to, which only the server's `boot.ipxe` fetches into iPXE's
//! file system and no disk image carries -- and the loader's device handle
//! carries iPXE's download protocol, which iPXE puts on every program it
//! starts. A loader started from a disk finds neither and sends nothing.
//!
//! The protocol only fetches, so the log goes as the query of a few GETs,
//! hex in parts of [`PART`] bytes, as `boot.ipxe`'s PCI report does
//! (`report/pci/...`). It is the only network a program `snp.efi` started
//! has: `snp.efi` opens the card's Simple Network Protocol exclusively,
//! which disconnects firmware's own TCP and HTTP from it.
//!
//! Plain HTTP with no authentication: anyone on the network path reads the
//! log, which holds where KASLR put the kernel. A netbooted machine already
//! runs whatever kernel that network serves it, so this tells nobody there
//! more than they could decide (`docs/NETBOOT.md` §6).
//!
//! **Nothing here can stop a boot.** Every failure is one line on the
//! console; a request is aborted after about [`WAIT_MS`], the whole send
//! after about [`TOTAL_MS`] (at most 64 parts of a 32 KiB log), and the
//! first request that fails ends the sending. Both count the 1 ms stalls
//! between polls, not the time spent inside iPXE's `poll`, so they are
//! approximate. Against a server that accepts connections and never
//! answers, a netbooted boot waits roughly 30 s more than it did; about
//! 5 s when the server is gone; nothing to speak of when it refuses (an
//! HTTP error ends the request at once).

use core::cell::Cell;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::bootlog;
use crate::console::println;
use crate::report_url::{MAX_URL as MAX_BASE, report_url as parse_report_url};
use crate::services::Services;
use crate::uefi::Status;
use crate::uefi::protocols::IpxeDownload;
use crate::uefi::tables::MemoryType;

/// The file `boot.ipxe` fetches the report URL into.
const REPORT_PATH: &str = "/FERRIX/REPORT.TXT";

/// Raw bytes of the log in one request: twice that in hex, a query of about
/// 1 KiB, well inside the server's 16 KiB request head. Small enough that
/// `test-netboot`'s log of some 500 bytes goes in more than one part.
const PART: usize = 512;

/// Room for one request's URL: the base, the file's name and the numbers,
/// the hex, and the terminating NUL.
const URL_BYTES: usize = MAX_BASE + 96 + 2 * PART + 1;

/// How long one request may take, in milliseconds, before it is aborted.
const WAIT_MS: u32 = 5000;

/// How long the whole send may take, in milliseconds.
const TOTAL_MS: u32 = 30_000;

/// Set once the loader leaves firmware: nothing may be sent after that.
static FIRMWARE_LEFT: AtomicBool = AtomicBool::new(false);

/// Say the loader is about to leave firmware.
pub(crate) fn firmware_left() {
    FIRMWARE_LEFT.store(true, Ordering::Relaxed);
}

/// Whether the loader has started to leave firmware.
pub(crate) fn has_left_firmware() -> bool {
    FIRMWARE_LEFT.load(Ordering::Relaxed)
}

/// Send the boot log as it stands, when iPXE started the loader; say what
/// came of it on the console.
pub(crate) fn send_boot_log(services: &Services) {
    // Both or nothing, and silently when either is missing: a loader started
    // from a disk says nothing it did not say before.
    let Some(download) = services.ipxe_download() else {
        return;
    };
    let mut base = [0u8; MAX_BASE];
    let Some(len) = report_url(services, &mut base) else {
        return;
    };
    let Some(base) = base.get(..len) else {
        return;
    };
    match send(services, download, base, "BOOTLOG.TXT") {
        Ok((bytes, parts)) => {
            println!(
                "  report   boot log sent to the netboot server, {bytes} bytes in {parts} parts"
            );
        }
        Err((part, failure)) => println!("  report   boot log not sent: part {part} {failure}"),
    }
}

/// The URL `REPORT.TXT` names, in `into`, and its length; `None` when the
/// volume has no such file or it is not one to use ([`parse_report_url`]),
/// which is said.
fn report_url(services: &Services, into: &mut [u8; MAX_BASE]) -> Option<usize> {
    let (file, len) = match services.read_file(REPORT_PATH, MemoryType::LOADER_DATA) {
        Ok(read) => read,
        Err(error) if error.status == Some(Status::NOT_FOUND) => return None,
        Err(error) => {
            println!("  report   {REPORT_PATH} could not be read: {error}");
            return None;
        }
    };
    // SAFETY: `read_file` filled `len` bytes of its own allocation, identity
    // mapped under boot services; it is freed only after the copy below.
    let bytes = unsafe { core::slice::from_raw_parts(file.address as *const u8, len as usize) };
    let copied = parse_report_url(bytes).map(|url| {
        if let Some(slot) = into.get_mut(..url.len()) {
            slot.copy_from_slice(url);
        }
        url.len()
    });
    let _ = services.free("freeing the report URL", file);
    match copied {
        Ok(len) => Some(len),
        Err(why) => {
            println!("  report   {REPORT_PATH} is ignored: {why}");
            None
        }
    }
}

/// Why a request failed.
#[derive(Clone, Copy, Debug)]
enum Failure {
    /// iPXE refused to start it.
    Start(Status),
    /// It ended with this status (an HTTP error, a connection refused).
    Ended(Status),
    /// It took longer than [`WAIT_MS`], or the send than [`TOTAL_MS`].
    TimedOut,
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failure::Start(status) => write!(f, "could not start: status {:#x}", status.code()),
            Failure::Ended(status) => write!(f, "failed: status {:#x}", status.code()),
            Failure::TimedOut => write!(
                f,
                "had no answer within {WAIT_MS} ms, or the send took {TOTAL_MS} ms in all"
            ),
        }
    }
}

/// Send the boot log as `name` under `base`, part by part; how many bytes
/// and parts went, or the part that failed and why. Prints nothing; each
/// part is copied out of the log before its request (`bootlog::copy`), so
/// nothing here holds a reference into the log while anything records.
fn send(
    services: &Services,
    download: *mut IpxeDownload,
    base: &[u8],
    name: &str,
) -> Result<(usize, usize), (usize, Failure)> {
    let total = bootlog::len();
    let parts = total.div_ceil(PART).max(1);
    let mut url = [0u8; URL_BYTES];
    let mut chunk = [0u8; PART];
    let mut budget = TOTAL_MS;
    for part in 0..parts {
        let from = part * PART;
        let want = total.saturating_sub(from).min(PART);
        let copied = bootlog::copy(from, chunk.get_mut(..want).unwrap_or(&mut []));
        let mut writer = UrlWriter {
            buffer: &mut url,
            used: 0,
        };
        writer.bytes(base);
        writer.bytes(name.as_bytes());
        writer.bytes(b"?part=");
        writer.number(part);
        writer.bytes(b"&of=");
        writer.number(parts);
        writer.bytes(b"&data=");
        for &byte in chunk.get(..copied).unwrap_or(&[]) {
            writer.hex(byte);
        }
        if !writer.terminate() {
            // Cannot happen with the bounds above; a URL cut short would
            // send a wrong part, so none is sent.
            return Err((part, Failure::Start(Status::LOAD_ERROR)));
        }
        fetch(services, download, &url, &mut budget).map_err(|failure| (part, failure))?;
    }
    Ok((total, parts))
}

/// One request's state, which iPXE's callbacks write.
struct Pending {
    done: Cell<bool>,
    status: Cell<Status>,
}

/// iPXE's data callback: the server's answer is a short `ok`, not kept.
///
/// # Safety
///
/// Called by iPXE only, from inside the download protocol's `poll`, with
/// the arguments `efi_download.c` passes; it reads none of them, so any
/// values are sound.
unsafe extern "efiapi" fn on_data(
    _context: *mut c_void,
    _buffer: *mut c_void,
    _length: usize,
    _offset: usize,
) -> Status {
    Status(0)
}

/// iPXE's finish callback: note that the request ended, and how.
///
/// # Safety
///
/// `context` must be the `Pending` [`fetch`] passed to `start`, still live.
/// iPXE calls this exactly once per request, from `efi_download_close`:
/// inside `poll` when the request ends, or inside `abort`
/// (`efi_download_abort` -> `efi_download_close`) before `abort` returns.
/// Both happen while `fetch` is still waiting, so the `Pending` on its stack
/// outlives every call.
unsafe extern "efiapi" fn on_finish(context: *mut c_void, status: Status) {
    // SAFETY: `context` is the live `Pending` this function's contract
    // names: `fetch` does not return before the request finished or was
    // aborted, and both call this before that.
    let pending = unsafe { &*context.cast::<Pending>() };
    pending.status.set(status);
    pending.done.set(true);
}

/// Fetch the NUL-terminated `url` through iPXE and wait for it to end,
/// [`WAIT_MS`] at most and no more than `budget` milliseconds, which it
/// spends.
fn fetch(
    services: &Services,
    download: *mut IpxeDownload,
    url: &[u8],
    budget: &mut u32,
) -> Result<(), Failure> {
    let pending = Pending {
        done: Cell::new(false),
        status: Cell::new(Status(0)),
    };
    let context = ptr::from_ref(&pending).cast_mut().cast::<c_void>();
    let mut file: *mut c_void = ptr::null_mut();
    // SAFETY: `download` is iPXE's live protocol from the loader's device
    // handle (`Services::ipxe_download`), which iPXE keeps installed while
    // the program it started runs under boot services.
    let protocol = unsafe { &*download };
    // SAFETY: `download` is iPXE's live protocol from the loader's device
    // handle; `url` is NUL terminated (`UrlWriter::terminate`); the callbacks
    // match iPXE's types, and `context` outlives the request (`on_finish`).
    let started = unsafe {
        (protocol.start)(
            download,
            url.as_ptr(),
            on_data,
            on_finish,
            context,
            &raw mut file,
        )
    };
    if started.is_error() {
        return Err(Failure::Start(started));
    }
    let mut waited = 0;
    while !pending.done.get() {
        if waited >= WAIT_MS || *budget == 0 {
            // SAFETY: the request was started and has not finished, so its
            // token is live; aborting calls `on_finish` before it returns.
            let _ = unsafe { (protocol.abort)(download, file, Status::TIMEOUT) };
            return Err(Failure::TimedOut);
        }
        // SAFETY: as `start`: iPXE's live protocol.
        let _ = unsafe { (protocol.poll)(download) };
        if !pending.done.get() {
            services.stall(1000);
            waited += 1;
            *budget = budget.saturating_sub(1);
        }
    }
    let status = pending.status.get();
    if status.is_error() {
        return Err(Failure::Ended(status));
    }
    Ok(())
}

/// A URL built into a fixed buffer, one NUL short of full at most.
struct UrlWriter<'a> {
    buffer: &'a mut [u8; URL_BYTES],
    used: usize,
}

impl UrlWriter<'_> {
    fn byte(&mut self, byte: u8) {
        // One byte is always kept for the NUL.
        if self.used + 1 < URL_BYTES
            && let Some(slot) = self.buffer.get_mut(self.used)
        {
            *slot = byte;
            self.used += 1;
        } else {
            self.used = URL_BYTES;
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
    }

    fn number(&mut self, value: usize) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut rest = value;
        loop {
            at -= 1;
            if let Some(slot) = digits.get_mut(at) {
                *slot = b'0' + (rest % 10) as u8;
            }
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        self.bytes(digits.get(at..).unwrap_or(&[]));
    }

    fn hex(&mut self, byte: u8) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for nibble in [byte >> 4, byte & 0xf] {
            self.byte(DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0'));
        }
    }

    /// End the URL with its NUL; false when it did not fit.
    fn terminate(&mut self) -> bool {
        if self.used >= URL_BYTES {
            return false;
        }
        match self.buffer.get_mut(self.used) {
            Some(slot) => {
                *slot = 0;
                true
            }
            None => false,
        }
    }
}
