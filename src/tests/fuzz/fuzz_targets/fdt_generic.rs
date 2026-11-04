//! Fuzz generic device-tree publication's parsing (`docs/DRIVER-MODEL.md`
//! §3, D9): the lineage walk, `ranges` translation, references and the
//! identity record the kernel writes for every node it publishes.
//!
//! The tree is firmware's, and the kernel walks every node of it at stage 10
//! to publish the ones it does not use. Beyond not panicking:
//!
//! 1. **The lineage walk visits each node once, with exactly its depth of
//!    ancestors**, and never a node deeper than `LINEAGE_DEPTH`.
//! 2. **A node's references stop at the bound**: at most `MAX_REFERENCES`
//!    plus the one that says it was cut, each with at most
//!    `MAX_SPECIFIER_CELLS` cells.
//! 3. **Every identity record reads back**: it parses, is no longer than
//!    `RECORD_BYTES` and the buffer, names the node it was written for, its
//!    path ends in the node's name unless it says it was cut, and it holds
//!    no more references than were read.

#![no_main]

use core::ops::ControlFlow;

use ferrix_fdt::Fdt;
use ferrix_fdt::generic::{LINEAGE_DEPTH, MAX_REFERENCES, MAX_SPECIFIER_CELLS};
use ferrix_fdt::identity::{self, FLAG_CUT, RECORD_BYTES, Record};
use libfuzzer_sys::fuzz_target;

/// The most nodes the per-node checks look at: references resolve phandles
/// by a walk of the tree each, so a large tree is cut to its first nodes.
const NODES: usize = 64;

fuzz_target!(|data: &[u8]| {
    let Ok(fdt) = Fdt::parse(data) else {
        return;
    };
    let total = fdt.nodes().filter(|node| node.depth < LINEAGE_DEPTH).count();
    let mut visited = 0;
    let mut buffer = vec![0u8; RECORD_BYTES];
    fdt.for_each_lineage(|node, ancestors| {
        visited += 1;
        assert!(node.depth < LINEAGE_DEPTH);
        assert_eq!(ancestors.len(), node.depth);
        for (level, ancestor) in ancestors.iter().enumerate() {
            assert_eq!(ancestor.depth, level);
        }
        if visited > NODES {
            return ControlFlow::Continue(());
        }

        let mut references = 0;
        fdt.for_each_reference(&node, |reference| {
            references += 1;
            assert!(references <= MAX_REFERENCES + 1);
            assert!(reference.cells.len() <= MAX_SPECIFIER_CELLS * 4);
            assert_eq!(reference.cells.len() % 4, 0);
            ControlFlow::Continue(())
        });

        for region in node.reg().take(4) {
            let _ = fdt.translate(&node, ancestors, region.address);
        }
        let _ = Fdt::memory_mapped(ancestors);
        let _ = fdt.interrupt_parent(&node, ancestors);

        // Each record in a buffer of a size the input chooses, so short ones
        // are cut as well as whole ones written.
        let size = identity::HEADER_BYTES + (node.id() as usize % RECORD_BYTES);
        let size = size.min(RECORD_BYTES);
        let encoded = identity::encode(&fdt, &node, ancestors, 0, &mut buffer[..size])
            .expect("a buffer of a header or more takes a record");
        assert!(encoded.length <= size);
        let record = Record::parse(&buffer[..size]).expect("a record written reads back");
        assert_eq!(record.node(), node.id());
        assert_eq!(record.bytes().len(), encoded.length);
        assert!(record.references().count() <= encoded.references);
        assert!(encoded.references <= references);
        if record.flags() & FLAG_CUT == 0 && node.depth > 0 {
            assert!(record.path().ends_with(node.name));
        }
        for reference in record.references() {
            assert!(reference.cells().count() <= MAX_SPECIFIER_CELLS);
        }
        for child in record.subtree() {
            assert!(child.depth >= 1);
            let _ = child.properties().count();
        }
        let _ = record.compatible().count();
        ControlFlow::Continue(())
    });
    assert_eq!(visited, total);
});
