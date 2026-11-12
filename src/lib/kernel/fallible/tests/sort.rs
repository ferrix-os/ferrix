//! ferrix-fallible's sort without recursion (`sort_flat_by_key`, finding
//! F-112, `L.user.245`): in order by key and a permutation of its input, at
//! lengths 0 to 60,000, ascending, descending, all equal and random with
//! duplicates, for a pair and for a tuple key. A file of its own, so that
//! ring 0's count of the library's lines holds the sort and not its tests
//! (consultant ledger 9337).

use ferrix_fallible::sort_flat_by_key;
// The crate's own dependency, which this test does not name.
use ferrix_sync as _;

/// A 64-bit linear congruential generator: random enough for inputs,
/// and the same run to run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// An entry sorted by a tuple key, as `room_to_count_out`'s `Counting`
/// is by its view order; `tag` tells two entries of one key apart.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    object: usize,
    id: u64,
    kind: u8,
    tag: usize,
}

impl Entry {
    fn order(&self) -> (usize, u64, u8) {
        (self.object, self.id, self.kind)
    }
}

/// The lengths every shape is tried at.
const LENGTHS: [usize; 6] = [0, 1, 2, 3, 1_000, 60_000];

/// Each input shape at `len`: ascending, descending, all equal, and
/// random with duplicates (values drawn from a range a tenth of `len`).
fn shapes(len: usize, seed: u64) -> Vec<Vec<u64>> {
    let mut random = Lcg(seed);
    let range = (len as u64 / 10).max(1);
    std::vec![
        (0..len as u64).collect(),
        (0..len as u64).rev().collect(),
        std::vec![7; len],
        (0..len).map(|_| random.next() % range).collect(),
    ]
}

/// Sorted by key, and a permutation of the input: the same multiset as
/// core's sort makes of it, compared whole.
fn check<T: Clone + Ord + core::fmt::Debug, K: Ord>(input: &[T], key: impl Fn(&T) -> K + Copy) {
    let mut ours = input.to_vec();
    sort_flat_by_key(&mut ours, key);
    assert!(
        ours.windows(2)
            .all(|pair| matches!(pair, [one, two] if key(one) <= key(two))),
        "not in order by key at length {}",
        input.len()
    );
    let mut theirs = input.to_vec();
    theirs.sort_unstable();
    let mut permuted = ours;
    permuted.sort_unstable();
    assert_eq!(permuted, theirs, "not a permutation of the input");
}

/// Verifies: L.user.245
#[test]
fn sorts_pairs_of_every_shape_and_length() {
    for len in LENGTHS {
        for values in shapes(len, 1) {
            let pairs: Vec<(u64, bool)> = values
                .iter()
                .map(|&value| (value, value % 3 == 0))
                .collect();
            check(&pairs, |&pair| pair);
        }
    }
}

/// Verifies: L.user.245
#[test]
fn sorts_entries_by_a_tuple_key() {
    for len in LENGTHS {
        for values in shapes(len, 2) {
            let entries: Vec<Entry> = values
                .iter()
                .enumerate()
                .map(|(tag, &value)| Entry {
                    object: (value % 5) as usize,
                    id: value / 5,
                    kind: (value % 2) as u8,
                    tag,
                })
                .collect();
            let mut ours = entries.clone();
            sort_flat_by_key(&mut ours, Entry::order);
            assert!(
                ours.windows(2)
                    .all(|pair| matches!(pair, [one, two] if one.order() <= two.order())),
                "entries not in order at length {len}"
            );
            let mut tags: Vec<usize> = ours.iter().map(|entry| entry.tag).collect();
            tags.sort_unstable();
            assert_eq!(tags, (0..len).collect::<Vec<_>>(), "not a permutation");
        }
    }
}
