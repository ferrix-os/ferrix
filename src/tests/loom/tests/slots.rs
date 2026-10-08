//! A task's slot cell (`sched::task::SlotCell`, `L.sched.63`; po9-cert's
//! S4, ledger line 415): one atomic pointer, null while a queue, a sleeper
//! set or the reaper's list holds the slot.
//!
//! Kernel sites: `SlotCell::take` (a swap with null), `SlotCell::put` (a
//! compare-exchange from null, FX-0534 when it fails), `SlotCell::holds`
//! (a load); their callers `Task::take_run_slot`, `return_run_slot`,
//! `take_sleep_slot`, `return_sleep_slot`, `holds_slots` and
//! `holds_sleep_slot`; and the direct switch's `SlotCell::take_held` and
//! `put_held` (`L.sched.69`), through `Task::take_run_slot_at_home` and
//! `return_run_slot_at_home`, a load and a store each under the home
//! queue's lock (`held_under_the_home_lock`).
//!
//! A node is a number here, never zero. The models assert the cell's two
//! promises over every interleaving:
//! - *One holder*: a node is never held by two takers at once.
//! - *Nothing lost*: once every holder has given its node back, the cell
//!   holds it.
//!
//! The give-back is a compare-exchange, not the load and store first
//! proposed: `loom` 0.7.2 lost a node given back by a plain store against a
//! concurrent swap (a store, a swap and then a load after both joins read
//! null), so the model could not be held to the cheaper form; the
//! compare-exchange also makes FX-0534's assertion exact, as po9-cert's S2
//! allowed.
//!
//! Each has a control, which `loom` must find failing: a take made as a load
//! and then a store (two takers both get the node), and a give-back without
//! the empty-cell assertion made twice (the second overwrites a node the
//! first gave back, so one is lost).

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;

/// The preemption bound every model runs under, as the other models'.
const PREEMPTIONS: usize = 3;

fn model(body: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(PREEMPTIONS);
    builder.check(body);
}

/// The cell, with the node as a number: zero is null.
struct Cell {
    node: AtomicUsize,
    /// The control: take by a load and a store, not a swap.
    split_take: bool,
    /// The control: give back without requiring the cell empty.
    unchecked_put: bool,
}

impl Cell {
    fn new(node: usize, split_take: bool, unchecked_put: bool) -> Cell {
        Cell {
            node: AtomicUsize::new(node),
            split_take,
            unchecked_put,
        }
    }

    /// `SlotCell::take`.
    fn take(&self) -> Option<usize> {
        let node = if self.split_take {
            let node = self.node.load(Ordering::Acquire);
            self.node.store(0, Ordering::Release);
            node
        } else {
            self.node.swap(0, Ordering::AcqRel)
        };
        (node != 0).then_some(node)
    }

    /// `SlotCell::put`: a compare-exchange from empty, FX-0534 when the
    /// cell is not empty. The control stores without looking.
    fn put(&self, node: usize) {
        if self.unchecked_put {
            self.node.store(node, Ordering::Release);
            return;
        }
        assert!(
            self.node
                .compare_exchange(0, node, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "a task's slot was given back while its cell held one"
        );
    }

    /// `SlotCell::holds`.
    fn holds(&self) -> bool {
        self.node.load(Ordering::Acquire) != 0
    }
}

/// Two takers race for the slot -- two queues' wakers, as a task woken on
/// two processors at once would make them -- and the winner, the only one
/// with the node, gives it back while a third asks whether it is held.
fn two_takers(split_take: bool) {
    model(move || {
        // The control gives back without looking as well, so that the only
        // thing left to catch two takers is the holders' count.
        let cell = Arc::new(Cell::new(7, split_take, split_take));
        let held = Arc::new(AtomicUsize::new(0));
        let takers: Vec<_> = (0..2)
            .map(|_| {
                let (cell, held) = (Arc::clone(&cell), Arc::clone(&held));
                thread::spawn(move || {
                    if let Some(node) = cell.take() {
                        assert_eq!(node, 7, "a take answered a node the cell never held");
                        let before = held.fetch_add(1, Ordering::AcqRel);
                        assert_eq!(before, 0, "two takers held the slot at once");
                        held.fetch_sub(1, Ordering::AcqRel);
                        cell.put(node);
                    }
                })
            })
            .collect();
        let looker = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || {
                let _ = cell.holds();
            })
        };
        for taker in takers {
            taker.join().unwrap();
        }
        looker.join().unwrap();
        assert!(
            cell.holds(),
            "the slot was lost: no holder has it and the cell is empty"
        );
    });
}

#[test]
fn two_takers_and_a_look() {
    two_takers(false);
}

/// The control: a take made as a load and then a store lets both takers
/// have the node -- and, with them, one taker's store of null can wipe the
/// other's give-back. Either failure is the control firing: "two takers
/// held the slot at once" or "the slot was lost".
#[test]
#[should_panic(expected = "the slot")]
fn control_a_take_that_is_not_a_swap() {
    two_takers(true);
}

/// A holder gives its slot back while another context takes it again as
/// soon as it is there -- a queue letting a task go and the next queue
/// taking it -- and a give-back, if ever made twice, must not lose a node.
fn a_give_back_and_a_take(twice: bool, unchecked_put: bool) {
    model(move || {
        let cell = Arc::new(Cell::new(0, false, unchecked_put));
        let giver = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || {
                cell.put(7);
                if twice {
                    // A holder that gives back a node it no longer has: the
                    // one-holder rule forbids it, and the assertion stops it.
                    cell.put(8);
                }
            })
        };
        let taker = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || cell.take())
        };
        giver.join().unwrap();
        let taken = taker.join().unwrap();
        let left = cell.take();
        let mut nodes: Vec<usize> = taken.into_iter().chain(left).collect();
        nodes.sort_unstable();
        let expected: Vec<usize> = if twice { vec![7, 8] } else { vec![7] };
        assert_eq!(nodes, expected, "a node given back was lost");
    });
}

#[test]
fn a_give_back_against_a_take() {
    a_give_back_and_a_take(false, false);
}

/// With the compare-exchange, a give-back into a full cell stops (FX-0534)
/// wherever the second finds the first still there.
#[test]
#[should_panic(expected = "given back while its cell held one")]
fn a_double_give_back_is_stopped() {
    a_give_back_and_a_take(true, false);
}

/// The control: a plain store, with no look at the cell, lets a double
/// give-back lose a node.
#[test]
#[should_panic(expected = "a node given back was lost")]
fn control_a_double_give_back_without_the_assertion() {
    a_give_back_and_a_take(true, true);
}

/// `SlotCell::take_held` and `put_held` (`L.sched.69`): the direct switch's
/// take of the peer's run slot and give-back of the caller's, each a load
/// and a store, made under the home queue's lock -- here a `Mutex` -- which
/// every other taker and giver of those two cells holds too: a waker of
/// the peer taking its slot to queue it (a swap, `insert_at`), and a taker
/// of the caller's cell that gives back what it got. With the lock, no
/// node is lost or held twice. Without it -- the control -- the load and
/// store race the swaps.
fn held_under_the_home_lock(locked: bool) {
    model(move || {
        let peer = Arc::new(Cell::new(7, false, false));
        let caller = Arc::new(Cell::new(0, false, false));
        let home = Arc::new(loom::sync::Mutex::new(()));
        let direct = {
            let (peer, caller, home) = (Arc::clone(&peer), Arc::clone(&caller), Arc::clone(&home));
            thread::spawn(move || {
                let guard = locked.then(|| home.lock().unwrap());
                // `take_held`: a load and, if a node was there, a store.
                let node = peer.node.load(Ordering::Acquire);
                if node != 0 {
                    peer.node.store(0, Ordering::Release);
                }
                // `put_held`: a load that must read empty, and a store of
                // the node the queue let go (9).
                assert_eq!(
                    caller.node.load(Ordering::Acquire),
                    0,
                    "the slot was given back into a full cell"
                );
                caller.node.store(9, Ordering::Release);
                drop(guard);
                (node != 0).then_some(node)
            })
        };
        let waker = {
            let (peer, home) = (Arc::clone(&peer), Arc::clone(&home));
            thread::spawn(move || {
                let _guard = home.lock().unwrap();
                peer.take()
            })
        };
        let looker = {
            let (caller, home) = (Arc::clone(&caller), Arc::clone(&home));
            thread::spawn(move || {
                let _guard = home.lock().unwrap();
                if let Some(node) = caller.take() {
                    caller.put(node);
                }
            })
        };
        let by_direct = direct.join().unwrap();
        let by_waker = waker.join().unwrap();
        looker.join().unwrap();
        let mut nodes: Vec<usize> = by_direct
            .into_iter()
            .chain(by_waker)
            .chain(peer.take())
            .chain(caller.take())
            .collect();
        nodes.sort_unstable();
        assert_eq!(nodes, vec![7, 9], "the slot was lost or held twice");
    });
}

#[test]
fn a_held_take_and_give_back_under_the_home_lock() {
    held_under_the_home_lock(true);
}

/// The control: the same load and store without the home lock lose a node
/// or let two holders have one.
#[test]
#[should_panic(expected = "the slot was")]
fn control_a_held_take_and_give_back_without_the_lock() {
    held_under_the_home_lock(false);
}
