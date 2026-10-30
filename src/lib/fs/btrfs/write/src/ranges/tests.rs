use super::*;

/// Verifies: L.btrfs.18
#[test]
fn inserts_merge_with_touching_runs() {
    let mut set = RangeSet::new();
    assert!(set.insert(10, 10));
    assert!(set.insert(30, 10));
    assert!(set.insert(20, 10));
    assert_eq!(set.iter().collect::<alloc::vec::Vec<_>>(), [(10, 30)]);
    assert!(!set.insert(15, 1), "a byte already present is refused");
    assert_eq!(set.total(), 30);
}

/// Verifies: L.btrfs.95
#[test]
fn removes_split_the_run_they_are_in() {
    let mut set = RangeSet::new();
    assert!(set.insert(0, 100));
    assert!(set.remove(40, 20));
    assert_eq!(
        set.iter().collect::<alloc::vec::Vec<_>>(),
        [(0, 40), (60, 40)]
    );
    assert!(!set.remove(30, 20), "a range across a gap is refused");
    assert!(set.remove(0, 40));
    assert!(set.contains(60, 40));
    assert!(!set.overlaps(0, 60));
}

/// Verifies: L.btrfs.96
#[test]
fn first_fit_aligns_and_avoids_the_boundary() {
    let mut set = RangeSet::new();
    assert!(set.insert(4096, 1 << 20));
    // 16 KiB blocks at 16 KiB alignment, never across 64 KiB.
    assert_eq!(set.first_fit(16384, 16384, 65536, 0), Some(16384));
    assert_eq!(set.first_fit(16384, 16384, 65536, 60000), Some(65536));
    // Wraps to the start when nothing fits after the cursor.
    assert_eq!(set.first_fit(16384, 16384, 65536, 2 << 20), Some(16384));
    assert_eq!(set.first_fit(2 << 20, 4096, 0, 0), None);
}

/// Verifies: L.btrfs.97
#[test]
fn first_prefix_takes_what_the_run_has() {
    let mut set = RangeSet::new();
    assert!(set.insert(0, 8192));
    assert!(set.insert(65536, 1 << 20));
    assert_eq!(set.first_prefix(1 << 30, 4096, 4096, 0), Some((0, 8192)));
    assert_eq!(
        set.first_prefix(1 << 30, 16384, 4096, 0),
        Some((65536, 1 << 20))
    );
}

/// Verifies: L.btrfs.98
#[test]
fn add_unions_overlapping_and_touching_runs() {
    let mut set = RangeSet::new();
    set.add(10, 10);
    set.add(40, 10);
    set.add(15, 10);
    assert_eq!(
        set.iter().collect::<alloc::vec::Vec<_>>(),
        [(10, 15), (40, 10)]
    );
    set.add(25, 15);
    assert_eq!(set.iter().collect::<alloc::vec::Vec<_>>(), [(10, 40)]);
    set.add(12, 3);
    assert_eq!(set.total(), 40, "a range already held changes nothing");
}

/// Verifies: L.btrfs.99
#[test]
fn extract_takes_exactly_the_shared_bytes() {
    let mut set = RangeSet::new();
    assert!(set.insert(0, 100));
    assert!(set.insert(200, 100));
    let mut other = RangeSet::new();
    assert!(other.insert(90, 120));
    assert!(other.insert(500, 10));
    let taken = set.extract(&other);
    assert_eq!(
        taken.iter().collect::<alloc::vec::Vec<_>>(),
        [(90, 10), (200, 10)]
    );
    assert_eq!(
        set.iter().collect::<alloc::vec::Vec<_>>(),
        [(0, 90), (210, 90)]
    );
    assert!(set.extract(&other).is_empty(), "nothing shared is left");
}

/// `holds_at_least` answers what comparing the whole total answers, for
/// every amount around the total, through random edits.
#[test]
fn holds_at_least_agrees_with_the_total() {
    let mut set = RangeSet::new();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |below: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % below
    };
    for _ in 0..2_000 {
        let start = next(4_096) * 16;
        let len = (next(64) + 1) * 16;
        if next(3) == 0 {
            let _ = set.remove(start, len);
        } else {
            set.add(start, len);
        }
        let total = set.total();
        for want in [
            0,
            1,
            total.saturating_sub(16),
            total,
            total + 1,
            total + 4096,
        ] {
            assert_eq!(set.holds_at_least(want), total >= want, "{want} of {total}");
        }
    }
}
