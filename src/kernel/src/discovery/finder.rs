//! A way of finding devices: the one interface the PCI walk, the device
//! tree's `virtio,mmio` nodes and the boards' bindings implement.
//!
//! [`device::publish`](crate::device::publish) runs the finders it is given
//! in order, then checks and publishes what they found. The contract:
//!
//! * **What a finder returns is untrusted.** Its nodes are firmware's and a
//!   device's description, as a finder read it. `publish` takes an aperture
//!   away from any node whose aperture an earlier node already holds, and
//!   checks every node -- exactly its apertures, its vectors, nothing shared
//!   -- as it did before there were finders. A finder cannot publish a node
//!   `publish` would refuse.
//! * **Order is precedence.** Finders run in the order given, and an earlier
//!   finder's node keeps an aperture a later one also claims. The boot gives
//!   the PCI walk, then the tree's nodes, then the boards'.
//! * **A failure stops discovery.** A finder that fails ends the run before
//!   any later finder runs and before anything is published, and the boot
//!   halts on it, as a failed PCI walk always has. The tree's and the
//!   boards' finders fail only on running out of memory: what they cannot
//!   use they leave alone and say why.
//! * **Only publish makes a node count.** `DeviceNode::empty` and `mint`
//!   are crate-visible so that finders outside `device` can build nodes,
//!   and that is safe because a node reaches a driver only through
//!   `publish`, which runs once at boot and refuses a second call before
//!   touching anything: a node built anywhere else can never be published.
//!   A change that wants to publish a node later has to argue with this.
//! * **Allocation is fallible.** A node is added to the list with
//!   `fallible::try_push`, and running out of memory is a failure like any
//!   other.
//!
//! The core defines this and knows no finder: the item's PCI walk implements
//! it, and `main.rs` hands the finders to `publish`. A finder keeps its own
//! failure and lends it as something to print, so the core names no finder's
//! error type.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::fmt;

use crate::device::{DeviceNode, Reserved};

/// What finders share while they run: the memory no aperture may overlap,
/// and the device tree interrupt lines already given to a node.
#[derive(Debug)]
pub struct Context<'r> {
    /// Memory no aperture may overlap.
    pub reserved: &'r Reserved,
    /// Interrupt lines a finder's node already holds.
    pub(crate) held: BTreeSet<u32>,
}

impl<'r> Context<'r> {
    /// A run over `reserved`, with no line held yet.
    pub(crate) const fn new(reserved: &'r Reserved) -> Self {
        Context {
            reserved,
            held: BTreeSet::new(),
        }
    }
}

/// A way of finding devices.
pub trait Finder {
    /// What its boot lines start with: `pci`, `tree`, `board`.
    fn name(&self) -> &'static str;

    /// Physical ranges it reads while finding -- the PCI walk's ECAM windows
    /// -- which no node's aperture may overlap. None, unless it says so.
    fn reads(&self) -> &[(u64, u64)] {
        &[]
    }

    /// Find its devices and add them to `nodes`.
    ///
    /// # Errors
    ///
    /// [`Failed`] when it could not: discovery ends there, and
    /// [`failure`](Finder::failure) says why.
    fn find(&mut self, cx: &mut Context<'_>, nodes: &mut Vec<DeviceNode>) -> Result<(), Failed>;

    /// Why `find` failed, to print; `None` while it has not.
    fn failure(&self) -> Option<&dyn fmt::Display>;

    /// Print its boot lines, once `find` has run.
    fn report(&self) {}
}

/// A finder could not find its devices; it says why in
/// [`failure`](Finder::failure).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Failed;

/// Running out of memory while adding a node.
#[derive(Debug)]
pub(crate) struct OutOfMemory;

impl fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no memory left to add a device node")
    }
}
