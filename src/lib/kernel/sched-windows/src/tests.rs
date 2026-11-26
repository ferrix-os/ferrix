//! Host tests: the order, the frame, the pick, and G.1's generic set over all three classes.

extern crate std;

use alloc::vec::Vec;

use ferrix_sched::{Config, EntityState, Ordered, RunQueue, Slot};
use ferrix_sched_edf::OverFair;

use super::frame::{
    FLOOR_NS, FRAME_CPUS, PARTITIONS, Part, Refusal, Row, WINDOWS_MOST, Window, seal, set_window,
};
use super::{
    Fixed, LEVELS, Now, PROGRAM_LEVELS, Placed, QUANTUM_NS, SERVER_LEVELS, Windows, key_of,
    level_of, level_of_key,
};

/// A task: its id, level and partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Task(u64, u8, u8);

impl Placed for Task {
    fn level(&self) -> u8 {
        self.1
    }
    fn partition(&self) -> u8 {
        self.2
    }
}

const CONFIG: Config = Config {
    slice_ns: 3_000_000,
};

fn slot() -> Slot<Task> {
    Slot::new().expect("memory")
}

fn add(class: &mut Windows<Task>, task: Task) {
    class
        .enqueue(task.0, task, EntityState::new(1024), slot())
        .map_err(|_| ())
        .expect("queued");
}

fn ms(n: u64) -> u64 {
    n * 1_000_000
}

fn window(partition: u8, offset: u64, length: u64) -> Window {
    Window {
        partition,
        offset_ns: offset,
        length_ns: length,
    }
}

#[test]
fn keys_order_plainly_at_the_edges() {
    // WN.5: level 0 before level 39 whatever the sequences. key_of's words all lie below 2^62,
    // where a wrapping compare happens to agree; `Ordered` takes any word, so the set below
    // also holds two more than half the range apart, which only a plain compare orders.
    let top = (1_u64 << 56) - 1;
    assert!(key_of(0, top) < key_of(39, 0));
    assert!(key_of(0, 0) < key_of(1, 0));
    assert!(key_of(38, top) < key_of(39, 0));
    assert_eq!(level_of_key(key_of(39, top)), 39);
    assert_eq!(level_of_key(key_of(200, 5)), LEVELS - 1);
    let mut set = Ordered::new();
    for (id, key) in [(1, key_of(39, 0)), (2, key_of(0, top)), (3, key_of(0, 1))] {
        set.insert(id, key, Task(id, 0, 0), EntityState::new(1), slot());
    }
    assert_eq!(set.check(), Ok(3));
    let order: Vec<u64> = core::iter::from_fn(|| set.pop_first().map(|left| left.0)).collect();
    assert_eq!(order, [3, 2, 1]);
    let mut wide = Ordered::new();
    for (id, key) in [(1, (1_u64 << 63) + 5), (2, 1)] {
        wide.insert(id, key, Task(id, 0, 0), EntityState::new(1), slot());
    }
    assert_eq!(wide.check(), Ok(2));
    let order: Vec<u64> = core::iter::from_fn(|| wide.pop_first().map(|left| left.0)).collect();
    assert_eq!(order, [2, 1]);
}

#[test]
fn ordered_keeps_state_and_finds_by_id() {
    let mut set = Ordered::new();
    let state = EntityState {
        weight: 77,
        vlag: -5,
        sum_exec: 9,
        request_ns: 11,
    };
    set.insert(4, 10, Task(4, 0, 0), state, slot());
    set.insert(5, 5, Task(5, 0, 0), EntityState::new(1), slot());
    assert_eq!(set.key_of(4), Some(10));
    let (id, payload, back, _) = set.remove(4).expect("there");
    assert_eq!((id, payload, back), (4, Task(4, 0, 0), state));
    assert_eq!(set.len(), 1);
    assert_eq!(set.check(), Ok(1));
    assert!(set.remove(4).is_none());
}

#[test]
fn ordered_stays_balanced_over_many() {
    let mut set = Ordered::new();
    for id in 0..500_u64 {
        set.insert(
            id,
            id * 7 % 101,
            Task(id, 0, 0),
            EntityState::new(1),
            slot(),
        );
    }
    assert_eq!(set.check(), Ok(500));
    for id in (0..500_u64).step_by(3) {
        assert!(set.remove(id).is_some());
    }
    assert!(set.check().is_ok());
    let mut last = (0, 0);
    while let Some((id, _, _, _)) = set.pop_first() {
        let _ = id;
        let _ = &mut last;
    }
    assert!(set.is_empty());
}

#[test]
fn fixed_equals_in_arrival_and_head_first() {
    let mut fixed = Fixed::new();
    for id in 1..=3 {
        fixed.push(id, 5, Task(id, 5, 0), EntityState::new(1), slot(), false);
    }
    fixed.push(9, 5, Task(9, 5, 0), EntityState::new(1), slot(), true);
    fixed.push(8, 4, Task(8, 4, 0), EntityState::new(1), slot(), false);
    let order: Vec<u64> = core::iter::from_fn(|| fixed.pop().map(|left| left.0)).collect();
    assert_eq!(order, [8, 9, 1, 2, 3]);
}

fn rows() -> [Row; FRAME_CPUS] {
    [Row::empty(); FRAME_CPUS]
}

#[test]
fn the_seal_refuses_each_by_name() {
    let mut plan = rows();
    set_window(&mut plan, 0, window(0, 0, ms(1))).expect("first");
    assert_eq!(
        set_window(&mut plan, 0, window(1, ms(1) / 2, ms(1))),
        Err(Refusal::Overlap)
    );
    assert_eq!(
        set_window(&mut plan, 0, window(8, ms(2), ms(1))),
        Err(Refusal::Partition)
    );
    assert_eq!(
        set_window(&mut plan, 0, window(1, ms(2), 0)),
        Err(Refusal::Empty)
    );
    assert_eq!(
        set_window(&mut plan, FRAME_CPUS, window(1, 0, ms(1))),
        Err(Refusal::OutOfService)
    );
    set_window(&mut plan, 0, window(1, ms(1), ms(9))).expect("second");
    assert_eq!(seal(&plan, ms(9), 1), Err(Refusal::PastMajor));
    assert_eq!(seal(&plan, 0, 1), Err(Refusal::Empty));
    // A window on a processor not in service.
    let mut other = plan;
    set_window(&mut other, 1, window(0, 0, ms(1))).expect("cpu 1");
    assert_eq!(seal(&other, ms(10), 1), Err(Refusal::OutOfService));
    // A processor in service with no partition-0 window of the floor.
    assert_eq!(seal(&plan, ms(10), 0b11), Err(Refusal::NoPartitionZero));
    let mut short = rows();
    set_window(&mut short, 0, window(0, 0, FLOOR_NS - 1)).expect("short");
    assert_eq!(seal(&short, ms(10), 1), Err(Refusal::NoPartitionZero));
    assert_eq!(seal(&plan, ms(10), 1), Ok(()));
    assert!(Part::new(plan[1], ms(10), 0).is_none());
    let mut full = rows();
    for at in 0..WINDOWS_MOST as u64 {
        set_window(&mut full, 0, window(0, at * ms(1), ms(1))).expect("room");
    }
    assert_eq!(
        set_window(&mut full, 0, window(0, ms(20), ms(1))),
        Err(Refusal::Full)
    );
}

fn two_window_class() -> Windows<Task> {
    // Partition 0 from 0 to 1 ms, 1 from 1 to 4, a gap to 6, 2 from 6 to 10.
    let mut plan = rows();
    set_window(&mut plan, 0, window(2, ms(6), ms(4))).expect("2");
    set_window(&mut plan, 0, window(0, 0, ms(1))).expect("0");
    set_window(&mut plan, 0, window(1, ms(1), ms(3))).expect("1");
    seal(&plan, ms(10), 1).expect("sealed");
    let mut class = Windows::new(CONFIG).expect("class");
    assert!(class.install(Part::new(plan[0], ms(10), 0).expect("part")));
    class
}

/// L.sched.170's refused half, through the body the kernel's `may_return` is: a sealed frame
/// with no row for processor 1 installs nothing there, and processor 1 may not come back;
/// processor 0, its row installed, may; unsealed, either may. The accepted half on the
/// kernel's own sealed frame is `windows_check::after_seal`'s.
///
/// Verifies: L.sched.170
#[test]
fn a_processor_with_no_sealed_row_may_not_return() {
    let mut plan = rows();
    set_window(&mut plan, 0, window(0, 0, ms(1))).expect("0");
    set_window(&mut plan, 0, window(1, ms(1), ms(9))).expect("1");
    seal(&plan, ms(10), 1).expect("sealed");
    let mut with_row = Windows::<Task>::new(CONFIG).expect("class");
    assert!(with_row.install(Part::new(plan[0], ms(10), 0).expect("part")));
    let without_row = Windows::<Task>::new(CONFIG).expect("class");
    assert!(Part::new(plan[1], ms(10), 0).is_none());
    assert!(with_row.may_return(true));
    assert!(!without_row.may_return(true));
    assert!(without_row.may_return(false));
    assert!(with_row.may_return(false));
}

#[test]
fn a_window_runs_only_its_partition_and_idles_when_empty() {
    let mut class = two_window_class();
    add(&mut class, Task(10, 5, 1));
    add(&mut class, Task(20, 5, 2));
    class.note_time(ms(0));
    assert_eq!(class.window(), Now::In(0));
    assert!(!class.runnable_now());
    assert!(!class.is_empty());
    assert!(class.pick_next().is_none(), "partition 0's window idles");
    assert_eq!(
        class.decision_in_ns(),
        Some(ms(1)),
        "an idle processor wakes at the window's end"
    );
    class.note_time(ms(2));
    assert_eq!(class.pick_next().map(|task| task.0), Some(10));
    class.note_time(ms(5));
    assert_eq!(class.window(), Now::Gap);
    assert!(class.should_preempt(), "its window is over");
    assert!(class.pick_next().is_none(), "the gap is nobody's");
    class.note_time(ms(7));
    assert_eq!(class.pick_next().map(|task| task.0), Some(20));
    // The next frame: partition 1 again from 11 ms.
    class.note_time(ms(11) + 10);
    assert_eq!(class.pick_next().map(|task| task.0), Some(10));
    assert!(class.check_invariants().is_ok());
}

#[test]
fn the_overrun_is_measured() {
    let mut class = two_window_class();
    add(&mut class, Task(10, 5, 1));
    class.note_time(ms(2));
    assert!(class.pick_next().is_some());
    class.note_time(ms(4) + 300);
    assert!(class.update_curr(10));
    let _ = class.pick_next();
    assert_eq!(class.worst_overrun_ns(), 300);
    assert_eq!(class.windows_ended(), 1);
    // The window's end and the pick that ended it, for the boot check's judge (WX.3).
    assert_eq!(class.last_overrun(), Some((ms(4), ms(4) + 300)));
    assert_eq!(class.now(), ms(4) + 300);
}

#[test]
fn a_higher_level_preempts_and_equals_take_turns() {
    let mut class = Windows::new(CONFIG).expect("class");
    add(&mut class, Task(1, 10, 1));
    add(&mut class, Task(2, 10, 1));
    assert_eq!(class.pick_next().map(|task| task.0), Some(1));
    assert!(!class.should_preempt());
    assert!(!class.update_curr(QUANTUM_NS - 1));
    assert!(class.update_curr(1), "the quantum is spent");
    assert!(class.should_preempt());
    assert_eq!(
        class.pick_next().map(|task| task.0),
        Some(2),
        "behind its equal"
    );
    add(&mut class, Task(3, 4, 1));
    assert!(class.should_preempt(), "a higher level preempts at once");
    assert_eq!(class.pick_next().map(|task| task.0), Some(3));
    // Task 2 was put back with its quantum left, before task 1.
    let _ = class.remove_curr();
    assert_eq!(class.pick_next().map(|task| task.0), Some(2));
    add(&mut class, Task(4, 30, 1));
    assert!(!class.should_preempt(), "a lower level waits");
}

#[test]
fn a_quantum_remainder_carries_across_a_window_and_a_sleep() {
    let mut class = two_window_class();
    add(&mut class, Task(10, 5, 1));
    class.note_time(ms(2));
    assert!(class.pick_next().is_some());
    let _ = class.update_curr(ms(1) / 2);
    class.note_time(ms(4));
    let _ = class.pick_next();
    class.note_time(ms(11));
    assert!(class.pick_next().is_some());
    assert_eq!(class.remaining_ns(), Some(QUANTUM_NS - ms(1) / 2));
    let (_, task, state, slot) = class.remove_curr().expect("running");
    assert_eq!(
        state.vlag,
        i64::try_from(QUANTUM_NS - ms(1) / 2).expect("fits")
    );
    class
        .enqueue(task.0, task, state, slot)
        .map_err(|_| ())
        .expect("again");
    assert!(class.pick_next().is_some());
    assert_eq!(class.remaining_ns(), Some(QUANTUM_NS - ms(1) / 2));
}

#[test]
fn a_wake_across_partitions_is_not_admitted_now() {
    let mut class = two_window_class();
    class.note_time(ms(2));
    assert!(class.admits_at(1, ms(2)));
    assert!(!class.admits_at(2, ms(2)));
    assert!(!class.admits_at(0, ms(2)));
}

/// WT.3 (ledger 7639), boot 5's breach on a690af537: a class last told the time inside
/// partition 2's window, a halted processor's, asked at an instant in partition 0's window of a
/// later frame, does not admit partition 2, does not read partition 2's waiting task as work,
/// and offers it to no steal; at the class's own stale time it still would, which is what the
/// wake read before. An earlier instant than the class's never moves the answer back.
///
/// Verifies: L.sched.164
#[test]
fn a_wake_reads_the_window_at_its_own_instant() {
    let mut class = two_window_class();
    add(&mut class, Task(20, 5, 2));
    class.note_time(ms(7));
    let later = ms(10) + ms(1) / 2;
    assert!(class.admits_at(2, ms(7)), "the stale time is partition 2's");
    assert!(
        !class.admits_at(2, later),
        "partition 0's window, a frame later"
    );
    assert!(class.admits_at(0, later));
    assert!(!class.runnable_at(later));
    assert!(!class.should_preempt_at(later));
    assert_eq!(class.latest_where_at(later, |_| true), None);
    assert!(
        class.admits_at(2, ms(3)),
        "never back past the class's time"
    );
}

#[test]
fn steal_sees_the_window_alone() {
    let mut class = two_window_class();
    add(&mut class, Task(10, 5, 1));
    add(&mut class, Task(20, 5, 2));
    class.note_time(ms(2));
    assert_eq!(class.latest_where(|_| true), Some(10));
    class.note_time(ms(7));
    assert_eq!(class.latest_where(|_| true), Some(20));
    class.note_time(ms(5) + 10 * ms(10));
    assert_eq!(class.latest_where(|_| true), None);
}

#[test]
fn before_the_seal_every_set_is_one_by_level() {
    let mut class = Windows::new(CONFIG).expect("class");
    add(&mut class, Task(1, 20, 1));
    add(&mut class, Task(2, 3, 2));
    add(&mut class, Task(3, 10, 0));
    let order: Vec<u64> = core::iter::from_fn(|| {
        let id = class.pick_next().map(|task| task.0);
        let _ = class.remove_curr();
        id
    })
    .collect();
    assert_eq!(order, [2, 3, 1]);
}

#[test]
fn a_second_part_is_refused_and_a_partition_past_the_last_too() {
    let mut class = two_window_class();
    let part = class.part().expect("part");
    assert!(!class.install(part));
    assert!(class.uninstall());
    assert!(class.install(part));
    let refused = class.enqueue(9, Task(9, 0, PARTITIONS as u8), EntityState::new(1), slot());
    assert!(refused.is_err());
}

#[test]
fn set_weight_takes_the_level_not_the_weight() {
    let mut class = Windows::new(CONFIG).expect("class");
    add(&mut class, Task(1, 20, 1));
    add(&mut class, Task(2, 21, 1));
    // A huge weight changes nothing of the order.
    assert_eq!(class.set_weight(2, 88_761), Ok(true));
    assert_eq!(class.pick_next().map(|task| task.0), Some(1));
}

/// G.1's generic set: what every class answers the same, over `fair`, `edf` and `windows`.
macro_rules! generic {
    ($name:ident, $make:expr) => {
        #[test]
        fn $name() {
            let mut class = $make;
            assert!(class.is_empty());
            assert!(class.pick_next().is_none());
            for id in 1..=4_u64 {
                class
                    .enqueue(id, Task(id, 10, 1), EntityState::new(1024), slot())
                    .map_err(|_| ())
                    .expect("queued");
            }
            assert_eq!(class.len(), 4);
            assert_eq!(class.queued(), 4);
            let first = class.pick_next().map(|task| task.0).expect("one");
            assert_eq!(class.current_id(), Some(first));
            assert_eq!(class.len(), 4);
            assert_eq!(class.queued(), 3);
            assert!(class.remaining_ns().is_some());
            let mut seen = 0;
            class.for_each(|_| seen += 1);
            assert_eq!(seen, 4);
            assert!(class.get(first).is_some_and(|view| view.running));
            let (id, task, _, _) = class.remove_curr().expect("running");
            assert_eq!((id, task.0), (first, first));
            let other = (1..=4).find(|&id| id != first).expect("another");
            assert!(class.remove(other).is_some());
            assert!(class.remove(other).is_none());
            assert_eq!(class.len(), 2);
            assert!(class.latest_where(|_| true).is_some());
            assert!(class.check_invariants().is_ok());
            while class.pick_next().is_some() {
                let _ = class.remove_curr();
            }
            assert!(class.is_empty());
        }
    };
}

generic!(generic_fair, RunQueue::<Task>::new(CONFIG).expect("fair"));
generic!(generic_edf, OverFair::<Task>::new(CONFIG).expect("edf"));
generic!(
    generic_windows,
    Windows::<Task>::new(CONFIG).expect("windows")
);

/// L.sched.169's level half, read directly (ledger 7688 WO.3): for every nice from -20 to 19, a
/// program's level is beneath (after) the level of every server, of every nice, and each sits in
/// its own range; the threads test's sched-policy step shows the same order live.
///
/// Verifies: L.sched.169
#[test]
fn every_programs_level_is_beneath_every_servers() {
    let weight = |nice: i32| ferrix_sched::weight_of_nice(nice).expect("a nice in the table");
    let servers: Vec<u8> = (-20..=19)
        .map(|nice| level_of(weight(nice), false))
        .collect();
    let highest_server = servers.iter().copied().max().expect("forty servers");
    for nice in -20..=19 {
        let program = level_of(weight(nice), true);
        assert!(
            program > highest_server && PROGRAM_LEVELS.contains(&program) && program < LEVELS,
            "a program's level is not beneath every server's: nice {nice} took {program}, \
             a server {highest_server}"
        );
        let server = level_of(weight(nice), false);
        assert!(
            SERVER_LEVELS.contains(&server),
            "a server's level is outside its range: nice {nice} took {server}"
        );
    }
    // The level is the nice's: a lower nice never takes a later level.
    for nice in -20..19 {
        for program in [false, true] {
            assert!(
                level_of(weight(nice), program) <= level_of(weight(nice + 1), program),
                "a level is not its nice's: nice {nice} came after nice {}",
                nice + 1
            );
        }
    }
}
