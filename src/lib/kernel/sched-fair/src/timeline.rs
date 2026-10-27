//! Entities ordered by when they are due: a kernel's sleepers.
//!
//! The run queue's tree, keyed by the instant an entity is due instead of by
//! its virtual deadline, and holding each entity in the [`Slot`] its owner
//! lent -- so that filing a sleeper and waking one, which the kernel does
//! from its scheduler and its timer interrupt, allocates nothing (the
//! kernel's finding F-23).

use crate::Entity;
use crate::tree::{Key, Slot, Tree};

/// Entities by the instant they are due, earliest first; among equal
/// instants, by identifier.
#[derive(Debug)]
pub struct Timeline<T> {
    /// The entities, their instants in the deadline field.
    tree: Tree<T>,
}

impl<T> Default for Timeline<T> {
    fn default() -> Self {
        Timeline::new()
    }
}

impl<T> Timeline<T> {
    /// No entities.
    #[must_use]
    pub const fn new() -> Timeline<T> {
        Timeline { tree: Tree::new() }
    }

    /// How many entities are filed.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.tree.len()
    }

    /// Whether none are.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// File entity `id` with `payload`, due at `at`, in `slot`. Allocates
    /// nothing. An entity is filed once; filing the same one again files it
    /// twice.
    pub fn insert(&mut self, at: u64, id: u64, payload: T, slot: Slot<T>) {
        let Slot(mut node) = slot;
        node.entity = Entity {
            id,
            weight: 0,
            vruntime: 0,
            deadline: at,
            sum_exec: 0,
            payload: Some(payload),
        };
        self.tree.insert(node);
    }

    /// When the earliest entity is due.
    #[must_use]
    pub fn first_due(&self) -> Option<u64> {
        self.tree.first().map(|entity| entity.deadline)
    }

    /// Take out the earliest entity if it is due by `now`: its identifier,
    /// its payload and its slot.
    pub fn pop_due(&mut self, now: u64) -> Option<(u64, T, Slot<T>)> {
        let first = self.tree.first()?;
        if first.deadline > now {
            return None;
        }
        let key = first.key();
        self.take(key)
    }

    /// Take out entity `id`, wherever it is filed: a walk, since the order
    /// is by instant and not by name.
    pub fn remove(&mut self, id: u64) -> Option<(T, Slot<T>)> {
        let key = self.tree.find(id)?;
        self.take(key).map(|(_, payload, slot)| (payload, slot))
    }

    /// Take out the entity at `key`, emptying its slot.
    fn take(&mut self, key: Key) -> Option<(u64, T, Slot<T>)> {
        let mut node = self.tree.remove(key)?;
        let payload = node.entity.payload.take()?;
        Some((key.id, payload, Slot(node)))
    }

    /// Check the order and balance of what is filed, returning how many
    /// entities that is.
    ///
    /// # Errors
    ///
    /// The first violation found, as a sentence.
    pub fn check(&self) -> Result<usize, &'static str> {
        self.tree.check()
    }
}
