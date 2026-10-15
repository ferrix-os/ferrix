//! What the cores that serve devices through a control channel share: a
//! device's claim by a driver's control channel, which a quiesce waits out,
//! and the numbers their nodes are published under, which a driver started
//! again gets back.
//!
//! # Why a quiesce waits for the claim
//!
//! A driver's death fires `devmgr`'s `TERMINATED` when its handles close,
//! which raises `PEER_CLOSED` on the core's end of the control channel but
//! does not wait for the core's task to see it. Until that task ends the
//! device is still claimed, and a driver `devmgr` starts again in the
//! meantime is refused its control channel as `InUse`. So `device_quiesce`
//! waits, bounded, for every claim whose driver end has closed -- as it
//! already did for a block ring (`block_ring::wait_until_unserved`) -- and
//! refuses only a claim a live driver still holds.
//!
//! # Why the lowest free number
//!
//! A program opens `/dev/dri/card0`, and `compositor_drm::cards` stops at
//! the first gap. A card whose driver died and came back must be `card0`
//! again, not `card1` after a hole, or nothing that looks for a screen finds
//! it.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_native_abi::signals::Signals;

use crate::device::DeviceNode;
use crate::object::channel::Endpoint;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::timer;

/// Why a device node cannot be quiesced.
///
/// Defined here rather than in `crate::interfaces::block_ring` because the claim is the
/// core's: a quiesce asks whether anything still serves a node, and the answer
/// must not depend on which uncertified subsystem happens to be serving it.
/// `block_ring`, `render` and `display` each answer with this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StillServed {
    /// A driver holds its end of the ring's control channel: it is alive
    /// and serving, and quiescing under it is refused.
    ByADriver,
    /// The driver is gone but the ring's task has not ended within the
    /// patience, or the caller was terminated meanwhile.
    Waiting,
}

/// How long a quiesce waits for a core's task to let go of a device whose
/// driver has gone: the same patience the block ring gives.
const PATIENCE_NANOS: u64 = 5_000_000_000;

/// Devices claimed through a core's control channels: each held by the
/// core's end of the channel its driver was handed.
#[derive(Debug)]
pub struct Claims {
    held: SpinLock<Vec<(Arc<DeviceNode>, Arc<Endpoint>)>>,
    released: WaitQueue,
}

impl Claims {
    /// DOC-OWED
    #[allow(
        clippy::new_without_default,
        reason = "`new` is the one way to make it, at a place the kernel chooses; a `Default` would be a second"
    )]
    pub const fn new() -> Self {
        Self {
            held: SpinLock::new(Vec::new()),
            released: WaitQueue::new(),
        }
    }

    /// Claim `node` for the channel whose core end is `control`; `false`
    /// when it is claimed already, or when there was no memory to record the
    /// claim -- which the caller answers as it does a device in use.
    pub fn claim(&self, node: &Arc<DeviceNode>, control: &Arc<Endpoint>) -> bool {
        self.claim_up_to(node, control, 1)
    }

    /// Claim `node` for one more channel, `limit` at most at once: a USB
    /// host's driver asks the input core for a channel per keyboard or mouse
    /// behind it, all on the host's one node. `false` as [`Claims::claim`].
    ///
    /// # Which callers share a node, and why that is safe
    ///
    /// One only: the input core, for a USB host's node -- the STM32 USB
    /// host's, and a PCI function of xHCI's class (0C:03:30) -- whose
    /// `DeviceNode::input_functions` is `USB_INPUT_FUNCTIONS` (eight); every
    /// other node, and every other core, claims with a limit of one. The
    /// argument is the same for both hosts, since neither part of it depends
    /// on how the node was found. Sharing
    /// opens no DMA path (T.DMA): an input control channel carries no memory
    /// -- HELLO hands over only the driver's port and READY only the core's
    /// (`ferrix-inputctl`), so no VMO, pin or IOMMU mapping passes through
    /// it -- and every channel is made by the holder of the node's `MANAGE`
    /// handle (`native::control_channel`), which `devmgr` gives one driver,
    /// so the host controller's DMA stays that driver's own domain however
    /// many claims it holds. A core whose channel carries memory for a
    /// device's DMA must claim with a limit of one, or give each claim a
    /// domain of its own.
    pub fn claim_up_to(
        &self,
        node: &Arc<DeviceNode>,
        control: &Arc<Endpoint>,
        limit: usize,
    ) -> bool {
        let mut held = self.held.lock();
        let claims = held
            .iter()
            .filter(|(claimed, _)| Arc::ptr_eq(claimed, node))
            .count();
        if claims >= limit {
            return false;
        }
        crate::fallible::try_push(&mut held, (Arc::clone(node), Arc::clone(control))).is_ok()
    }

    /// Let `node` go, every claim of it, and wake a quiesce waiting for it.
    pub fn release(&self, node: &Arc<DeviceNode>) {
        self.held
            .lock()
            .retain(|(claimed, _)| !Arc::ptr_eq(claimed, node));
        self.released.wake_all();
    }

    /// Let go of the one claim of `node` made through `control`, leaving the
    /// node's others -- a USB host's other devices -- theirs.
    pub fn release_one(&self, node: &Arc<DeviceNode>, control: &Arc<Endpoint>) {
        self.held.lock().retain(|(claimed, through)| {
            !(Arc::ptr_eq(claimed, node) && Arc::ptr_eq(through, control))
        });
        self.released.wake_all();
    }

    /// Whether `node` is claimed through any channel.
    fn is_claimed(&self, node: &Arc<DeviceNode>) -> bool {
        self.held
            .lock()
            .iter()
            .any(|(claimed, _)| Arc::ptr_eq(claimed, node))
    }

    /// Whether a driver still holds its end of a channel `node` is claimed
    /// through.
    fn is_held_by_a_driver(&self, node: &Arc<DeviceNode>) -> bool {
        self.held.lock().iter().any(|(claimed, control)| {
            Arc::ptr_eq(claimed, node) && !control.signals().intersects(Signals::PEER_CLOSED)
        })
    }

    /// Wait until `node` is not claimed, for a quiesce: at once when it is
    /// not, bounded when every driver end it is claimed through has closed,
    /// refused while any driver still holds one. `cancelled` ends the wait
    /// early for a caller that is being terminated.
    ///
    /// # Errors
    ///
    /// [`StillServed`], as `block_ring::wait_until_unserved` answers.
    pub fn wait_until_released(
        &self,
        node: &Arc<DeviceNode>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), StillServed> {
        self.wait_within(node, cancelled, PATIENCE_NANOS)
    }

    /// [`Claims::wait_until_released`] with the patience given rather than
    /// the product's five seconds: the check proves in milliseconds what the
    /// patience's end answers.
    ///
    /// # Errors
    ///
    /// As [`Claims::wait_until_released`].
    pub fn wait_within(
        &self,
        node: &Arc<DeviceNode>,
        cancelled: &dyn Fn() -> bool,
        patience: u64,
    ) -> Result<(), StillServed> {
        let deadline = timer::now_nanos().saturating_add(patience);
        loop {
            if !self.is_claimed(node) {
                return Ok(());
            }
            if self.is_held_by_a_driver(node) {
                return Err(StillServed::ByADriver);
            }
            let released = self
                .released
                .wait_until_deadline(|| !self.is_claimed(node) || cancelled(), deadline);
            if !released || cancelled() {
                return Err(StillServed::Waiting);
            }
        }
    }
}

/// The numbers a core's nodes are published under, from `first` upwards,
/// each the lowest one nothing holds.
#[derive(Debug)]
pub struct Numbers {
    first: u32,
    taken: SpinLock<Vec<u32>>,
}

impl Numbers {
    /// DOC-OWED
    pub const fn new(first: u32) -> Self {
        Self {
            first,
            taken: SpinLock::new(Vec::new()),
        }
    }

    /// The lowest number nothing holds, now held; `None` when there was no
    /// memory to hold it.
    pub fn take(&self) -> Option<u32> {
        let mut taken = self.taken.lock();
        let mut number = self.first;
        while taken.contains(&number) {
            number = number.saturating_add(1);
        }
        crate::fallible::try_push(&mut taken, number).ok()?;
        Some(number)
    }

    /// Give `number` back, for the next node to be published under.
    pub fn give_back(&self, number: u32) {
        self.taken.lock().retain(|&held| held != number);
    }
}
