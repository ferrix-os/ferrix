//! Virtual Ethernet pairs (`docs/NETNS.md` section 3.3): two interfaces, in
//! one network namespace or two, such that what leaves one arrives at the
//! other.
//!
//! # A pair is a record, and the stacks know their end by its backing
//!
//! Each end is an interface of some namespace's stack whose
//! [`Backing::Veth`] names the pair and which end it is. The pair record says
//! where each end is now -- a namespace and an index, both of which change when
//! an end moves. A frame an end transmits is taken out of its stack's egress
//! by [`NetCore::with_crossing`] as a [`Frame`], and [`forward`] carries it.
//!
//! # No two stack locks at once
//!
//! [`forward`] runs with no lock held. For each frame it looks up the peer
//! under the pair's leaf lock, releases it, and enters the peer's stack alone;
//! what that reception produced for a pair is handed back and put behind the
//! frames still to carry. It is a loop over a queue, not a recursion, and it
//! stops after [`MAX_FORWARD`] frames, dropping the rest as a network does.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_linux_abi::errno::Errno;
use ferrix_net::iface::{Backing, IFF_LOWER_UP, IFF_RUNNING, Interface};

use super::NetNamespace;
use crate::sync::SpinLock;

/// How many frames one call to [`forward`] carries before it drops the rest.
pub(crate) const MAX_FORWARD: usize = 4096;

/// The MTU of a pair: Ethernet's.
const MTU: u32 = 1500;

/// The number of the next pair.
static NEXT_PAIR: AtomicU64 = AtomicU64::new(1);

/// A frame an end transmitted, on its way to the other.
#[derive(Debug)]
pub(crate) struct Frame {
    /// Which pair.
    pub(crate) pair: u64,
    /// The end that sent it.
    pub(crate) end: u8,
    /// The Ethernet frame.
    pub(crate) bytes: Vec<u8>,
}

/// Which pair and end a backing is, if it is one.
pub(crate) const fn end_of(backing: Backing) -> Option<(u64, u8)> {
    match backing {
        Backing::Veth { pair, end } => Some((pair, end)),
        Backing::Software | Backing::Device(_) => None,
    }
}

/// Where one end is.
#[derive(Debug)]
struct End {
    /// The namespace it is in.
    namespace: Weak<NetNamespace>,
    /// Its index there.
    index: u32,
}

/// A pair.
#[derive(Debug)]
struct Pair {
    /// Where its two ends are.
    ends: SpinLock<[End; 2]>,
    /// The record and its place in the table, charged to the job that made
    /// the pair; the interfaces are the namespaces' tables' to pay for.
    _charge: Charge,
}

/// Every pair, by number.
static PAIRS: SpinLock<BTreeMap<u64, Arc<Pair>>> = SpinLock::new(BTreeMap::new());

/// How many pairs exist.
pub(crate) fn count() -> usize {
    PAIRS.lock().len()
}

/// The pair, if it is still there.
fn pair(number: u64) -> Option<Arc<Pair>> {
    PAIRS.lock().get(&number).cloned()
}

/// The namespace and index of the end opposite `end` of pair `number`.
pub(crate) fn peer(number: u64, end: u8) -> Option<(Arc<NetNamespace>, u32)> {
    let pair = pair(number)?;
    let ends = pair.ends.lock();
    let other = ends.get(usize::from(1 - (end & 1)))?;
    Some((other.namespace.upgrade()?, other.index))
}

/// Carry the frames that left one end of a pair to the other, and what their
/// reception sends back, until there are none or [`MAX_FORWARD`] are carried.
/// Called with no lock held.
pub(crate) fn forward(frames: Vec<Frame>) {
    if frames.is_empty() {
        return;
    }
    let mut work: VecDeque<Frame> = frames.into();
    let mut budget = MAX_FORWARD;
    while let Some(frame) = work.pop_front() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some((target, index)) = peer(frame.pair, frame.end) else {
            continue;
        };
        work.extend(target.core().receive_crossing(index, &frame.bytes));
    }
}

/// An address for an end: locally administered, unicast, and different for
/// each end of each pair. Not secret, and not meant to be.
fn hardware_address(number: u64, end: u8) -> [u8; 6] {
    let noise = crate::timer::now_nanos().wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40;
    let [.., high, low] = number.to_be_bytes();
    let [.., mixed] = noise.to_be_bytes();
    [0x02, 0xFE, high, low, mixed, end]
}

/// The first `vethN` no interface of `namespace` has.
fn free_name(namespace: &NetNamespace) -> Vec<u8> {
    namespace.core().look(|stack| {
        (0_u32..)
            .map(|n| alloc::format!("veth{n}").into_bytes())
            .find(|name| stack.interface_by_name(name).is_none())
            .unwrap_or_default()
    })
}

/// Make a pair with one end in `one` and the other in `two`, and answer the
/// indexes of the ends there. A name of `None` is the first free `vethN`.
///
/// # Errors
///
/// `ENOSPC` past a namespace's ceiling, `ENOMEM` past a job's or a
/// namespace's charge, `EEXIST` for a name a namespace already has.
pub(crate) fn create(
    (one, one_name): (&Arc<NetNamespace>, Option<&[u8]>),
    (two, two_name): (&Arc<NetNamespace>, Option<&[u8]>),
) -> Result<(u32, u32), Errno> {
    let same = one.same(two);
    one.roomy(if same { 2 } else { 1 })?;
    two.roomy(if same { 2 } else { 1 })?;
    one.admit()?;
    two.admit()?;
    let charge =
        Charge::bytes(arc_footprint::<Pair>().saturating_add(3 * size_of::<(u64, Arc<Pair>)>()))
            .map_err(|_| Errno::ENOMEM)?;
    let number = NEXT_PAIR.fetch_add(1, Ordering::Relaxed);
    // A name asked for twice in one namespace is a name that exists.
    if same && one_name.is_some() && one_name == two_name {
        return Err(Errno::EEXIST);
    }
    let record = crate::fallible::try_arc(Pair {
        ends: SpinLock::new([
            End {
                namespace: Weak::new(),
                index: 0,
            },
            End {
                namespace: Weak::new(),
                index: 0,
            },
        ]),
        _charge: charge,
    })
    .map_err(|_| Errno::ENOMEM)?;
    let first = attach(&record, number, 0, one, one_name)?;
    // Findable now, so a failure to make the second end is undone by the same
    // path as any deletion.
    let _ = PAIRS.lock().insert(number, Arc::clone(&record));
    let second = match attach(&record, number, 1, two, two_name) {
        Ok(index) => index,
        Err(errno) => {
            destroy(number);
            return Err(errno);
        }
    };
    for space in [one, two] {
        let _ = space.fit(0);
    }
    Ok((first, second))
}

/// Make end `end` of pair `number` in `space`, under `name` or the first free
/// `vethN`, and record where it is.
fn attach(
    record: &Pair,
    number: u64,
    end: u8,
    space: &Arc<NetNamespace>,
    name: Option<&[u8]>,
) -> Result<u32, Errno> {
    let name = name.map_or_else(|| free_name(space), <[u8]>::to_vec);
    let mut interface = Interface::ethernet(0, &name, hardware_address(number, end), MTU);
    interface.backing = Backing::Veth { pair: number, end };
    let index = space
        .core()
        .with(|stack, _| stack.attach_interface(interface))
        .map_err(|_| Errno::EEXIST)?;
    if let Some(slot) = record.ends.lock().get_mut(usize::from(end & 1)) {
        *slot = End {
            namespace: Arc::downgrade(space),
            index,
        };
    }
    Ok(index)
}

/// Destroy a pair: both ends leave their stacks, wherever they are, and the
/// record goes.
pub(crate) fn destroy(number: u64) {
    let Some(pair) = PAIRS.lock().remove(&number) else {
        return;
    };
    let ends = pair.ends.lock();
    let here: Vec<(Arc<NetNamespace>, u32)> = ends
        .iter()
        .filter_map(|end| Some((end.namespace.upgrade()?, end.index)))
        .collect();
    drop(ends);
    for (namespace, index) in here {
        let _ = namespace.core().with(|stack, _| {
            // Only if it is still this pair's: an index is reused.
            let ours = stack
                .interface(index)
                .is_some_and(|each| end_of(each.backing).is_some_and(|(n, _)| n == number));
            ours && stack.remove_interface(index)
        });
        let _ = namespace.fit(0);
    }
}

/// An end is now `index` of `namespace`: a move.
pub(crate) fn moved(number: u64, end: u8, namespace: &Arc<NetNamespace>, index: u32) {
    if let Some(pair) = pair(number)
        && let Some(slot) = pair.ends.lock().get_mut(usize::from(end & 1))
    {
        *slot = End {
            namespace: Arc::downgrade(namespace),
            index,
        };
    }
}

/// Give both ends the carrier flags if both are up, and take them from both
/// if not: a pair runs when both its ends are up.
pub(crate) fn refresh(number: u64) {
    let Some(pair) = pair(number) else {
        return;
    };
    let ends: Vec<(Arc<NetNamespace>, u32)> = pair
        .ends
        .lock()
        .iter()
        .filter_map(|end| Some((end.namespace.upgrade()?, end.index)))
        .collect();
    let up = ends.len() == 2
        && ends.iter().all(|(namespace, index)| {
            namespace
                .core()
                .look(|stack| stack.interface(*index).is_some_and(Interface::is_up))
        });
    for (namespace, index) in &ends {
        namespace.core().with(|stack, _| {
            if let Some(interface) = stack.interface_mut(*index) {
                if up {
                    interface.flags |= IFF_RUNNING | IFF_LOWER_UP;
                } else {
                    interface.flags &= !(IFF_RUNNING | IFF_LOWER_UP);
                }
            }
        });
    }
}
