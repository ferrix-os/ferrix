//! Host tests of generic publication's parsing: lineage, `ranges`
//! translation, references and the identity record (`docs/DRIVER-MODEL.md`
//! §3, D9).

#![allow(
    unused_results,
    reason = "the writer's methods chain, and a test builds a tree statement by statement"
)]

extern crate std;

use core::ops::ControlFlow;
use std::string::String;
use std::vec;
use std::vec::Vec;

use crate::Fdt;
use crate::generic::{MAX_REFERENCES, ReferenceKind};
use crate::identity::{
    self, FLAG_CUT, FLAG_KERNEL_DEPENDS, FLAG_MISSING_REFERENCE, RECORD_BYTES, REFERENCE_DISABLED,
    REFERENCE_MISSING, Record,
};
use crate::write::Writer;

/// A tree shaped like QEMU `virt`'s, with a bus of its own, a pin controller
/// and the references every kind of node makes.
fn tree() -> Vec<u8> {
    let mut structs = vec![0u8; 8192];
    let mut strings = vec![0u8; 2048];
    let mut out = vec![0u8; 16384];
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("")
        .cells("#address-cells", &[2])
        .cells("#size-cells", &[2])
        .cells("interrupt-parent", &[1]);
    w.begin("chosen")
        .strings("stdout-path", &["/pl011@9000000"])
        .end();
    w.begin("intc@8000000")
        .strings("compatible", &["arm,gic-v3"])
        .cells("#interrupt-cells", &[3])
        .property("interrupt-controller", &[])
        .cells("reg", &[0, 0x800_0000, 0, 0x1_0000])
        .cells("phandle", &[1])
        .end();
    w.begin("apb-pclk")
        .strings("compatible", &["fixed-clock"])
        .cells("#clock-cells", &[0])
        .cells("clocks", &[3])
        .cells("phandle", &[2])
        .end();
    w.begin("oscillator")
        .strings("compatible", &["fixed-clock"])
        .cells("#clock-cells", &[0])
        .cells("phandle", &[3])
        .end();
    w.begin("pl011@9000000")
        .strings("compatible", &["arm,pl011", "arm,primecell"])
        .cells("reg", &[0, 0x900_0000, 0, 0x1000])
        .cells("clocks", &[2, 2])
        .strings("clock-names", &["uartclk", "apb_pclk"])
        .end();
    w.begin("seconds@9010000")
        .strings("compatible", &["ferrix,test-seconds", "arm,primecell"])
        .cells("reg", &[0, 0x901_0000, 0, 0x1000])
        .cells("interrupts", &[0, 2, 4])
        .cells("clocks", &[2])
        .strings("clock-names", &["apb_pclk"])
        .cells("vs-supply", &[99])
        .cells("pinctrl-0", &[5])
        .strings("pinctrl-names", &["default"])
        .end();
    w.begin("off-clock")
        .strings("compatible", &["fixed-clock"])
        .cells("#clock-cells", &[1])
        .strings("status", &["disabled"])
        .cells("phandle", &[4])
        .end();
    w.begin("soc")
        .strings("compatible", &["simple-bus"])
        .cells("#address-cells", &[1])
        .cells("#size-cells", &[1])
        .cells("ranges", &[0, 0, 0x4000_0000, 0x1000_0000]);
    w.begin("pinctrl@1000")
        .strings("compatible", &["vendor,pins"])
        .cells("reg", &[0x1000, 0x400]);
    w.begin("uart-pins")
        .cells("phandle", &[5])
        .cells("pins", &[1, 2])
        .end();
    w.end();
    w.begin("i2c@2000")
        .strings("compatible", &["vendor,i2c"])
        .cells("reg", &[0x2000, 0x400])
        .cells("#address-cells", &[1])
        .cells("#size-cells", &[0])
        .cells("clocks", &[4, 7]);
    w.begin("sensor@48")
        .strings("compatible", &["national,lm75"])
        .cells("reg", &[0x48])
        .end();
    w.end();
    w.end();
    w.end();
    let length = w.finish(&mut out).expect("the tree fits");
    out.truncate(length);
    out
}

/// The node at `path` and its ancestors, found through the lineage walk.
fn lineage<'a>(fdt: &Fdt<'a>, path: &str) -> (crate::Node<'a>, Vec<crate::Node<'a>>) {
    let wanted = fdt.find_node(path).expect("the node is there");
    let mut found = None;
    fdt.for_each_lineage(|node, ancestors| {
        if node.id() == wanted.id() {
            found = Some((node, ancestors.to_vec()));
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    found.expect("the walk visits it")
}

/// Verifies: L.discovery.12
#[test]
fn the_lineage_walk_gives_each_node_its_ancestors() {
    let blob = tree();
    let fdt = Fdt::parse(&blob).expect("parses");
    let (node, ancestors) = lineage(&fdt, "/soc/i2c@2000/sensor@48");
    assert_eq!(node.name, "sensor@48");
    let names: Vec<&str> = ancestors.iter().map(|a| a.name).collect();
    assert_eq!(names, ["", "soc", "i2c@2000"]);
    let mut visited = 0;
    fdt.for_each_lineage(|node, ancestors| {
        assert_eq!(ancestors.len(), node.depth);
        visited += 1;
        ControlFlow::Continue(())
    });
    assert_eq!(visited, fdt.nodes().count());
}

/// Verifies: L.discovery.12
#[test]
fn a_node_too_deep_is_never_visited() {
    let mut structs = vec![0u8; 8192];
    let mut strings = vec![0u8; 256];
    let mut out = vec![0u8; 16384];
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("");
    for _ in 0..crate::generic::LINEAGE_DEPTH + 2 {
        w.begin("bus").property("ranges", &[]);
    }
    for _ in 0..crate::generic::LINEAGE_DEPTH + 2 {
        w.end();
    }
    w.end();
    let length = w.finish(&mut out).expect("fits");
    let fdt = Fdt::parse(&out[..length]).expect("parses");
    let mut deepest = 0;
    fdt.for_each_lineage(|node, _| {
        deepest = deepest.max(node.depth);
        ControlFlow::Continue(())
    });
    assert_eq!(deepest, crate::generic::LINEAGE_DEPTH - 1);
}

/// Verifies: L.discovery.12
#[test]
fn registers_are_translated_through_a_bus_and_not_past_one_without_ranges() {
    let blob = tree();
    let fdt = Fdt::parse(&blob).expect("parses");
    let (node, ancestors) = lineage(&fdt, "/soc/pinctrl@1000");
    assert!(Fdt::memory_mapped(&ancestors));
    let region = node.reg().next().expect("a reg");
    assert_eq!(
        fdt.translate(&node, &ancestors, region.address),
        Some(0x4000_1000)
    );
    // Outside every range of the bus.
    assert_eq!(fdt.translate(&node, &ancestors, 0x2000_0000), None);
    // The sensor sits on an I2C bus, which has no ranges: it is the
    // controller's, not the kernel's to publish.
    let (sensor, ancestors) = lineage(&fdt, "/soc/i2c@2000/sensor@48");
    assert!(!Fdt::memory_mapped(&ancestors));
    assert_eq!(fdt.translate(&sensor, &ancestors, 0x48), None);
    // A root node is the processor's address already.
    let (root_child, ancestors) = lineage(&fdt, "/seconds@9010000");
    assert_eq!(
        fdt.translate(&root_child, &ancestors, 0x901_0000),
        Some(0x901_0000)
    );
}

/// Verifies: L.discovery.9
#[test]
fn references_name_their_provider_and_carry_its_cells_uninterpreted() {
    let blob = tree();
    let fdt = Fdt::parse(&blob).expect("parses");
    let (node, _) = lineage(&fdt, "/seconds@9010000");
    let mut seen = Vec::new();
    fdt.for_each_reference(&node, |reference| {
        seen.push((
            reference.kind,
            String::from(reference.property),
            reference.provider.map(|p| p.name),
            reference.cells.len(),
            reference.missing,
        ));
        ControlFlow::Continue(())
    });
    assert_eq!(
        seen,
        [
            (
                ReferenceKind::Clock,
                "clocks".into(),
                Some("apb-pclk"),
                0,
                false
            ),
            (ReferenceKind::Supply, "vs-supply".into(), None, 0, true),
            // The pin configuration's provider is the controller above it.
            (
                ReferenceKind::PinControl,
                "pinctrl-0".into(),
                Some("pinctrl@1000"),
                0,
                false
            ),
        ]
    );
    let (i2c, _) = lineage(&fdt, "/soc/i2c@2000");
    let mut disabled = Vec::new();
    fdt.for_each_reference(&i2c, |reference| {
        disabled.push((reference.disabled, reference.cells.to_vec()));
        ControlFlow::Continue(())
    });
    assert_eq!(disabled, [(true, vec![0, 0, 0, 7])]);
}

/// Verifies: L.discovery.9, L.discovery.11
#[test]
fn reference_kinds_are_named_by_the_binding_spellings_only() {
    for (name, kind) in [
        ("clocks", Some(ReferenceKind::Clock)),
        ("resets", Some(ReferenceKind::Reset)),
        ("power-domains", Some(ReferenceKind::PowerDomain)),
        ("pinctrl-0", Some(ReferenceKind::PinControl)),
        ("pinctrl-12", Some(ReferenceKind::PinControl)),
        ("pinctrl-names", None),
        ("pinctrl-", None),
        ("vdd-supply", Some(ReferenceKind::Supply)),
        ("-supply", None),
        ("interconnects", Some(ReferenceKind::Interconnect)),
        ("interrupts-extended", Some(ReferenceKind::Interrupt)),
        ("reset-gpios", Some(ReferenceKind::Gpio)),
        ("dmas", Some(ReferenceKind::Dma)),
        ("mboxes", Some(ReferenceKind::Mailbox)),
        ("iommus", Some(ReferenceKind::Iommu)),
        ("interrupts", None),
        ("clock-names", None),
    ] {
        assert_eq!(ReferenceKind::of(name), kind, "{name}");
        if let Some(kind) = kind {
            assert_eq!(ReferenceKind::from_u8(kind as u8), Some(kind));
        }
    }
    assert!(ReferenceKind::Clock.kernel_dependency());
    assert!(ReferenceKind::Supply.kernel_dependency());
    assert!(!ReferenceKind::Dma.kernel_dependency());
    assert!(!ReferenceKind::Interrupt.kernel_dependency());
}

/// Verifies: L.discovery.12
#[test]
fn a_node_with_more_references_than_the_bound_is_cut_there() {
    let mut structs = vec![0u8; 16384];
    let mut strings = vec![0u8; 256];
    let mut out = vec![0u8; 32768];
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("");
    w.begin("clock")
        .cells("#clock-cells", &[0])
        .cells("phandle", &[1])
        .end();
    let many = vec![1u32; MAX_REFERENCES + 10];
    w.begin("consumer").cells("clocks", &many).end();
    w.end();
    let length = w.finish(&mut out).expect("fits");
    let fdt = Fdt::parse(&out[..length]).expect("parses");
    let consumer = fdt.find_node("/consumer").expect("there");
    let mut count = 0;
    let mut cut = 0;
    fdt.for_each_reference(&consumer, |reference| {
        count += 1;
        if reference.cut {
            cut += 1;
        }
        ControlFlow::Continue(())
    });
    assert_eq!(count, MAX_REFERENCES + 1);
    assert_eq!(cut, 1);
}

/// Verifies: L.discovery.9
#[test]
fn an_identity_record_reads_back_what_the_tree_said() {
    let blob = tree();
    let fdt = Fdt::parse(&blob).expect("parses");
    let (node, ancestors) = lineage(&fdt, "/seconds@9010000");
    let mut bytes = vec![0u8; RECORD_BYTES];
    let encoded =
        identity::encode(&fdt, &node, &ancestors, FLAG_KERNEL_DEPENDS, &mut bytes).expect("fits");
    assert_eq!(encoded.references, 3);
    assert_ne!(
        encoded.flags & FLAG_MISSING_REFERENCE,
        0,
        "vs-supply names no node"
    );
    assert_eq!(encoded.flags & FLAG_CUT, 0);
    let record = Record::parse(&bytes).expect("reads back");
    assert!(record.kernel_depends());
    assert_eq!(record.node(), node.id());
    assert_eq!(record.path(), "/seconds@9010000");
    let compatible: Vec<&str> = record.compatible().collect();
    assert_eq!(compatible, ["ferrix,test-seconds", "arm,primecell"]);
    assert!(record.is_compatible("ferrix,test-seconds"));
    let clock_names: Vec<&str> = record.names("clock-names").collect();
    assert_eq!(clock_names, ["apb_pclk"]);
    let pinctrl_names: Vec<&str> = record.names("pinctrl-names").collect();
    assert_eq!(pinctrl_names, ["default"]);
    let intc = fdt.find_node("/intc@8000000").expect("there");
    assert_eq!(record.interrupt_parent(), intc.id());
    let references: Vec<_> = record.references().collect();
    assert_eq!(references.len(), 3);
    let clock = fdt.find_node("/apb-pclk").expect("there");
    assert_eq!(references[0].kind, Some(ReferenceKind::Clock));
    assert_eq!(references[0].provider, clock.id());
    assert!(references[0].resolved());
    assert_eq!(references[1].property, "vs-supply");
    assert_eq!(references[1].flags & REFERENCE_MISSING, REFERENCE_MISSING);
    assert!(!references[1].resolved());
    let controller = fdt.find_node("/soc/pinctrl@1000").expect("there");
    let pins = fdt.find_node("/soc/pinctrl@1000/uart-pins").expect("there");
    assert_eq!(references[2].provider, controller.id());
    assert_eq!(references[2].target, pins.id());
}

/// Verifies: L.discovery.9
#[test]
fn a_bus_controller_record_carries_its_subtree_and_disabled_references() {
    let blob = tree();
    let fdt = Fdt::parse(&blob).expect("parses");
    let (node, ancestors) = lineage(&fdt, "/soc/i2c@2000");
    let mut bytes = vec![0u8; RECORD_BYTES];
    let _ = identity::encode(&fdt, &node, &ancestors, 0, &mut bytes).expect("fits");
    let record = Record::parse(&bytes).expect("reads back");
    assert_eq!(record.path(), "/soc/i2c@2000");
    let reference = record.references().next().expect("one");
    assert_eq!(reference.flags & REFERENCE_DISABLED, REFERENCE_DISABLED);
    assert_eq!(reference.cells().collect::<Vec<_>>(), [7]);
    let children: Vec<_> = record.subtree().collect();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].depth, 1);
    assert_eq!(children[0].name, "sensor@48");
    let properties: Vec<_> = children[0].properties().collect();
    assert_eq!(properties[0].0, "compatible");
    assert_eq!(properties[0].1, b"national,lm75\0");
}

/// Verifies: L.discovery.12
#[test]
fn a_record_that_does_not_fit_is_cut_and_says_so() {
    let blob = tree();
    let fdt = Fdt::parse(&blob).expect("parses");
    let (node, ancestors) = lineage(&fdt, "/soc/i2c@2000");
    let mut small = vec![0u8; identity::HEADER_BYTES + 24];
    let encoded = identity::encode(&fdt, &node, &ancestors, 0, &mut small).expect("a header");
    assert_ne!(encoded.flags & FLAG_CUT, 0);
    assert!(encoded.length <= small.len());
    let record = Record::parse(&small).expect("still a record");
    assert_ne!(record.flags() & FLAG_CUT, 0);
    let mut tiny = vec![0u8; identity::HEADER_BYTES - 1];
    assert!(identity::encode(&fdt, &node, &ancestors, 0, &mut tiny).is_none());
}

/// Verifies: L.discovery.12
#[test]
fn bytes_that_are_not_a_record_are_refused() {
    assert!(Record::parse(&[0; 8]).is_err());
    assert!(Record::parse(&[0; 64]).is_err());
    let blob = tree();
    let fdt = Fdt::parse(&blob).expect("parses");
    let (node, ancestors) = lineage(&fdt, "/seconds@9010000");
    let mut bytes = vec![0u8; RECORD_BYTES];
    let _ = identity::encode(&fdt, &node, &ancestors, 0, &mut bytes).expect("fits");
    let mut bad = bytes.clone();
    bad[16..18].copy_from_slice(&0xFFF0u16.to_le_bytes());
    assert!(Record::parse(&bad).is_err());
    let mut long = bytes;
    long[8..12].copy_from_slice(&((RECORD_BYTES as u32) + 4).to_le_bytes());
    assert!(Record::parse(&long).is_err());
}

/// A provider that does not say how wide its specifiers are ends its
/// property, and says so: the reference names its node but is missing, the
/// signal the kernel's closure fails closed on (D4).
/// Verifies: L.discovery.11, L.discovery.12
#[test]
fn a_provider_without_cells_is_reported_with_its_node() {
    let mut structs = vec![0u8; 4096];
    let mut strings = vec![0u8; 256];
    let mut out = vec![0u8; 8192];
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("");
    w.begin("clock").cells("phandle", &[1]).end();
    w.begin("consumer").cells("clocks", &[1, 5, 1, 6]).end();
    w.end();
    let length = w.finish(&mut out).expect("fits");
    let fdt = Fdt::parse(&out[..length]).expect("parses");
    let consumer = fdt.find_node("/consumer").expect("there");
    let mut seen = Vec::new();
    fdt.for_each_reference(&consumer, |reference| {
        seen.push((reference.missing, reference.target.map(|node| node.name)));
        ControlFlow::Continue(())
    });
    assert_eq!(
        seen,
        [(true, Some("clock"))],
        "one reference, the rest unread"
    );
}

/// A phandle that names no node ends its property and says whether words
/// follow it unread: the second route by which the kernel's closure would
/// fall short, and fails closed instead (D4, ledger 4559 (14)).
/// Verifies: L.discovery.11, L.discovery.12
#[test]
fn a_missing_provider_says_whether_references_follow_it_unread() {
    let mut structs = vec![0u8; 4096];
    let mut strings = vec![0u8; 256];
    let mut out = vec![0u8; 8192];
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("");
    w.begin("clock")
        .cells("#clock-cells", &[0])
        .cells("phandle", &[1])
        .end();
    w.begin("first").cells("clocks", &[77, 1]).end();
    w.begin("last").cells("clocks", &[1, 77]).end();
    w.end();
    let length = w.finish(&mut out).expect("fits");
    let fdt = Fdt::parse(&out[..length]).expect("parses");
    let read = |path: &str| {
        let node = fdt.find_node(path).expect("there");
        let mut seen = Vec::new();
        fdt.for_each_reference(&node, |reference| {
            seen.push((reference.missing, reference.unread_after));
            ControlFlow::Continue(())
        });
        seen
    };
    assert_eq!(
        read("/first"),
        [(true, true)],
        "the reference after it is never read"
    );
    assert_eq!(
        read("/last"),
        [(false, false), (true, false)],
        "nothing follows the missing one"
    );
}
