//! The firmware framebuffer as a device node, when the command line asks.
//!
//! A machine whose GPU has no driver still has the screen its firmware left
//! lit: the GOP framebuffer the loader hands over. `ferrix.efifb` lends it to
//! a ring-3 driver, `efifb`, which serves it to the display core as a card and
//! shows each frame by copying it there (`docs/DISPLAY.md` §8). The kernel
//! still draws nothing but a panic.
//!
//! Off, the default and the reference configuration, this finder adds
//! nothing and says nothing, and the framebuffer stays reserved from every
//! aperture as it always was. On, it adds at most one node, whose one
//! aperture is the framebuffer [`DeviceNode::firmware_framebuffer`] mints
//! under [`crate::device::Reserved`]'s single exemption for it.
//!
//! # One memory type
//!
//! The kernel maps the framebuffer for the panic screen, and a driver maps
//! the same pages: the two must be one type (`user::memory_type`). On x86-64
//! the kernel's mapping is uncached device memory, so the aperture is not
//! prefetchable, and the kernel takes a hold of that type over it which it
//! never gives back: a mapping of any other type is refused. The Arm kernels
//! map it normal non-cacheable, which no user mapping of an aperture can
//! match yet, so the option is not honoured there and says so.
//!
//! # The boot console
//!
//! `console::screen` draws on the framebuffer through a slice it holds alone.
//! Once the node exists a driver can map the window, so the console stops as
//! the node is made, before any program runs.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::{BootView, Framebuffer};
use ferrix_sync::Once;

use crate::device::DeviceNode;
use crate::discovery::finder::{Context, Failed, Finder, OutOfMemory};
use crate::fallible;
use crate::user::memory_type::{self, Hold, MemoryType};

/// The command-line word that asks for the node.
pub(crate) const OPTION: &str = "ferrix.efifb";

/// Whether the command line said [`OPTION`], as the finder read it: what
/// stage 10's check holds a node to.
static ASKED: AtomicBool = AtomicBool::new(false);

/// The kernel's own hold on the lent framebuffer's type, kept for the life
/// of the machine.
static KERNEL_TYPE: Once<Arc<Hold>> = Once::new();

/// The type the kernel maps the framebuffer with, where the node is minted.
pub(crate) const KERNEL_MAPPING: MemoryType = MemoryType::Device;

/// Whether the command line said [`OPTION`].
pub(crate) fn asked(view: &BootView<'_>) -> bool {
    view.flag(OPTION)
}

/// The finder: the framebuffer, when [`OPTION`] asked for it.
#[derive(Debug)]
pub(crate) struct FirmwareFramebuffer {
    /// The loader's framebuffer, when the option is on.
    framebuffer: Option<Framebuffer>,
    /// What became of it, for the boot line.
    outcome: Option<Result<(u32, u32), &'static str>>,
    /// Whether `find` ran out of memory adding the node.
    out_of_memory: bool,
}

impl FirmwareFramebuffer {
    /// The finder for `view`'s machine.
    pub(crate) fn new(view: &BootView<'_>) -> Self {
        ASKED.store(asked(view), Ordering::Release);
        FirmwareFramebuffer {
            framebuffer: asked(view).then(|| view.raw().framebuffer),
            outcome: None,
            out_of_memory: false,
        }
    }

    /// The node, its type held, or why there is none.
    fn lend(framebuffer: &Framebuffer, cx: &Context<'_>) -> Result<DeviceNode, &'static str> {
        if !crate::arch::LENDS_FRAMEBUFFER {
            return Err("not honoured on this architecture");
        }
        let node = DeviceNode::firmware_framebuffer(framebuffer, cx.reserved)?;
        let (phys, len) = node
            .aperture_info(0)
            .map(|info| (info.phys, info.len))
            .ok_or("the framebuffer has no aperture")?;
        let hold = memory_type::hold(phys, len, KERNEL_MAPPING)
            .map_err(|_| "the framebuffer's memory type could not be held")?;
        let _ = KERNEL_TYPE.call_once(|| hold);
        // A driver may map the window from now on: the boot console, which
        // draws through a slice it holds alone, stops first.
        crate::console::screen::stop();
        Ok(node)
    }
}

/// Whether a node may exist at all: the option was given, on x86-64.
pub(crate) fn may_lend() -> bool {
    ASKED.load(Ordering::Acquire) && crate::arch::LENDS_FRAMEBUFFER
}

/// Whether a mapping of `len` bytes at `phys` as `kind` would be refused by
/// the kernel's hold on the lent framebuffer: what stage 10's check asks.
pub(crate) fn refuses(phys: u64, len: u64, kind: MemoryType) -> bool {
    KERNEL_TYPE.get().is_some() && memory_type::hold(phys, len, kind).is_err()
}

impl Finder for FirmwareFramebuffer {
    fn name(&self) -> &'static str {
        "firmware"
    }

    fn find(&mut self, cx: &mut Context<'_>, nodes: &mut Vec<DeviceNode>) -> Result<(), Failed> {
        let Some(framebuffer) = self.framebuffer else {
            return Ok(());
        };
        match Self::lend(&framebuffer, cx) {
            Ok(node) => {
                fallible::try_push(nodes, node).map_err(|_| {
                    self.out_of_memory = true;
                    Failed
                })?;
                self.outcome = Some(Ok((framebuffer.width, framebuffer.height)));
            }
            Err(why) => self.outcome = Some(Err(why)),
        }
        Ok(())
    }

    fn failure(&self) -> Option<&dyn fmt::Display> {
        self.out_of_memory
            .then_some(&OutOfMemory as &dyn fmt::Display)
    }

    fn report(&self) {
        match self.outcome {
            Some(Ok((width, height))) => crate::console::println!(
                "  display  firmware framebuffer {width}x{height} lent to a driver ({OPTION})"
            ),
            Some(Err(why)) => crate::console::println!(
                "  display  firmware framebuffer not lent ({OPTION}): {why}"
            ),
            None => {}
        }
    }
}
