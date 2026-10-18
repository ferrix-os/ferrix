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
//! that type is write-combining. The kernel maps its window write-combining
//! from its first map in early boot, the boot processor's PAT programmed
//! first and every secondary's in its trampoline
//! (`arch::framebuffer_flags`, `L.x86_64.176`), so lending it changes no type:
//! the lend reads every page of the window back write-combining
//! ([`window_combining`]) and refuses otherwise, mints the aperture
//! prefetchable, so that `io_mapping_map_combining` maps it, and takes a
//! hold of write-combining over it which it never gives back: a mapping of
//! any other type is refused. The Arm kernels map it normal non-cacheable,
//! which no user mapping of an aperture can match yet, so the option is not
//! honoured there and says so.
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

/// The type the kernel maps the framebuffer with once it is lent, where the
/// node is minted: write-combining (`docs/DISPLAY.md` §8).
pub(crate) const KERNEL_MAPPING: MemoryType = MemoryType::Combining;

/// Whether the command line said [`OPTION`].
pub(crate) fn asked(view: &BootView<'_>) -> bool {
    view.flag(OPTION)
}

/// What a lent framebuffer's boot line says.
#[derive(Clone, Copy, Debug)]
struct Lent {
    width: u32,
    height: u32,
    /// The type firmware's MTRRs give it, when it is one.
    mtrr: Option<u8>,
}

/// The finder: the framebuffer, when [`OPTION`] asked for it.
#[derive(Debug)]
pub(crate) struct FirmwareFramebuffer {
    /// The loader's framebuffer, when the option is on.
    framebuffer: Option<Framebuffer>,
    /// What became of it, for the boot line: the size lent and the type the
    /// MTRRs give it, or why it was not.
    outcome: Option<Result<Lent, &'static str>>,
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
        Self::may_lend_here()?;
        let node = DeviceNode::firmware_framebuffer(framebuffer, cx.reserved)?;
        let (phys, len) = node
            .aperture_info(0)
            .map(|info| (info.phys, info.len))
            .ok_or("the framebuffer has no aperture")?;
        Self::hold(phys, len)?;
        Ok(node)
    }

    /// Lend the framebuffer to `holder`, the Intel integrated GPU whose
    /// memory holds it, under `ferrix.intelgpu` (K4, `L.device.46`): its type
    /// held as for a node of its own, the function then driven by the Intel
    /// driver, which serves the screen as a copying card.
    fn lend_to(
        framebuffer: &Framebuffer,
        cx: &Context<'_>,
        holder: &mut DeviceNode,
    ) -> Result<(), &'static str> {
        Self::may_lend_here()?;
        holder.lend_framebuffer_here(
            framebuffer,
            cx.reserved,
            crate::device::intel_gpu_allowed(),
        )?;
        let (phys, len) = (0..holder.apertures().len())
            .filter_map(|index| holder.aperture_info(index))
            .find(|info| info.phys == framebuffer.phys)
            .map(|info| (info.phys, info.len))
            .ok_or("the framebuffer has no aperture")?;
        Self::hold(phys, len)
    }

    /// Whether this machine may lend the framebuffer at all.
    fn may_lend_here() -> Result<(), &'static str> {
        if !crate::arch::LENDS_FRAMEBUFFER {
            return Err("not honoured on this architecture");
        }
        // Write-combining selects PAT entry 1, which is write-through until
        // a processor programs it: every one must have. Refused here, nothing
        // is minted and the kernel's window stays uncached.
        if !crate::object::io_mapping::combining_ready() {
            return Err("a processor's PAT has no write-combining entry");
        }
        Ok(())
    }

    /// The kernel's window read back write-combining, the boot console
    /// stopped and the kernel's hold of `phys..phys + len` taken for good:
    /// the lend's last steps, for either node.
    fn hold(phys: u64, len: u64) -> Result<(), &'static str> {
        let (window, mapped) =
            crate::panic::screen::window().ok_or("the kernel has no window on the framebuffer")?;
        // The kernel's own window has been write-combining since its first
        // map (`arch::framebuffer_flags`); it is read back, not changed, and
        // a page of any other type refuses the lend before the console is
        // stopped or anything is minted.
        if !window_combining(window, mapped) {
            return Err("the kernel's window on the framebuffer is not write-combining");
        }
        // A driver may map the window from now on: the boot console, which
        // draws through a slice it holds alone, stops first.
        crate::console::screen::stop();
        let hold = memory_type::hold(phys, len, KERNEL_MAPPING)
            .map_err(|_| "the framebuffer's memory type could not be held")?;
        let _ = KERNEL_TYPE.call_once(|| hold);
        Ok(())
    }
}

/// Whether every page of the kernel's `virt..virt + len` reads back mapped
/// write-combining, not as device memory, writable and not user's, in the
/// kernel's root table: what the lend requires of the kernel's window on
/// the framebuffer, and refuses it on otherwise (L.device.43). One leaf
/// read per page of the window, under no lock but the table walk's own.
pub(crate) fn window_combining(virt: u64, len: u64) -> bool {
    let root = crate::mm::root_table();
    len != 0
        && (0..len.div_ceil(ferrix_bootinfo::PAGE_SIZE)).all(|page| {
            crate::mm::flags_in(root, virt + page * ferrix_bootinfo::PAGE_SIZE).is_some_and(
                |flags| {
                    MemoryType::of(false, flags.write_combining && !flags.device) == KERNEL_MAPPING
                        && flags.write
                        && !flags.user
                },
            )
        })
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
        // Under `ferrix.intelgpu`, to the Intel GPU holding it, if it does
        // (K4, `L.device.46`): no node of its own, so nothing yields it.
        if let Some(index) = crate::device::lend_target(nodes, crate::device::intel_gpu_allowed())
            && let Some(holder) = nodes.get_mut(index)
        {
            self.outcome = Some(Self::lend_to(&framebuffer, cx, holder).map(|()| Lent {
                width: framebuffer.width,
                height: framebuffer.height,
                mtrr: crate::arch::mtrr_type(framebuffer.phys, framebuffer.size),
            }));
            return Ok(());
        }
        match Self::lend(&framebuffer, cx) {
            Ok(node) => {
                fallible::try_push(nodes, node).map_err(|_| {
                    self.out_of_memory = true;
                    Failed
                })?;
                let mtrr = crate::arch::mtrr_type(framebuffer.phys, framebuffer.size);
                self.outcome = Some(Ok(Lent {
                    width: framebuffer.width,
                    height: framebuffer.height,
                    mtrr,
                }));
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
            Some(Ok(Lent {
                width,
                height,
                mtrr,
            })) => crate::console::println!(
                "  display  firmware framebuffer {width}x{height} lent to a driver ({OPTION}), \
                 write-combining; its MTRR type {}",
                mtrr_name(mtrr)
            ),
            Some(Err(why)) => crate::console::println!(
                "  display  firmware framebuffer not lent ({OPTION}): {why}"
            ),
            None => {}
        }
    }
}

/// An MTRR type's name, as the boot line says it: a page mapped
/// write-combining through the PAT is write-combining under each of them.
const fn mtrr_name(kind: Option<u8>) -> &'static str {
    match kind {
        Some(0x00) => "uncacheable",
        Some(0x01) => "write-combining",
        Some(0x04) => "write-through",
        Some(0x05) => "write-protected",
        Some(0x06) => "write-back",
        Some(_) => "reserved",
        None => "unknown",
    }
}
