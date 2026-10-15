//! The reassembly list's bounds.

use ferrix_kmem::testing::Job;

use crate::ring::{MAX_RUNS, RUN_COST, Reassembly};
use crate::seq::SeqNumber;

/// A peer sending one-byte segments with a gap after each makes one run per
/// segment. However large the receive buffer, the list stops at
/// [`MAX_RUNS`], drops the rest as it drops a run past its capacity, and is
/// charged for exactly what it holds; dropped, it gives the charge back.
#[test]
fn gapped_one_byte_segments_stop_at_the_run_cap() {
    let job = Job::enter(u64::MAX / 2);
    let mut list = Reassembly::with_capacity(1024 * 1024);
    list.charge_to(job.id()).unwrap();
    let base = SeqNumber(7_000);
    for run in 0..(MAX_RUNS as u32 + 500) {
        list.insert(base.advance(2 * run), &[1]);
    }
    assert_eq!(list.runs(), MAX_RUNS, "the runs stop at the cap");
    assert_eq!(list.held(), MAX_RUNS, "one byte held for each run kept");
    assert_eq!(
        job.used(),
        (MAX_RUNS * (RUN_COST + 1)) as u64,
        "the charge settles at what the runs kept cost"
    );
    drop(list);
    assert_eq!(job.used(), 0, "and goes back when the list does");
}
