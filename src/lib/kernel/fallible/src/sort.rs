//! A sort that neither allocates nor recurses, for a slice whose length a
//! program decides (finding F-112, `L.user.245`).
//!
//! The core library's `sort_unstable` is a quicksort that recurses on one
//! side of each partition, about `2 log2 n` levels at worst, and its frames
//! are large in the kernel's build: on develop 1118eb194 (release, x86-64) a
//! sort of 40,000 `(u64, bool)` took 10,440 bytes of a task's kernel stack.
//! A process's exit unmaps its whole address space at the bottom of the
//! system call return path and sorted one entry per region there, so a
//! process of tens of thousands of regions ran its stack into the guard page
//! (2026-11-12). [`sort_flat_by_key`] is a heapsort: `O(n log n)`
//! comparisons, no allocation, and the same frame at any length.

/// Sort `items` by `key`, in place: a heapsort, with no allocation and a
/// stack that does not grow with `items.len()`. Not stable: items with equal
/// keys end next to each other in no promised order.
pub fn sort_flat_by_key<T, K: Ord>(items: &mut [T], key: impl Fn(&T) -> K) {
    let len = items.len();
    // A max-heap, built from the last parent down.
    let mut parent = len / 2;
    while parent > 0 {
        parent -= 1;
        sift_down(items, parent, len, &key);
    }
    // The largest to the end, the heap one shorter, again.
    let mut end = len;
    while end > 1 {
        end -= 1;
        items.swap(0, end);
        sift_down(items, 0, end, &key);
    }
}

/// Move `items[root]` down the heap `items[..end]` until no child's key is
/// larger than its own: [`sort_flat_by_key`]'s step, a loop. `root` is below
/// `end`, which is at most `items.len()`. A child's index is computed with
/// checked arithmetic: for an item of non-zero size the length is at most
/// `isize::MAX` and `2 * root + 2` fits, but a slice of zero-sized items may
/// be `usize::MAX` long, and there an index that does not fit is past `end`,
/// a leaf, and the step ends.
fn sift_down<T, K: Ord>(items: &mut [T], mut root: usize, end: usize, key: &impl Fn(&T) -> K) {
    loop {
        let Some(left) = root.checked_mul(2).and_then(|twice| twice.checked_add(1)) else {
            return;
        };
        if left >= end {
            return;
        }
        let right = left + 1;
        let child = if right < end && larger(items, right, left, key) {
            right
        } else {
            left
        };
        if !larger(items, child, root, key) {
            return;
        }
        items.swap(root, child);
        root = child;
    }
}

/// Whether `items[one]`'s key is larger than `items[other]`'s. Both indexes
/// are in range wherever [`sift_down`] asks; an index out of range would
/// answer no, which only stops the step early.
fn larger<T, K: Ord>(items: &[T], one: usize, other: usize, key: &impl Fn(&T) -> K) -> bool {
    match (items.get(one), items.get(other)) {
        (Some(one), Some(other)) => key(one) > key(other),
        _ => false,
    }
}
