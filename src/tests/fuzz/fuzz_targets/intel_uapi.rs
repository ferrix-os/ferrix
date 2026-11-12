//! Fuzz the Intel Xe-LP driver's i915 uAPI front end
//! (`src/lib/drivers/gpu/intel-xelp/src/frontend`): every ioctl, open,
//! release, mmap and dmabuf event a program can send its render node, which
//! is mode 0666 (`docs/INTEL-GPU.md` §4.4), in fuzzer-chosen sequences from
//! several files in two processes.
//!
//! The input is a program: each op is a byte that picks what to do and the
//! bytes after it that say how (`Op`). Most ops build an ioctl's argument
//! the way Mesa does -- a handle a file was given, a softpinned address, a
//! fence array in the program's memory -- and then let the input bend it:
//! another file's handle, a handle already closed, a pointer one byte short
//! of the end of mapped memory, a size or offset at a limit. One op sends any
//! command number with raw argument bytes. The back end is a model of the
//! GPU that completes, hangs or refuses work when the input says, and the
//! host is a model of the chardev core with each process's descriptors.
//!
//! # The properties
//!
//! Not panicking is the floor; the library's arithmetic is checked here
//! (`cargo fuzz` builds with debug assertions). Beyond it:
//!
//! 1. **The back end's promises hold** (the front end's module comment): a
//!    binding is 4 KiB-aligned, inside 48 bits, overlaps no live binding of
//!    its VM and is unbound only while no submitted batch lies in it; a
//!    request is submitted to a live hardware context in order, with its
//!    batch inside a live binding of that context's VM; a hardware context
//!    and a VM are destroyed only when nothing uses them.
//! 2. **The host's are kept**: each request answered at most once, with 0 or
//!    an errno; only waits and execbuffers are held; a kernel fence is
//!    signalled once, and only one the driver made; a dmabuf cookie always
//!    names the same memory; an mmap stays inside its object.
//! 3. **Files never see each other's names.** After each op, every handle,
//!    syncobj, VM id and live context belongs to an open file (a closed
//!    file's slot is reused by the next open), and a call that names a
//!    handle another open file holds never succeeds. The tables' counts
//!    agree with each other and with the back end's: references of each VM,
//!    handles and bindings of each object, live memories, VMs, hardware
//!    contexts and bindings.
//! 4. **Every call is bounded**: the copies one call makes, and the back-end
//!    calls, stay under fixed bounds whatever the input, and the tables stay
//!    inside `limits`.
//! 5. **Everything goes with the last client.** When the run ends, every
//!    descriptor is closed, every file released and every request completed;
//!    then no object, handle, binding, VM, context, syncobj, kernel fence,
//!    held wait, memory, hardware context or back-end binding is left, and
//!    every kernel fence the driver made has been signalled.

#![no_main]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, OnceLock};

use ferrix_intel_xelp::frontend::fence::WaitKind;
use ferrix_intel_xelp::frontend::objects::{Binding, Tables};
use ferrix_intel_xelp::frontend::syncobj;
use ferrix_intel_xelp::frontend::{
    Backend, Completion, ContextParams, Engine, Facts, Frontend, Host, HwContext, Installed,
    RequestId, SubmitError, VmId, limits,
};
use ferrix_intel_xelp::ppgtt::{BindError, PAGE_2M, Ppgtt, PpgttError, PtMemory};
use ferrix_intel_xelp::sseu::{Fuses, gen12_sseu_info_init};
use ferrix_intel_xelp::uapi::{
    self, ContextCreateExt, ContextParam, ExecFence, ExecObject2, Execbuffer2, GemCreate,
    GemCreateExt, GemMmapOffset, GetParam, PrimeHandle, Query, QueryItem, SyncobjHandle,
    SyncobjTimelineWait, SyncobjTransfer, SyncobjWait, VmControl, ioctl,
};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::kmsg::Level;
use libfuzzer_sys::fuzz_target;

// ---------------------------------------------------------------------------
// Bounds of the harness itself
// ---------------------------------------------------------------------------

/// Ops one input runs, repeats included: enough to fill the smaller tables
/// (`limits::VMS`, `WAITS`, `FILES`), few enough that one run stays fast.
const MAX_CALLS: usize = 3000;
/// Processes, each with its own descriptors.
const PROCS: usize = 2;
/// The program's memory starts here and is one block, grown as ops build
/// arguments in it.
const BASE: u64 = 0x10_0000;
/// The most program memory one run builds.
const ARENA_MAX: usize = 1 << 20;
/// Copies one call may make: an execbuffer of every handle reads its list
/// once in chunks of 32 (four times before D3), plus fences, extensions and
/// points; the slack is a held execbuffer or two run again in the call.
const MAX_COPIES: u64 = 4 * (limits::EXEC_OBJECTS as u64 / 32 + 1) + 4096;
/// Bytes one call may copy.
const MAX_COPIED: u64 = 4 * limits::EXEC_OBJECTS as u64 * 56 + (1 << 20);
/// Back-end calls one call may make.
const MAX_BACKEND_CALLS: u64 = 4 * (limits::BINDINGS as u64 + limits::REQUESTS as u64) + 4096;

// ---------------------------------------------------------------------------
// The input
// ---------------------------------------------------------------------------

/// The input, read a field at a time; past its end every field is zero.
struct Input<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Input<'_> {
    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn u8(&mut self) -> u8 {
        let byte = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        byte
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }

    fn u32(&mut self) -> u32 {
        u32::from(self.u16()) | u32::from(self.u16()) << 16
    }

    fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | u64::from(self.u32()) << 32
    }

    fn bool(&mut self) -> bool {
        self.u8() & 1 != 0
    }

    /// One of `n` (0 when `n` is 0).
    fn pick(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            usize::from(self.u8()) % n
        }
    }

    /// A count, mostly small.
    fn count(&mut self, small: usize, large: usize) -> usize {
        let byte = self.u8();
        if byte >= 0xf0 {
            usize::from(self.u16()) % (large + 1)
        } else {
            usize::from(byte) % (small + 1)
        }
    }

    /// A size in bytes: page edges, the object limit's edges, overflows.
    fn size(&mut self) -> u64 {
        const MAX: u64 = i32::MAX as u64 * 4096;
        match self.u8() % 16 {
            0 => 0,
            1 => 1,
            2 => 4095,
            3 => 4096,
            4 => 4097,
            5 => 8192,
            6 => 0x1_0000,
            7 => 1 << 21,
            8 => MAX,
            9 => MAX + 1,
            10 => u64::MAX,
            11 => u64::MAX - 4094,
            12 => 1 << 48,
            13 => u64::from(self.u8()) * 4096,
            14 => u64::from(self.u16()),
            _ => self.u64(),
        }
    }

    /// A softpin address: a few pages that overlap each other, the 32- and
    /// 48-bit edges, non-canonical and canonical high addresses.
    fn gpu_address(&mut self) -> u64 {
        match self.u8() % 16 {
            0 => 0,
            1..=6 => u64::from(self.u8() % 16) * 0x1000,
            7 => u64::from(self.u8()) * 0x1_0000,
            8 => (1 << 32) - 0x1000,
            9 => 1 << 32,
            10 => (1 << 47) - 0x1000,
            11 => 0xffff_8000_0000_0000,
            12 => 0xffff_ffff_ffff_f000,
            13 => (1 << 48) - 0x1000,
            14 => 1 << 48,
            _ => self.u64(),
        }
    }

    /// A u32 at an edge, or any.
    fn word(&mut self) -> u32 {
        match self.u8() % 8 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => u32::MAX,
            4 => 0x8000_0000,
            5 => u32::from(self.u8()),
            _ => self.u32(),
        }
    }

    /// A u64 at an edge, or any.
    fn quad(&mut self) -> u64 {
        match self.u8() % 8 {
            0 => 0,
            1 => 1,
            2 => u64::MAX,
            3 => i64::MAX as u64,
            4 => 1 << 32,
            5 => u64::from(self.u8()),
            _ => self.u64(),
        }
    }

    /// A timeout in nanoseconds: none, a poll, past, near, far, for ever.
    fn timeout(&mut self) -> i64 {
        match self.u8() % 8 {
            0 => 0,
            1 => -1,
            2 => 1,
            3 => 1_000_000,
            4 => 1_000_000_000,
            5 => i64::MAX,
            6 => i64::MIN,
            _ => self.u64() as i64,
        }
    }
}

// ---------------------------------------------------------------------------
// The back end: a model of the GPU that checks the front end's promises
// ---------------------------------------------------------------------------

/// The engines the model has: render and copy, as M2's back end.
static ENGINES: [Engine; 2] = [
    Engine {
        class: 0,
        instance: 0,
    },
    Engine {
        class: 1,
        instance: 0,
    },
];

fn facts() -> Facts {
    Facts {
        device_id: 0x46d4,
        revision: 0,
        sseu: gen12_sseu_info_init(Fuses {
            slice_enable: 1,
            geometry_dss_enable: 0x3,
            eu_disable: 0x88,
        }),
        cs_timestamp_hz: 19_200_000,
        engines: &ENGINES,
        ggtt_bytes: 4 << 30,
        ppgtt_bits: 48,
        system_memory_bytes: 16 << 30,
        max_pat_index: 3,
    }
}

/// One object's memory. Dropping it frees it, which the model sees.
struct Memory {
    id: u32,
    size: u64,
    live: Rc<RefCell<BTreeSet<u32>>>,
    /// Memories freed since the last check, which [`check`] holds to D2.
    freed: Rc<RefCell<Vec<u32>>>,
}

impl Drop for Memory {
    fn drop(&mut self) {
        assert!(
            self.live.borrow_mut().remove(&self.id),
            "memory freed twice"
        );
        self.freed.borrow_mut().push(self.id);
    }
}

/// Failures the input arms; each fires once.
mod fault {
    pub const CREATE_MEMORY: u32 = 1 << 0;
    pub const VM_CREATE: u32 = 1 << 1;
    pub const BIND: u32 = 1 << 2;
    pub const CONTEXT_CREATE: u32 = 1 << 3;
    pub const SUBMIT_FULL: u32 = 1 << 4;
    pub const SUBMIT_FAILED: u32 = 1 << 5;
    pub const POPULATE: u32 = 1 << 6;
    pub const DMABUF_INSTALL: u32 = 1 << 7;
    pub const SYNC_INSTALL: u32 = 1 << 8;
    pub const DMABUF_FENCE: u32 = 1 << 9;
    pub const REPLY_MAP: u32 = 1 << 10;
    /// A bind's last page has no device address: the windows before it are
    /// written by then.
    pub const BIND_PARTWAY: u32 = 1 << 11;
    /// The next page-table page cannot be had.
    pub const PT_ALLOC: u32 = 1 << 12;
    /// Another thread of the program rewrites an entry of the next
    /// execbuffer's list after the driver's first, second, third or fourth
    /// read of it.
    pub const RACE_EXEC: u32 = 1 << 13;
}

/// The largest binding the model maps through its page tables; larger ones
/// (objects reach 8 TiB) are only recorded, so that a run stays fast.
const MAPPED_MAX: u64 = 8 << 20;

/// The model's page-table pages, each owned by the VM whose call made it:
/// the PPGTT code the real back end runs (`ferrix_intel_xelp::ppgtt`) over
/// memory that says whose every table is.
struct PtFake {
    tables: BTreeMap<u64, Box<[u64; 512]>>,
    /// Each table's VM.
    owner: BTreeMap<u64, u32>,
    next: u64,
    /// The VM the current call works on.
    current: u32,
    faults: Rc<Cell<u32>>,
}

impl PtFake {
    /// How many tables `vm` has.
    fn owned(&self, vm: u32) -> usize {
        self.owner.values().filter(|&&owner| owner == vm).count()
    }
}

impl PtMemory for PtFake {
    fn alloc_pt(&mut self) -> Option<u64> {
        if fire(&self.faults, fault::PT_ALLOC) {
            return None;
        }
        self.next += 0x1000;
        // Not zeroed: a table must be filled by whoever takes it.
        let _ = self.tables.insert(self.next, Box::new([0xdead_beef; 512]));
        let _ = self.owner.insert(self.next, self.current);
        Some(self.next)
    }

    fn free_pt(&mut self, table: u64) {
        assert!(
            self.tables.remove(&table).is_some(),
            "page table {table:#x} freed twice"
        );
        let _ = self.owner.remove(&table);
    }

    fn read_entry(&mut self, table: u64, index: usize) -> u64 {
        self.tables.get(&table).expect("a live table")[index]
    }

    fn write_entry(&mut self, table: u64, index: usize, value: u64) {
        self.tables.get_mut(&table).expect("a live table")[index] = value;
    }
}

/// The device address of page `page` of memory `id`: each memory its own
/// 4 GiB, 2 MiB-aligned, so a whole window maps as one 2 MiB entry.
fn device_page(id: u32, page: u64) -> u64 {
    (u64::from(id) << 32) | (page * 4096)
}

/// Take fault `bit` if armed.
fn fire(faults: &Cell<u32>, bit: u32) -> bool {
    let armed = faults.get() & bit != 0;
    faults.set(faults.get() & !bit);
    armed
}

#[derive(Debug)]
struct HwState {
    vm: u32,
    /// The newest request submitted to it.
    last: RequestId,
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    id: RequestId,
    context: u32,
    vm: u32,
    batch: u64,
    length: u32,
}

struct Model {
    facts: Facts,
    next: u32,
    live: Rc<RefCell<BTreeSet<u32>>>,
    freed: Rc<RefCell<Vec<u32>>>,
    /// For each memory unbound while requests of its VM ran, those
    /// requests: any may hold a GPU TLB entry for it at an address it
    /// never listed, so none may still run when it is freed (D2).
    exposed: RefCell<BTreeMap<u32, BTreeSet<RequestId>>>,
    faults: Rc<Cell<u32>>,
    /// Complete each submission at once.
    auto: bool,
    /// Each live VM's page tables.
    vms: BTreeMap<u32, Ppgtt>,
    pt: PtFake,
    /// (VM, address) to (bytes, memory).
    binds: BTreeMap<(u32, u64), (u64, u32)>,
    contexts: BTreeMap<u32, HwState>,
    in_flight: Vec<InFlight>,
    done: Vec<(RequestId, Completion)>,
    submitted: BTreeSet<RequestId>,
    calls: u64,
}

impl Model {
    fn new(faults: Rc<Cell<u32>>, auto: bool) -> Self {
        Self {
            facts: facts(),
            next: 0,
            live: Rc::new(RefCell::new(BTreeSet::new())),
            freed: Rc::new(RefCell::new(Vec::new())),
            exposed: RefCell::new(BTreeMap::new()),
            faults: faults.clone(),
            auto,
            vms: BTreeMap::new(),
            pt: PtFake {
                tables: BTreeMap::new(),
                owner: BTreeMap::new(),
                next: 0,
                current: 0,
                faults,
            },
            binds: BTreeMap::new(),
            contexts: BTreeMap::new(),
            in_flight: Vec::new(),
            done: Vec::new(),
            submitted: BTreeSet::new(),
            calls: 0,
        }
    }

    fn id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    /// Whether a submitted batch of `vm` lies in `[address, address + bytes)`.
    fn batch_in(&self, vm: u32, address: u64, bytes: u64) -> Option<RequestId> {
        self.in_flight
            .iter()
            .find(|run| {
                run.vm == vm
                    && run.batch < address.saturating_add(bytes)
                    && address < run.batch + u64::from(run.length)
            })
            .map(|run| run.id)
    }

    /// Map `bytes` of memory `id` at `address` in `vm` through its page
    /// tables, as the real back end does (`Ppgtt::bind_range`), its last
    /// page refused when [`fault::BIND_PARTWAY`] is armed. A bind that fails
    /// leaves no entry of its range and no table it made (D1): the front
    /// end records no binding for it, and frees the memory.
    fn map(
        &mut self,
        vm: u32,
        address: u64,
        id: u32,
        bytes: u64,
        pat_index: u8,
    ) -> Result<(), Errno> {
        let ppgtt = *self.vms.get(&vm).expect("a live VM");
        let partway = fire(&self.faults, fault::BIND_PARTWAY);
        let last = bytes / 4096 - 1;
        self.pt.current = vm;
        let tables = self.pt.owned(vm);
        let result = ppgtt.bind_range(
            &mut self.pt,
            address,
            bytes,
            u32::from(pat_index),
            &mut |_: &mut PtFake, page| {
                if partway && page == last {
                    Err(Errno::ENOMEM)
                } else {
                    Ok(device_page(id, page))
                }
            },
        );
        let windows = (address..address + bytes)
            .step_by(PAGE_2M as usize)
            .chain([address + bytes - 4096]);
        match result {
            Ok(_) => {
                for at in windows {
                    assert_eq!(
                        ppgtt.translate(&mut self.pt, at),
                        Some(device_page(id, (at - address) / 4096)),
                        "{at:#x} of a bind at {address:#x}+{bytes:#x} maps the wrong page"
                    );
                }
                Ok(())
            }
            Err(error) => {
                for at in windows {
                    assert_eq!(
                        ppgtt.translate(&mut self.pt, at),
                        None,
                        "{at:#x} still mapped after a bind at {address:#x}+{bytes:#x} failed"
                    );
                }
                assert_eq!(
                    self.pt.owned(vm),
                    tables,
                    "a failed bind at {address:#x}+{bytes:#x} left page tables"
                );
                match error {
                    BindError::Page(errno) => Err(errno),
                    BindError::Table(PpgttError::NoMemory) => Err(Errno::ENOMEM),
                    BindError::Table(other) => {
                        panic!("bind at {address:#x}+{bytes:#x} refused: {other:?}")
                    }
                }
            }
        }
    }

    /// Finish `count` of `context`'s submitted requests, oldest first: all
    /// ran, or the first hung and the rest of that context were cancelled
    /// (the reset `Completion::Hung` stands for).
    fn finish(&mut self, context: u32, count: usize, hung: bool) {
        let mut taken = 0;
        let mut index = 0;
        let mut guilty = false;
        while index < self.in_flight.len() {
            let run = self.in_flight[index];
            if run.context != context || (taken >= count && !guilty) {
                index += 1;
                continue;
            }
            let how = if !hung {
                Completion::Ok
            } else if !guilty {
                guilty = true;
                Completion::Hung
            } else {
                Completion::Cancelled
            };
            self.done.push((run.id, how));
            let _ = self.in_flight.remove(index);
            taken += 1;
        }
    }
}

impl Backend for Model {
    type Memory = Memory;

    fn facts(&self) -> &Facts {
        &self.facts
    }

    fn create_memory(&mut self, bytes: u64) -> Result<Memory, Errno> {
        self.calls += 1;
        assert!(bytes > 0 && bytes % 4096 == 0, "memory of {bytes:#x} bytes");
        assert!(bytes <= i32::MAX as u64 * 4096, "memory past i915's limit");
        if fire(&self.faults, fault::CREATE_MEMORY) {
            return Err(Errno::ENOMEM);
        }
        let id = self.id();
        let _ = self.live.borrow_mut().insert(id);
        Ok(Memory {
            id,
            size: bytes,
            live: self.live.clone(),
            freed: self.freed.clone(),
        })
    }

    fn populate(&mut self, memory: &Memory) -> Result<(), Errno> {
        self.calls += 1;
        assert!(self.live.borrow().contains(&memory.id));
        if fire(&self.faults, fault::POPULATE) {
            return Err(Errno::EDQUOT);
        }
        Ok(())
    }

    fn vm_create(&mut self) -> Result<VmId, Errno> {
        self.calls += 1;
        if fire(&self.faults, fault::VM_CREATE) {
            return Err(Errno::ENOMEM);
        }
        let id = self.id();
        self.pt.current = id;
        // `Ppgtt::create` gives back what it took when a table fails.
        let ppgtt = Ppgtt::create(&mut self.pt).map_err(|_| Errno::ENOMEM)?;
        let _ = self.vms.insert(id, ppgtt);
        Ok(VmId(id))
    }

    fn vm_destroy(&mut self, vm: VmId) {
        self.calls += 1;
        let Some(ppgtt) = self.vms.remove(&vm.0) else {
            panic!("VM {} destroyed while not alive", vm.0);
        };
        self.pt.current = vm.0;
        ppgtt.destroy(&mut self.pt);
        // Every table goes with it, whatever was still mapped (D4).
        assert_eq!(
            self.pt.owned(vm.0),
            0,
            "VM {} destroyed, its page tables left",
            vm.0
        );
        assert!(
            !self.contexts.values().any(|context| context.vm == vm.0),
            "VM {} destroyed under a live hardware context",
            vm.0
        );
        assert!(
            self.batch_in(vm.0, 0, u64::MAX).is_none(),
            "VM {} destroyed with a batch running in it",
            vm.0
        );
        // Destroying the PPGTT takes its mappings with it.
        self.binds.retain(|(owner, _), _| *owner != vm.0);
    }

    fn bind(
        &mut self,
        vm: VmId,
        address: u64,
        memory: &Memory,
        bytes: u64,
        pat_index: u8,
    ) -> Result<(), Errno> {
        self.calls += 1;
        assert!(self.vms.contains_key(&vm.0), "bind into dead VM {}", vm.0);
        assert!(
            self.live.borrow().contains(&memory.id),
            "bind of freed memory"
        );
        assert_eq!(address % 4096, 0, "bind at {address:#x}");
        assert!(
            bytes > 0 && bytes <= memory.size,
            "bind of {bytes:#x} bytes"
        );
        assert!(
            address.checked_add(bytes).is_some_and(|end| end <= 1 << 48),
            "bind {address:#x}+{bytes:#x} past 48 bits"
        );
        assert!(
            pat_index <= self.facts.max_pat_index,
            "PAT index {pat_index}"
        );
        for (&(owner, at), &(length, _)) in &self.binds {
            assert!(
                owner != vm.0 || at >= address + bytes || address >= at + length,
                "bind {address:#x}+{bytes:#x} overlaps {at:#x}+{length:#x} in VM {owner}"
            );
        }
        if fire(&self.faults, fault::BIND) {
            return Err(Errno::ENOMEM);
        }
        if bytes <= MAPPED_MAX {
            self.map(vm.0, address, memory.id, bytes, pat_index)?;
        }
        let _ = self.binds.insert((vm.0, address), (bytes, memory.id));
        Ok(())
    }

    fn unbind(&mut self, vm: VmId, address: u64, bytes: u64) {
        self.calls += 1;
        let Some((bound, memory)) = self.binds.remove(&(vm.0, address)) else {
            panic!("unbind of {address:#x} in VM {}, which is not bound", vm.0);
        };
        let running = self.in_flight.iter().filter(|run| run.vm == vm.0);
        let exposed: Vec<RequestId> = running.map(|run| run.id).collect();
        if !exposed.is_empty() {
            self.exposed
                .borrow_mut()
                .entry(memory)
                .or_default()
                .extend(exposed);
        }
        if let Some(ppgtt) = self.vms.get(&vm.0).copied() {
            self.pt.current = vm.0;
            ppgtt
                .clear_range(&mut self.pt, address, bytes)
                .expect("an unbind inside 48 bits");
            assert_eq!(
                ppgtt.translate(&mut self.pt, address),
                None,
                "{address:#x} mapped after its unbind"
            );
        }
        assert!(
            bytes >= bound,
            "unbind of {bytes:#x} of a {bound:#x} binding"
        );
        for &(owner, at) in self.binds.keys() {
            assert!(
                owner != vm.0 || at >= address.saturating_add(bytes) || at < address,
                "unbind of {address:#x}+{bytes:#x} reaches the binding at {at:#x}"
            );
        }
        if let Some(id) = self.batch_in(vm.0, address, bound) {
            panic!("unbind of {address:#x} under request {id}'s batch");
        }
    }

    fn context_create(
        &mut self,
        vm: VmId,
        engine: Engine,
        params: ContextParams,
    ) -> Result<HwContext, Errno> {
        self.calls += 1;
        assert!(self.vms.contains_key(&vm.0), "context in dead VM {}", vm.0);
        assert!(ENGINES.contains(&engine), "context on {engine:?}");
        assert_eq!(params, ContextParams::DEFAULT);
        if fire(&self.faults, fault::CONTEXT_CREATE) {
            return Err(Errno::ENOMEM);
        }
        let id = self.id();
        let _ = self.contexts.insert(id, HwState { vm: vm.0, last: 0 });
        Ok(HwContext(id))
    }

    fn context_destroy(&mut self, context: HwContext) {
        self.calls += 1;
        assert!(
            self.contexts.remove(&context.0).is_some(),
            "context {} destroyed while not alive",
            context.0
        );
        assert!(
            !self.in_flight.iter().any(|run| run.context == context.0),
            "context {} destroyed with a request on it",
            context.0
        );
    }

    fn submit(
        &mut self,
        context: HwContext,
        batch: u64,
        length: u32,
        request: RequestId,
    ) -> Result<(), SubmitError> {
        self.calls += 1;
        let Some(state) = self.contexts.get_mut(&context.0) else {
            panic!("submit to dead context {}", context.0);
        };
        assert!(
            request > state.last,
            "request {request} after {}",
            state.last
        );
        assert!(length > 0, "an empty batch");
        let vm = state.vm;
        let inside = self.binds.iter().any(|(&(owner, at), &(bytes, _))| {
            owner == vm && at <= batch && batch + u64::from(length) <= at + bytes
        });
        assert!(
            inside,
            "batch {batch:#x}+{length:#x} not inside a binding of VM {vm}"
        );
        assert!(
            self.submitted.insert(request),
            "request {request} submitted twice"
        );
        if fire(&self.faults, fault::SUBMIT_FULL) {
            let _ = self.submitted.remove(&request);
            return Err(SubmitError::Full);
        }
        if fire(&self.faults, fault::SUBMIT_FAILED) {
            return Err(SubmitError::Failed(Errno::EIO));
        }
        if let Some(state) = self.contexts.get_mut(&context.0) {
            state.last = request;
        }
        if self.auto {
            self.done.push((request, Completion::Ok));
        } else {
            self.in_flight.push(InFlight {
                id: request,
                context: context.0,
                vm,
                batch,
                length,
            });
        }
        Ok(())
    }

    fn service(&mut self, _now_ns: u64, _interrupt: bool) {
        self.calls += 1;
    }

    fn next_deadline(&self) -> Option<u64> {
        None
    }

    fn completed(&mut self, sink: &mut dyn FnMut(RequestId, Completion)) {
        self.calls += 1;
        for (id, completion) in self.done.drain(..) {
            sink(id, completion);
        }
    }

    fn timestamp(&mut self) -> u64 {
        0x1234_5678_9abc
    }
}

// ---------------------------------------------------------------------------
// The host: a model of the chardev core and the programs' descriptors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fd {
    /// One of this driver's dmabufs, by cookie.
    Dmabuf(u64),
    /// A sync file of this driver's (or, at cookie 0, the kernel's own
    /// signalled fence).
    Sync(u64),
    /// Another driver's dmabuf.
    ForeignDmabuf,
}

#[derive(Default)]
struct Process {
    fds: BTreeMap<i32, Fd>,
    next_fd: i32,
}

impl Process {
    fn install(&mut self, what: Fd) -> i32 {
        self.next_fd += 1;
        let fd = self.next_fd + 2;
        let _ = self.fds.insert(fd, what);
        fd
    }
}

#[derive(Debug, Default)]
struct DmabufState {
    /// Descriptors on it, every process's.
    fds: u32,
    /// Its last descriptor closed; the release not yet delivered.
    going: bool,
    /// The memory it was made over.
    memory: u32,
    /// Its fence slot's newest kernel fence.
    slot: Option<u64>,
}

struct FakeHost {
    arena: Vec<u8>,
    procs: [Process; PROCS],
    request_proc: BTreeMap<u64, usize>,
    replies: BTreeMap<u64, i32>,
    /// mmap requests and the pages they asked for.
    maps: BTreeMap<u64, u32>,
    dmabufs: BTreeMap<u64, DmabufState>,
    /// Every kernel fence the driver made, and its status once signalled.
    fences: BTreeMap<u64, Option<i32>>,
    abandoned: BTreeSet<u64>,
    now: u64,
    privileged: bool,
    faults: Rc<Cell<u32>>,
    copies: u64,
    copied: u64,
    said: u64,
    /// The exec-object list of the execbuffer op being run, and how many of
    /// its bytes the driver has read in the op (D3: each at most once).
    list: Option<(u64, u64)>,
    list_read: u64,
    /// A second thread of the program, rewriting the list between the
    /// driver's reads (`fault::RACE_EXEC`).
    race: Option<Race>,
}

/// A rewrite of an exec-object list by another thread of its program.
struct Race {
    /// The list.
    list: (u64, u64),
    /// Reads of the list to let through before rewriting it.
    after: u32,
    /// Where, and what.
    at: u64,
    bytes: Vec<u8>,
}

impl FakeHost {
    fn end(&self) -> u64 {
        BASE + self.arena.len() as u64
    }

    /// Room for `bytes` in the program's memory; its address, or an address
    /// past the end once the arena is full.
    fn alloc(&mut self, bytes: &[u8]) -> u64 {
        let at = self.arena.len().next_multiple_of(8);
        if at + bytes.len().max(8) > ARENA_MAX {
            return self.end() + 0x1000;
        }
        self.arena.resize(at + bytes.len().max(8), 0);
        self.arena[at..at + bytes.len()].copy_from_slice(bytes);
        BASE + at as u64
    }

    fn range(&self, address: u64, length: usize) -> Option<core::ops::Range<usize>> {
        let start = usize::try_from(address.checked_sub(BASE)?).ok()?;
        let end = start.checked_add(length)?;
        (end <= self.arena.len()).then_some(start..end)
    }

    fn read(&self, address: u64, length: usize) -> Vec<u8> {
        self.range(address, length)
            .map_or_else(|| vec![0; length], |range| self.arena[range].to_vec())
    }

    fn proc_of(&self, request: u64) -> usize {
        *self
            .request_proc
            .get(&request)
            .unwrap_or_else(|| panic!("request {request} was never made"))
    }

    fn count(&mut self, bytes: usize) {
        self.copies += 1;
        self.copied += bytes as u64;
    }
}

impl Host<Memory> for FakeHost {
    fn copy_in(&mut self, request: u64, address: u64, out: &mut [u8]) -> Result<(), Errno> {
        let _ = self.proc_of(request);
        self.count(out.len());
        if self.abandoned.contains(&request) {
            return Err(Errno::EFAULT);
        }
        let range = self.range(address, out.len()).ok_or(Errno::EFAULT)?;
        out.copy_from_slice(&self.arena[range]);
        let end = address + out.len() as u64;
        if let Some((list, length)) = self.list {
            let (from, to) = (address.max(list), end.min(list + length));
            self.list_read += to.saturating_sub(from);
        }
        if let Some(race) = &mut self.race
            && address < race.list.0 + race.list.1
            && race.list.0 < end
        {
            race.after = race.after.saturating_sub(1);
            if race.after == 0
                && let Some(race) = self.race.take()
                && let Some(range) = self.range(race.at, race.bytes.len())
            {
                self.arena[range].copy_from_slice(&race.bytes);
            }
        }
        Ok(())
    }

    fn copy_out(&mut self, request: u64, address: u64, bytes: &[u8]) -> Result<(), Errno> {
        let _ = self.proc_of(request);
        self.count(bytes.len());
        if self.abandoned.contains(&request) {
            return Err(Errno::EFAULT);
        }
        let range = self.range(address, bytes.len()).ok_or(Errno::EFAULT)?;
        self.arena[range].copy_from_slice(bytes);
        Ok(())
    }

    fn reply(&mut self, request: u64, status: i32) {
        let _ = self.proc_of(request);
        assert!(
            (-4095..=0).contains(&status),
            "request {request} answered {status}"
        );
        assert!(
            self.replies.insert(request, status).is_none(),
            "request {request} answered twice"
        );
    }

    fn reply_map(
        &mut self,
        request: u64,
        memory: &Memory,
        offset: u64,
        _write_combining: bool,
    ) -> Result<(), Errno> {
        let pages = *self
            .maps
            .get(&request)
            .unwrap_or_else(|| panic!("reply_map to request {request}, not an mmap"));
        assert!(
            memory.live.borrow().contains(&memory.id),
            "mmap of freed memory"
        );
        assert_eq!(offset % 4096, 0);
        assert!(
            offset + u64::from(pages) * 4096 <= memory.size,
            "mmap of {pages} pages at {offset:#x} of {:#x} bytes",
            memory.size
        );
        if fire(&self.faults, fault::REPLY_MAP) {
            return Err(Errno::ENOMEM);
        }
        self.reply(request, 0);
        Ok(())
    }

    fn dmabuf_install(
        &mut self,
        request: u64,
        memory: &Memory,
        cookie: u64,
        _writable: bool,
        _cloexec: bool,
    ) -> Result<Installed, Errno> {
        let proc = self.proc_of(request);
        assert!(
            memory.live.borrow().contains(&memory.id),
            "dmabuf of freed memory"
        );
        if fire(&self.faults, fault::DMABUF_INSTALL) {
            return Err(Errno::EMFILE);
        }
        let made = match self.dmabufs.get_mut(&cookie) {
            Some(state) if state.going => return Err(Errno::EAGAIN),
            Some(state) => {
                assert_eq!(
                    state.memory, memory.id,
                    "dmabuf cookie {cookie:#x} names another object's memory"
                );
                state.fds += 1;
                false
            }
            None => {
                let _ = self.dmabufs.insert(
                    cookie,
                    DmabufState {
                        fds: 1,
                        going: false,
                        memory: memory.id,
                        slot: None,
                    },
                );
                true
            }
        };
        let fd = self.procs[proc].install(Fd::Dmabuf(cookie));
        Ok(Installed { fd, made })
    }

    fn dmabuf_resolve(&mut self, request: u64, fd: i32) -> Result<u64, Errno> {
        let proc = self.proc_of(request);
        match self.procs[proc].fds.get(&fd) {
            Some(Fd::Dmabuf(cookie)) => Ok(*cookie),
            _ => Err(Errno::EBADF),
        }
    }

    fn dmabuf_fence(&mut self, dmabuf: u64, fence: u64) -> Result<(), Errno> {
        let Some(state) = self.dmabufs.get_mut(&dmabuf) else {
            return Err(Errno::EBADF);
        };
        if fire(&self.faults, fault::DMABUF_FENCE) {
            return Err(Errno::ENOMEM);
        }
        state.slot = Some(fence);
        assert!(
            self.fences.insert(fence, None).is_none(),
            "kernel fence {fence} made twice"
        );
        Ok(())
    }

    fn sync_install(&mut self, request: u64, fence: u64, _cloexec: bool) -> Result<i32, Errno> {
        let proc = self.proc_of(request);
        if fire(&self.faults, fault::SYNC_INSTALL) {
            return Err(Errno::EMFILE);
        }
        assert!(fence != 0, "a sync file with cookie 0");
        assert!(
            self.fences.insert(fence, None).is_none(),
            "kernel fence {fence} made twice"
        );
        Ok(self.procs[proc].install(Fd::Sync(fence)))
    }

    fn sync_resolve(&mut self, request: u64, fd: i32) -> Result<(u64, bool), Errno> {
        let proc = self.proc_of(request);
        match self.procs[proc].fds.get(&fd) {
            Some(Fd::Sync(0)) => Ok((0, true)),
            Some(Fd::Sync(cookie)) => Ok((
                *cookie,
                self.fences.get(cookie).is_some_and(Option::is_some),
            )),
            _ => Err(Errno::EINVAL),
        }
    }

    fn sync_signal(&mut self, fence: u64, status: i32) {
        assert!(
            (-4095..=0).contains(&status),
            "fence {fence} signalled {status}"
        );
        match self.fences.get_mut(&fence) {
            Some(slot @ None) => *slot = Some(status),
            Some(Some(_)) => panic!("kernel fence {fence} signalled twice"),
            None => panic!("kernel fence {fence} signalled, never made"),
        }
    }

    fn now_ns(&self) -> u64 {
        self.now
    }

    fn privileged(&self, _request: u64) -> bool {
        self.privileged
    }

    fn say(&mut self, _level: Level, line: core::fmt::Arguments<'_>) {
        struct Sink(usize);
        impl core::fmt::Write for Sink {
            fn write_str(&mut self, text: &str) -> core::fmt::Result {
                self.0 += text.len();
                Ok(())
            }
        }
        let mut sink = Sink(0);
        let _ = core::fmt::write(&mut sink, line);
        self.said += sink.0 as u64;
    }
}

// ---------------------------------------------------------------------------
// The harness's own records of what each file was told
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Gem,
    Sync,
    Context,
    Vm,
}

struct FileRec {
    id: u64,
    proc: usize,
    open: bool,
    /// Ids it was told, by kind.
    names: BTreeMap<Kind, Vec<u32>>,
    offsets: Vec<u64>,
}

thread_local! {
    /// The front end the last run used, to be emptied for the next.
    static FRONTEND: RefCell<Option<Box<Frontend<Model>>>> = const { RefCell::new(None) };
}

struct Rig {
    frontend: Box<Frontend<Model>>,
    host: FakeHost,
    faults: Rc<Cell<u32>>,
    files: Vec<FileRec>,
    next_file: u64,
    next_request: u64,
    /// Ids a file was told and has since let go of.
    dead: BTreeMap<Kind, Vec<u32>>,
    /// Requests issued and not yet answered, and their file.
    pending: BTreeMap<u64, u64>,
    calls: usize,
}

/// Say what a call came to, when `INTEL_UAPI_TRACE` is set: for checking
/// what a seed or a crashing input does (`tools/common/gen/
/// seed-intel-uapi-fuzz-corpus.py`), never while fuzzing.
fn trace(line: core::fmt::Arguments<'_>) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("INTEL_UAPI_TRACE").is_some()) {
        eprintln!("intel_uapi: {line}");
    }
}

/// The ioctls that may be held.
fn holdable(cmd: u32) -> bool {
    let number = uapi::ioc_nr(cmd);
    number == uapi::core_nr::SYNCOBJ_WAIT
        || number == uapi::core_nr::SYNCOBJ_TIMELINE_WAIT
        || number == uapi::DRM_COMMAND_BASE + uapi::nr::GEM_WAIT
        || number == uapi::DRM_COMMAND_BASE + uapi::nr::GEM_EXECBUFFER2
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .map_or(0, |b| u32::from_le_bytes(b.try_into().expect("4")))
}

impl Rig {
    fn new(auto: bool, privileged: bool, trace: bool) -> Self {
        let faults = Rc::new(Cell::new(0));
        // The tables are megabytes: one allocation per thread, emptied for
        // each run, rather than allocated and zeroed afresh.
        let mut frontend = FRONTEND
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_else(|| Box::new(Frontend::new(Model::new(faults.clone(), auto))));
        frontend.state = Tables::new();
        frontend.backend = Model::new(faults.clone(), auto);
        frontend.set_trace(trace);
        Rig {
            frontend,
            host: FakeHost {
                arena: Vec::new(),
                procs: Default::default(),
                request_proc: BTreeMap::new(),
                replies: BTreeMap::new(),
                maps: BTreeMap::new(),
                dmabufs: BTreeMap::new(),
                fences: BTreeMap::new(),
                abandoned: BTreeSet::new(),
                now: 1_000,
                privileged,
                faults: faults.clone(),
                copies: 0,
                copied: 0,
                said: 0,
                list: None,
                list_read: 0,
                race: None,
            },
            faults,
            files: Vec::new(),
            next_file: 100,
            next_request: 1,
            dead: BTreeMap::new(),
            pending: BTreeMap::new(),
            calls: 0,
        }
    }

    fn request(&mut self, proc: usize) -> u64 {
        self.next_request += 1;
        let _ = self.host.request_proc.insert(self.next_request, proc);
        self.next_request
    }

    /// Start counting one call's work.
    fn begin(&mut self) {
        self.host.copies = 0;
        self.host.copied = 0;
        self.frontend.backend.calls = 0;
    }

    /// The work one call did stayed under its bounds.
    fn end(&self, what: &str) {
        assert!(
            self.host.copies <= MAX_COPIES && self.host.copied <= MAX_COPIED,
            "{what}: {} copies, {} bytes",
            self.host.copies,
            self.host.copied
        );
        assert!(
            self.frontend.backend.calls <= MAX_BACKEND_CALLS,
            "{what}: {} back-end calls",
            self.frontend.backend.calls
        );
    }

    /// The file an op names: an open one, mostly, or one closed or never
    /// opened. Its index in [`Rig::files`], if it has one, and its id.
    fn file(&mut self, input: &mut Input<'_>) -> (Option<usize>, u64) {
        let open: Vec<usize> = (0..self.files.len())
            .filter(|&i| self.files[i].open)
            .collect();
        let byte = input.u8();
        if byte < 240 && !open.is_empty() {
            let index = open[usize::from(byte) % open.len()];
            return (Some(index), self.files[index].id);
        }
        if byte < 248 && !self.files.is_empty() {
            let index = usize::from(byte) % self.files.len();
            return (Some(index), self.files[index].id);
        }
        (None, u64::from(byte) << 32)
    }

    fn proc_of_file(&self, file: Option<usize>) -> usize {
        file.map_or(0, |index| self.files[index].proc)
    }

    /// A name of `kind` for `file` to use: its own, another open file's, one
    /// let go of, or a number at an edge.
    fn name(&self, input: &mut Input<'_>, kind: Kind, file: Option<usize>) -> u32 {
        let own = file
            .and_then(|index| self.files[index].names.get(&kind))
            .cloned()
            .unwrap_or_default();
        let mode = input.u8() % 8;
        if mode <= 4 && !own.is_empty() {
            return own[input.pick(own.len())];
        }
        if mode == 5 {
            let others: Vec<u32> = self
                .files
                .iter()
                .enumerate()
                .filter(|(index, rec)| Some(*index) != file && rec.open)
                .flat_map(|(_, rec)| rec.names.get(&kind).cloned().unwrap_or_default())
                .collect();
            if !others.is_empty() {
                return others[input.pick(others.len())];
            }
        }
        if mode == 6
            && let Some(dead) = self.dead.get(&kind)
            && !dead.is_empty()
        {
            return dead[input.pick(dead.len())];
        }
        input.word()
    }

    /// A descriptor of `proc`: one it holds, mostly, or a number it does not.
    fn fd(&self, input: &mut Input<'_>, proc: usize) -> i32 {
        let fds: Vec<i32> = self.host.procs[proc].fds.keys().copied().collect();
        if input.u8() % 8 != 7 && !fds.is_empty() {
            return fds[input.pick(fds.len())];
        }
        input.word() as i32
    }

    /// A pointer to `valid` (`length` bytes there), mostly, or one that is
    /// not: null, short of the end, past the end, wrapping, misaligned.
    fn ptr(&self, input: &mut Input<'_>, valid: u64, length: usize) -> u64 {
        match input.u8() % 16 {
            0..=10 => valid,
            11 => 0,
            12 => self.host.end().saturating_sub((length / 2).max(1) as u64),
            13 => u64::MAX - 7,
            14 => 1 << 47,
            _ => valid.wrapping_add(1),
        }
    }

    /// Issue ioctl `cmd` on file `file` with `arg` at a pointer `input`
    /// chooses; what it answered (`None` while held) and the argument's
    /// bytes after it.
    fn ioctl(
        &mut self,
        input: &mut Input<'_>,
        file: (Option<usize>, u64),
        cmd: u32,
        arg: &[u8],
    ) -> (Option<i32>, Vec<u8>) {
        let proc = self.proc_of_file(file.0);
        let valid = self.host.alloc(arg);
        // The argument itself is a bad pointer one call in sixteen.
        let address = if input.u8() % 16 == 15 {
            self.ptr(input, valid, arg.len())
        } else {
            valid
        };
        let request = self.request(proc);
        self.begin();
        self.frontend
            .ioctl(&mut self.host, request, file.1, cmd, address);
        self.end("ioctl");
        let after = self.host.read(valid, arg.len());
        let status = self.host.replies.get(&request).copied();
        trace(format_args!(
            "file {} ioctl {cmd:#010x} (nr {:#04x}) -> {status:?}",
            file.1,
            uapi::ioc_nr(cmd)
        ));
        if file.0.is_none_or(|index| !self.files[index].open) {
            assert_eq!(status, Some(-9), "ioctl on a file not open: {status:?}");
        }
        if status.is_none() {
            assert!(holdable(cmd), "ioctl {cmd:#x} held");
            let _ = self.pending.insert(request, file.1);
        }
        (status, after)
    }

    /// File `file` was told `id` of `kind`: no other open file may hold it.
    fn told(&mut self, file: Option<usize>, kind: Kind, id: u32) {
        let Some(index) = file else { return };
        if kind == Kind::Context && id == 0 {
            return;
        }
        for (other, rec) in self.files.iter().enumerate() {
            if other != index && rec.open {
                assert!(
                    !rec.names.get(&kind).is_some_and(|ids| ids.contains(&id)),
                    "{kind:?} {id} told to file {} while file {} holds it",
                    self.files[index].id,
                    rec.id
                );
            }
        }
        let names = self.files[index].names.entry(kind).or_default();
        if !names.contains(&id) {
            names.push(id);
        }
    }

    /// A call by `file` that named `id` of `kind` succeeded: it is not a
    /// name another open file holds.
    fn named(&self, file: Option<usize>, kind: Kind, id: u32) {
        let Some(index) = file else { return };
        if kind == Kind::Context && id == 0 {
            return;
        }
        if self.files[index]
            .names
            .get(&kind)
            .is_some_and(|ids| ids.contains(&id))
        {
            return;
        }
        for rec in &self.files {
            if rec.open && rec.id != self.files[index].id {
                assert!(
                    !rec.names.get(&kind).is_some_and(|ids| ids.contains(&id)),
                    "file {} used {kind:?} {id}, which file {} holds",
                    self.files[index].id,
                    rec.id
                );
            }
        }
    }

    /// Bring the records in line with the tables: a name a file no longer
    /// holds there is let go of (closed, destroyed, or never really made).
    fn reconcile(&mut self) {
        let state = &self.frontend.state;
        for rec in &mut self.files {
            if !rec.open {
                continue;
            }
            let slot = state.file_slot(rec.id).expect("an open file has a slot");
            for (kind, ids) in &mut rec.names {
                ids.retain(|&id| {
                    let held = match kind {
                        Kind::Gem => state.object_of(slot, id).is_some(),
                        Kind::Sync => state.syncobj_of(slot, id).is_some(),
                        Kind::Context => state.context_of(slot, id).is_some(),
                        Kind::Vm => state.vm_of(slot, u64::from(id)).is_some(),
                    };
                    if !held {
                        self.dead.entry(*kind).or_default().push(id);
                    }
                    held
                });
            }
        }
        for dead in self.dead.values_mut() {
            if dead.len() > 64 {
                let _ = dead.drain(..dead.len() - 64);
            }
        }
    }

    /// Release the dmabufs whose last descriptor closed.
    fn deliver_releases(&mut self) {
        let going: Vec<u64> = self
            .host
            .dmabufs
            .iter()
            .filter(|(_, state)| state.going)
            .map(|(cookie, _)| *cookie)
            .collect();
        for cookie in going {
            let _ = self.host.dmabufs.remove(&cookie);
            self.begin();
            self.frontend.dmabuf_release(&mut self.host, cookie);
            self.end("dmabuf_release");
        }
    }

    fn close_fd(&mut self, proc: usize, fd: i32) {
        if let Some(Fd::Dmabuf(cookie)) = self.host.procs[proc].fds.remove(&fd)
            && let Some(state) = self.host.dmabufs.get_mut(&cookie)
        {
            state.fds -= 1;
            if state.fds == 0 {
                state.going = true;
            }
        }
    }

    fn open(&mut self, proc: usize) {
        let request = self.request(proc);
        self.next_file += 1;
        let id = self.next_file;
        self.begin();
        self.frontend
            .open(&mut self.host, request, id, 100 + proc as u32);
        self.end("open");
        let status = self.host.replies[&request];
        trace(format_args!("proc {proc} open file {id} -> {status}"));
        if status == 0 {
            self.files.push(FileRec {
                id,
                proc,
                open: true,
                names: BTreeMap::new(),
                offsets: Vec::new(),
            });
        } else {
            assert!(
                self.files.iter().filter(|rec| rec.open).count() >= limits::FILES
                    || self.frontend.state.vms.len() == limits::VMS
                    || self.frontend.state.contexts.len() == limits::CONTEXTS,
                "open refused {status} with room"
            );
        }
    }

    fn release(&mut self, index: usize) {
        if !self.files[index].open {
            return;
        }
        let id = self.files[index].id;
        self.files[index].open = false;
        for (kind, ids) in std::mem::take(&mut self.files[index].names) {
            self.dead.entry(kind).or_default().extend(ids);
        }
        self.begin();
        self.frontend.release(&mut self.host, id);
        self.end("release");
        trace(format_args!("file {id} released"));
        // Its held calls are dropped, not answered: the core abandons them.
        self.pending.retain(|_, file| *file != id);
    }

    fn service(&mut self, interrupt: bool) {
        self.begin();
        self.frontend.service(&mut self.host, interrupt);
        self.end("service");
    }

    fn settle_answers(&mut self) {
        let replies = &self.host.replies;
        self.pending
            .retain(|request, _| !replies.contains_key(request));
    }
}

// ---------------------------------------------------------------------------
// The ops
// ---------------------------------------------------------------------------

/// Commands for the raw op: every one the driver defines, some it refuses
/// by table, and some of other drivers.
const RAW_COMMANDS: &[u32] = &[
    ioctl::VERSION,
    ioctl::GET_CAP,
    ioctl::SET_CLIENT_CAP,
    ioctl::GEM_CLOSE,
    ioctl::GEM_FLINK,
    ioctl::GEM_OPEN,
    ioctl::PRIME_HANDLE_TO_FD,
    ioctl::PRIME_FD_TO_HANDLE,
    ioctl::SYNCOBJ_CREATE,
    ioctl::SYNCOBJ_DESTROY,
    ioctl::SYNCOBJ_HANDLE_TO_FD,
    ioctl::SYNCOBJ_FD_TO_HANDLE,
    ioctl::SYNCOBJ_WAIT,
    ioctl::SYNCOBJ_RESET,
    ioctl::SYNCOBJ_SIGNAL,
    ioctl::SYNCOBJ_TIMELINE_WAIT,
    ioctl::SYNCOBJ_QUERY,
    ioctl::SYNCOBJ_TRANSFER,
    ioctl::SYNCOBJ_TIMELINE_SIGNAL,
    ioctl::SYNCOBJ_EVENTFD,
    ioctl::GETPARAM,
    ioctl::QUERY,
    ioctl::GEM_CREATE,
    ioctl::GEM_CREATE_EXT,
    ioctl::GEM_MMAP_OFFSET,
    ioctl::GEM_MMAP,
    ioctl::GEM_PREAD,
    ioctl::GEM_PWRITE,
    ioctl::GEM_EXECBUFFER2,
    ioctl::GEM_EXECBUFFER2_WR,
    ioctl::GEM_WAIT,
    ioctl::GEM_BUSY,
    ioctl::GEM_THROTTLE,
    ioctl::GEM_CONTEXT_CREATE_EXT,
    ioctl::GEM_CONTEXT_CREATE,
    ioctl::GEM_CONTEXT_DESTROY,
    ioctl::GEM_CONTEXT_GETPARAM,
    ioctl::GEM_CONTEXT_SETPARAM,
    ioctl::GEM_VM_CREATE,
    ioctl::GEM_VM_DESTROY,
    ioctl::GET_RESET_STATS,
    ioctl::REG_READ,
    ioctl::GEM_SET_TILING,
    ioctl::GEM_GET_TILING,
    ioctl::GEM_SET_DOMAIN,
    ioctl::GEM_SW_FINISH,
    ioctl::GEM_SET_CACHING,
    ioctl::GEM_GET_CACHING,
    ioctl::GEM_MADVISE,
    ioctl::GEM_USERPTR,
    ioctl::GEM_GET_APERTURE,
    ioctl::PERF_OPEN,
    ioctl::DMA_BUF_SYNC,
    ioctl::DMA_BUF_EXPORT_SYNC_FILE,
    // SET_CLIENT_NAME, GEM_CHANGE_HANDLE, an unknown core number, the
    // first i915 number past the table, the end of the driver range.
    0xc010_64d1,
    0xc008_64d2,
    0xc008_645f,
    0xc008_647d,
    0xc008_649f,
];

fn run_op(rig: &mut Rig, input: &mut Input<'_>, op: u8) {
    match op % 32 {
        0 => {
            let proc = input.pick(PROCS);
            rig.open(proc);
        }
        1 => {
            if let (Some(index), _) = rig.file(input) {
                rig.release(index);
            }
        }
        2 => gem_create(rig, input),
        3 => gem_create_ext(rig, input),
        4 => {
            let file = rig.file(input);
            let handle = rig.name(input, Kind::Gem, file.0);
            let pad = if input.u8() == 0xff { input.u32() } else { 0 };
            let args = uapi::GemClose { handle, pad }.to_bytes();
            let (status, _) = rig.ioctl(input, file, ioctl::GEM_CLOSE, &args);
            if status == Some(0) {
                rig.named(file.0, Kind::Gem, handle);
            }
        }
        5 => gem_small(rig, input),
        6 => {
            let file = rig.file(input);
            let bo_handle = rig.name(input, Kind::Gem, file.0);
            let flags = if input.u8() == 0xff { input.u32() } else { 0 };
            let timeout_ns = input.timeout();
            let args = uapi::GemWait {
                bo_handle,
                flags,
                timeout_ns,
            }
            .to_bytes();
            let (status, _) = rig.ioctl(input, file, ioctl::GEM_WAIT, &args);
            if status == Some(0) {
                rig.named(file.0, Kind::Gem, bo_handle);
            }
        }
        7 => mmap_offset(rig, input),
        8 => mmap(rig, input),
        9 => prime_export(rig, input),
        10 => prime_import(rig, input),
        11 | 12 => execbuffer(rig, input),
        13 => context_create(rig, input),
        14 => {
            let file = rig.file(input);
            let id = rig.name(input, Kind::Context, file.0);
            let pad = if input.u8() == 0xff { input.u32() } else { 0 };
            let mut args = [0_u8; 8];
            args[..4].copy_from_slice(&id.to_le_bytes());
            args[4..].copy_from_slice(&pad.to_le_bytes());
            let (status, _) = rig.ioctl(input, file, ioctl::GEM_CONTEXT_DESTROY, &args);
            if status == Some(0) {
                rig.named(file.0, Kind::Context, id);
            }
        }
        15 => context_param(rig, input),
        16 => {
            let file = rig.file(input);
            let mut args = VmControl::default();
            if input.u8() == 0xff {
                args.flags = input.u32();
                let ext = extension_chain(rig, input);
                args.extensions = ext;
            }
            let (status, after) = rig.ioctl(input, file, ioctl::GEM_VM_CREATE, &args.to_bytes());
            if status == Some(0) {
                let id = VmControl::from_bytes(&after).expect("vm").vm_id;
                rig.told(file.0, Kind::Vm, id);
            }
        }
        17 => {
            let file = rig.file(input);
            let vm_id = rig.name(input, Kind::Vm, file.0);
            let mut args = VmControl {
                vm_id,
                ..VmControl::default()
            };
            if input.u8() == 0xff {
                args.flags = input.u32();
                args.extensions = input.quad();
            }
            let (status, _) = rig.ioctl(input, file, ioctl::GEM_VM_DESTROY, &args.to_bytes());
            if status == Some(0) {
                rig.named(file.0, Kind::Vm, vm_id);
            }
        }
        18 => {
            let file = rig.file(input);
            let flags = match input.u8() % 4 {
                0 | 1 => 0,
                2 => 1,
                _ => input.u32(),
            };
            let args = uapi::SyncobjCreate { handle: 0, flags }.to_bytes();
            let (status, after) = rig.ioctl(input, file, ioctl::SYNCOBJ_CREATE, &args);
            if status == Some(0) {
                rig.told(file.0, Kind::Sync, le32(&after, 0));
            }
        }
        19 => {
            let file = rig.file(input);
            let handle = rig.name(input, Kind::Sync, file.0);
            let pad = if input.u8() == 0xff { input.u32() } else { 0 };
            let args = uapi::SyncobjDestroy { handle, pad }.to_bytes();
            let (status, _) = rig.ioctl(input, file, ioctl::SYNCOBJ_DESTROY, &args);
            if status == Some(0) {
                rig.named(file.0, Kind::Sync, handle);
            }
        }
        20 => syncobj_fd(rig, input),
        21 => {
            let file = rig.file(input);
            let src_handle = rig.name(input, Kind::Sync, file.0);
            let dst_handle = rig.name(input, Kind::Sync, file.0);
            let args = SyncobjTransfer {
                src_handle,
                dst_handle,
                src_point: point(input),
                dst_point: point(input),
                flags: if input.u8() % 4 == 0 { input.u32() } else { 0 },
                pad: if input.u8() == 0xff { 1 } else { 0 },
            };
            let (status, _) = rig.ioctl(input, file, ioctl::SYNCOBJ_TRANSFER, &args.to_bytes());
            if status == Some(0) {
                rig.named(file.0, Kind::Sync, src_handle);
                rig.named(file.0, Kind::Sync, dst_handle);
            }
        }
        22 => syncobj_array(rig, input),
        23 => syncobj_wait(rig, input),
        24 => info(rig, input),
        25 => raw(rig, input),
        26 => descriptors(rig, input),
        27 => dmabuf_import(rig, input),
        28 => {
            // The GPU finishes, or hangs, some of one context's work.
            let contexts: Vec<u32> = rig
                .frontend
                .backend
                .in_flight
                .iter()
                .map(|run| run.context)
                .collect();
            if !contexts.is_empty() {
                let context = contexts[input.pick(contexts.len())];
                let count = input.count(4, 64).max(1);
                let hung = input.u8() % 8 == 0;
                rig.frontend.backend.finish(context, count, hung);
                trace(format_args!(
                    "context {context}: {count} finished, hung {hung}"
                ));
            }
            rig.service(true);
        }
        29 => {
            rig.host.now = rig.host.now.saturating_add(match input.u8() % 4 {
                0 => 1,
                1 => 1_000_000,
                2 => 3_000_000_000,
                _ => input.u64() >> 8,
            });
            let interrupt = input.bool();
            rig.deliver_releases();
            rig.service(interrupt);
        }
        30 => {
            // A held call: its program takes a signal, or gives up.
            let held: Vec<u64> = rig.pending.keys().copied().collect();
            if !held.is_empty() {
                let request = held[input.pick(held.len())];
                if input.bool() {
                    rig.begin();
                    rig.frontend.interrupt(&mut rig.host, request);
                    rig.end("interrupt");
                    if let Some(status) = rig.host.replies.get(&request) {
                        assert!(*status == -512 || *status <= 0);
                    }
                } else {
                    let _ = rig.host.abandoned.insert(request);
                }
            }
        }
        _ => {
            let mask = input.u16();
            rig.faults.set(u32::from(mask));
        }
    }
}

fn point(input: &mut Input<'_>) -> u64 {
    match input.u8() % 8 {
        0..=2 => 0,
        3 => 1,
        4 => 2,
        5 => u64::MAX,
        _ => u64::from(input.u8()),
    }
}

fn gem_create(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let args = GemCreate {
        size: input.size(),
        handle: input.word() & 0xff,
        pad: 0,
    };
    let (status, after) = rig.ioctl(input, file, ioctl::GEM_CREATE, &args.to_bytes());
    if status == Some(0) {
        let made = GemCreate::from_bytes(&after).expect("create");
        assert!(made.size >= args.size && made.size % 4096 == 0);
        rig.told(file.0, Kind::Gem, made.handle);
    }
}

/// A chain of `i915_user_extension`s in the program's memory: none, or a
/// few of names the input chooses, or a loop.
fn extension_chain(rig: &mut Rig, input: &mut Input<'_>) -> u64 {
    let count = input.count(3, 600);
    let looped = input.u8() % 16 == 0;
    let mut next = 0_u64;
    let mut first_at = 0;
    for index in 0..count {
        let mut ext = [0_u8; 32];
        ext[..8].copy_from_slice(&next.to_le_bytes());
        ext[8..12].copy_from_slice(&u32::from(input.u8() % 4).to_le_bytes());
        if input.u8() == 0xff {
            ext[12..16].copy_from_slice(&input.u32().to_le_bytes());
        }
        next = rig.host.alloc(&ext);
        if index == 0 {
            first_at = next;
        }
    }
    if looped && let Some(range) = rig.host.range(first_at, 8) {
        // The first link (the chain's last) points back at the head.
        rig.host.arena[range].copy_from_slice(&next.to_le_bytes());
    }
    if next == 0 {
        0
    } else {
        rig.ptr(input, next, 32)
    }
}

fn gem_create_ext(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let mut args = GemCreateExt {
        size: input.size(),
        handle: 0,
        flags: if input.u8() % 8 == 0 { input.word() } else { 0 },
        extensions: 0,
    };
    match input.u8() % 4 {
        0 => {}
        1 => args.extensions = extension_chain(rig, input),
        _ => {
            // MEMORY_REGIONS, as Mesa sends it: system memory, once.
            let count = input.count(2, 9);
            let mut regions = Vec::new();
            for _ in 0..count {
                let (class, instance) = match input.u8() % 4 {
                    0..=2 => (0_u16, 0_u16),
                    _ => (input.u16(), input.u16()),
                };
                regions.extend_from_slice(&class.to_le_bytes());
                regions.extend_from_slice(&instance.to_le_bytes());
            }
            let at = rig.host.alloc(&regions);
            let regions_ptr = rig.ptr(input, at, regions.len());
            let mut ext = [0_u8; 48];
            let next = if input.u8() % 8 == 0 {
                extension_chain(rig, input)
            } else {
                0
            };
            ext[..8].copy_from_slice(&next.to_le_bytes());
            ext[8..12].copy_from_slice(&0_u32.to_le_bytes());
            let num = if input.u8() % 8 == 0 {
                input.word()
            } else {
                count as u32
            };
            ext[36..40].copy_from_slice(&num.to_le_bytes());
            ext[40..48].copy_from_slice(&regions_ptr.to_le_bytes());
            let ext_at = rig.host.alloc(&ext);
            args.extensions = rig.ptr(input, ext_at, 48);
        }
    }
    let (status, after) = rig.ioctl(input, file, ioctl::GEM_CREATE_EXT, &args.to_bytes());
    if status == Some(0) {
        let made = GemCreateExt::from_bytes(&after).expect("create_ext");
        rig.told(file.0, Kind::Gem, made.handle);
    }
}

/// The GEM ioctls whose argument is a handle and a word or two.
fn gem_small(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let handle = rig.name(input, Kind::Gem, file.0);
    let a = input.word() % 4;
    let b = input.word();
    let (cmd, args): (u32, Vec<u8>) = match input.u8() % 10 {
        0 => (
            ioctl::GEM_BUSY,
            uapi::GemBusy { handle, busy: 0 }.to_bytes().to_vec(),
        ),
        1 => (
            ioctl::GEM_SET_TILING,
            uapi::SetTiling {
                handle,
                tiling_mode: a,
                stride: b & 0x7_ff80,
                swizzle_mode: 0,
            }
            .to_bytes()
            .to_vec(),
        ),
        2 => (
            ioctl::GEM_GET_TILING,
            uapi::GetTiling {
                handle,
                ..Default::default()
            }
            .to_bytes()
            .to_vec(),
        ),
        3 => (
            ioctl::GEM_SET_CACHING,
            uapi::Caching { handle, caching: a }.to_bytes().to_vec(),
        ),
        4 => (
            ioctl::GEM_GET_CACHING,
            uapi::Caching { handle, caching: 0 }.to_bytes().to_vec(),
        ),
        5 => (
            ioctl::GEM_SET_DOMAIN,
            uapi::SetDomain {
                handle,
                read_domains: b & 0xc1,
                write_domain: if a == 0 { b & 0xc1 } else { 0 },
            }
            .to_bytes()
            .to_vec(),
        ),
        6 => (ioctl::GEM_SW_FINISH, handle.to_le_bytes().to_vec()),
        7 => (
            ioctl::GEM_MADVISE,
            uapi::Madvise {
                handle,
                madv: a % 3,
                retained: 0,
            }
            .to_bytes()
            .to_vec(),
        ),
        8 => (ioctl::GEM_GET_APERTURE, vec![0; 16]),
        _ => (ioctl::GEM_THROTTLE, Vec::new()),
    };
    let (status, _) = rig.ioctl(input, file, cmd, &args);
    if status == Some(0) && cmd != ioctl::GEM_GET_APERTURE && cmd != ioctl::GEM_THROTTLE {
        rig.named(file.0, Kind::Gem, handle);
    }
}

fn mmap_offset(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let handle = rig.name(input, Kind::Gem, file.0);
    let flags = match input.u8() % 8 {
        0..=2 => 2,
        3..=5 => 1,
        6 => u64::from(input.u8() % 6),
        _ => input.quad(),
    };
    let extensions = if input.u8() % 16 == 0 {
        extension_chain(rig, input)
    } else {
        0
    };
    let args = GemMmapOffset {
        handle,
        pad: 0,
        offset: 0,
        flags,
        extensions,
    };
    let (status, after) = rig.ioctl(input, file, ioctl::GEM_MMAP_OFFSET, &args.to_bytes());
    if status == Some(0) {
        rig.named(file.0, Kind::Gem, handle);
        if let Some(index) = file.0 {
            let offset = GemMmapOffset::from_bytes(&after).expect("offset").offset;
            rig.files[index].offsets.push(offset);
        }
    }
}

fn mmap(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let offsets: Vec<u64> = rig
        .files
        .iter()
        .flat_map(|rec| rec.offsets.clone())
        .collect();
    let mut offset = match input.u8() % 8 {
        0..=5 if !offsets.is_empty() => offsets[input.pick(offsets.len())],
        _ => input.u64(),
    };
    if input.u8() % 4 == 0 {
        offset = offset.wrapping_add(u64::from(input.u8()) * 4096);
    }
    let pages = match input.u8() % 8 {
        0..=4 => 1,
        5 => 0,
        6 => u32::from(input.u8()),
        _ => input.u32(),
    };
    let proc = rig.proc_of_file(file.0);
    let request = rig.request(proc);
    let _ = rig.host.maps.insert(request, pages);
    rig.begin();
    rig.frontend
        .mmap(&mut rig.host, request, file.1, offset, pages);
    rig.end("mmap");
    trace(format_args!(
        "file {} mmap {offset:#x} {pages} pages -> {:?}",
        file.1,
        rig.host.replies.get(&request)
    ));
    assert!(
        rig.host.replies.contains_key(&request),
        "mmap request {request} not answered"
    );
}

fn prime_export(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let handle = rig.name(input, Kind::Gem, file.0);
    let flags = match input.u8() % 8 {
        0..=5 => uapi::drm::CLOEXEC | uapi::drm::RDWR,
        6 => 0,
        _ => input.u32(),
    };
    let args = PrimeHandle {
        handle,
        flags,
        fd: -1,
    };
    let (status, _) = rig.ioctl(input, file, ioctl::PRIME_HANDLE_TO_FD, &args.to_bytes());
    if status == Some(0) {
        rig.named(file.0, Kind::Gem, handle);
    }
}

fn prime_import(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let proc = rig.proc_of_file(file.0);
    let fd = rig.fd(input, proc);
    let args = PrimeHandle {
        handle: 0,
        flags: 0,
        fd,
    };
    let (status, after) = rig.ioctl(input, file, ioctl::PRIME_FD_TO_HANDLE, &args.to_bytes());
    if status == Some(0) {
        let handle = PrimeHandle::from_bytes(&after).expect("prime").handle;
        rig.told(file.0, Kind::Gem, handle);
    }
}

/// Descriptors: a process closes one, passes one to the other process, or
/// takes a sync file of a dmabuf's fences (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`,
/// served by the core).
fn descriptors(rig: &mut Rig, input: &mut Input<'_>) {
    let proc = input.pick(PROCS);
    let fd = rig.fd(input, proc);
    match input.u8() % 4 {
        0 | 1 => {
            rig.close_fd(proc, fd);
            if input.bool() {
                rig.deliver_releases();
            }
        }
        2 => {
            let to = input.pick(PROCS);
            if let Some(what) = rig.host.procs[proc].fds.get(&fd).copied() {
                if let Fd::Dmabuf(cookie) = what
                    && let Some(state) = rig.host.dmabufs.get_mut(&cookie)
                {
                    state.fds += 1;
                    state.going = false;
                }
                let _ = rig.host.procs[to].install(what);
            } else if input.bool() {
                let _ = rig.host.procs[to].install(Fd::ForeignDmabuf);
            }
        }
        _ => {
            let fence = match rig.host.procs[proc].fds.get(&fd) {
                Some(Fd::Dmabuf(cookie)) => rig
                    .host
                    .dmabufs
                    .get(cookie)
                    .and_then(|state| state.slot)
                    .filter(|fence| rig.host.fences.get(fence).is_some_and(Option::is_none))
                    .unwrap_or(0),
                _ => 0,
            };
            let _ = rig.host.procs[proc].install(Fd::Sync(fence));
        }
    }
}

fn dmabuf_import(rig: &mut Rig, input: &mut Input<'_>) {
    let proc = input.pick(PROCS);
    let fd = rig.fd(input, proc);
    let cookie = match rig.host.procs[proc].fds.get(&fd) {
        Some(Fd::Dmabuf(cookie)) => *cookie,
        _ => {
            if input.bool() {
                return;
            }
            input.u64()
        }
    };
    let flags = match input.u8() % 8 {
        0..=2 => 1,
        3..=5 => 2,
        6 => 3,
        _ => input.u32(),
    };
    let kind = u32::from(input.u8() % 5);
    let value = if kind == 1 {
        let sync = rig.fd(input, proc);
        match rig.host.procs[proc].fds.get(&sync) {
            Some(Fd::Sync(fence)) => *fence,
            _ => input.u64(),
        }
    } else {
        input.u64()
    };
    let request = rig.request(proc);
    rig.begin();
    rig.frontend
        .dmabuf_import(&mut rig.host, request, cookie, flags, kind, value);
    rig.end("dmabuf_import");
    trace(format_args!(
        "proc {proc} dmabuf import {cookie:#x} kind {kind} -> {:?}",
        rig.host.replies.get(&request)
    ));
    assert!(rig.host.replies.contains_key(&request));
}

fn execbuffer(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let proc = rig.proc_of_file(file.0);
    let count = input.count(4, 48);
    let mut objects = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..count {
        let handle = rig.name(input, Kind::Gem, file.0);
        let mut flags = uapi::exec::OBJECT_PINNED | uapi::exec::OBJECT_SUPPORTS_48B_ADDRESS;
        let bits = input.u8();
        if bits & 1 != 0 {
            flags |= uapi::exec::OBJECT_WRITE;
        }
        if bits & 2 != 0 {
            flags |= uapi::exec::OBJECT_ASYNC;
        }
        if bits & 4 != 0 {
            flags |= uapi::exec::OBJECT_PAD_TO_SIZE;
        }
        if bits & 8 != 0 {
            flags |= uapi::exec::OBJECT_CAPTURE;
        }
        if bits & 0x30 == 0x30 {
            flags ^= u64::from(input.u8()) << (input.u8() % 64);
        }
        let entry = ExecObject2 {
            handle,
            relocation_count: if bits == 0xff { input.word() } else { 0 },
            relocs_ptr: 0,
            alignment: if bits & 0xc0 == 0xc0 { input.quad() } else { 0 },
            offset: input.gpu_address(),
            flags,
            rsvd1: if flags & uapi::exec::OBJECT_PAD_TO_SIZE != 0 {
                input.size()
            } else {
                0
            },
            rsvd2: 0,
        };
        handles.push(handle);
        objects.extend_from_slice(&entry.to_bytes());
    }
    let list = rig.host.alloc(&objects);
    let buffers_ptr = rig.ptr(input, list, objects.len());
    rig.host.race = None;
    if count > 0 && fire(&rig.faults, fault::RACE_EXEC) {
        // Only now are its bytes read, so that inputs without the fault
        // read as they did before it.
        let index = input.pick(count);
        let at = index * ExecObject2::SIZE;
        let mut entry =
            ExecObject2::from_bytes(&objects[at..at + ExecObject2::SIZE]).expect("an exec object");
        match input.u8() % 4 {
            0 => entry.handle = rig.name(input, Kind::Gem, file.0),
            1 => entry.offset = input.gpu_address(),
            2 => {
                entry.flags |= uapi::exec::OBJECT_PAD_TO_SIZE;
                entry.rsvd1 = input.size();
            }
            _ => entry.flags ^= u64::from(input.u8()) << (input.u8() % 64),
        }
        rig.host.race = Some(Race {
            list: (list, objects.len() as u64),
            after: u32::from(input.u8() % 4) + 1,
            at: list + at as u64,
            bytes: entry.to_bytes().to_vec(),
        });
    }

    let mut flags = uapi::exec::NO_RELOC | uapi::exec::HANDLE_LUT;
    flags |= match input.u8() % 8 {
        0..=3 => uapi::exec::RENDER,
        4 | 5 => uapi::exec::BLT,
        6 => u64::from(input.u8() % 8),
        _ => input.u64() & uapi::exec::RING_MASK,
    };
    let bits = input.u8();
    let mut rsvd2 = 0_u64;
    let mut syncs = Vec::new();
    // How many of `syncs` the call was told of: a call names only what it
    // was told (`Rig::named`).
    let mut syncs_told = usize::MAX;
    let (mut cliprects_ptr, mut num_cliprects) = (0_u64, 0_u32);
    if bits & 1 != 0 {
        flags |= uapi::exec::BATCH_FIRST;
    }
    if bits & 2 != 0 {
        flags |= uapi::exec::FENCE_OUT;
    }
    if bits & 4 != 0 {
        flags |= if input.u8() % 4 == 0 {
            uapi::exec::FENCE_SUBMIT
        } else {
            uapi::exec::FENCE_IN
        };
        rsvd2 = u64::from(rig.fd(input, proc) as u32);
    }
    if bits & 0x18 == 0x08 {
        // A fence array of syncobjs to wait on and signal.
        flags |= uapi::exec::FENCE_ARRAY;
        let fences = input.count(3, 300);
        let mut array = Vec::new();
        for _ in 0..fences {
            let handle = rig.name(input, Kind::Sync, file.0);
            syncs.push(handle);
            let fence_flags = match input.u8() % 8 {
                0..=2 => uapi::exec::FENCE_WAIT,
                3..=5 => uapi::exec::FENCE_SIGNAL,
                6 => 0,
                _ => input.u32(),
            };
            array.extend_from_slice(
                &ExecFence {
                    handle,
                    flags: fence_flags,
                }
                .to_bytes(),
            );
        }
        let at = rig.host.alloc(&array);
        cliprects_ptr = rig.ptr(input, at, array.len());
        num_cliprects = if input.u8() % 16 == 0 {
            input.word()
        } else {
            fences as u32
        };
        syncs_told = num_cliprects as usize;
    } else if bits & 0x18 == 0x10 {
        // The timeline-fences extension, as anv sends it.
        flags |= uapi::exec::USE_EXTENSIONS;
        let fences = input.count(3, 300);
        let mut handles_bytes = Vec::new();
        let mut values = Vec::new();
        for _ in 0..fences {
            let handle = rig.name(input, Kind::Sync, file.0);
            syncs.push(handle);
            let fence_flags = match input.u8() % 8 {
                0..=2 => uapi::exec::FENCE_WAIT,
                3..=5 => uapi::exec::FENCE_SIGNAL,
                6 => 0,
                _ => input.u32(),
            };
            handles_bytes.extend_from_slice(
                &ExecFence {
                    handle,
                    flags: fence_flags,
                }
                .to_bytes(),
            );
            values.extend_from_slice(&point(input).to_le_bytes());
        }
        let handles_at = rig.host.alloc(&handles_bytes);
        let values_at = rig.host.alloc(&values);
        let ext = uapi::ExecbufferExtTimelineFences {
            next_extension: if input.u8() % 16 == 0 {
                extension_chain(rig, input)
            } else {
                0
            },
            name: if input.u8() % 16 == 0 {
                input.word()
            } else {
                0
            },
            flags: 0,
            fence_count: if input.u8() % 16 == 0 {
                input.quad()
            } else {
                fences as u64
            },
            handles_ptr: rig.ptr(input, handles_at, handles_bytes.len()),
            values_ptr: rig.ptr(input, values_at, values.len()),
        };
        syncs_told = usize::try_from(ext.fence_count).unwrap_or(usize::MAX);
        let at = rig.host.alloc(&ext.to_bytes());
        cliprects_ptr = rig.ptr(input, at, uapi::ExecbufferExtTimelineFences::SIZE);
    } else if bits & 0x18 == 0x18 {
        num_cliprects = input.word();
        cliprects_ptr = input.quad();
    }
    if bits & 0x20 != 0 {
        flags |= input.u64() & !(uapi::exec::RING_MASK);
    }
    let context = rig.name(input, Kind::Context, file.0);
    let args = Execbuffer2 {
        buffers_ptr,
        buffer_count: if input.u8() % 16 == 0 {
            input.word()
        } else {
            count as u32
        },
        batch_start_offset: if bits & 0x40 != 0 {
            input.word() & !7
        } else {
            0
        },
        batch_len: if bits & 0x80 != 0 { input.word() } else { 0 },
        dr1: 0,
        dr4: if input.u8() % 32 == 0 {
            input.word()
        } else {
            0
        },
        num_cliprects,
        cliprects_ptr,
        flags,
        rsvd1: u64::from(context),
        rsvd2,
    };
    let cmd = if input.bool() {
        ioctl::GEM_EXECBUFFER2_WR
    } else {
        ioctl::GEM_EXECBUFFER2
    };
    rig.host.list = Some((list, objects.len() as u64));
    rig.host.list_read = 0;
    let (status, _) = rig.ioctl(input, file, cmd, &args.to_bytes());
    // D3: the list is read once, into the driver's own copy, which every
    // pass after the checks uses.
    assert!(
        rig.host.list_read <= objects.len() as u64,
        "an execbuffer read {} bytes of its {}-byte object list",
        rig.host.list_read,
        objects.len()
    );
    rig.host.list = None;
    rig.host.race = None;
    if status == Some(0) {
        rig.named(file.0, Kind::Context, context);
        for handle in handles.into_iter().take(args.buffer_count as usize) {
            rig.named(file.0, Kind::Gem, handle);
        }
        for handle in syncs.into_iter().take(syncs_told) {
            rig.named(file.0, Kind::Sync, handle);
        }
    }
}

/// A `drm_i915_gem_context_create_ext_setparam` or `context_param`'s
/// param, size and value, as the input chooses them.
fn context_param_value(
    rig: &mut Rig,
    input: &mut Input<'_>,
    file: Option<usize>,
) -> (u64, u32, u64) {
    use uapi::context::*;
    let param = match input.u8() % 16 {
        0 | 1 => PARAM_VM,
        2 | 3 => PARAM_ENGINES,
        4 => PARAM_PRIORITY,
        5 => PARAM_BANNABLE,
        6 => PARAM_RECOVERABLE,
        7 => PARAM_PERSISTENCE,
        8 => PARAM_NO_ERROR_CAPTURE,
        9 => PARAM_PROTECTED_CONTENT,
        10 => PARAM_SSEU,
        11 => PARAM_GTT_SIZE,
        12 => PARAM_RINGSIZE,
        13 => PARAM_LOW_LATENCY,
        14 => PARAM_CONTEXT_IMAGE,
        _ => input.quad(),
    };
    match param {
        PARAM_VM => {
            let vm = rig.name(input, Kind::Vm, file);
            let value = if input.u8() % 8 == 0 {
                input.quad()
            } else {
                u64::from(vm)
            };
            (param, 0, value)
        }
        PARAM_ENGINES => {
            let count = input.count(3, 70);
            let mut map = Vec::new();
            let extensions = if input.u8() % 16 == 0 {
                extension_chain(rig, input)
            } else {
                0
            };
            map.extend_from_slice(&extensions.to_le_bytes());
            for _ in 0..count {
                let (class, instance) = match input.u8() % 8 {
                    0..=2 => (0_u16, 0_u16),
                    3 | 4 => (1, 0),
                    5 => (0xffff, 0xffff),
                    6 => (2, 0),
                    _ => (input.u16(), input.u16()),
                };
                map.extend_from_slice(&class.to_le_bytes());
                map.extend_from_slice(&instance.to_le_bytes());
            }
            let at = rig.host.alloc(&map);
            let value = match input.u8() % 16 {
                15 => input.quad(),
                14 => u64::MAX - 3,
                _ => rig.ptr(input, at, map.len()),
            };
            let size = if input.u8() % 8 == 0 {
                input.word()
            } else {
                map.len() as u32
            };
            (param, size, value)
        }
        PARAM_PRIORITY => {
            let value = match input.u8() % 4 {
                0 => 0,
                1 => 1023,
                2 => (-1023_i64) as u64,
                _ => input.quad(),
            };
            (param, 0, value)
        }
        _ => {
            let size = if input.u8() % 8 == 0 { input.word() } else { 0 };
            (param, size, input.quad() & 0x1ff_ffff)
        }
    }
}

fn context_create(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let mut flags = match input.u8() % 4 {
        0 => 0,
        1 => uapi::context::CREATE_FLAGS_SINGLE_TIMELINE,
        2 => uapi::context::CREATE_FLAGS_USE_EXTENSIONS,
        _ => input.word(),
    };
    let mut extensions = 0;
    if flags & uapi::context::CREATE_FLAGS_USE_EXTENSIONS != 0 {
        let count = input.count(2, 20);
        let mut next = 0_u64;
        for _ in 0..count {
            let (param, size, value) = context_param_value(rig, input, file.0);
            let ext = uapi::ContextCreateExtSetparam {
                next_extension: next,
                ctx_id: if input.u8() == 0xff { 1 } else { 0 },
                size,
                param,
                value,
            };
            let mut bytes = ext.to_bytes();
            let name = if input.u8() % 16 == 0 {
                input.word()
            } else {
                0
            };
            bytes[8..12].copy_from_slice(&name.to_le_bytes());
            next = rig.host.alloc(&bytes);
        }
        extensions = if next == 0 {
            input.quad()
        } else {
            rig.ptr(input, next, 56)
        };
    } else if input.u8() % 8 == 0 {
        flags |= uapi::context::CREATE_FLAGS_USE_EXTENSIONS;
        extensions = input.quad();
    }
    let args = ContextCreateExt {
        ctx_id: 0,
        flags,
        extensions,
    };
    let cmd = if input.u8() % 8 == 0 {
        ioctl::GEM_CONTEXT_CREATE
    } else {
        ioctl::GEM_CONTEXT_CREATE_EXT
    };
    let (status, after) = rig.ioctl(input, file, cmd, &args.to_bytes());
    if status == Some(0) {
        rig.told(file.0, Kind::Context, le32(&after, 0));
    }
}

fn context_param(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let ctx_id = rig.name(input, Kind::Context, file.0);
    let set = input.bool();
    let (param, size, value) = context_param_value(rig, input, file.0);
    let args = ContextParam {
        ctx_id,
        size,
        param,
        value,
    };
    let cmd = if set {
        ioctl::GEM_CONTEXT_SETPARAM
    } else {
        ioctl::GEM_CONTEXT_GETPARAM
    };
    let (status, after) = rig.ioctl(input, file, cmd, &args.to_bytes());
    if status == Some(0) {
        rig.named(file.0, Kind::Context, ctx_id);
        if set && param == uapi::context::PARAM_VM {
            rig.named(file.0, Kind::Vm, value as u32);
        }
        if !set && param == uapi::context::PARAM_VM {
            let made = ContextParam::from_bytes(&after).expect("param").value;
            rig.told(file.0, Kind::Vm, made as u32);
        }
    }
}

fn syncobj_fd(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let proc = rig.proc_of_file(file.0);
    let handle = rig.name(input, Kind::Sync, file.0);
    let to_fd = input.bool();
    let flags = match input.u8() % 8 {
        0..=3 => 1,
        4 | 5 => 3,
        6 => 0,
        _ => input.u32(),
    };
    let fd = if to_fd { -1 } else { rig.fd(input, proc) };
    let args = SyncobjHandle {
        handle,
        flags,
        fd,
        pad: if input.u8() == 0xff { 1 } else { 0 },
        point: if flags & 2 != 0 { point(input) } else { 0 },
    };
    let cmd = if to_fd {
        ioctl::SYNCOBJ_HANDLE_TO_FD
    } else {
        ioctl::SYNCOBJ_FD_TO_HANDLE
    };
    let (status, _) = rig.ioctl(input, file, cmd, &args.to_bytes());
    if status == Some(0) {
        rig.named(file.0, Kind::Sync, handle);
    }
}

/// An array of `count` of `file`'s syncobj handles in the program's memory.
fn sync_handles(
    rig: &mut Rig,
    input: &mut Input<'_>,
    file: Option<usize>,
    count: usize,
) -> (u64, Vec<u32>) {
    let mut bytes = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..count {
        let handle = rig.name(input, Kind::Sync, file);
        handles.push(handle);
        bytes.extend_from_slice(&handle.to_le_bytes());
    }
    let at = rig.host.alloc(&bytes);
    (rig.ptr(input, at, bytes.len()), handles)
}

fn sync_points(rig: &mut Rig, input: &mut Input<'_>, count: usize) -> u64 {
    let mut bytes = Vec::new();
    for _ in 0..count {
        bytes.extend_from_slice(&point(input).to_le_bytes());
    }
    let at = rig.host.alloc(&bytes);
    if input.u8() % 8 == 0 {
        0
    } else {
        rig.ptr(input, at, bytes.len())
    }
}

fn syncobj_array(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let count = input.count(3, 70);
    let (handles_ptr, handles) = sync_handles(rig, input, file.0, count);
    let count_handles = if input.u8() % 16 == 0 {
        input.word()
    } else {
        count as u32
    };
    let (cmd, args) = match input.u8() % 4 {
        0 => (
            ioctl::SYNCOBJ_RESET,
            uapi::SyncobjArray {
                handles: handles_ptr,
                count_handles,
                pad: 0,
            }
            .to_bytes()
            .to_vec(),
        ),
        1 => (
            ioctl::SYNCOBJ_SIGNAL,
            uapi::SyncobjArray {
                handles: handles_ptr,
                count_handles,
                pad: 0,
            }
            .to_bytes()
            .to_vec(),
        ),
        2 => {
            let points = sync_points(rig, input, count);
            (
                ioctl::SYNCOBJ_TIMELINE_SIGNAL,
                uapi::SyncobjTimelineArray {
                    handles: handles_ptr,
                    points,
                    count_handles,
                    flags: if input.u8() % 16 == 0 {
                        input.word()
                    } else {
                        0
                    },
                }
                .to_bytes()
                .to_vec(),
            )
        }
        _ => {
            let points = sync_points(rig, input, count);
            (
                ioctl::SYNCOBJ_QUERY,
                uapi::SyncobjTimelineArray {
                    handles: handles_ptr,
                    points,
                    count_handles,
                    flags: u32::from(input.u8() % 3),
                }
                .to_bytes()
                .to_vec(),
            )
        }
    };
    let (status, _) = rig.ioctl(input, file, cmd, &args);
    if status == Some(0) {
        for handle in handles.into_iter().take(count_handles as usize) {
            rig.named(file.0, Kind::Sync, handle);
        }
    }
}

fn syncobj_wait(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let count = input.count(3, 70);
    let (handles_ptr, handles) = sync_handles(rig, input, file.0, count);
    let count_handles = if input.u8() % 16 == 0 {
        input.word()
    } else {
        count as u32
    };
    let flags = u32::from(input.u8() % 16) | if input.u8() == 0xff { input.u32() } else { 0 };
    let timeout = match input.u8() % 4 {
        0 => 0,
        1 => i64::try_from(rig.host.now.saturating_add(1_000_000)).unwrap_or(i64::MAX),
        _ => input.timeout(),
    };
    let (cmd, args) = if input.bool() {
        let points = sync_points(rig, input, count);
        (
            ioctl::SYNCOBJ_TIMELINE_WAIT,
            SyncobjTimelineWait {
                handles: handles_ptr,
                points,
                timeout_nsec: timeout,
                count_handles,
                flags,
                first_signaled: 0,
                pad: 0,
                deadline_nsec: 0,
            }
            .to_bytes()
            .to_vec(),
        )
    } else {
        (
            ioctl::SYNCOBJ_WAIT,
            SyncobjWait {
                handles: handles_ptr,
                timeout_nsec: timeout,
                count_handles,
                flags: flags & !4,
                first_signaled: 0,
                pad: 0,
                deadline_nsec: 0,
            }
            .to_bytes()
            .to_vec(),
        )
    };
    let (status, _) = rig.ioctl(input, file, cmd, &args);
    if status == Some(0) {
        for handle in handles.into_iter().take(count_handles as usize) {
            rig.named(file.0, Kind::Sync, handle);
        }
    }
}

/// GETPARAM, QUERY, VERSION, GET_CAP, GET_RESET_STATS and REG_READ.
fn info(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    match input.u8() % 6 {
        0 => {
            let slot = rig.host.alloc(&[0; 4]);
            let param = match input.u8() % 4 {
                0..=2 => i32::from(input.u8() % 64),
                _ => input.u32() as i32,
            };
            let value = rig.ptr(input, slot, 4);
            let args = GetParam { param, value };
            let _ = rig.ioctl(input, file, ioctl::GETPARAM, &args.to_bytes());
        }
        1 => {
            let count = input.count(3, 40);
            let mut items = Vec::new();
            for _ in 0..count {
                let query_id = match input.u8() % 10 {
                    0..=2 => 2,
                    3 | 4 => 1,
                    5 | 6 => 4,
                    7 => u64::from(input.u8() % 9),
                    _ => input.quad(),
                };
                let length = match input.u8() % 6 {
                    0 | 1 => 0,
                    2 => 16 + 56 * 2,
                    3 => 104,
                    4 => -1,
                    _ => input.u32() as i32,
                };
                let room = usize::try_from(length).unwrap_or(0).min(4096);
                let data = rig.host.alloc(&vec![0; room.max(8)]);
                let item = QueryItem {
                    query_id,
                    length,
                    flags: if input.u8() % 16 == 0 {
                        input.word()
                    } else {
                        0
                    },
                    data_ptr: rig.ptr(input, data, room),
                };
                items.extend_from_slice(&item.to_bytes());
            }
            let at = rig.host.alloc(&items);
            let args = Query {
                num_items: if input.u8() % 16 == 0 {
                    input.word()
                } else {
                    count as u32
                },
                flags: if input.u8() % 16 == 0 {
                    input.word()
                } else {
                    0
                },
                items_ptr: rig.ptr(input, at, items.len()),
            };
            let _ = rig.ioctl(input, file, ioctl::QUERY, &args.to_bytes());
        }
        2 => {
            let mut lengths = [0_u64; 3];
            let mut pointers = [0_u64; 3];
            for index in 0..3 {
                lengths[index] = match input.u8() % 4 {
                    0 => 0,
                    1 => 4,
                    2 => 64,
                    _ => input.quad(),
                };
                let room = usize::try_from(lengths[index]).unwrap_or(0).min(64);
                let at = rig.host.alloc(&vec![0; room.max(8)]);
                pointers[index] = rig.ptr(input, at, room);
            }
            let args = uapi::Version {
                name_len: lengths[0],
                name: pointers[0],
                date_len: lengths[1],
                date: pointers[1],
                desc_len: lengths[2],
                desc: pointers[2],
                ..Default::default()
            };
            let _ = rig.ioctl(input, file, ioctl::VERSION, &args.to_bytes());
        }
        3 => {
            let args = uapi::GetCap {
                capability: input.quad() % 24,
                value: 0,
            };
            let _ = rig.ioctl(input, file, ioctl::GET_CAP, &args.to_bytes());
        }
        4 => {
            let ctx_id = rig.name(input, Kind::Context, file.0);
            let args = uapi::ResetStats {
                ctx_id,
                flags: if input.u8() % 16 == 0 {
                    input.word()
                } else {
                    0
                },
                ..Default::default()
            };
            let (status, _) = rig.ioctl(input, file, ioctl::GET_RESET_STATS, &args.to_bytes());
            if status == Some(0) {
                rig.named(file.0, Kind::Context, ctx_id);
            }
        }
        _ => {
            let offset = match input.u8() % 4 {
                0 => 0x2358,
                1 => 0x2359,
                _ => input.quad(),
            };
            let args = uapi::RegRead { offset, val: 0 };
            let _ = rig.ioctl(input, file, ioctl::REG_READ, &args.to_bytes());
        }
    }
}

/// Any command with any argument: the bytes from the input, with pointers
/// into the program's memory patched in where the input says.
fn raw(rig: &mut Rig, input: &mut Input<'_>) {
    let file = rig.file(input);
    let cmd = match input.u8() % 8 {
        0..=5 => RAW_COMMANDS[input.pick(RAW_COMMANDS.len())],
        6 => {
            // A known number with another size or direction.
            let known = RAW_COMMANDS[input.pick(RAW_COMMANDS.len())];
            (known & 0x0000_ffff) | (u32::from(input.u16()) << 16)
        }
        _ => input.u32(),
    };
    let length = usize::from(input.u8()) % 136;
    let mut arg: Vec<u8> = (0..length).map(|_| input.u8()).collect();
    let patches = input.count(2, 8);
    for _ in 0..patches {
        let at = input.pick(17) * 8;
        let blob_length = input.count(64, 4096);
        let blob: Vec<u8> = (0..blob_length.min(256)).map(|_| input.u8()).collect();
        let mut blob = blob;
        blob.resize(blob_length, 0);
        let address = rig.host.alloc(&blob);
        let pointer = rig.ptr(input, address, blob_length);
        if let Some(field) = arg.get_mut(at..at + 8) {
            field.copy_from_slice(&pointer.to_le_bytes());
        }
    }
    let _ = rig.ioctl(input, file, cmd, &arg);
}

// ---------------------------------------------------------------------------
// The checks after every op, and at the end
// ---------------------------------------------------------------------------

fn check(rig: &Rig) {
    let state = &rig.frontend.state;
    let model = &rig.frontend.backend;
    let open: Vec<u64> = rig
        .files
        .iter()
        .filter(|rec| rec.open)
        .map(|rec| rec.id)
        .collect();

    // Files: the tables hold exactly the open ones.
    assert_eq!(state.files.len(), open.len(), "files");
    let mut slots = BTreeSet::new();
    for id in &open {
        let slot = state.file_slot(*id).expect("an open file's slot");
        assert!(slots.insert(slot));
    }
    let open_slot = |slot: u8| slots.contains(&slot);

    // Handles name objects of open files, and each object counts them.
    let mut handles = BTreeMap::<u16, u32>::new();
    for (_, handle) in state.handles.iter() {
        assert!(
            open_slot(handle.file),
            "a handle of closed file slot {}",
            handle.file
        );
        assert!(
            state.objects.get(usize::from(handle.object)).is_some(),
            "a handle of a freed object"
        );
        *handles.entry(handle.object).or_default() += 1;
    }
    let mut bindings = BTreeMap::<u16, u32>::new();
    let mut previous: Option<Binding> = None;
    for binding in state.bindings.as_slice() {
        if let Some(before) = previous {
            assert!(
                before.end().is_some_and(|end| end <= binding.key),
                "bindings {before:?} and {binding:?} overlap"
            );
        }
        previous = Some(*binding);
        assert!(binding.address() + binding.size <= 1 << 48);
        assert!(
            state.vms.get(usize::from(binding.vm())).is_some(),
            "a binding in a freed VM"
        );
        assert!(
            state.objects.get(usize::from(binding.object)).is_some(),
            "a binding of a freed object"
        );
        *bindings.entry(binding.object).or_default() += 1;
    }
    for (slot, object) in state.objects.iter() {
        let slot = u16::try_from(slot).expect("slot");
        assert_eq!(
            object.handles,
            handles.get(&slot).copied().unwrap_or(0),
            "object {slot}'s handles"
        );
        assert_eq!(
            object.bindings,
            bindings.get(&slot).copied().unwrap_or(0),
            "object {slot}'s bindings"
        );
        assert_eq!(
            object.mmap_files & !slots.iter().fold(0_u32, |all, s| all | 1 << (s % 32)),
            0,
            "object {slot} mappable by a closed file"
        );
        let idle = object.orphaned() && object.idle(|fence| state.requests.status(fence));
        assert!(
            !(idle && (object.bindings > 0 || object.unreached(&state.requests))),
            "object {slot} unreferenced and idle, not freed"
        );
    }

    // VMs: references are their names and contexts.
    let mut refs = BTreeMap::<u16, u32>::new();
    for (_, name) in state.vm_names.iter() {
        assert!(
            open_slot(name.file),
            "a VM id of closed file slot {}",
            name.file
        );
        assert!(state.vms.get(usize::from(name.vm)).is_some());
        *refs.entry(name.vm).or_default() += 1;
    }
    let mut hw = 0;
    for (slot, context) in state.contexts.iter() {
        assert!(
            context.closed || open_slot(context.file),
            "context {slot} of a closed file"
        );
        assert!(
            state.vms.get(usize::from(context.vm)).is_some(),
            "context {slot}'s VM freed"
        );
        *refs.entry(context.vm).or_default() += 1;
        hw += context.hw.iter().flatten().count();
    }
    let mut backed = 0;
    for (slot, vm) in state.vms.iter() {
        let slot = u16::try_from(slot).expect("slot");
        assert_eq!(
            vm.refs,
            refs.get(&slot).copied().unwrap_or(0),
            "VM {slot}'s references"
        );
        backed += usize::from(vm.backend.is_some());
    }
    for (_, sync) in state.syncobjs.iter() {
        assert!(
            open_slot(sync.file),
            "a syncobj of closed file slot {}",
            sync.file
        );
    }
    // D10: a held wait names only its own file's live syncobjs; one
    // destroyed under it is forgotten, never read again, since its slot
    // may be another file's next.
    for wait in state.waits.iter() {
        let WaitKind::Syncobjs(sync) = &wait.kind else {
            continue;
        };
        for &slot in sync.slots.iter().take(usize::from(sync.count)) {
            if slot == syncobj::GONE {
                continue;
            }
            let owner = state.syncobjs.get(usize::from(slot)).map(|sync| sync.file);
            assert_eq!(
                owner,
                Some(wait.file),
                "a held wait of file slot {} names syncobj slot {slot}",
                wait.file
            );
        }
    }
    for (_, kernel) in state.kernel_fences.iter() {
        assert_eq!(
            rig.host.fences.get(&kernel.cookie),
            Some(&None),
            "kernel fence {} tracked but not live in the core",
            kernel.cookie
        );
    }

    // The limits.
    let requests = &state.requests;
    assert!(requests.next - requests.retired - 1 <= limits::REQUESTS as u64);
    assert!(state.waits.len() <= limits::WAITS);

    // The back end agrees.
    assert_eq!(
        model.live.borrow().len(),
        state.objects.len(),
        "live memories"
    );
    assert_eq!(model.vms.len(), backed, "back-end VMs");
    // D2: memory goes back only once no request that could reach it
    // through a stale TLB entry runs, and never while mapped.
    for id in model.freed.borrow_mut().drain(..) {
        assert!(
            !model.binds.values().any(|&(_, memory)| memory == id),
            "memory {id} freed while bound"
        );
        let exposed = model.exposed.borrow_mut().remove(&id);
        for request in exposed.into_iter().flatten() {
            assert!(
                !model.in_flight.iter().any(|run| run.id == request),
                "memory {id} freed while request {request}, running in a VM it was unbound from, may still reach it"
            );
        }
    }
    for (table, vm) in &model.pt.owner {
        assert!(
            model.vms.contains_key(vm),
            "page table {table:#x} of destroyed VM {vm}"
        );
    }
    assert_eq!(model.contexts.len(), hw, "hardware contexts");
    assert_eq!(
        model.binds.len(),
        state.bindings.as_slice().len(),
        "back-end bindings"
    );

    // Every call the harness is still waiting for is held, unless its
    // program gave up on it (a full table sweeps those out unanswered).
    let live = rig
        .pending
        .keys()
        .filter(|request| !rig.host.abandoned.contains(request))
        .count();
    assert!(
        live <= state.waits.len(),
        "{live} calls unanswered, {} held",
        state.waits.len()
    );
}

/// Close everything and let every request finish; nothing may be left.
fn teardown(rig: &mut Rig) {
    rig.faults.set(0);
    for proc in 0..PROCS {
        let fds: Vec<i32> = rig.host.procs[proc].fds.keys().copied().collect();
        for fd in fds {
            rig.close_fd(proc, fd);
        }
    }
    rig.deliver_releases();
    for index in 0..rig.files.len() {
        rig.release(index);
    }
    for _ in 0..(limits::REQUESTS + 8) {
        let contexts: BTreeSet<u32> = rig
            .frontend
            .backend
            .in_flight
            .iter()
            .map(|run| run.context)
            .collect();
        for context in contexts {
            rig.frontend.backend.finish(context, usize::MAX, false);
        }
        rig.service(true);
        if rig.frontend.backend.in_flight.is_empty() && rig.frontend.backend.done.is_empty() {
            let requests = &rig.frontend.state.requests;
            if requests.retired + 1 == requests.next {
                break;
            }
        }
    }
    check(rig);
    let state = &rig.frontend.state;
    let requests = &state.requests;
    assert_eq!(
        requests.retired + 1,
        requests.next,
        "requests not retired: {requests:?}"
    );
    assert!(
        state.objects.is_empty(),
        "objects left: {:?}",
        state.objects
    );
    assert!(state.handles.is_empty(), "handles left");
    assert!(state.bindings.as_slice().is_empty(), "bindings left");
    assert!(state.vms.is_empty(), "VMs left");
    assert!(state.vm_names.is_empty(), "VM ids left");
    assert!(state.contexts.is_empty(), "contexts left");
    assert!(state.syncobjs.is_empty(), "syncobjs left");
    assert!(state.kernel_fences.is_empty(), "kernel fences left");
    assert!(state.waits.is_empty(), "held waits left");
    let model = &rig.frontend.backend;
    assert!(model.live.borrow().is_empty(), "memories left");
    assert!(model.vms.is_empty() && model.contexts.is_empty() && model.binds.is_empty());
    assert!(model.pt.tables.is_empty(), "page tables left");
    for (fence, status) in &rig.host.fences {
        assert!(status.is_some(), "kernel fence {fence} never signalled");
    }
}

/// One input, start to end.
fn run(bytes: &[u8]) {
    let mut input = Input { bytes, at: 0 };
    let setup = input.u8();
    let mut rig = Rig::new(setup & 1 != 0, setup & 2 != 0, setup & 4 != 0);
    // A file in each process to start from, as every program has one.
    rig.open(0);
    rig.open(1);
    while !input.done() && rig.calls < MAX_CALLS {
        let op = input.u8();
        // The top bit repeats the op: the same bytes again, enough times to
        // reach a table's limit.
        let times = if op & 0x80 != 0 {
            usize::from(input.u8()) * 2 + 1
        } else {
            1
        };
        let start = input.at;
        let mut end = start;
        for _ in 0..times {
            if rig.calls >= MAX_CALLS {
                break;
            }
            input.at = start;
            rig.calls += 1;
            run_op(&mut rig, &mut input, op & 0x7f);
            end = input.at;
            rig.settle_answers();
            rig.reconcile();
            check(&rig);
        }
        input.at = end;
    }
    teardown(&mut rig);
    let Rig { frontend, .. } = rig;
    FRONTEND.with(|slot| *slot.borrow_mut() = Some(frontend));
}

/// The thread every input runs on: emptying the front end's tables builds a
/// new set of them on the stack, megabytes, more than the fuzzer's own
/// thread has to spare under AddressSanitizer.
struct Worker {
    inputs: Mutex<SyncSender<Vec<u8>>>,
    done: Mutex<Receiver<()>>,
}

fn worker() -> &'static Worker {
    static WORKER: OnceLock<Worker> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (inputs, receive) = sync_channel::<Vec<u8>>(0);
        let (finished, done) = sync_channel(0);
        let _ = std::thread::Builder::new()
            .name("intel_uapi".into())
            .stack_size(256 << 20)
            .spawn(move || {
                while let Ok(bytes) = receive.recv() {
                    run(&bytes);
                    let _ = finished.send(());
                }
            })
            .expect("the worker thread");
        Worker {
            inputs: Mutex::new(inputs),
            done: Mutex::new(done),
        }
    })
}

fuzz_target!(|bytes: &[u8]| {
    let worker = worker();
    worker
        .inputs
        .lock()
        .expect("inputs")
        .send(bytes.to_vec())
        .expect("the worker takes it");
    // A panic on the worker aborts the process (libfuzzer-sys's hook), so
    // the answer comes or the run ends there.
    worker
        .done
        .lock()
        .expect("done")
        .recv()
        .expect("the worker finishes");
});
