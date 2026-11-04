//! Stage 10's checks of generic device-tree publication
//! (`docs/DRIVER-MODEL.md` §3; the consultant's D3-D10 and D21, ledger
//! 4475), run by [`super::publish`] on every boot, on every architecture.
//!
//! Most machines Ferrix boots have no tree, or a tree with nothing in it to
//! show a rule on, so the rules are held on a made-up tree first, built
//! here with `ferrix_fdt::write` and walked by the very code that walks the
//! machine's: [`Ownership::derive`] over the uses a check names,
//! [`find_generic`] after a node a binding holds, then the shared-page rule
//! [`super::withhold_shared_pages`]. Then the published nodes are held to
//! the same rules. Each failure names the rule it broke, which is what
//! `gate.sh control` reads.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_fdt::Fdt;
use ferrix_fdt::identity::{FLAG_KERNEL_DEPENDS, RECORD_MAGIC, Record};
use ferrix_fdt::write::Writer;
use ferrix_native_abi::types::DEVICE_TREE_GENERIC;

use super::{DeviceNode, Failure, Location, Reserved, page_span, withhold_shared_pages};
use crate::discovery::tree::{
    self, ClaimRefused, GenericReport, KernelUse, Ownership, find_generic,
};
use crate::fallible;

/// What the checks counted, for the boot line.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct GenericChecks {
    /// Rules held on the made-up tree.
    pub(super) rules: usize,
    /// Published generic nodes whose record read back through
    /// `device_config_read` and `device_info`.
    pub(super) published: usize,
}

/// The made-up tree's addresses: a console, a node sharing its page, two
/// nodes sharing one page, a private line's node, a shared line's, one
/// in a page the kernel reserves another part of, and one a binding holds.
const CONSOLE: u32 = 0x7000_0000;
const SHADOW: u32 = 0x7000_0800;
/// The shadow's second range, in a page of its own, and a node sharing that
/// page: kernel-owned by overlap, the shadow lends that page too (D7).
const SHADOW_FAR: u32 = 0x7000_3000;
const NEIGHBOUR: u32 = 0x7000_3800;
const LEFT: u32 = 0x7100_0000;
const RIGHT: u32 = 0x7100_0800;
const PRIVATE: u32 = 0x7200_0000;
const SHARED: u32 = 0x7300_0000;
const EDGE: u32 = 0x7400_0F00;
const RESERVED: u64 = 0x7400_0000;
const BOUND: u32 = 0x7500_0000;
/// The shared line the made-up tree names: SPI 900, GIC identifier 932,
/// which no device of any machine Ferrix boots raises.
const SHARED_LINE: u32 = 900;

/// The made-up tree, into `out`.
#[allow(
    unused_results,
    reason = "the writer's methods chain; a tree is built statement by statement"
)]
fn made_up(out: &mut [u8]) -> Option<usize> {
    let mut structs = fallible::try_filled(0u8, 4096).ok()?;
    let mut strings = fallible::try_filled(0u8, 512).ok()?;
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("")
        .cells("#address-cells", &[1])
        .cells("#size-cells", &[1])
        .cells("interrupt-parent", &[1]);
    w.begin("chosen")
        .strings("stdout-path", &["/uart@70000000"])
        .end();
    w.begin("intc@7f000000")
        .strings("compatible", &["arm,gic-400"])
        .cells("#interrupt-cells", &[3])
        .property("interrupt-controller", &[])
        .cells("reg", &[0x7F00_0000, 0x1000])
        .cells("phandle", &[1])
        .end();
    w.begin("uart@70000000")
        .strings("compatible", &["test,console"])
        .cells("reg", &[CONSOLE, 0x100])
        .cells("clocks", &[2])
        .end();
    w.begin("shadow@70000800")
        .strings("compatible", &["test,shadow"])
        .cells("reg", &[SHADOW, 0x100, SHADOW_FAR, 0x100])
        .end();
    w.begin("neighbour@70003800")
        .strings("compatible", &["test,neighbour"])
        .cells("reg", &[NEIGHBOUR, 0x100])
        .end();
    w.begin("clock-near")
        .strings("compatible", &["test,clock"])
        .cells("#clock-cells", &[0])
        .cells("clocks", &[3])
        .cells("phandle", &[2])
        .end();
    w.begin("clock-far")
        .strings("compatible", &["test,clock"])
        .cells("#clock-cells", &[0])
        .cells("phandle", &[3])
        .end();
    w.begin("left@71000000")
        .strings("compatible", &["test,left"])
        .cells("reg", &[LEFT, 0x100])
        .end();
    w.begin("right@71000800")
        .strings("compatible", &["test,right"])
        .cells("reg", &[RIGHT, 0x100])
        .end();
    w.begin("private@72000000")
        .strings("compatible", &["test,private"])
        .cells("reg", &[PRIVATE, 0x1000])
        .cells("interrupts", &[1, 7, 4])
        .end();
    w.begin("shared@73000000")
        .strings("compatible", &["test,shared"])
        .cells("reg", &[SHARED, 0x1000])
        .cells("interrupts", &[0, SHARED_LINE, 4])
        .end();
    w.begin("edge@74000f00")
        .strings("compatible", &["test,edge"])
        .cells("reg", &[EDGE, 0x80])
        .end();
    w.begin("bound@75000000")
        .strings("compatible", &["test,bound"])
        .cells("reg", &[BOUND, 0x1000])
        .end();
    w.end();
    w.finish(out)
}

/// The made-up node at `path` among `nodes`, by its identity record.
fn node_at<'n>(nodes: &'n [DeviceNode], path: &str) -> Option<&'n DeviceNode> {
    nodes
        .iter()
        .find(|node| Record::parse(node.identity()).is_ok_and(|record| record.path() == path))
}

/// A failure on the made-up tree.
const fn made_up_failure(what: &'static str) -> Failure {
    Failure {
        location: Location::Tree(0),
        what,
    }
}

/// Hold the made-up tree to every rule of generic publication.
///
/// Verifies: L.discovery.10, L.discovery.11, L.device.60, L.device.61, L.device.62
/// Verifies: L.device.63, L.device.65, H.DEV.7, H.DEV.8
fn check_made_up() -> Result<usize, Failure> {
    let fail = made_up_failure;
    let mut blob =
        fallible::try_filled(0u8, 8192).map_err(|_| fail("no memory for the made-up tree"))?;
    let length = made_up(&mut blob).ok_or(fail("the made-up tree does not fit"))?;
    let tree = Fdt::parse(blob.get(..length).unwrap_or_default())
        .map_err(|_| fail("the made-up tree does not parse"))?;
    // The kernel uses the console and the interrupt controller, by node, as
    // claim_tree_uses registers them.
    let id = |path: &str| tree.find_node(path).map(|node| node.id());
    let (Some(console), Some(controller)) = (id("/uart@70000000"), id("/intc@7f000000")) else {
        return Err(fail("the made-up tree lost its console"));
    };
    let uses = [KernelUse::Node(console), KernelUse::Node(controller)];
    let ownership = Ownership::derive(&tree, &uses)
        .map_err(|_| fail("no memory to derive the kernel-owned set"))?;
    let reserved = Reserved::synthetic(None, &[(RESERVED, 0x100)])
        .map_err(|_| fail("no memory for the made-up reserved ranges"))?;
    // A binding's node already holds `bound`'s registers.
    let mut nodes = Vec::new();
    let mut board = DeviceNode::empty(Location::Tree(u64::from(BOUND)));
    board.mint(u64::from(BOUND), 0x1000, false, &reserved);
    fallible::try_push(&mut nodes, board).map_err(|_| fail("no memory for a node"))?;
    let mut held = BTreeSet::new();
    let report: GenericReport =
        find_generic(&tree, &ownership, &reserved, &mut held, &mut nodes, false)
            .map_err(|_| fail("no memory to walk the made-up tree"))?;
    let _ = withhold_shared_pages(&mut nodes);
    let generic: Vec<&DeviceNode> = nodes.iter().filter(|node| node.is_generic()).collect();

    // D3: the console, and a node overlapping its page, are kernel-owned:
    // derived from the console's use, listed nowhere.
    if node_at(&nodes, "/uart@70000000").is_some() {
        return Err(fail("a node the kernel uses was published"));
    }
    if node_at(&nodes, "/shadow@70000800").is_some()
        || !ownership.owns(id("/shadow@70000800").unwrap_or(0))
    {
        return Err(fail(
            "a node overlapping a kernel-owned node's registers was not kernel-owned",
        ));
    }
    // D7: every page of a kernel-owned node is the kernel's, also one owned
    // by overlap alone.
    if node_at(&nodes, "/neighbour@70003800").is_some() {
        return Err(fail(
            "a node sharing a page with a kernel-owned node's other registers was published",
        ));
    }
    // D4: the console's clock and that clock's clock, two references away.
    for path in ["/clock-near", "/clock-far"] {
        let flagged = node_at(&nodes, path)
            .is_some_and(|node| node.identity_flags() & FLAG_KERNEL_DEPENDS != 0);
        if !flagged {
            return Err(fail(
                "KERNEL_DEPENDS was not set across a transitive reference",
            ));
        }
    }
    if node_at(&nodes, "/left@71000000")
        .is_some_and(|node| node.identity_flags() & FLAG_KERNEL_DEPENDS != 0)
    {
        return Err(fail(
            "KERNEL_DEPENDS was set on a node the kernel does not depend on",
        ));
    }
    // D7: a page two published nodes share is minted to neither, and a
    // range in a page the kernel reserves another part of is withheld whole.
    for path in ["/left@71000000", "/right@71000800"] {
        if node_at(&nodes, path).is_none_or(|node| !node.apertures().is_empty()) {
            return Err(fail("a page two published nodes share was minted"));
        }
    }
    if node_at(&nodes, "/edge@74000f00").is_none_or(|node| !node.apertures().is_empty()) {
        return Err(fail(
            "a range sharing a page with reserved memory was minted",
        ));
    }
    let shared = node_at(&nodes, "/shared@73000000");
    if shared.is_none_or(|node| node.apertures().len() != 1 || node.vector_count() != 1) {
        return Err(fail(
            "a generic node was refused a whole page or a shared line of its own",
        ));
    }
    // D8: a private line is never minted.
    if node_at(&nodes, "/private@72000000").is_none_or(|node| node.vector_count() != 0)
        || report.private_refused != 1
    {
        return Err(fail("a private interrupt was minted to a generic node"));
    }
    // D10: a node a binding holds is not published again.
    if node_at(&nodes, "/bound@75000000").is_some() || report.bound != 1 {
        return Err(fail("a node a binding holds was published again"));
    }
    // D5: every generic node reaches memory.
    if generic.iter().any(|node| !node.reaches_memory()) {
        return Err(fail("a generic node was described as reaching no memory"));
    }
    if generic.len() != 7 || report.published != 7 {
        return Err(fail(
            "the made-up tree did not publish its seven generic nodes",
        ));
    }
    Ok(11)
}

/// A made-up tree whose console's clock names a provider that does not say
/// how wide its specifiers are, beside a node that would be published.
#[allow(
    unused_results,
    reason = "the writer's methods chain; a tree is built statement by statement"
)]
fn unreadable(out: &mut [u8], missing_first: bool) -> Option<usize> {
    let mut structs = fallible::try_filled(0u8, 1024).ok()?;
    let mut strings = fallible::try_filled(0u8, 256).ok()?;
    let mut w = Writer::new(&mut structs, &mut strings);
    w.begin("")
        .cells("#address-cells", &[1])
        .cells("#size-cells", &[1]);
    w.begin("chosen")
        .strings("stdout-path", &["/uart@70000000"])
        .end();
    w.begin("uart@70000000")
        .strings("compatible", &["test,console"])
        .cells("reg", &[CONSOLE, 0x100])
        .cells("clocks", if missing_first { &[77, 3] } else { &[2, 9] })
        .end();
    w.begin("clock-after-a-missing-one")
        .strings("compatible", &["test,clock"])
        .cells("#clock-cells", &[0])
        .cells("phandle", &[3])
        .end();
    w.begin("clock-without-cells")
        .strings("compatible", &["test,clock"])
        .cells("phandle", &[2])
        .end();
    w.begin("other@71000000")
        .strings("compatible", &["test,other"])
        .cells("reg", &[LEFT, 0x1000])
        .end();
    w.end();
    w.finish(out)
}

/// `KERNEL_DEPENDS` fails closed (D4): a kernel-owned node whose references
/// cannot all be read stops publication of the whole tree.
///
/// Verifies: L.discovery.11
fn check_unread_closure() -> Result<(), Failure> {
    // A provider without #clock-cells, and a phandle naming no node before
    // a reference that is never read: either leaves the closure short.
    for missing_first in [false, true] {
        check_unread_case(missing_first)?;
    }
    Ok(())
}

/// One case of [`check_unread_closure`].
fn check_unread_case(missing_first: bool) -> Result<(), Failure> {
    let fail = made_up_failure;
    let mut blob =
        fallible::try_filled(0u8, 4096).map_err(|_| fail("no memory for the made-up tree"))?;
    let length =
        unreadable(&mut blob, missing_first).ok_or(fail("the made-up tree does not fit"))?;
    let tree = Fdt::parse(blob.get(..length).unwrap_or_default())
        .map_err(|_| fail("the made-up tree does not parse"))?;
    let console = tree
        .find_node("/uart@70000000")
        .map(|node| node.id())
        .ok_or(fail("the made-up tree lost its console"))?;
    let ownership = Ownership::derive(&tree, &[KernelUse::Node(console)])
        .map_err(|_| fail("no memory to derive the kernel-owned set"))?;
    let reserved =
        Reserved::synthetic(None, &[]).map_err(|_| fail("no memory for reserved ranges"))?;
    let mut nodes = Vec::new();
    let mut held = BTreeSet::new();
    let report = find_generic(&tree, &ownership, &reserved, &mut held, &mut nodes, false)
        .map_err(|_| fail("no memory to walk the made-up tree"))?;
    if report.stopped != Some(console) || !nodes.is_empty() {
        return Err(fail(
            "publication went on past a kernel-owned node whose references could not all be read",
        ));
    }
    Ok(())
}

/// Hold the published nodes to the rules, and every generic one's record to
/// what `device_info` and `device_config_read` say of it.
///
/// Verifies: L.device.61, L.device.63, L.device.64, L.device.65
fn check_published(nodes: &[DeviceNode]) -> Result<usize, Failure> {
    let mut pages: Vec<(u64, u64, usize)> = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        for aperture in node.apertures() {
            if let Some((start, end)) = page_span(aperture.phys(), aperture.len()) {
                fallible::try_push(&mut pages, (start, end, index)).map_err(|_| Failure {
                    location: node.location(),
                    what: "no memory to check the published nodes",
                })?;
            }
        }
    }
    let mut ids = BTreeSet::new();
    let mut published = 0;
    for (index, node) in nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.is_generic())
    {
        let fail = |what| Failure {
            location: node.location(),
            what,
        };
        if !node.reaches_memory() {
            return Err(fail("a generic node was described as reaching no memory"));
        }
        let record = Record::parse(node.identity())
            .map_err(|_| fail("a generic node's record does not read back"))?;
        if !fallible::insert_into_set(&mut ids, record.node()).map_err(|_| fail("no memory"))? {
            return Err(fail("one tree node was published twice"));
        }
        for aperture in node.apertures() {
            let Some((start, end)) = page_span(aperture.phys(), aperture.len()) else {
                continue;
            };
            if cfg_early_windows().any(|(low, high)| start < high && low < end) {
                return Err(fail(
                    "a generic node holds a page the kernel's console maps",
                ));
            }
            if pages
                .iter()
                .any(|&(low, high, owner)| owner != index && start < high && low < end)
            {
                return Err(fail("a page two published nodes share was minted"));
            }
        }
        let info = node.describe();
        let length = u32::try_from(node.identity().len()).unwrap_or(u32::MAX);
        if info.virtio != DEVICE_TREE_GENERIC
            || info.class != length
            || info.device_id != node.identity_flags()
        {
            return Err(fail("device_info does not describe a generic node as one"));
        }
        if node.config_read(0, 4) != Ok(RECORD_MAGIC) || node.config_write(0, 4, 0).is_ok() {
            return Err(fail(
                "a generic node's record is not read-only through its configuration space",
            ));
        }
        published += 1;
    }
    Ok(published)
}

/// The kernel-owned set is sealed: a claim now is refused, a board window
/// outside every claim is refused, and the console early boot mapped is
/// claimed.
///
/// Verifies: L.discovery.8, L.discovery.10, H.DEV.8
fn check_sealed() -> Result<(), Failure> {
    // A set of its own, so that a kernel that publishes nothing runs the rule.
    let mut claims = Vec::new();
    let refused = tree::claim_in(&mut claims, true, KernelUse::Range(0x1000, 0x2000));
    if refused != Err(ClaimRefused::Sealed) {
        return Err(made_up_failure("a kernel claim after the seal was taken"));
    }
    // After the seal a board window maps only inside an earlier claim: the
    // top page of the address space is claimed by nothing.
    let top = u64::MAX - (PAGE_SIZE - 1);
    if tree::use_window_in(&mut claims, true, top - PAGE_SIZE, top) != Err(ClaimRefused::NotClaimed)
    {
        return Err(made_up_failure(
            "a window outside every claim was mapped after the seal",
        ));
    }
    // The console, and every window early boot mapped, is a claimed use.
    #[cfg(ferrix_personality = "user")]
    for (start, end) in crate::early::device_windows() {
        if !tree::sealed() || !tree::covered(start, end) || tree::use_window(start, end).is_err() {
            return Err(made_up_failure(
                "a window early boot mapped is not claimed as the kernel's",
            ));
        }
    }
    Ok(())
}

/// Every check of generic publication, on the made-up tree and on `nodes`.
///
/// # Errors
///
/// The first rule broken.
pub(super) fn check(nodes: &[DeviceNode]) -> Result<GenericChecks, Failure> {
    check_sealed()?;
    check_unread_closure()?;
    let rules = check_made_up()? + 4;
    let published = check_published(nodes)?;
    Ok(GenericChecks { rules, published })
}

/// Early boot's windows, in a kernel that records them; none in one that does
/// not.
fn cfg_early_windows() -> impl Iterator<Item = (u64, u64)> {
    #[cfg(ferrix_personality = "user")]
    return crate::early::device_windows();
    #[cfg(not(ferrix_personality = "user"))]
    return core::iter::empty();
}
