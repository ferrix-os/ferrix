//! An offline disk sends nothing more and never changes an answer
//! (`docs/BLOCK-RING.md` §6.3, "A disk that stops answering"; the
//! certification consultant's review, ledger 4290 (2)).
//!
//! `test-disk-offline` shows the machine answering once a stalled disk is
//! offline, but it cannot show these two rules: btrfs waits for each write,
//! so nothing is ever queued behind the one on the ring when the disk goes,
//! and nothing answers a request twice. So this plays the ring's task and
//! the driver itself, on a disk of its own that no ring serves: it queues
//! more writes than the disk's depth lets onto its ring, takes the first two
//! off the queue as a driver would hold them, and lets the disk go offline
//! with a patience of 0. Then it requires:
//!
//! * every request answered EIO, and a new one refused;
//! * the drain to empty the queue -- the commands held and the commands
//!   still queued all answered -- with nothing left to dispatch, and the
//!   disk not dispatchable (stopped dispatch: a command answered as failed
//!   is never sent later);
//! * a completion of a held command that comes between the disk going
//!   offline and the drain -- the queue still knowing it -- and late ones
//!   after the drain, as a driver that recovered would send, to leave every
//!   answer EIO (an answer is never replaced);
//! * a successor taking the disk up to find it online with nothing to
//!   dispatch.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_blkring::{Device, DeviceFlags, DiskName};
use ferrix_block::{Limits, Request, Token};
use ferrix_vfs::Errno;

use super::{MAX_PARTS, RingDisk, answer, ticks};
use crate::object::port::Port;

/// One sector of 512 bytes per command, a depth of two on the ring, and
/// four writes: two held on the "ring", two left queued.
const BLOCK: u32 = 512;
const DEPTH: u32 = 2;
const WRITES: u64 = 4;

/// Run the check.
///
/// # Errors
///
/// Which rule did not hold.
pub(super) fn run() -> Result<(), &'static str> {
    let device = Device::new(BLOCK, 64, 1, DeviceFlags::default(), u64::from(BLOCK) * 4)
        .map_err(|_| "offline-check: no device")?;
    let limits =
        Limits::new(BLOCK, 64, 1, MAX_PARTS, DEPTH).map_err(|_| "offline-check: no limits")?;
    let name = DiskName::for_index(25).ok_or("offline-check: no name")?;
    let port = Port::new().map_err(|_| "offline-check: no port")?;
    let disk = Arc::new(RingDisk::new(device, name, limits, Arc::clone(&port)));
    let mut ids = Vec::new();
    for at in 0..WRITES {
        // Sectors apart, so the queue merges none of them.
        let request = Request::write(0, at * 2, 1).sync();
        let id = disk
            .submit(request, Some(vec![0xa5; BLOCK as usize]), 0)
            .map_err(|_| "offline-check: a write was not queued")?;
        ids.push(id);
    }
    // The ring's task puts two on the ring, and the "driver" holds them.
    let held: Vec<u64> = {
        let mut state = disk.state.lock();
        let mut held = Vec::new();
        while let Some(command) = state.queue.dispatch(ticks()) {
            held.push(command.token.raw());
        }
        held
    };
    if held.len() != DEPTH as usize {
        return Err("offline-check: the queue did not put the disk's depth on the ring");
    }
    disk.offline_if_silent(0);
    if !disk.state.lock().offline {
        return Err("offline-check: a disk silent past its patience was not taken offline");
    }
    if disk
        .submit(
            Request::write(0, 60, 1).sync(),
            Some(vec![0; BLOCK as usize]),
            0,
        )
        .is_ok()
    {
        return Err("offline-check: an offline disk took a new request");
    }
    {
        let mut state = disk.state.lock();
        // The driver completes the first command it held, successfully, in
        // the moment between the disk going offline and the task's drain:
        // the queue still knows it, and its caller was answered EIO already.
        let late = held.first().copied().ok_or("offline-check: nothing held")?;
        let _ = answer(&mut state, Token::from_raw(late), Ok(()), None);
        if !state.drain_offline(held.iter().copied().skip(1)) {
            return Err("offline-check: the drain did not see the disk offline");
        }
        if state.queue.outstanding() != 0 || state.queue.dispatch(ticks()).is_some() {
            return Err(
                "offline-check: an offline disk's queue kept a command that could be sent later",
            );
        }
        if state.dispatchable() {
            return Err("offline-check: an offline disk was dispatchable");
        }
        // And the rest of what it held, after the drain.
        for &token in held.iter().skip(1) {
            let _ = answer(&mut state, Token::from_raw(token), Ok(()), None);
        }
    }
    for &id in &ids {
        if disk.collect(id, &mut []) != Err(Errno::EIO) {
            return Err(
                "offline-check: a request answered EIO when the disk went offline was answered \
                 again",
            );
        }
    }
    // A new driver takes the disk up.
    disk.take_up(port);
    let mut state = disk.state.lock();
    if state.offline {
        return Err("offline-check: a disk taken up by a new driver stayed offline");
    }
    if state.queue.dispatch(ticks()).is_some() || state.queue.outstanding() != 0 {
        return Err("offline-check: the disk's successor found a command to send");
    }
    Ok(())
}
