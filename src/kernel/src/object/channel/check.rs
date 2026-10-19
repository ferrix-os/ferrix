//! A channel end's state word equals its inbox (`docs/OPAQUE-KERNEL.md` §9.8,
//! 2e): every operation that changes whether an inbox is empty, in a long
//! pseudo-random order on one pair, with the word compared to the inbox
//! under its lock after each, and the close's mark compared to the peer's
//! `closed` at the end.
//!
//! A boot check rather than a host test, because `Inbox` and `Half` are the
//! kernel's own and only the kernel builds them.

use alloc::vec::Vec;

use super::{Endpoint, NONEMPTY, PEER_CLOSED, SMALL_BYTES, Sender};

/// How many operations the run makes.
const STEPS: u32 = 4096;

/// Whether `end`'s word says what its inbox and its peer say, read under
/// its inbox lock.
fn agrees(end: &Endpoint) -> bool {
    let own = end.own();
    let inbox = own.inbox.lock();
    let word = own.word();
    (word & NONEMPTY != 0) == !inbox.is_empty() && (word & PEER_CLOSED != 0) == end.peer_closed()
}

/// Drive both ends of a pair through writes, small writes, reads, small
/// reads and put-backs -- each across the empty boundary once, then in a
/// long pseudo-random order -- and then a close, comparing each end's word after
/// every step. Answers the steps made.
///
/// # Errors
///
/// A word that disagrees with its inbox or its peer, naming the operation
/// after which it did, or a pair that could not be made.
/// Verifies: `L.object.140`
pub(crate) fn run() -> Result<u32, &'static str> {
    let (first, second) = Endpoint::pair().map_err(|_| "no memory for the word check's pair")?;
    // Every operation across the empty-to-not boundary once, in order,
    // first: a long random run rarely empties a queue it keeps writing to.
    let step = |what: &'static str| -> Result<(), &'static str> {
        if agrees(&first) && agrees(&second) {
            Ok(())
        } else {
            crate::console::println!("  chword   disagreed after {what}");
            Err("a channel end's state word disagreed with its inbox")
        }
    };
    let _ = second.write_small(Sender::KERNEL, &[1_u8; SMALL_BYTES]);
    step("a small write into an empty inbox")?;
    let message = first
        .read(SMALL_BYTES, 0, true)
        .map_err(|_| "the word check's first read found nothing")?;
    step("a read that emptied the inbox")?;
    first
        .unread(message)
        .map_err(|_| "the word check's put-back was refused")?;
    step("a put-back into an empty inbox")?;
    let _ = first.read_small();
    step("a small read that emptied the inbox")?;
    let _ = second.write(Sender::KERNEL, alloc::vec![2_u8; 3], 0, || Ok::<_, ()>(Vec::new()));
    step("a write into an empty inbox")?;
    let _ = first.read_small();
    step("a small read of a queued message")?;

    let mut seed: u32 = 0x2e2e_0001;
    // Messages read and held to put back, with the end each came from.
    let mut held: Vec<(bool, super::ChannelMessage)> =
        crate::fallible::try_with_capacity(4).map_err(|_| "no memory for the word check")?;
    for _ in 0..STEPS {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let on_first = seed & 0x100 == 0;
        let (end, other) = if on_first {
            (&first, &second)
        } else {
            (&second, &first)
        };
        let what = match (seed >> 16) % 5 {
            0 => {
                let _ = other.write(Sender::KERNEL, alloc::vec![7_u8; 3], 0, || Ok::<_, ()>(Vec::new()));
                "a write"
            }
            1 => {
                let _ = other.write_small(Sender::KERNEL, &[5_u8; SMALL_BYTES]);
                "a small write"
            }
            2 => {
                if held.len() < held.capacity()
                    && let Ok(message) = end.read(SMALL_BYTES * 4, 0, true)
                {
                    // NOALLOC: within the capacity reserved before the run.
                    held.push((on_first, message));
                }
                "a read"
            }
            3 => {
                let _ = end.read_small();
                "a small read"
            }
            _ => {
                if let Some((from_first, message)) = held.pop() {
                    let _ = if from_first { &first } else { &second }.unread(message);
                }
                "a put-back"
            }
        };
        step(what)?;
    }
    for (from_first, message) in held.drain(..) {
        let _ = if from_first { &first } else { &second }.unread(message);
    }
    drop(second);
    if !agrees(&first) || first.own().word() & PEER_CLOSED == 0 {
        return Err("a closed peer was not marked in the survivor's state word");
    }
    Ok(STEPS)
}
