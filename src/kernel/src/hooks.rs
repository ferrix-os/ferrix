//! Short lists of registrations, which is how the certified item reaches code
//! above it without naming it.
//!
//! The item has to act on things it does not vouch for: power has to commit a
//! filesystem before the machine stops, and device enumeration has to ask a
//! board's support which of its peripherals are ready for a driver. Naming the
//! filesystem or the board from the item would put them in it, whatever
//! `docs/certification/ITEM.md` says. So the item defines what it needs as a
//! type, keeps a list of them here, and the load ring registers into the list
//! at bring-up -- explicitly, from `main.rs`, in the order boot runs, rather
//! than by a link-time table nobody can read the order of.
//!
//! One exception is bound at link time instead, because a registered pointer
//! is an indirect call and the path it sits on counts nanoseconds: the fast
//! path for `channel_write_read` and its filter look (T2), declared in
//! `trap.rs` and defined in `main.rs`. They are not a table either. Each is
//! named in the manifest's `composition_root.hooks`, defined once, in
//! `main.rs`, as one forwarding call, and `check-item-boundary.py` fails any
//! other such declaration (`docs/certification/ITEM.md` §2).
//!
//! A list is a handful of [`Once`] cells, not a locked vector. It is written a
//! few times at bring-up and read afterwards from paths that must not take a
//! [`crate::sync::SpinLock`] -- a power-off, and board support that waits on
//! the timer while it prepares a device -- so a reader takes nothing, and
//! nothing is allocated when the machine is stopping.

use ferrix_sync::Once;

/// A list of at most `N` registrations of `T`, in the order they were made.
pub(crate) struct Hooks<T: 'static, const N: usize> {
    /// Each registration, filled from the front.
    slots: [Once<&'static T>; N],
}

/// Every slot of a [`Hooks`] was already taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Full;

impl<T: 'static, const N: usize> Hooks<T, N> {
    /// An empty list.
    pub(crate) const fn new() -> Self {
        Hooks {
            slots: [const { Once::new() }; N],
        }
    }

    /// Add `hook` after every registration made before it.
    ///
    /// # Errors
    ///
    /// [`Full`] when `N` registrations have been made already. The bound is
    /// the item's statement of how many it expects, so passing it is a boot
    /// that has changed shape, and the caller says so.
    pub(crate) fn register(&self, hook: &'static T) -> Result<(), Full> {
        for slot in &self.slots {
            let mut taken = false;
            let _ = slot.call_once(|| {
                taken = true;
                hook
            });
            if taken {
                return Ok(());
            }
        }
        Err(Full)
    }

    /// Every registration, in the order it was made.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &'static T> + '_ {
        self.slots.iter().filter_map(|slot| slot.get().copied())
    }

    /// How many registrations have been made.
    pub(crate) fn len(&self) -> usize {
        self.iter().count()
    }
}
