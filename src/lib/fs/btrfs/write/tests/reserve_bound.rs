//! The memory an operation is admitted for is the memory it takes (W1-c,
//! `L.btrfs.124`, `L.btrfs.125`; ledger 8337 W1.4, W1.5, ledger 8470 CR.1).
//!
//! An integration test, so that it can install a global allocator: the crate
//! itself forbids `unsafe`, and implementing `GlobalAlloc` is. The allocator
//! is a model of the kernel's reserved heap (`mm::heap_scope`, `L.mm.140`):
//! while this thread runs inside a [`Model`] reserve's `within`, every
//! allocation of at most a page is served by a real `ferrix_heap::Heap` --
//! the kernel's own size classes, slabs and page accounting -- over pages
//! the reservation lets it draw, and one that finds no page within the
//! reservation's size is an overrun, served by the system as the kernel
//! serves it by the shared heap. Larger allocations go to the system, as the
//! kernel's go outside the scope; the model records the largest.
//!
//! So the test measures in the kernel's own units, frames drawn with slab
//! rounding and partial slabs, and asserts:
//!
//! * a node of each worst shape, read from its bytes, takes no more than
//!   [`node_frames`] of drawn frames (W1.4), and a whole operation's worth
//!   of them no more than its admission;
//! * scripts of every kind of edit, on a fresh volume, with leaves full of
//!   the worst payloads, run with no overrun (W1.5);
//! * the allocations above a page that stay outside the claim are named and
//!   sized (CR.1).

#![allow(missing_docs, reason = "a test crate")]
#![allow(clippy::unwrap_used, reason = "a test")]
#![allow(clippy::expect_used, reason = "a test")]
#![allow(
    clippy::indexing_slicing,
    reason = "a test, over fixed arrays and whole blocks"
)]
#![allow(
    clippy::print_stderr,
    reason = "the measurements are the test's report"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::items::{FS_TREE_OBJECTID, S_IFDIR, S_IFREG, Timespec};
use ferrix_btrfs::tree::{BtrfsKey, KeyPtr, Node};
use ferrix_btrfs::volume::{Device, ReadKind};
use ferrix_btrfs_write::node::{Body, LeafItem, TreeNode};
use ferrix_btrfs_write::{
    Error, NewInode, OPERATION_FRAMES, Reserve, WriteDevice, WriteVolume, node_frames,
};
use ferrix_fallible as _;
use ferrix_heap::{Backing, Heap, PAGE_SIZE, Request};

// ---------------------------------------------------------------------------
// The model of the kernel's reserved heap
// ---------------------------------------------------------------------------

/// Pages one thread's model may ever draw: 64 MiB of address space,
/// committed only as touched.
const ARENA_PAGES: usize = 16384;

/// The page supply of one thread's model heap: pages of an arena, counted
/// against the reservation's size as `Floor::Drawn` counts them.
struct Pages {
    base: usize,
    /// Pages of the arena never handed out yet, from the front.
    fresh: usize,
    /// A free page's address, linked through its first word; 0 for none.
    free: usize,
    /// Pages out: drawn.
    drawn: u64,
    /// The reservation's size, in pages.
    size: u64,
    /// Free objects per slab page, the kernel's frame record's count.
    slab_free: Vec<u16>,
}

// SAFETY: `allocate_pages` hands out whole, page-aligned pages of the arena,
// which this thread alone owns and nothing else uses, never one already out;
// the link accessors read and write eight aligned bytes inside such a page,
// and only addresses the heap was given.
unsafe impl Backing for Pages {
    fn allocate_pages(&mut self, order: u8) -> Option<u64> {
        // The model serves the requests of at most a page alone: a slab page
        // or one block of order 0.
        if order != 0 || self.drawn >= self.size {
            return None;
        }
        let page = if self.free != 0 {
            let page = self.free;
            // SAFETY: a free page's first word is the link written below.
            self.free = unsafe { (page as *const usize).read() };
            page
        } else if self.fresh < ARENA_PAGES {
            self.fresh += 1;
            self.base + (self.fresh - 1) * PAGE_SIZE
        } else {
            return None;
        };
        self.drawn += 1;
        Some(page as u64)
    }

    fn deallocate_pages(&mut self, address: u64, order: u8) {
        assert_eq!(order, 0, "the model gave out no larger block");
        let page = address as usize;
        // SAFETY: the page is the heap's, given back whole.
        unsafe { (page as *mut usize).write(self.free) };
        self.free = page;
        self.drawn -= 1;
    }

    fn read_link(&self, at: u64) -> u64 {
        // SAFETY: an aligned word inside a page of this arena (the trait's
        // contract).
        unsafe { (at as *const u64).read() }
    }

    fn write_link(&mut self, at: u64, value: u64) {
        // SAFETY: as `read_link`.
        unsafe { (at as *mut u64).write(value) }
    }

    fn slab_free(&self, page: u64) -> u16 {
        self.slab_free
            .get((page as usize - self.base) / PAGE_SIZE)
            .copied()
            .unwrap_or(0)
    }

    fn set_slab_free(&mut self, page: u64, objects: u16) {
        let at = (page as usize - self.base) / PAGE_SIZE;
        if let Some(count) = self.slab_free.get_mut(at) {
            *count = objects;
        }
    }
}

/// One thread's model heap.
struct ModelHeap {
    heap: Heap,
    pages: Pages,
    /// Allocations of at most a page that found no room: the kernel's
    /// `heap_scope_overruns`.
    overruns: u64,
    /// The largest allocation above a page made in the scope, in bytes.
    largest: usize,
}

impl ModelHeap {
    fn contains(&self, address: usize) -> bool {
        (self.pages.base..self.pages.base + ARENA_PAGES * PAGE_SIZE).contains(&address)
    }
}

std::thread_local! {
    static MODEL: RefCell<Option<ModelHeap>> = const { RefCell::new(None) };
    /// Whether this thread is inside a model reserve's `within`.
    static SCOPED: Cell<bool> = const { Cell::new(false) };
    /// Whether this thread is inside the model's own code, whose
    /// allocations are the system's.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

/// Every arena ever made, as (base, end): a pointer into one whose thread's
/// model is gone is leaked, never given to the system.
static ARENAS: [AtomicUsize; 512] = [const { AtomicUsize::new(0) }; 512];
static ARENA_COUNT: AtomicUsize = AtomicUsize::new(0);

fn in_any_arena(address: usize) -> bool {
    let count = ARENA_COUNT.load(Ordering::Acquire).min(ARENAS.len() / 2);
    (0..count).any(|at| {
        let base = ARENAS[2 * at].load(Ordering::Acquire);
        let end = ARENAS[2 * at + 1].load(Ordering::Acquire);
        (base..end).contains(&address)
    })
}

/// The system allocator, with this thread's scoped allocations of at most a
/// page served by its model heap.
struct Modelled;

// SAFETY: every pointer handed out is either the system's, passed through
// with its layout, or an object of the model heap, which serves only layouts
// of at most a page from pages it owns; a free goes back to whichever made
// it, by the address's arena.
unsafe impl GlobalAlloc for Modelled {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let request = Request::new(layout.size(), layout.align());
        let scoped = SCOPED.try_with(Cell::get).unwrap_or(false)
            && !INSIDE.try_with(Cell::get).unwrap_or(true);
        if scoped {
            INSIDE.with(|inside| inside.set(true));
            let served = MODEL
                .try_with(|model| {
                    let mut model = model.borrow_mut();
                    let model = model.as_mut()?;
                    if request.effective_size() > PAGE_SIZE {
                        model.largest = model.largest.max(layout.size());
                        return None;
                    }
                    let address = model.heap.allocate(&mut model.pages, request).ok();
                    if address.is_none() {
                        model.overruns += 1;
                    }
                    address
                })
                .ok()
                .flatten();
            INSIDE.with(|inside| inside.set(false));
            if let Some(address) = served {
                return address as *mut u8;
            }
        }
        // SAFETY: forwarded with the caller's layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let address = pointer as usize;
        if !INSIDE.try_with(Cell::get).unwrap_or(true) {
            INSIDE.with(|inside| inside.set(true));
            let freed = MODEL
                .try_with(|model| {
                    let mut model = model.borrow_mut();
                    let Some(model) = model.as_mut().filter(|model| model.contains(address)) else {
                        return false;
                    };
                    let request = Request::new(layout.size(), layout.align());
                    model
                        .heap
                        .deallocate(&mut model.pages, address as u64, request);
                    true
                })
                .unwrap_or(false);
            INSIDE.with(|inside| inside.set(false));
            if freed {
                return;
            }
        }
        if in_any_arena(address) {
            return;
        }
        // SAFETY: not the model's, so the system's, with its layout.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Modelled = Modelled;

/// Give this thread a fresh model heap, drawing nothing yet.
fn fresh_model() {
    let layout = Layout::from_size_align(ARENA_PAGES * PAGE_SIZE, PAGE_SIZE).unwrap();
    // SAFETY: a non-zero layout; the arena is never given back.
    let base = unsafe { System.alloc(layout) } as usize;
    assert_ne!(base, 0, "no arena for the model heap");
    let at = ARENA_COUNT.fetch_add(1, Ordering::AcqRel);
    assert!(2 * at + 1 < ARENAS.len(), "too many arenas");
    ARENAS[2 * at].store(base, Ordering::Release);
    ARENAS[2 * at + 1].store(base + ARENA_PAGES * PAGE_SIZE, Ordering::Release);
    let model = ModelHeap {
        heap: Heap::new(),
        pages: Pages {
            base,
            fresh: 0,
            free: 0,
            drawn: 0,
            size: 0,
            slab_free: vec![0; ARENA_PAGES],
        },
        overruns: 0,
        largest: 0,
    };
    // An old model's objects still out are leaked into its arena.
    MODEL.with(|slot| drop(slot.replace(Some(model))));
}

fn with_model<T>(body: impl FnOnce(&mut ModelHeap) -> T) -> T {
    MODEL.with(|model| body(model.borrow_mut().as_mut().expect("no model heap")))
}

fn drawn() -> u64 {
    with_model(|model| model.pages.drawn)
}

fn overruns() -> u64 {
    with_model(|model| model.overruns)
}

fn largest() -> usize {
    with_model(|model| model.largest)
}

/// Run `body` inside the model's scope with the reservation `frames` frames,
/// as `Reserve::within` does.
fn scoped<T>(frames: u64, body: impl FnOnce() -> T) -> T {
    with_model(|model| model.pages.size = frames);
    SCOPED.with(|scoped| scoped.set(true));
    let answer = body();
    SCOPED.with(|scoped| scoped.set(false));
    answer
}

/// The writer's reservation over this thread's model heap: what the kernel's
/// `MountReserve` is over a K12 entry.
#[derive(Debug)]
struct Model {
    /// Whether `grow_to` refuses.
    refuse: bool,
    /// The most it promises beyond what is drawn.
    most: u64,
}

impl Reserve for Model {
    fn grow_to(&self, frames: u64) -> bool {
        if self.refuse {
            return false;
        }
        with_model(|model| model.pages.size = frames);
        true
    }

    fn drawn(&self) -> u64 {
        drawn()
    }

    fn most(&self) -> u64 {
        self.most
    }

    fn within(&self, body: &mut dyn FnMut()) {
        let was = SCOPED.with(|scoped| scoped.replace(true));
        body();
        SCOPED.with(|scoped| scoped.set(was));
    }

    fn shrink(&self) {
        with_model(|model| model.pages.size = 0);
    }
}

// ---------------------------------------------------------------------------
// A device
// ---------------------------------------------------------------------------

/// Every fixture is 128 MiB.
const IMAGE_SIZE: u64 = 128 * 1024 * 1024;
const BLOCK: usize = 4096;
/// An empty volume with `mkfs.btrfs`'s default profiles and features.
const BLANK: &[u8] = include_bytes!("../../format/testdata/blank.img.packed");

/// Run `body` with the scope suspended, as the kernel's `Disk` suspends it
/// around every device call (`mm::heap_scope_suspended`; ledger 8337 W1.3):
/// what the device allocates is never the operation's.
fn unscoped<T>(body: impl FnOnce() -> T) -> T {
    let was = SCOPED.with(|scoped| scoped.replace(false));
    let answer = body();
    SCOPED.with(|scoped| scoped.set(was));
    answer
}

/// A volume in memory: whole blocks by offset. Its blocks are the device's
/// memory, made outside the scope.
#[derive(Debug)]
struct MemDevice {
    blocks: std::collections::BTreeMap<u64, Box<[u8; BLOCK]>>,
}

impl MemDevice {
    fn blank() -> MemDevice {
        let mut blocks = std::collections::BTreeMap::new();
        for record in BLANK.as_chunks::<{ 8 + BLOCK }>().0 {
            let offset = u64::from_le_bytes(record[..8].try_into().unwrap());
            let block: [u8; BLOCK] = record[8..].try_into().unwrap();
            let _ = blocks.insert(offset, Box::new(block));
        }
        MemDevice { blocks }
    }
}

impl Device for MemDevice {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        _kind: ReadKind,
    ) -> Result<(), BtrfsError> {
        if physical + buf.len() as u64 > IMAGE_SIZE {
            return Err(BtrfsError::DeviceRead { physical });
        }
        unscoped(|| self.read_blocks(physical, buf));
        Ok(())
    }
}

impl MemDevice {
    fn read_blocks(&self, physical: u64, buf: &mut [u8]) {
        for (done, byte) in buf.iter_mut().enumerate() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            *byte = self
                .blocks
                .get(&base)
                .map_or(0, |block| block[(at - base) as usize]);
        }
    }

    fn write_blocks(&mut self, physical: u64, data: &[u8]) {
        for (done, &byte) in data.iter().enumerate() {
            let at = physical + done as u64;
            let base = at - at % BLOCK as u64;
            let block = self
                .blocks
                .entry(base)
                .or_insert_with(|| Box::new([0; BLOCK]));
            block[(at - base) as usize] = byte;
        }
    }
}

impl WriteDevice for MemDevice {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> Result<(), Error> {
        if physical + data.len() as u64 > IMAGE_SIZE {
            return Err(Error::DeviceWrite { physical });
        }
        unscoped(|| self.write_blocks(physical, data));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Worst nodes, measured
// ---------------------------------------------------------------------------

/// The node sizes measured: the fixtures' (4 KiB), `mkfs.btrfs`'s default
/// (16 KiB, the N150's and the gates'), and the largest btrfs allows.
const NODESIZES: [u32; 3] = [4096, 16384, 65536];
/// A node's header, and a leaf item's descriptor.
const HEADER: usize = 101;
const ITEM: usize = 25;
const KEY_PTR: usize = 33;

/// A leaf of `nodesize` holding as many items of `sizes` (taken in turn) as
/// fit, serialised: the bytes a read gives.
fn leaf_bytes(nodesize: u32, sizes: &[usize]) -> Vec<u8> {
    let mut left = nodesize as usize - HEADER;
    let mut items = Vec::new();
    for (index, &size) in sizes.iter().cycle().enumerate() {
        if ITEM + size > left || index > 8192 {
            break;
        }
        left -= ITEM + size;
        items.push(LeafItem {
            key: BtrfsKey::new(1000 + index as u64, 24, 0),
            data: vec![0x5a; size],
        });
    }
    let node = TreeNode {
        bytenr: 0x40_0000,
        generation: 7,
        owner: FS_TREE_OBJECTID,
        level: 0,
        body: Body::Leaf(items),
    };
    let mut out = vec![0u8; nodesize as usize];
    node.write(&mut out, &[1; 16], &[2; 16]).unwrap();
    out
}

/// An internal node of `nodesize` with `count` pointers, serialised.
fn internal_bytes(nodesize: u32, count: usize) -> Vec<u8> {
    let ptrs = (0..count)
        .map(|index| KeyPtr {
            key: BtrfsKey::new(1000 + index as u64, 1, 0),
            blockptr: 0x80_0000 + index as u64 * u64::from(nodesize),
            generation: 7,
        })
        .collect();
    let node = TreeNode {
        bytenr: 0x40_0000,
        generation: 7,
        owner: FS_TREE_OBJECTID,
        level: 1,
        body: Body::Internal(ptrs),
    };
    let mut out = vec![0u8; nodesize as usize];
    node.write(&mut out, &[1; 16], &[2; 16]).unwrap();
    out
}

/// Frames drawn, on a fresh model heap, by reading `bytes` as a node, and
/// the largest allocation above a page it made.
fn measure(bytes: &[u8]) -> (u64, usize, TreeNode) {
    fresh_model();
    let node = Node::parse(bytes, 0x40_0000).unwrap();
    let read = scoped(u64::MAX, || TreeNode::from_node(&node).unwrap());
    assert_eq!(overruns(), 0, "a node read with no limit ran past it");
    (drawn(), largest(), read)
}

/// The payload sizes tried for a leaf: each class's size and one byte past
/// it, the sizes around a page, and the largest item a node of `nodesize`
/// holds.
fn payloads(nodesize: u32) -> Vec<usize> {
    let largest = nodesize as usize - HEADER - ITEM;
    let mut sizes = vec![0, 1, 7, 8, 9];
    for class in [
        16usize, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
    ] {
        sizes.extend([class - 1, class, class + 1]);
    }
    sizes.extend([3000, 5000, 12000, largest]);
    sizes.retain(|&size| size <= largest);
    sizes
}

#[test]
fn every_leaf_of_one_payload_size_is_within_a_nodes_frames() {
    for nodesize in NODESIZES {
        let bound = node_frames(nodesize);
        let mut worst = (0, 0);
        for size in payloads(nodesize) {
            let (frames, _, _) = measure(&leaf_bytes(nodesize, &[size]));
            assert!(
                frames <= bound,
                "a {nodesize}-byte leaf of {size}-byte items drew {frames} frames, past the \
                 bound of {bound}"
            );
            worst = worst.max((frames, size));
        }
        eprintln!(
            "reserve_bound: nodesize {nodesize}: the worst leaf of one payload size drew {} \
             frames ({}-byte items); the bound is {bound}",
            worst.0, worst.1
        );
    }
}

#[test]
fn a_leaf_of_every_class_is_within_a_nodes_frames_and_the_partial_pages() {
    // Every class's partly used page in one node: what OPERATION_FRAMES
    // allows for once an operation.
    let sizes = [1, 9, 17, 33, 65, 129, 257, 513, 1025, 2049];
    for nodesize in NODESIZES {
        let (frames, _, _) = measure(&leaf_bytes(nodesize, &sizes));
        let bound = node_frames(nodesize) + PARTIAL_PAGES;
        assert!(
            frames <= bound,
            "a mixed {nodesize}-byte leaf drew {frames} frames, past {bound}"
        );
        eprintln!("reserve_bound: nodesize {nodesize}: a leaf of every class drew {frames} frames");
    }
}

/// A partly used slab page for each of the kernel heap's nine size classes.
const PARTIAL_PAGES: u64 = 9;

#[test]
fn an_internal_node_is_within_a_nodes_frames() {
    for nodesize in NODESIZES {
        let most = (nodesize as usize - HEADER) / KEY_PTR;
        for count in [1, 50, 102, 103, 200, most] {
            let count = count.min(most);
            let (frames, _, _) = measure(&internal_bytes(nodesize, count));
            assert!(
                frames <= node_frames(nodesize),
                "{count} pointers of a {nodesize}-byte node drew {frames} frames"
            );
        }
    }
}

#[test]
fn an_operations_worth_of_worst_nodes_is_within_its_admission() {
    // 64 nodes, the most an edit is admitted for (`Need::EDIT`), each the
    // worst of the leaves above, in one heap, inside one admission's frames.
    let worst = [1025usize, 2049, 513, 257, 1, 9];
    for nodesize in NODESIZES {
        fresh_model();
        let admitted = 64 * node_frames(nodesize) + OPERATION_FRAMES;
        let nodes: Vec<Vec<u8>> = (0..64)
            .map(|at| leaf_bytes(nodesize, &[worst[at % worst.len()]]))
            .collect();
        let read = scoped(admitted, || {
            nodes
                .iter()
                .map(|bytes| TreeNode::from_node(&Node::parse(bytes, 0x40_0000).unwrap()).unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            overruns(),
            0,
            "64 worst {nodesize}-byte nodes ran past an edit's admission"
        );
        eprintln!(
            "reserve_bound: nodesize {nodesize}: 64 worst leaves drew {} frames of an \
             admission of {admitted}",
            drawn()
        );
        drop(read);
    }
}

#[test]
fn the_allocations_above_a_page_are_named_and_sized() {
    // CR.1: outside the claim, fallible as before, and measured here.
    for nodesize in NODESIZES {
        let body = nodesize as usize - HEADER;
        let full_of_empty = measure(&leaf_bytes(nodesize, &[0])).1;
        let full_internal = measure(&internal_bytes(nodesize, body / KEY_PTR)).1;
        let largest_item = measure(&leaf_bytes(nodesize, &[body - ITEM])).1;
        eprintln!(
            "reserve_bound: nodesize {nodesize}: allocations above a page: a full leaf's item \
             list {full_of_empty} bytes (Vec<LeafItem>, {} items of {} bytes), a full internal \
             node's pointers {full_internal} bytes (Vec<KeyPtr>, {} of {} bytes), the largest \
             item's payload {largest_item} bytes",
            body / ITEM,
            size_of::<LeafItem>(),
            body / KEY_PTR,
            size_of::<KeyPtr>()
        );
        assert_eq!(full_of_empty, body / ITEM * size_of::<LeafItem>());
        assert_eq!(full_internal, body / KEY_PTR * size_of::<KeyPtr>());
        // A 4 KiB node's largest item is less than a page, and in the claim.
        let expected = if body - ITEM > PAGE_SIZE {
            body - ITEM
        } else {
            0
        };
        assert_eq!(largest_item, expected);
    }
}

// ---------------------------------------------------------------------------
// Scripts, run inside the admission
// ---------------------------------------------------------------------------

const ROOT: u64 = 256;
const NOW: Timespec = Timespec {
    sec: 1_790_000_000,
    nsec: 11,
};

fn new(mode: u32) -> NewInode {
    NewInode {
        mode,
        uid: 0,
        gid: 0,
        rdev: 0,
        now: NOW,
    }
}

/// A blank volume with the model reserve, its most `most`.
fn reserved(most: u64) -> WriteVolume<MemDevice> {
    fresh_model();
    let mut volume = WriteVolume::open(MemDevice::blank()).unwrap();
    volume.set_reserve(Box::new(Model {
        refuse: false,
        most,
    }));
    volume
}

/// Every kind of edit, on a fresh volume, runs inside its reservation:
/// frames are drawn from it, and none of its allocations of at most a page
/// runs past what it was admitted for.
///
/// Verifies: L.btrfs.125
#[test]
fn every_kind_of_edit_runs_within_its_admission() {
    let mut volume = reserved(u64::MAX);
    let dir = volume.create(ROOT, b"d", &new(S_IFDIR | 0o755)).unwrap();
    // Names enough for the directory's items to fill and split leaves.
    for i in 0..600 {
        let name = format!("{i:0100}");
        let _ = volume
            .create(dir, name.as_bytes(), &new(S_IFREG | 0o644))
            .unwrap();
    }
    let file = volume.create(ROOT, b"f", &new(S_IFREG | 0o644)).unwrap();
    let data = vec![0xa5u8; 3 * 1024 * 1024 + 100];
    volume
        .write_file(file, 0, &data, data.len() as u64)
        .unwrap();
    volume.commit().unwrap();
    for i in (0..600).step_by(3) {
        let name = format!("{i:0100}");
        let ino = volume.unlink(dir, name.as_bytes(), NOW).unwrap();
        volume.evict(ino).unwrap();
    }
    volume
        .rename(dir, format!("{:0100}", 1).as_bytes(), ROOT, b"moved", NOW)
        .unwrap();
    volume.truncate(file, 4096).unwrap();
    volume.commit().unwrap();
    assert_eq!(overruns(), 0, "an edit ran past its admission");
    assert!(drawn() > 0, "the edits drew nothing from their reservation");
    eprintln!("reserve_bound: the script left {} frames drawn", drawn());
}

#[test]
fn leaves_full_of_the_worst_payloads_run_within_their_admission() {
    let mut volume = reserved(u64::MAX);
    let largest = volume.nodesize() as usize - HEADER - ITEM;
    for (at, &size) in [1usize, 1025, 2049, 513, 9, largest, 257]
        .iter()
        .enumerate()
    {
        for i in 0..200u64 {
            let key = BtrfsKey::new(1_000_000 + (at as u64) * 1000 + i, 24, 0);
            volume
                .insert(FS_TREE_OBJECTID, key, vec![0x3c; size])
                .unwrap();
        }
        volume.commit().unwrap();
    }
    assert_eq!(overruns(), 0, "an insert ran past its admission");
}

/// Verifies: L.btrfs.125
#[test]
fn a_commit_past_the_most_is_due_first() {
    // A reservation that may promise little, a commit of an empty
    // transaction's worth and room for a few hundred nodes: the volume says
    // a commit is due long before its dirty set would need more.
    fresh_model();
    let volume = WriteVolume::open(MemDevice::blank()).unwrap();
    let nodesize = volume.nodesize();
    drop(volume);
    let most = (2 * 64 + 64 + 40) * node_frames(nodesize) + OPERATION_FRAMES;
    let mut volume = reserved(most);
    let mut due = 0;
    for i in 0..4000 {
        if volume.commit_due() {
            due += 1;
            volume.commit().unwrap();
        }
        let name = format!("{i:0200}");
        let _ = volume
            .create(ROOT, name.as_bytes(), &new(S_IFREG | 0o644))
            .unwrap();
    }
    volume.commit().unwrap();
    assert!(
        due > 0,
        "4000 names never made a commit due at a most of {most} frames"
    );
    assert_eq!(
        overruns(),
        0,
        "with commits made when due, an operation ran past its admission"
    );
    eprintln!("reserve_bound: 4000 names at a most of {most} frames: {due} commits due");
}

#[test]
fn a_refused_admission_answers_out_of_memory_and_changes_nothing() {
    fresh_model();
    let mut volume = WriteVolume::open(MemDevice::blank()).unwrap();
    let _ = volume
        .create(ROOT, b"before", &new(S_IFREG | 0o644))
        .unwrap();
    volume.set_reserve(Box::new(Model {
        refuse: true,
        most: u64::MAX,
    }));
    assert_eq!(
        volume.create(ROOT, b"refused", &new(S_IFREG | 0o644)),
        Err(Error::OutOfMemory)
    );
    assert_eq!(
        volume.aborted(),
        None,
        "a refused admission aborted the transaction"
    );
    assert_eq!(volume.commit(), Err(Error::OutOfMemory));
    assert_eq!(
        volume.aborted(),
        None,
        "a refused commit aborted the transaction"
    );
    assert_eq!(volume.lookup(ROOT, b"refused").unwrap(), None);
    assert!(volume.lookup(ROOT, b"before").unwrap().is_some());
}
