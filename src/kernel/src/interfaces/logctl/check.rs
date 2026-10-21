//! The log core's boot check, played from the driver's end of the channel.
//!
//! No machine the boot tests run on has a device whose binding may read the
//! log -- the Pixel 7's USB controller is the only one -- so the check claims
//! the log the way `log_control_create` does once the binding is judged, and
//! drives the channel as `usbdev` would. The binding's refusal, on every other
//! device, is the item's native call check's (`syscall/native_check.rs`).
//!
//! It requires: a first DATA from the oldest byte the log keeps, with what the
//! ring had already lost counted in it; a second that goes on from where the
//! first stopped; a second reader refused while the first holds the log; a
//! driver that breaks the protocol refused and its claim ended; and a claim
//! ended by its channel closing.

use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use ferrix_logctl::message::{MAX_BYTES, Message, READ_BYTES, Refusal};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;

use super::CreateError;
use crate::console::log;
use crate::object::Transfer;
use crate::object::channel::{Endpoint, ReadError, Sender};
use crate::timer;

/// How much each of the check's READs asks for.
const ASK: u32 = 64;

/// How long the check waits for an answer, or for a claim to end.
const PATIENCE_NANOS: u64 = 2_000_000_000;

/// What the check established, for the boot log.
#[derive(Debug)]
pub(crate) struct Report {
    /// Bytes the two DATA carried.
    pub(crate) carried: usize,
    /// What the first said the log had lost before it.
    pub(crate) lost: u64,
    /// How long the last claim took to end once its channel closed, in
    /// milliseconds: the task notices within one look.
    pub(crate) released_ms: u64,
}

/// Run the check.
///
/// # Errors
///
/// The first property that did not hold, as a sentence.
pub(crate) fn run() -> Result<Report, &'static str> {
    if super::claimed() {
        return Err("the log was claimed before the log core's check ran");
    }
    let driver = super::claim().map_err(|_| "the log check could not claim the log")?;
    if !matches!(super::claim(), Err(CreateError::InUse)) {
        return Err("a second reader was let in while one held the log");
    }
    let (first, lost) = read(&driver)?;
    let (second, _) = read(&driver)?;
    compare(&first, &second, lost)?;

    // DATA is the kernel's to send: a driver that sends one loses its claim,
    // though its end of the channel is still open. The core says why with a
    // REFUSED first; that is a courtesy, and once on an aarch64 boot under a
    // loaded host (566ca8be) the channel closed with none queued, which
    // docs/BACKLOG.md has a row for. So what is required is the property a
    // driver relies on: no answer but REFUSED or the channel closing, and
    // the claim ended.
    write(
        &driver,
        &Message::Data {
            lost: 0,
            bytes: b"x",
        },
    )?;
    let answer = receive(&driver)?;
    if answer.is_some_and(|kind| kind != Message::Refused(Refusal::Protocol).kind()) {
        return Err("a driver that sent DATA was answered with something other than REFUSED");
    }
    if answer.is_none() {
        // Said on every boot it happens, so the row counts its sightings.
        crate::console::println!(
            "  logctl   SIGHTING: a driver that sent DATA lost its claim with no REFUSED queued (docs/BACKLOG.md)"
        );
    }
    let _ = released()?;
    drop(driver);

    // And a claim ends when its channel closes.
    let driver = super::claim().map_err(|_| "the log could not be claimed again")?;
    drop(driver);
    let released_ms = released()?;
    Ok(Report {
        carried: first.len().saturating_add(second.len()),
        lost,
        released_ms,
    })
}

/// The first two DATA must be the log's own bytes, in order, from its oldest:
/// what a local read from the start gives, when nothing was lost between.
fn compare(first: &[u8], second: &[u8], lost: u64) -> Result<(), &'static str> {
    let mut cursor = 0u64;
    let mut local = vec![0u8; first.len().saturating_add(second.len())];
    let read = log::read(&mut cursor, &mut local);
    if read.lost != lost {
        // The ring went round between the DATA and this read, and the start
        // it reads from has moved: nothing to compare against.
        return Ok(());
    }
    let (head, tail) = local.split_at(first.len().min(local.len()));
    if head != first || tail != second {
        return Err("a reader's DATA are not the log's bytes, in order, from the oldest kept");
    }
    Ok(())
}

/// Send READ for [`ASK`] bytes and take the DATA that answers it: its bytes
/// and its lost count.
fn read(driver: &Endpoint) -> Result<(Vec<u8>, u64), &'static str> {
    write(driver, &Message::Read { max: ASK })?;
    let message = next(driver)?;
    match Message::decode(&message) {
        Ok(Message::Data { lost, bytes }) if !bytes.is_empty() && bytes.len() <= ASK as usize => {
            Ok((bytes.to_vec(), lost))
        }
        _ => Err("a READ was not answered with DATA of at most what it asked for"),
    }
}

/// Write `message` from the driver's end.
fn write(driver: &Endpoint, message: &Message<'_>) -> Result<(), &'static str> {
    let mut bytes = vec![0u8; READ_BYTES.max(message.encoded_len())];
    let len = message
        .encode_into(&mut bytes)
        .map_err(|_| "the log check could not encode its message")?;
    bytes.truncate(len);
    driver
        .write(Sender::KERNEL, bytes, 0, || {
            Ok::<Vec<Transfer>, Infallible>(Vec::new())
        })
        .map_err(|_| "the log core's end of the channel would not take a message")
}

/// The next message's type, or `None` if the channel closed with nothing.
fn receive(driver: &Endpoint) -> Result<Option<u32>, &'static str> {
    match next(driver) {
        Ok(bytes) => Ok(Message::decode(&bytes).ok().map(|message| message.kind())),
        Err(_) if driver.signals().intersects(Signals::PEER_CLOSED) => Ok(None),
        Err(why) => Err(why),
    }
}

/// The next message's bytes, waited for.
fn next(driver: &Endpoint) -> Result<Vec<u8>, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        match driver.read(MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => return Ok(message.bytes),
            Err(ReadError::Empty) => {}
            Err(_) => return Err("the log core's channel closed without answering"),
        }
        let ready = driver.waiters().wait_until_deadline(
            || {
                driver
                    .signals()
                    .intersects(Signals::READABLE | Signals::PEER_CLOSED)
            },
            deadline,
        );
        if !ready {
            return Err("the log core did not answer within two seconds");
        }
    }
}

/// Wait for the claim to end, and answer how long it took in milliseconds.
fn released() -> Result<u64, &'static str> {
    let start = timer::now_nanos();
    let deadline = start.saturating_add(PATIENCE_NANOS);
    while super::claimed() {
        if timer::now_nanos() >= deadline {
            return Err("the log's claim did not end with its reader");
        }
        crate::sched::sleep_for(1_000_000);
    }
    Ok(timer::now_nanos().saturating_sub(start) / 1_000_000)
}
