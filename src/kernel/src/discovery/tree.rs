//! The device tree's `virtio,mmio` nodes, as a [`Finder`].

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::fmt;

use ferrix_bootinfo::BootView;
use ferrix_fdt::{Fdt, GicInterrupt, VIRTIO_MMIO_COMPATIBLE};

use crate::device::{self, DeviceNode, Location};
use crate::discovery::description::{self, Description};
use crate::discovery::finder::{Context, Failed, Finder, OutOfMemory};
use crate::fallible;

/// Every `virtio,mmio` node in the device tree, on a machine without ACPI.
///
/// Its vectors are decoded only when the tree's interrupt controller takes
/// three-cell GIC specifiers, which is every tree Ferrix boots with; a node's
/// `interrupt-parent` is not followed, so a machine with a second interrupt
/// controller would need that first. A vector is taken only if it is a shared
/// peripheral interrupt that no kernel handler and no earlier node holds. The
/// first that fails ends the node's list, as a bad specifier does, because a
/// driver asks for its interrupts by position.
#[derive(Debug)]
pub struct VirtioMmio {
    /// The tree, on a machine it describes.
    tree: Option<Fdt<'static>>,
    /// Whether `find` ran out of memory adding a node.
    out_of_memory: bool,
}

impl VirtioMmio {
    /// The finder for `view`'s machine: none to find without a device tree
    /// it is read by.
    pub fn new(view: &BootView<'_>) -> Self {
        let tree = match description::of(view) {
            Description::Tree(tree) => Some(tree),
            _ => None,
        };
        VirtioMmio {
            tree,
            out_of_memory: false,
        }
    }
}

impl Finder for VirtioMmio {
    fn name(&self) -> &'static str {
        "tree"
    }

    fn find(&mut self, cx: &mut Context<'_>, nodes: &mut Vec<DeviceNode>) -> Result<(), Failed> {
        let Some(tree) = &self.tree else {
            return Ok(());
        };
        let gic = tree
            .interrupt_controller()
            .is_some_and(|controller| controller.interrupt_cells() == Some(3));

        for found in tree.compatible_nodes(VIRTIO_MMIO_COMPATIBLE) {
            let Some(region) = found.reg().next() else {
                continue;
            };
            let mut node = DeviceNode::empty(Location::VirtioMmio(region.address));
            node.mint(region.address, region.size, false, cx.reserved);
            if gic {
                add_lines(&mut node, found.gic_interrupts(), &mut cx.held);
            }
            fallible::try_push(nodes, node).map_err(|_| {
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

/// Give `node` the tree's `interrupts` in order, each only if it is a shared
/// line no kernel handler and no earlier node holds. The first that is not
/// ends the list, and it and every one after it are withheld: a driver asks
/// for its interrupts by position.
fn add_lines(
    node: &mut DeviceNode,
    mut interrupts: impl Iterator<Item = GicInterrupt>,
    held: &mut BTreeSet<u32>,
) {
    for interrupt in interrupts.by_ref() {
        if !device::claim_line(interrupt.id, held) {
            node.withhold_lines(1);
            break;
        }
        node.add_line(&interrupt);
    }
    node.withhold_lines(interrupts.count());
}
