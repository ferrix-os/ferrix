//! The boards' device-tree bindings: the registry board support fills at
//! bring-up, and the [`Finder`] that publishes a node for each binding the
//! tree has.
//!
//! How the list is filled: board support -- `platform/`, which is load, not
//! core -- calls [`register_board`] at bring-up with a static
//! [`BoardBinding`], before discovery runs. The registry holds only those
//! statics and calls only the functions in them; it names no board.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use ferrix_bootinfo::BootView;
use ferrix_fdt::{Fdt, GicInterrupt};

use crate::device::{DeviceNode, DmaShape, Location, Reserved};
use crate::discovery::description::{self, Description};
use crate::discovery::finder::{Context, Failed, Finder, OutOfMemory};
use crate::fallible;
use crate::hooks::{Full, Hooks};

/// A peripheral a board's support found in the device tree and made ready
/// for a driver: what a [`BoardBinding`]'s `prepare` hands back.
#[derive(Debug)]
pub struct BoardDevice {
    /// Its register ranges, `(phys, len)`, in the order its driver maps
    /// them. Each must become an aperture, or the device is left alone.
    pub registers: Vec<(u64, u64)>,
    /// Its interrupt, as the tree gives it.
    pub interrupt: GicInterrupt,
    /// How it reaches memory.
    pub dma: DmaShape,
    /// What preparing it did, for the boot log.
    pub summary: String,
}

/// What preparing a board's peripheral for a driver is: find it in the
/// tree, clock it and take it out of reset, and say where it is. `Ok(None)`
/// on a machine that does not have it; `Err` says what stopped it.
pub(crate) type Prepare = fn(&Fdt<'_>) -> Result<Option<BoardDevice>, &'static str>;

/// The one clock of a board's peripheral a driver may ask for: the rate
/// wanted, and whether to set it or only ask what it would be. The rate it
/// is, or would be.
pub(crate) type Clock = fn(u64, bool) -> Result<u64, &'static str>;

/// A board's support for one device-tree binding the kernel knows, which
/// the board registers at bring-up with [`register_board`].
///
/// The core cannot name the board: board support is uncertified load, and a
/// registry that called into it would have put it in the core. So the board
/// hands the registry this instead, and the registry mints the node's
/// apertures and vector from what `prepare` says, under the same rules as
/// every other node's.
#[derive(Debug)]
pub struct BoardBinding {
    /// Which binding it is: `TREE_STM32_HDMI` and the rest.
    pub binding: u16,
    /// What the boot log's lines about it start with: `display`, `usb`.
    pub label: &'static str,
    /// What is left alone when it cannot be handed over, in a sentence:
    /// "the board's USB host".
    pub device: &'static str,
    /// Find it and make it ready.
    pub prepare: Prepare,
    /// Its clock a driver may set, if it has one: `device_clock`.
    pub clock: Option<Clock>,
}

/// Every registered [`BoardBinding`], in the order their nodes are published.
/// Eight is five more than the one board Ferrix knows registers.
static BOARD: Hooks<BoardBinding, 8> = Hooks::new();

/// Publish a node for `binding`'s peripheral whenever the tree has it,
/// after every binding registered before it.
///
/// Registered before [`crate::device::publish`] runs, which `main.rs`'s bring-up
/// order
/// makes so; a binding registered after it is never asked.
///
/// # Errors
///
/// [`Full`] when the list is.
pub fn register_board(binding: &'static BoardBinding) -> Result<(), Full> {
    BOARD.register(binding)
}

/// How many board bindings are registered, for the boot's check that board
/// support registered before enumeration.
pub fn board_bindings() -> usize {
    BOARD.len()
}

/// Ask for `node`'s clock at `hz`, setting it when `set` is: `None` when the
/// node's binding has no clock a driver may ask for.
pub fn board_clock(node: &DeviceNode, hz: u64, set: bool) -> Option<Result<u64, &'static str>> {
    let binding = node.tree_binding()?;
    let clock = BOARD
        .iter()
        .find(|registered| registered.binding == binding)?
        .clock?;
    Some(clock(hz, set))
}

/// A registered board binding's peripheral, when the tree has one: its
/// registers and interrupt, once the board's `prepare` has made it ready.
fn board_node(
    tree: &Fdt<'_>,
    board: &BoardBinding,
    reserved: &Reserved,
    held: &mut BTreeSet<u32>,
) -> Option<DeviceNode> {
    let label = board.label;
    let device = board.device;
    let prepared = match (board.prepare)(tree) {
        Ok(found) => found?,
        Err(why) => {
            crate::console::println!("  {label:<8} {device} is left alone: {why}");
            return None;
        }
    };
    let first = prepared.registers.first()?;
    let mut node = DeviceNode::empty(Location::Tree(first.0));
    node.bind_board(board.binding, prepared.dma);
    for &(phys, len) in &prepared.registers {
        node.mint(phys, len, false, reserved);
    }
    if node.apertures_minted() != prepared.registers.len() {
        crate::console::println!(
            "  {label:<8} {device} is left alone: its registers overlap memory the kernel uses"
        );
        return None;
    }
    let interrupt = prepared.interrupt;
    if !crate::device::claim_line(interrupt.id, held) {
        crate::console::println!(
            "  {label:<8} {device} is left alone: interrupt {} is taken",
            interrupt.id
        );
        return None;
    }
    node.add_line(&interrupt);
    crate::console::println!("  {label:<8} {}", prepared.summary);
    Some(node)
}

/// A node for every registered binding whose peripheral the tree has, in
/// the order they registered.
#[derive(Debug)]
pub struct Boards {
    /// The tree, on a machine it describes.
    tree: Option<Fdt<'static>>,
    /// Whether `find` ran out of memory adding a node.
    out_of_memory: bool,
}

impl Boards {
    /// The finder for `view`'s machine: none to find without a device tree
    /// it is read by.
    pub fn new(view: &BootView<'_>) -> Self {
        let tree = match description::of(view) {
            Description::Tree(tree) => Some(tree),
            _ => None,
        };
        Boards {
            tree,
            out_of_memory: false,
        }
    }
}

impl Finder for Boards {
    fn name(&self) -> &'static str {
        "board"
    }

    fn find(&mut self, cx: &mut Context<'_>, nodes: &mut Vec<DeviceNode>) -> Result<(), Failed> {
        let Some(tree) = &self.tree else {
            return Ok(());
        };
        for board in BOARD.iter() {
            if let Some(node) = board_node(tree, board, cx.reserved, &mut cx.held) {
                fallible::try_push(nodes, node).map_err(|_| {
                    self.out_of_memory = true;
                    Failed
                })?;
            }
        }
        Ok(())
    }

    fn failure(&self) -> Option<&dyn fmt::Display> {
        self.out_of_memory
            .then_some(&OutOfMemory as &dyn fmt::Display)
    }
}
