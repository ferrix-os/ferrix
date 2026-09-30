//! Tests for the request queue: one rule at a time, then the model over
//! seeded random sequences.
//!
//! The device in most tests has a capacity of 1000 sectors, commands of up to
//! 64 sectors and 8 parts, and a depth of 4. Reads expire after 10 ticks and
//! asynchronous writes after 100, so a test chooses whether expiry is in play
//! by choosing `now`.

extern crate std;

use std::vec;
use std::vec::Vec;

use crate::model::{self, Stats};
use crate::{
    Config, Dispatch, Limits, LimitsError, Op, Queue, Request, SubmitError, Token, TokenError,
};

fn limits(max_sectors: u32, max_parts: u32, depth: u32) -> Limits {
    Limits::new(512, 1000, max_sectors, max_parts, depth).unwrap()
}

fn config() -> Config {
    Config {
        read_expiry: 10,
        write_expiry: 100,
        plug_threshold: 4,
        max_requests: 64,
    }
}

fn queue() -> Queue {
    Queue::new(limits(64, 8, 4), config())
}

/// A dispatch, copied out of the queue's borrow.
#[derive(Debug, PartialEq, Eq)]
struct Sent {
    token: Token,
    op: Op,
    sector: u64,
    count: u32,
    fua: bool,
    ids: Vec<u64>,
}

impl From<Dispatch<'_>> for Sent {
    fn from(d: Dispatch<'_>) -> Self {
        Sent {
            token: d.token,
            op: d.op,
            sector: d.sector,
            count: d.count,
            fua: d.fua,
            ids: d.parts.iter().map(|p| p.id.0).collect(),
        }
    }
}

fn next(q: &mut Queue, now: u64) -> Option<Sent> {
    q.dispatch(now).map(Sent::from)
}

fn submit(q: &mut Queue, now: u64, requests: &[Request]) {
    for r in requests {
        q.submit(now, *r).unwrap();
    }
}

fn done(q: &mut Queue, sent: &Sent) {
    let _ = q.complete(sent.token, ()).unwrap();
}

/// Dispatch everything eligible at `now` and return the ids of each command.
fn drain_ids(q: &mut Queue, now: u64) -> Vec<Vec<u64>> {
    let mut out = Vec::new();
    while let Some(sent) = next(q, now) {
        done(q, &sent);
        out.push(sent.ids);
    }
    out
}

// ---------------------------------------------------------------------------
// Limits and validation
// ---------------------------------------------------------------------------

#[test]
fn limits_refuse_what_no_device_reports() {
    assert_eq!(Limits::new(511, 8, 1, 1, 1), Err(LimitsError::BlockSize));
    assert_eq!(Limits::new(1000, 8, 1, 1, 1), Err(LimitsError::BlockSize));
    assert_eq!(
        Limits::new(1 << 17, 8, 1, 1, 1),
        Err(LimitsError::BlockSize)
    );
    assert_eq!(Limits::new(4096, 8, 0, 1, 1), Err(LimitsError::MaxSectors));
    assert_eq!(Limits::new(4096, 8, 1, 0, 1), Err(LimitsError::MaxParts));
    assert_eq!(Limits::new(4096, 8, 1, 1, 0), Err(LimitsError::QueueDepth));
    let l = Limits::new(4096, 0, 1, 1, 1).unwrap();
    assert_eq!(l.bytes(3), 12288, "three 4 KiB sectors");
    assert_eq!(l.bytes(u32::MAX), u64::from(u32::MAX) * 4096, "no overflow");
}

#[test]
fn submit_refuses_malformed_requests() {
    let mut q = queue();
    let cases = [
        (Request::read(1, 0, 1).fua(), SubmitError::FuaWithoutWrite),
        (
            Request::discard(1, 0, 1).fua(),
            SubmitError::FuaWithoutWrite,
        ),
        (Request::flush(1).fua(), SubmitError::FuaWithoutWrite),
        (
            Request {
                sector: 3,
                ..Request::flush(1)
            },
            SubmitError::FlushWithRange,
        ),
        (
            Request {
                count: 1,
                ..Request::flush(1)
            },
            SubmitError::FlushWithRange,
        ),
        (Request::read(1, 5, 0), SubmitError::Empty),
        (Request::write(1, 5, 0), SubmitError::Empty),
        (Request::discard(1, 5, 0), SubmitError::Empty),
        (Request::write(1, u64::MAX, 1), SubmitError::Overflow),
        (Request::write(1, 999, 2), SubmitError::PastCapacity),
        (Request::read(1, 0, 65), SubmitError::TooLarge),
    ];
    for (request, error) in cases {
        assert_eq!(q.submit(0, request), Err(error), "{request:?}");
    }
    assert!(q.is_empty(), "nothing refused was queued");
    assert_eq!(
        q.submit(0, Request::read(1, 999, 1)),
        Ok(()),
        "the last sector is fine"
    );
}

#[test]
fn ids_are_unique_while_outstanding_and_reusable_after() {
    let mut q = queue();
    submit(&mut q, 0, &[Request::read(7, 0, 1)]);
    assert_eq!(
        q.submit(0, Request::read(7, 50, 1)),
        Err(SubmitError::DuplicateId)
    );
    let sent = next(&mut q, 0).unwrap();
    assert_eq!(
        q.submit(0, Request::read(7, 50, 1)),
        Err(SubmitError::DuplicateId)
    );
    done(&mut q, &sent);
    assert_eq!(q.submit(0, Request::read(7, 50, 1)), Ok(()), "free again");
}

#[test]
fn a_full_queue_refuses() {
    let mut q = Queue::new(
        limits(64, 8, 4),
        Config {
            max_requests: 2,
            ..config()
        },
    );
    submit(
        &mut q,
        0,
        &[Request::read(1, 0, 1), Request::read(2, 10, 1)],
    );
    assert_eq!(q.submit(0, Request::read(3, 20, 1)), Err(SubmitError::Full));
    let sent = next(&mut q, 0).unwrap();
    assert_eq!(
        q.submit(0, Request::read(3, 20, 1)),
        Err(SubmitError::Full),
        "on the device still counts"
    );
    done(&mut q, &sent);
    assert_eq!(q.submit(0, Request::read(3, 20, 1)), Ok(()));
}

// ---------------------------------------------------------------------------
// Merging
// ---------------------------------------------------------------------------

#[test]
fn back_merge() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 2), Request::write(2, 12, 3)],
    );
    let sent = next(&mut q, 0).unwrap();
    assert_eq!((sent.sector, sent.count, sent.ids), (10, 5, vec![1, 2]));
    assert!(next(&mut q, 0).is_none(), "one command");
}

#[test]
fn front_merge_keeps_submission_order() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 12, 3), Request::write(2, 10, 2)],
    );
    let d = q.dispatch(0).unwrap();
    assert_eq!((d.sector, d.count), (10, 5));
    let parts: Vec<(u64, u64, u32)> = d
        .parts
        .iter()
        .map(|p| (p.id.0, p.sector, p.count))
        .collect();
    assert_eq!(
        parts,
        vec![(1, 12, 3), (2, 10, 2)],
        "parts by submission, not sector"
    );
}

#[test]
fn a_request_closing_a_gap_joins_both_neighbours() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::read(1, 10, 2),
            Request::read(2, 14, 2),
            Request::read(3, 12, 2),
        ],
    );
    let sent = next(&mut q, 0).unwrap();
    assert_eq!((sent.sector, sent.count, sent.ids), (10, 6, vec![1, 2, 3]));
}

#[test]
fn discards_merge() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::discard(1, 100, 8), Request::discard(2, 108, 8)],
    );
    let sent = next(&mut q, 0).unwrap();
    assert_eq!((sent.op, sent.sector, sent.count), (Op::Discard, 100, 16));
}

#[test]
fn different_ops_and_gaps_do_not_merge() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::read(1, 10, 2),
            Request::write(2, 12, 2),
            Request::read(3, 15, 2),
        ],
    );
    assert_eq!(drain_ids(&mut q, 0), vec![vec![1], vec![2], vec![3]]);
}

#[test]
fn overlapping_requests_do_not_merge() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 4), Request::write(2, 12, 4)],
    );
    assert_eq!(drain_ids(&mut q, 0), vec![vec![1], vec![2]]);
}

#[test]
fn merge_refused_by_max_sectors() {
    let mut q = Queue::new(limits(4, 8, 4), config());
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 3), Request::write(2, 13, 1)],
    );
    submit(&mut q, 0, &[Request::write(3, 14, 1)]);
    assert_eq!(
        drain_ids(&mut q, 0),
        vec![vec![1, 2], vec![3]],
        "4 sectors fit, 5 do not"
    );
}

#[test]
fn merge_refused_by_max_parts() {
    let mut q = Queue::new(limits(64, 2, 4), config());
    submit(
        &mut q,
        0,
        &[
            Request::read(1, 0, 1),
            Request::read(2, 1, 1),
            Request::read(3, 2, 1),
        ],
    );
    assert_eq!(drain_ids(&mut q, 0), vec![vec![1, 2], vec![3]]);
}

#[test]
fn fua_does_not_merge_with_non_fua() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 2), Request::write(2, 12, 2).fua()],
    );
    submit(&mut q, 0, &[Request::write(3, 14, 2)]);
    let sent: Vec<(Vec<u64>, bool)> = core::iter::from_fn(|| {
        let s = next(&mut q, 0)?;
        done(&mut q, &s);
        Some((s.ids, s.fua))
    })
    .collect();
    assert_eq!(
        sent,
        vec![(vec![1], false), (vec![2], true), (vec![3], false)]
    );
}

#[test]
fn back_to_back_fua_writes_merge() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 10, 2).fua(),
            Request::write(2, 12, 2).fua(),
        ],
    );
    let sent = next(&mut q, 0).unwrap();
    assert_eq!(
        (sent.sector, sent.count, sent.fua, sent.ids),
        (10, 4, true, vec![1, 2])
    );
}

#[test]
fn fua_writes_with_a_request_between_do_not_merge() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 10, 2).fua(),
            Request::read(2, 500, 1),
            Request::write(3, 12, 2).fua(),
        ],
    );
    assert_eq!(drain_ids(&mut q, 0), vec![vec![1], vec![2], vec![3]]);
}

#[test]
fn flush_never_merges() {
    let mut q = queue();
    submit(&mut q, 0, &[Request::flush(1), Request::flush(2)]);
    let first = next(&mut q, 0).unwrap();
    assert_eq!(
        (first.op, first.sector, first.count, &first.ids),
        (Op::Flush, 0, 0, &vec![1])
    );
    assert!(
        next(&mut q, 0).is_none(),
        "the second flush waits for the first"
    );
    done(&mut q, &first);
    assert_eq!(next(&mut q, 0).unwrap().ids, vec![2]);
}

// ---------------------------------------------------------------------------
// Barriers
// ---------------------------------------------------------------------------

#[test]
fn a_barrier_waits_for_everything_before_it_to_complete() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 10, 1),
            Request::write(2, 20, 1),
            Request::flush(3),
        ],
    );
    let a = next(&mut q, 0).unwrap();
    let b = next(&mut q, 0).unwrap();
    assert_eq!((a.ids.clone(), b.ids.clone()), (vec![1], vec![2]));
    assert!(
        next(&mut q, 0).is_none(),
        "both earlier writes are on the device"
    );
    done(&mut q, &a);
    assert!(
        next(&mut q, 0).is_none(),
        "dispatched is not enough; completed is"
    );
    done(&mut q, &b);
    assert_eq!(next(&mut q, 0).unwrap().op, Op::Flush);
}

#[test]
fn work_after_a_barrier_waits_for_it_to_complete() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 1).fua(), Request::read(2, 5, 1)],
    );
    let barrier = next(&mut q, 0).unwrap();
    assert_eq!(barrier.ids, vec![1]);
    assert!(
        next(&mut q, 0).is_none(),
        "the read must wait although depth allows it"
    );
    submit(&mut q, 0, &[Request::read(3, 6, 1)]);
    assert!(next(&mut q, 0).is_none(), "and so must anything newer");
    done(&mut q, &barrier);
    assert_eq!(
        next(&mut q, 0).unwrap().ids,
        vec![2, 3],
        "which may merge among itself"
    );
}

#[test]
fn work_submitted_after_a_barrier_never_overtakes_it_even_if_overdue() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 900, 1),
            Request::flush(2),
            Request::read(3, 0, 1),
        ],
    );
    assert_eq!(next(&mut q, 1000).unwrap().ids, vec![1]);
    assert!(
        next(&mut q, 1000).is_none(),
        "an overdue read does not jump the barrier"
    );
}

#[test]
fn no_merge_crosses_a_barrier() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 10, 2),
            Request::flush(2),
            Request::write(3, 12, 2),
        ],
    );
    // 1 and 3 abut across the flush and must stay apart; 3 and 4 abut on the
    // same side of it and join.
    submit(&mut q, 0, &[Request::write(4, 14, 2)]);
    assert_eq!(drain_ids(&mut q, 0), vec![vec![1], vec![2], vec![3, 4]]);
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

#[test]
fn the_elevator_climbs_then_wraps_once() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 500, 1),
            Request::write(2, 100, 1),
            Request::write(3, 300, 1),
        ],
    );
    let first = next(&mut q, 0).unwrap();
    assert_eq!(first.sector, 100, "lowest from sector zero");
    done(&mut q, &first);
    submit(
        &mut q,
        0,
        &[Request::write(4, 50, 1), Request::write(5, 400, 1)],
    );
    let order: Vec<u64> = core::iter::from_fn(|| {
        let s = next(&mut q, 0)?;
        done(&mut q, &s);
        Some(s.sector)
    })
    .collect();
    assert_eq!(
        order,
        vec![300, 400, 500, 50],
        "up from 101, then wrap to the bottom"
    );
}

/// The tick at which a read waiting behind the elevator is dispatched, when
/// a new read lands just ahead of the head every tick and one is dispatched
/// every tick: the depth-32 stall of a ring disk, in miniature.
fn starved_until(config: Config) -> u64 {
    let mut q = Queue::new(Limits::new(512, 1_000_000, 64, 8, 4).unwrap(), config);
    // Move the head past the victim's sector.
    submit(&mut q, 0, &[Request::read(1, 500, 1)]);
    let first = next(&mut q, 0).unwrap();
    done(&mut q, &first);
    submit(&mut q, 0, &[Request::read(2, 10, 1)]);
    for (tick, id) in (1..2_000).zip(3..) {
        let ahead = 1_000 + tick * 10;
        submit(&mut q, tick, &[Request::read(id, ahead, 1)]);
        let sent = next(&mut q, tick).unwrap();
        done(&mut q, &sent);
        if sent.ids.contains(&2) {
            return tick;
        }
    }
    u64::MAX
}

#[test]
fn a_fast_device_bounds_a_read_behind_the_elevator() {
    let bound = Config::fast_device().read_expiry;
    let waited = starved_until(Config::fast_device());
    assert!(
        waited <= bound + 1,
        "a read behind the elevator waited {waited} ticks on a fast device, past {bound}"
    );
    // The negative control: under the spinning-disk default the same read
    // starves until its half-second expiry, which is the stall a ring disk
    // showed at depth 32. Should the default ever change, this says so.
    let starved = starved_until(Config::default());
    assert!(
        starved >= Config::default().read_expiry,
        "under the default a read behind the elevator left after {starved} ticks"
    );
}

#[test]
fn an_overdue_unit_beats_the_elevator() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 1), Request::read(2, 900, 1)],
    );
    assert_eq!(
        next(&mut q, 20).unwrap().ids,
        vec![2],
        "the read expired at tick 10"
    );
    assert_eq!(next(&mut q, 20).unwrap().ids, vec![1]);
}

#[test]
fn before_expiry_the_elevator_decides() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 1), Request::read(2, 900, 1)],
    );
    assert_eq!(
        next(&mut q, 9).unwrap().ids,
        vec![1],
        "the read has a tick left"
    );
}

#[test]
fn the_most_overdue_leaves_first() {
    let mut q = queue();
    submit(&mut q, 0, &[Request::write(1, 500, 1)]); // due at 100
    submit(&mut q, 50, &[Request::read(2, 700, 1)]); // due at 60
    submit(&mut q, 55, &[Request::write(3, 100, 1).sync()]); // due at 65
    let order: Vec<u64> = core::iter::from_fn(|| next(&mut q, 200).map(|s| s.ids[0])).collect();
    assert_eq!(order, vec![2, 3, 1]);
}

#[test]
fn a_sync_write_merged_into_an_async_one_brings_its_deadline() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::write(1, 10, 1), Request::read(2, 900, 1)],
    );
    submit(&mut q, 5, &[Request::write(3, 11, 1).sync()]); // due at 15, read at 10
    assert_eq!(next(&mut q, 12).unwrap().ids, vec![2]);
    let parts = q.dispatch(12).unwrap();
    assert_eq!(parts.parts.iter().map(|p| p.deadline).min(), Some(15));
}

// ---------------------------------------------------------------------------
// Plugging and depth
// ---------------------------------------------------------------------------

#[test]
fn a_plug_holds_dispatch_until_the_threshold() {
    let mut q = queue();
    q.plug();
    for id in 0..3 {
        submit(&mut q, 0, &[Request::read(id, id * 10, 1)]);
        assert!(
            next(&mut q, 1000).is_none(),
            "{id}: below the threshold, even overdue"
        );
    }
    submit(&mut q, 0, &[Request::read(3, 30, 1)]);
    assert!(
        next(&mut q, 0).is_some(),
        "four queued requests release a plug"
    );
    assert!(next(&mut q, 0).is_none(), "and three queued again hold it");
    q.unplug();
    assert!(next(&mut q, 0).is_some());
}

#[test]
fn plugging_lets_a_burst_merge() {
    let mut q = queue();
    q.plug();
    submit(&mut q, 0, &[Request::write(1, 0, 1)]);
    assert!(next(&mut q, 0).is_none());
    submit(&mut q, 0, &[Request::write(2, 1, 1)]);
    q.unplug();
    assert_eq!(next(&mut q, 0).unwrap().ids, vec![1, 2]);
}

#[test]
fn queue_depth_bounds_commands_on_the_device() {
    let mut q = Queue::new(limits(64, 8, 2), config());
    submit(
        &mut q,
        0,
        &[
            Request::read(1, 0, 1),
            Request::read(2, 10, 1),
            Request::read(3, 20, 1),
        ],
    );
    let a = next(&mut q, 0).unwrap();
    let _b = next(&mut q, 0).unwrap();
    assert!(next(&mut q, 0).is_none(), "two on a device of depth two");
    assert_eq!(q.in_flight(), 2);
    done(&mut q, &a);
    assert_eq!(next(&mut q, 0).unwrap().ids, vec![3]);
}

// ---------------------------------------------------------------------------
// Completion and requeue
// ---------------------------------------------------------------------------

#[test]
fn completion_answers_every_request_with_the_result() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 0, 1),
            Request::write(2, 1, 1),
            Request::write(3, 2, 1),
        ],
    );
    let sent = next(&mut q, 0).unwrap();
    assert_eq!(q.parts(sent.token).map(<[_]>::len), Some(3));
    let c = q.complete(sent.token, Err::<(), i32>(-5)).unwrap();
    let answers: Vec<(u64, Result<(), i32>)> = c.requests().map(|(id, r)| (id.0, r)).collect();
    assert_eq!(answers, vec![(1, Err(-5)), (2, Err(-5)), (3, Err(-5))]);
    assert_eq!((c.op, c.sector, c.count), (Op::Write, 0, 3));
    assert!(q.is_empty());
    assert_eq!(q.parts(sent.token), None, "gone once completed");
}

#[test]
fn requeue_splits_a_command_into_its_requests_for_good() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 0, 1),
            Request::write(2, 1, 1),
            Request::write(3, 2, 1),
        ],
    );
    let sent = next(&mut q, 0).unwrap();
    assert_eq!(q.requeue(sent.token), Ok(3));
    assert_eq!((q.queued(), q.in_flight()), (3, 0));
    submit(&mut q, 0, &[Request::write(4, 3, 1)]);
    let mut order = drain_ids(&mut q, 0);
    order.sort();
    assert_eq!(
        order,
        vec![vec![1], vec![2], vec![3], vec![4]],
        "nothing merges with them"
    );
}

#[test]
fn a_requeued_part_fails_alone() {
    let mut q = queue();
    submit(&mut q, 0, &[Request::read(1, 0, 4), Request::read(2, 4, 4)]);
    let merged = next(&mut q, 0).unwrap();
    let _ = q.requeue(merged.token).unwrap();
    let first = next(&mut q, 0).unwrap();
    let second = next(&mut q, 0).unwrap();
    let bad = q.complete(second.token, Err::<(), u8>(1)).unwrap();
    assert_eq!(bad.parts().len(), 1);
    assert_eq!((bad.sector, bad.count), (4, 4), "the failing range, alone");
    done(&mut q, &first);
}

#[test]
fn a_requeued_barrier_keeps_its_place() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[
            Request::write(1, 10, 2).fua(),
            Request::write(2, 12, 2).fua(),
            Request::read(3, 0, 1),
        ],
    );
    let merged = next(&mut q, 0).unwrap();
    assert_eq!(merged.ids, vec![1, 2]);
    let _ = q.requeue(merged.token).unwrap();
    let first = next(&mut q, 0).unwrap();
    assert_eq!((first.ids.clone(), first.fua), (vec![1], true));
    assert!(
        next(&mut q, 0).is_none(),
        "the second part is still a barrier"
    );
    done(&mut q, &first);
    let second = next(&mut q, 0).unwrap();
    assert_eq!(second.ids, vec![2]);
    assert!(next(&mut q, 0).is_none(), "the read waits for both");
    done(&mut q, &second);
    assert_eq!(next(&mut q, 0).unwrap().ids, vec![3]);
}

#[test]
fn stale_and_invented_tokens_are_errors() {
    let mut q = queue();
    submit(
        &mut q,
        0,
        &[Request::read(1, 0, 1), Request::read(2, 10, 1)],
    );
    let sent = next(&mut q, 0).unwrap();
    done(&mut q, &sent);
    assert_eq!(q.complete(sent.token, ()), Err(TokenError::AlreadyFinished));
    assert_eq!(q.requeue(sent.token), Err(TokenError::AlreadyFinished));
    let invented = Token::from_raw(sent.token.raw() + 1);
    assert_eq!(q.complete(invented, ()), Err(TokenError::NeverIssued));
    assert_eq!(q.outstanding(), 1, "neither disturbed the queue");
    let other = next(&mut q, 0).unwrap();
    assert_eq!(
        other.token, invented,
        "the invented number is the next one issued"
    );
    let _ = q.requeue(other.token).unwrap();
    assert_eq!(
        q.complete(other.token, ()),
        Err(TokenError::AlreadyFinished)
    );
}

#[test]
fn an_empty_queue_dispatches_nothing() {
    let mut q = queue();
    assert!(next(&mut q, 0).is_none());
    submit(&mut q, 0, &[Request::flush(1)]);
    let f = next(&mut q, 0).unwrap();
    done(&mut q, &f);
    assert!(next(&mut q, u64::MAX).is_none());
    assert!(q.is_empty());
}

// ---------------------------------------------------------------------------
// The model, over seeded random sequences
// ---------------------------------------------------------------------------

/// A small deterministic generator: xorshift64*, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next() >> 32) as u8).collect()
    }
}

fn add(total: &mut Stats, one: Stats) {
    total.submitted += one.submitted;
    total.rejected += one.rejected;
    total.dispatched += one.dispatched;
    total.merged += one.merged;
    total.barriers += one.barriers;
    total.requeued += one.requeued;
    total.completed += one.completed;
    total.refused_tokens += one.refused_tokens;
    total.expired += one.expired;
}

#[test]
fn random_sequences_keep_every_rule() {
    let (seeds, len) = if cfg!(miri) { (12, 400) } else { (3000, 1200) };
    let mut total = Stats::default();
    for seed in 1..=seeds {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
        let data = rng.bytes(len);
        match model::run(&data) {
            Ok(stats) => add(&mut total, stats),
            Err(violation) => panic!("seed {seed}: {violation:?}"),
        }
    }
    assert_eq!(
        total.completed, total.submitted,
        "every request completed once"
    );
    for (name, count) in [
        ("rejected", total.rejected),
        ("merged", total.merged),
        ("barriers", total.barriers),
        ("requeued", total.requeued),
        ("refused tokens", total.refused_tokens),
        ("expired", total.expired),
    ] {
        assert!(count > 0, "the sequences never exercised {name}: {total:?}");
    }
}

#[test]
fn the_model_accepts_any_short_input() {
    for len in 0..16 {
        let data: Vec<u8> = (0..len).map(|i| (i * 37) as u8).collect();
        assert_eq!(model::run(&data).map(|_| ()), Ok(()), "len {len}");
    }
}

// -- Per-cgroup bandwidth ---------------------------------------------------------

mod throttling {
    use super::vec;
    use crate::{Limiter, Throttle};

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn an_unlimited_clock_never_waits() {
        let mut limiter = Limiter::UNLIMITED;
        for now in [0, 5, 1_000_000] {
            let start = limiter.earliest(now);
            assert_eq!(start, now);
            limiter.commit(start, u64::MAX);
        }
        assert_eq!(limiter.earliest(7), 7);
    }

    #[test]
    fn a_rate_spaces_out_what_it_admits() {
        let mut limiter = Limiter::UNLIMITED;
        limiter.set_rate(Some(1000));
        // A thousand bytes a second, in requests of a thousand: one a second.
        let mut starts = vec![];
        for _ in 0..4 {
            let start = limiter.earliest(0);
            limiter.commit(start, 1000);
            starts.push(start);
        }
        assert_eq!(starts, [0, SECOND, 2 * SECOND, 3 * SECOND]);
    }

    #[test]
    fn an_idle_clock_owes_nothing() {
        let mut limiter = Limiter::UNLIMITED;
        limiter.set_rate(Some(1000));
        limiter.commit(0, 1000);
        // A minute later the work admitted has long been paid for.
        let start = limiter.earliest(60 * SECOND);
        assert_eq!(start, 60 * SECOND);
    }

    #[test]
    fn a_rate_holds_over_any_stretch() {
        let mut limiter = Limiter::UNLIMITED;
        limiter.set_rate(Some(4096));
        let mut now = 0;
        let mut admitted = 0_u64;
        // Requests of odd sizes, each asked for at the instant the last
        // started: what was started by `now` never exceeds the rate times
        // `now`, plus the request that began it.
        for size in (1..200).map(|n| n * 97 % 5000 + 1) {
            let start = limiter.earliest(now);
            limiter.commit(start, size);
            now = start;
            admitted += size;
            let allowed = u128::from(now) * 4096 / u128::from(SECOND) + 5000;
            assert!(u128::from(admitted) <= allowed, "{admitted} by {now}");
        }
    }

    #[test]
    fn setting_a_rate_forgives_what_came_before() {
        let mut limiter = Limiter::UNLIMITED;
        limiter.set_rate(Some(1));
        limiter.commit(0, 1_000_000);
        limiter.set_rate(Some(1000));
        assert_eq!(limiter.earliest(3), 3);
        limiter.set_rate(None);
        assert_eq!(limiter.rate(), None);
    }

    #[test]
    fn a_request_waits_for_the_tighter_of_bytes_and_operations() {
        let mut throttle = Throttle::UNLIMITED;
        assert!(!throttle.is_limited());
        throttle.bytes.set_rate(Some(1_000_000));
        throttle.ios.set_rate(Some(2));
        assert!(throttle.is_limited());
        // Small requests: two operations a second is the limit.
        let mut starts = vec![];
        for _ in 0..4 {
            let start = throttle.earliest(0);
            throttle.commit(start, 512);
            starts.push(start);
        }
        assert_eq!(starts, [0, SECOND / 2, SECOND, 3 * SECOND / 2]);
        // Large ones: a megabyte a second is.
        let mut throttle = Throttle::UNLIMITED;
        throttle.bytes.set_rate(Some(1_000_000));
        throttle.ios.set_rate(Some(1000));
        let first = throttle.earliest(0);
        throttle.commit(first, 1_000_000);
        assert_eq!(throttle.earliest(0), SECOND);
    }

    #[test]
    fn a_parent_holds_however_its_children_share_it() {
        // Two children under a parent limited to two operations a second: each
        // asks for a start, the latest of the two levels, and pays both.
        let mut parent = Throttle::UNLIMITED;
        parent.ios.set_rate(Some(2));
        let mut children = [Throttle::UNLIMITED; 2];
        let mut starts = vec![];
        for turn in 0..4 {
            let child = &mut children[turn % 2];
            let start = child.earliest(0).max(parent.earliest(0));
            child.commit(start, 512);
            parent.commit(start, 512);
            starts.push(start);
        }
        assert_eq!(starts, [0, SECOND / 2, SECOND, 3 * SECOND / 2]);
    }

    #[test]
    fn the_clock_saturates() {
        let mut limiter = Limiter::UNLIMITED;
        limiter.set_rate(Some(1));
        limiter.commit(u64::MAX - 1, u64::MAX);
        assert_eq!(limiter.earliest(0), u64::MAX);
    }
}
