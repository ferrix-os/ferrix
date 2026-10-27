//! The run queue's tree: queued entities in virtual-deadline order, each
//! subtree remembering the smallest virtual runtime inside it.
//!
//! # Why a tree augmented with a minimum
//!
//! The pick EEVDF asks for is "the earliest deadline among the eligible", and
//! eligibility is a condition on the *other* field: an entity is eligible when
//! its virtual runtime is at or behind the queue's virtual time. A tree
//! ordered by deadline answers "earliest" by walking left. Knowing, at every
//! node, the smallest virtual runtime below it answers "is anything down
//! there eligible at all" without looking. Together they make the pick one
//! walk from the root — go left while the left subtree holds something
//! eligible, take this node if it is eligible, otherwise go right — which is
//! O(log n), and the same shape Linux's has had since it sorted its tree by
//! deadline.
//!
//! # Why AVL, and why boxes
//!
//! Balanced, because a run queue's worst case is its common case: a thousand
//! tasks of equal weight asking for equal slices arrive in deadline order,
//! which is exactly the sequence that turns an unbalanced tree into a list.
//! AVL rather than red-black because its invariant is one small number per
//! node, and the check that recomputes it is a dozen lines — a tree nobody can
//! check is not a tree to trust with every scheduling decision on the machine.
//!
//! Nodes are boxed and owned by their parent rather than kept as indices into
//! a vector, so that no lookup here can fail. The crate forbids `unsafe` and
//! the workspace forbids indexing, and an arena would have turned every
//! rotation into a chain of `Option`s to save one allocation per node.
//!
//! # Nodes are lent, not allocated
//!
//! A queue is changed on every scheduling decision and every wake-up, some of
//! them from interrupt handlers, and none of those can be told that memory ran
//! out (the kernel's finding F-23). So the tree never allocates: each entity
//! arrives in a [`Slot`] its owner allocated when it could still fail -- a
//! kernel task, as it is made -- and leaves in the same slot. A node taken out
//! of the tree to become the running entity, and put back, is the same node.

use alloc::boxed::Box;
use core::cmp::Ordering;

use crate::Entity;

/// `a` is strictly before `b` in virtual time, which is allowed to wrap: two
/// values are compared by the sign of their difference, which is right for
/// every pair closer than half the counter's range.
pub(crate) const fn before(a: u64, b: u64) -> bool {
    (a.wrapping_sub(b) as i64) < 0
}

/// The earlier of two virtual times.
const fn earlier(a: u64, b: u64) -> u64 {
    if before(b, a) { b } else { a }
}

/// Where an entity sorts: by deadline, and among equal deadlines by
/// identifier, so that no two entities compare equal and the order — and with
/// it every pick — is deterministic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Key {
    /// The entity's virtual deadline.
    pub(crate) deadline: u64,
    /// The entity's identifier.
    pub(crate) id: u64,
}

impl Key {
    /// This key's place relative to `other`.
    fn order(self, other: Key) -> Ordering {
        if self.deadline == other.deadline {
            self.id.cmp(&other.id)
        } else if before(self.deadline, other.deadline) {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }
}

/// A subtree, or nothing.
type Link<T> = Option<Box<Node<T>>>;

/// The memory one entity occupies on a queue, lent by whoever owns the
/// entity and handed back when it leaves.
///
/// Allocated with [`Slot::new`], which is where running out of memory is
/// reported; nothing afterwards allocates. A slot holds no payload while it
/// is not on a queue.
#[derive(Debug)]
pub struct Slot<T>(pub(crate) Box<Node<T>>);

impl<T> Slot<T> {
    /// A fresh slot.
    ///
    /// # Errors
    ///
    /// [`ferrix_fallible::AllocError`] when there is no memory for one.
    pub fn new() -> Result<Slot<T>, ferrix_fallible::AllocError> {
        ferrix_fallible::try_box(Node {
            entity: Entity {
                id: 0,
                weight: 0,
                vruntime: 0,
                deadline: 0,
                sum_exec: 0,
                payload: None,
            },
            left: None,
            right: None,
            height: 1,
            min_vruntime: 0,
        })
        .map(Slot)
    }

    /// The node itself, for a holder that keeps it as a raw pointer while
    /// no queue holds it (the kernel's task, whose slot cells are atomics:
    /// `docs/OPAQUE-KERNEL.md` §9.7). Nothing is allocated or freed.
    #[must_use]
    pub fn into_box(self) -> Box<Node<T>> {
        self.0
    }

    /// A slot again, from the node [`Slot::into_box`] gave.
    #[must_use]
    pub fn from_box(node: Box<Node<T>>) -> Slot<T> {
        Slot(node)
    }
}

/// One queued entity, and the facts about the subtree below it. Opaque
/// outside this crate: a [`Slot`]'s storage, which a holder may keep boxed.
#[derive(Debug)]
pub struct Node<T> {
    /// The entity.
    pub(crate) entity: Entity<T>,
    /// Everything with an earlier key.
    left: Link<T>,
    /// Everything with a later one.
    right: Link<T>,
    /// Height of the subtree rooted here: one for a leaf.
    height: u8,
    /// The smallest virtual runtime in the subtree rooted here.
    min_vruntime: u64,
}

impl<T> Node<T> {
    /// Where this node sorts.
    const fn key(&self) -> Key {
        self.entity.key()
    }

    /// Recompute the height and the minimum from the children, which must
    /// already be right.
    fn update(&mut self) {
        self.height = 1 + height(&self.left).max(height(&self.right));
        let mut min = self.entity.vruntime;
        for child in [&self.left, &self.right].into_iter().flatten() {
            min = earlier(min, child.min_vruntime);
        }
        self.min_vruntime = min;
    }

    /// Left height less right height.
    fn balance(&self) -> i16 {
        i16::from(height(&self.left)) - i16::from(height(&self.right))
    }
}

/// The height of a subtree: zero for none.
fn height<T>(link: &Link<T>) -> u8 {
    link.as_ref().map_or(0, |node| node.height)
}

/// Rotate `node`'s left child up into its place.
fn rotate_right<T>(mut node: Box<Node<T>>) -> Box<Node<T>> {
    let Some(mut pivot) = node.left.take() else {
        return node;
    };
    node.left = pivot.right.take();
    node.update();
    pivot.right = Some(node);
    pivot.update();
    pivot
}

/// Rotate `node`'s right child up into its place.
fn rotate_left<T>(mut node: Box<Node<T>>) -> Box<Node<T>> {
    let Some(mut pivot) = node.right.take() else {
        return node;
    };
    node.right = pivot.left.take();
    node.update();
    pivot.left = Some(node);
    pivot.update();
    pivot
}

/// Restore the AVL condition at `node`, whose children are balanced and at
/// most two apart in height, and bring its derived fields up to date.
fn rebalance<T>(mut node: Box<Node<T>>) -> Box<Node<T>> {
    node.update();
    let balance = node.balance();
    if balance > 1 {
        if node.left.as_ref().is_some_and(|left| left.balance() < 0) {
            node.left = node.left.take().map(rotate_left);
        }
        return rotate_right(node);
    }
    if balance < -1 {
        if node.right.as_ref().is_some_and(|right| right.balance() > 0) {
            node.right = node.right.take().map(rotate_right);
        }
        return rotate_left(node);
    }
    node
}

/// Insert the detached node `new` into the subtree `link`, returning the new
/// subtree.
fn insert<T>(link: Link<T>, mut new: Box<Node<T>>) -> Box<Node<T>> {
    let Some(mut node) = link else {
        new.left = None;
        new.right = None;
        new.update();
        return new;
    };
    if new.key().order(node.key()) == Ordering::Less {
        node.left = Some(insert(node.left.take(), new));
    } else {
        node.right = Some(insert(node.right.take(), new));
    }
    rebalance(node)
}

/// Detach the leftmost node of `node`'s subtree, returning what is left and
/// the node itself.
fn remove_min<T>(mut node: Box<Node<T>>) -> (Link<T>, Box<Node<T>>) {
    match node.left.take() {
        None => {
            let rest = node.right.take();
            (rest, node)
        }
        Some(left) => {
            let (rest, min) = remove_min(left);
            node.left = rest;
            (Some(rebalance(node)), min)
        }
    }
}

/// Remove the entity with `key` from the subtree `link`, returning the new
/// subtree and the entity's node, detached, if it was there.
///
/// A node with two children is replaced by its successor *node*, relinked,
/// rather than by a copy of the successor's entity: the entities never move,
/// which is what lets a caller hold a key across other operations, and what
/// lets every entity keep the node it arrived in.
fn remove<T>(link: Link<T>, key: Key) -> (Link<T>, Option<Box<Node<T>>>) {
    let Some(mut node) = link else {
        return (None, None);
    };
    match key.order(node.key()) {
        Ordering::Less => {
            let (left, found) = remove(node.left.take(), key);
            node.left = left;
            (Some(rebalance(node)), found)
        }
        Ordering::Greater => {
            let (right, found) = remove(node.right.take(), key);
            node.right = right;
            (Some(rebalance(node)), found)
        }
        Ordering::Equal => {
            let rest = match (node.left.take(), node.right.take()) {
                (None, right) => right,
                (left, None) => left,
                (Some(left), Some(right)) => {
                    let (right, mut successor) = remove_min(right);
                    successor.left = Some(left);
                    successor.right = right;
                    Some(rebalance(successor))
                }
            };
            (rest, Some(node))
        }
    }
}

/// Visit every entity in the subtree `link`, in key order.
fn walk<T, F: FnMut(&Entity<T>)>(link: &Link<T>, visit: &mut F) {
    if let Some(node) = link {
        walk(&node.left, visit);
        visit(&node.entity);
        walk(&node.right, visit);
    }
}

/// The key of the entity named `id` in the subtree `link`: a walk of the
/// whole subtree, since the tree is ordered by deadline and not by name.
fn find<T>(link: &Link<T>, id: u64) -> Option<Key> {
    let node = link.as_ref()?;
    if node.entity.id == id {
        return Some(node.key());
    }
    find(&node.left, id).or_else(|| find(&node.right, id))
}

/// The latest key in the subtree `link` whose entity satisfies `wanted`.
fn last_where<T, F: Fn(&Entity<T>) -> bool>(link: &Link<T>, wanted: &F) -> Option<Key> {
    let node = link.as_ref()?;
    last_where(&node.right, wanted)
        .or_else(|| wanted(&node.entity).then_some(node.key()))
        .or_else(|| last_where(&node.left, wanted))
}

/// What a subtree proved about itself: its height, its smallest virtual
/// runtime, and how many entities it holds.
type Proof = (u8, Option<u64>, usize);

/// Check the subtree `link`, whose keys must all lie strictly between `low`
/// and `high`.
fn check_node<T>(
    link: &Link<T>,
    low: Option<Key>,
    high: Option<Key>,
) -> Result<Proof, &'static str> {
    let Some(node) = link else {
        return Ok((0, None, 0));
    };
    let key = node.key();
    if low.is_some_and(|low| key.order(low) != Ordering::Greater)
        || high.is_some_and(|high| key.order(high) != Ordering::Less)
    {
        return Err("the tree is out of deadline order");
    }

    let (left_height, left_min, left_count) = check_node(&node.left, low, Some(key))?;
    let (right_height, right_min, right_count) = check_node(&node.right, Some(key), high)?;

    if node.height != 1 + left_height.max(right_height) {
        return Err("a node's height is stale");
    }
    if (i16::from(left_height) - i16::from(right_height)).abs() > 1 {
        return Err("the tree is out of balance");
    }
    let mut min = node.entity.vruntime;
    for child in [left_min, right_min].into_iter().flatten() {
        min = earlier(min, child);
    }
    if min != node.min_vruntime {
        return Err("a subtree's smallest virtual runtime is stale");
    }
    Ok((node.height, Some(min), left_count + right_count + 1))
}

/// The queued entities.
#[derive(Debug)]
pub(crate) struct Tree<T> {
    /// The root, if anything is queued.
    root: Link<T>,
    /// How many entities are in it.
    len: usize,
}

impl<T> Tree<T> {
    /// An empty tree.
    pub(crate) const fn new() -> Tree<T> {
        Tree { root: None, len: 0 }
    }

    /// How many entities are in it.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Whether it is empty.
    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add the entity in `node`, which is detached. Its key must not already
    /// be in the tree, which the run queue's refusal of a second entity with
    /// the same name guarantees. Allocates nothing.
    pub(crate) fn insert(&mut self, node: Box<Node<T>>) {
        self.root = Some(insert(self.root.take(), node));
        self.len += 1;
    }

    /// Take out the entity with `key`, in its node.
    pub(crate) fn remove(&mut self, key: Key) -> Option<Box<Node<T>>> {
        let (root, found) = remove(self.root.take(), key);
        self.root = root;
        if found.is_some() {
            self.len -= 1;
        }
        found
    }

    /// Take out the entity with the earliest key, in its node.
    pub(crate) fn pop_first(&mut self) -> Option<Box<Node<T>>> {
        let key = self.first()?.key();
        self.remove(key)
    }

    /// The key of the entity named `id`: a walk, for the rare callers that
    /// know only the name.
    pub(crate) fn find(&self, id: u64) -> Option<Key> {
        find(&self.root, id)
    }

    /// The entity with `key`.
    pub(crate) fn get(&self, key: Key) -> Option<&Entity<T>> {
        let mut link = &self.root;
        while let Some(node) = link {
            link = match key.order(node.key()) {
                Ordering::Less => &node.left,
                Ordering::Greater => &node.right,
                Ordering::Equal => return Some(&node.entity),
            };
        }
        None
    }

    /// The entity with the earliest key.
    pub(crate) fn first(&self) -> Option<&Entity<T>> {
        let mut node = self.root.as_ref()?;
        while let Some(left) = node.left.as_ref() {
            node = left;
        }
        Some(&node.entity)
    }

    /// The earliest-deadline entity whose virtual runtime satisfies
    /// `eligible`, which must be monotone: true of every runtime at or before
    /// one it is true of. Eligibility is — it is "at or behind the average".
    pub(crate) fn pick(&self, eligible: impl Fn(u64) -> bool) -> Option<&Entity<T>> {
        let mut link = &self.root;
        while let Some(node) = link {
            // Anything eligible to the left has an earlier deadline than
            // anything here or to the right, so it wins if it exists.
            if node
                .left
                .as_ref()
                .is_some_and(|left| eligible(left.min_vruntime))
            {
                link = &node.left;
                continue;
            }
            if eligible(node.entity.vruntime) {
                return Some(&node.entity);
            }
            link = &node.right;
        }
        None
    }

    /// Visit every entity, in deadline order.
    pub(crate) fn for_each(&self, mut visit: impl FnMut(&Entity<T>)) {
        walk(&self.root, &mut visit);
    }

    /// The latest-deadline entity satisfying `wanted`.
    pub(crate) fn last_where(&self, wanted: impl Fn(&Entity<T>) -> bool) -> Option<Key> {
        last_where(&self.root, &wanted)
    }

    /// Check order, balance and every node's derived fields, returning how
    /// many entities the tree holds.
    pub(crate) fn check(&self) -> Result<usize, &'static str> {
        let (_, _, count) = check_node(&self.root, None, None)?;
        Ok(count)
    }
}
