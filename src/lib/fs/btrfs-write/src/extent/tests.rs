use alloc::vec;

use super::*;

#[test]
fn a_skinny_tree_block_item_round_trips() {
    // The bytes `mkfs.btrfs` wrote for the root tree's block in the blank
    // image: refs 1, generation 8, TREE_BLOCK, one TREE_BLOCK_REF to root 1.
    let mut data = vec![0u8; 33];
    data[0] = 1;
    data[8] = 8;
    data[16] = 2;
    data[24] = TREE_BLOCK_REF_KEY;
    data[25] = 1;
    let key = BtrfsKey::new(30_572_544, METADATA_ITEM_KEY, 0);
    let (record, stored) = ExtentRecord::parse(&key, &data).unwrap();
    assert_eq!(stored, 1);
    record.check_total(stored).unwrap();
    assert_eq!(record.to_items(&key, 249).unwrap(), vec![(key, data)]);
}

#[test]
fn inline_refs_come_out_in_the_tree_checkers_order() {
    let mut record = ExtentRecord::new(9, EXTENT_FLAG_DATA);
    for objectid in 256..262 {
        record
            .apply(
                Backref::Data {
                    root: 5,
                    objectid,
                    offset: 0,
                },
                1,
            )
            .unwrap();
    }
    record
        .apply(Backref::SharedData { parent: 1 << 20 }, 2)
        .unwrap();
    let key = BtrfsKey::new(1 << 30, EXTENT_ITEM_KEY, 4096);
    let items = record.to_items(&key, 1000).unwrap();
    assert_eq!(items.len(), 1, "everything fits inline");
    let (parsed, stored) = ExtentRecord::parse(&key, &items[0].1).unwrap();
    parsed.check_total(stored).unwrap();
    assert_eq!(parsed, record);
    // Walk the inline refs as `check_extent_item` does.
    let data = &items[0].1;
    let (mut at, mut last_type, mut last_seq) = (24usize, 0u8, u64::MAX);
    while at < data.len() {
        let kind = data[at];
        let seq = if kind == EXTENT_DATA_REF_KEY {
            let root = u64::from_le_bytes(data[at + 1..at + 9].try_into().unwrap());
            let objectid = u64::from_le_bytes(data[at + 9..at + 17].try_into().unwrap());
            let offset = u64::from_le_bytes(data[at + 17..at + 25].try_into().unwrap());
            hash_extent_data_ref(root, objectid, offset)
        } else {
            u64::from_le_bytes(data[at + 1..at + 9].try_into().unwrap())
        };
        assert!(kind >= last_type, "types ascend");
        if kind > last_type {
            last_seq = u64::MAX;
        }
        assert!(seq <= last_seq, "sequences descend within a type");
        last_type = kind;
        last_seq = seq;
        at += if kind == EXTENT_DATA_REF_KEY { 29 } else { 13 };
    }
}

#[test]
fn refs_past_the_inline_limit_become_keyed_items() {
    let mut record = ExtentRecord::new(9, EXTENT_FLAG_DATA);
    for objectid in 256..300 {
        record
            .apply(
                Backref::Data {
                    root: 5,
                    objectid,
                    offset: 4096,
                },
                3,
            )
            .unwrap();
    }
    let key = BtrfsKey::new(1 << 30, EXTENT_ITEM_KEY, 8192);
    let items = record.to_items(&key, 249).unwrap();
    assert!(items.len() > 1);
    assert!(items[0].1.len() <= 249);
    let (mut parsed, stored) = ExtentRecord::parse(&items[0].0, &items[0].1).unwrap();
    for (k, d) in &items[1..] {
        assert_eq!(k.item_type, EXTENT_DATA_REF_KEY);
        parsed.add_keyed(k, d).unwrap();
    }
    parsed.check_total(stored).unwrap();
    assert_eq!(parsed.total(), 44 * 3);
    assert_eq!(parsed, record);
}

/// Verifies: L.btrfs.17
#[test]
fn a_count_cannot_go_below_zero() {
    let mut record = ExtentRecord::new(9, EXTENT_FLAG_TREE_BLOCK);
    assert!(record.apply(Backref::Tree { root: 5 }, -1).is_err());
    record.apply(Backref::Tree { root: 5 }, 1).unwrap();
    assert!(
        record.apply(Backref::Tree { root: 5 }, 1).is_err(),
        "one owner, one reference"
    );
}

#[test]
fn the_data_ref_hash_matches_btrfs_progs() {
    // Printed by `btrfs inspect-internal dump-tree -t extent` for the inline
    // data references of the `none` fixture.
    assert_eq!(hash_extent_data_ref(5, 261, 0), 0x0dfb_591f_6e09_3901);
    assert_eq!(
        hash_extent_data_ref(5, 261, 1_048_576),
        0x0dfb_591f_ae05_093f
    );
    assert_eq!(hash_extent_data_ref(5, 262, 0), 0x0dfb_591f_7df1_59f2);
}
