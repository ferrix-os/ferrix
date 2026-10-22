//! Stage 10's power node (`docs/POWERCTL.md` §4.1, `L.device.51`): one
//! node, `TREE_X86_POWER`, when the architecture's power core says the
//! machine has one -- only under `ferrix.powerctl` on a processor its model
//! table names, or under the test-only `ferrix.powerctl.fake` -- and
//! nothing at all otherwise. A refusal is the core's one line and no node,
//! never a failed finder: the boot goes on.

use alloc::vec::Vec;
use core::fmt;

use ferrix_bootinfo::BootView;

use crate::device::DeviceNode;
use crate::discovery::finder::{Context, Failed, Finder, OutOfMemory};
use crate::fallible;

/// The power node's finder.
pub(crate) struct Power {
    /// Whether the core said to publish a node.
    publish: bool,
    /// Whether adding it ran out of memory.
    out_of_memory: bool,
}

impl Power {
    /// Ask the core, once, on the boot processor.
    pub(crate) fn new(view: &BootView<'_>) -> Self {
        Power {
            publish: crate::arch::power_probe(view),
            out_of_memory: false,
        }
    }
}

impl Finder for Power {
    fn name(&self) -> &'static str {
        "power"
    }

    fn find(&mut self, _cx: &mut Context<'_>, nodes: &mut Vec<DeviceNode>) -> Result<(), Failed> {
        if self.publish {
            fallible::try_push(nodes, DeviceNode::power()).map_err(|_| {
                self.out_of_memory = true;
                Failed
            })?;
        }
        Ok(())
    }

    fn failure(&self) -> Option<&dyn fmt::Display> {
        self.out_of_memory
            .then_some(&OutOfMemory as &dyn fmt::Display)
    }
}
