//! The front without devices: the lane's bookkeeping and the loop's side.

use std::os::unix::net::UnixStream;

use compositor_socket::Connection;
use compositor_wire::ObjectId;

use super::{Front, Lane, age_line};
use crate::devices::Devices;

fn lane(serial: u64, pointers: &[u32]) -> (Lane, Connection) {
    let (ours, theirs) = UnixStream::pair().expect("a socket pair");
    let connection = Connection::new(ours).expect("a connection");
    let far = Connection::new(theirs).expect("a connection");
    (
        Lane {
            serial,
            sender: connection.sender(),
            pointers: pointers.iter().copied().map(ObjectId).collect(),
        },
        far,
    )
}

fn pointers(front: &Front) -> Option<Vec<u32>> {
    super::lock(&front.shared.lane)
        .as_ref()
        .map(|lane| lane.pointers.iter().map(|id| id.0).collect())
}

/// A relative pointer the client destroyed leaves the lane before the
/// client's messages are sent; the last one leaving ends the lane; another
/// connection's flush changes nothing.
#[test]
fn a_destroyed_pointer_leaves_the_lane_before_the_flush() {
    let front = Front::with(Devices::default(), false, false);
    let (held, _far) = lane(7, &[20, 21]);
    front.set_lane(Some(held));
    front.sync_lane(8, &[]);
    assert_eq!(pointers(&front), Some(vec![20, 21]));
    front.sync_lane(7, &[ObjectId(21)]);
    assert_eq!(pointers(&front), Some(vec![21]));
    front.sync_lane(7, &[]);
    assert_eq!(pointers(&front), None);
}

/// Without the thread the loop reads the devices itself and waits on them,
/// and a front with none has nothing to give.
#[test]
fn without_the_thread_the_loop_reads_the_devices() {
    let front = Front::with(Devices::default(), false, false);
    assert!(!front.threaded());
    assert!(front.raw_fds().is_empty());
    assert!(front.take(None).is_empty());
}

/// With the thread the loop waits on the pipe alone, and a front dropped
/// stops its thread.
#[test]
fn with_the_thread_the_loop_waits_on_its_pipe() {
    let front = Front::with(Devices::default(), true, true);
    assert!(front.threaded());
    assert_eq!(front.raw_fds().len(), 1);
    assert!(front.take(None).is_empty());
    drop(front);
}

#[test]
fn an_age_is_now_less_the_stamp() {
    assert_eq!(
        age_line(1_000, 1_250, "lane"),
        "hyprix: input-age stamp 1000 age 250 lane"
    );
    assert_eq!(
        age_line(1_000, 900, "loop"),
        "hyprix: input-age stamp 1000 age 0 loop"
    );
}
