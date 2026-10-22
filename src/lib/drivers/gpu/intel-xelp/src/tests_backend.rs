//! Host tests for the back end ([`crate::backend`]) on the engine model:
//! bring-up with the default image, bindings, contexts, submissions and
//! completions, round-robin between contexts, a full ring, and a stalled
//! request reset, its context banned.

extern crate std;

use std::vec::Vec;

use crate::backend::{
    Completion, Error, HANG_NS, Identity, KernelObject, Platform, SubmitError, UapiEngine,
    XelpBackend,
};
use crate::commands::{Cs, emit_store_dword, gpu};
use crate::engine::ENGINES;
use crate::io::{Clock, Mmio};
use crate::model::Gpu;
use crate::ppgtt::PtMemory;

const HWSP: u32 = 0x10_0000;

/// The model as a platform: GGTT objects from `0x20_0000` up (the first page
/// asked for is the engine's status page the model watches), client memory
/// as model pages, and time that runs the engine whenever it passes.
struct Model {
    gpu: Gpu,
    reports: Vec<crate::backend::FrequencyReport>,
    next_ggtt: u32,
    hwsp_given: bool,
    now: u64,
    next_page: u64,
}

impl Model {
    fn new() -> Self {
        Model {
            gpu: Gpu::new(&ENGINES[0], HWSP),
            reports: Vec::new(),
            next_ggtt: 0x20_0000,
            hwsp_given: false,
            now: 0,
            next_page: 0x4000_0000,
        }
    }
}

impl Mmio for Model {
    fn read32(&mut self, offset: u32) -> u32 {
        self.gpu.read32(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.gpu.write32(offset, value);
    }
}

impl Clock for Model {
    fn now_us(&mut self) -> u64 {
        self.now
    }

    fn delay_us(&mut self, us: u32) {
        self.now += u64::from(us);
        let _ = self.gpu.run();
    }
}

impl PtMemory for Model {
    fn alloc_pt(&mut self) -> Option<u64> {
        self.gpu.tables.alloc_pt()
    }

    fn free_pt(&mut self, table: u64) {
        self.gpu.tables.free_pt(table);
    }

    fn read_entry(&mut self, table: u64, index: usize) -> u64 {
        self.gpu.tables.read_entry(table, index)
    }

    fn write_entry(&mut self, table: u64, index: usize, value: u64) {
        self.gpu.tables.write_entry(table, index, value);
    }
}

impl Platform for Model {
    type Memory = Vec<u64>;

    fn create_memory(&mut self, bytes: u64) -> Result<Vec<u64>, Error> {
        let pages: Vec<u64> = (0..bytes / 4096)
            .map(|n| self.next_page + n * 4096)
            .collect();
        self.next_page += bytes;
        for &page in &pages {
            self.gpu.add_page(page);
        }
        Ok(pages)
    }

    fn memory_page(&mut self, memory: &Vec<u64>, index: u64) -> Result<u64, Error> {
        memory.get(index as usize).copied().ok_or(Error::Invalid)
    }

    fn kernel_object(&mut self, pages: u32) -> Result<KernelObject, Error> {
        if !self.hwsp_given && pages == 1 {
            self.hwsp_given = true;
            return Ok(KernelObject {
                id: HWSP,
                pages,
                ggtt: HWSP,
            });
        }
        let ggtt = self.next_ggtt;
        self.next_ggtt += pages * 4096;
        self.gpu.add_object(ggtt, pages * 4096);
        Ok(KernelObject {
            id: ggtt,
            pages,
            ggtt,
        })
    }

    fn free_kernel_object(&mut self, object: KernelObject) {
        let _ = self.gpu.objects.remove(&object.ggtt);
    }

    fn words(&mut self, object: KernelObject) -> &mut [u32] {
        self.gpu.object(object.ggtt)
    }

    fn read_object(&mut self, object: KernelObject, index: usize) -> u32 {
        self.gpu.object(object.ggtt)[index]
    }

    fn write_object(&mut self, object: KernelObject, index: usize, value: u32) {
        self.gpu.object(object.ggtt)[index] = value;
    }

    fn frequency_report(&mut self, report: &crate::backend::FrequencyReport) {
        self.reports.push(*report);
    }
}

const RENDER: UapiEngine = UapiEngine {
    class: 0,
    instance: 0,
};

fn backend() -> XelpBackend<Model> {
    XelpBackend::new(
        Model::new(),
        0b11,
        &ENGINES[..4],
        1,
        Identity {
            device_id: 0x46d4,
            revision: 0,
            ggtt_bytes: 4 << 30,
            system_memory_bytes: 16 << 30,
        },
    )
    .expect("the GT comes up")
}

/// A batch page at `at` in `vm` storing `value` at `target`, bound; the
/// target page bound too.
fn store_batch(
    be: &mut XelpBackend<Model>,
    vm: usize,
    at: u64,
    target: u64,
    value: u32,
) -> Vec<u64> {
    let batch = be.platform().create_memory(4096).expect("memory");
    let out = be.platform().create_memory(4096).expect("memory");
    let mut words = [0_u32; 6];
    let mut cs = Cs::new(&mut words);
    emit_store_dword(&mut cs, target, value);
    cs.push(gpu::MI_BATCH_BUFFER_END);
    be.platform().gpu.pages.get_mut(&batch[0]).expect("page")[..6].copy_from_slice(&words);
    be.bind(vm, at, &batch, 4096, 0).expect("bound");
    be.bind(vm, target & !0xfff, &out, 4096, 0).expect("bound");
    out
}

/// Let the engine run what it was given, then service at `now`.
fn step(be: &mut XelpBackend<Model>, now: u64) -> Vec<(u64, Completion)> {
    let _ = be.platform().gpu.run();
    be.service(now, true);
    let mut done = Vec::new();
    be.completed(&mut |id, how| done.push((id, how)));
    done
}

#[test]
fn the_back_end_comes_up_with_a_default_image() {
    let mut be = backend();
    // The first context ran restore-inhibited, and no completion of it is
    // left for the front end.
    assert_eq!(be.platform().gpu.inhibited_runs, 1);
    let mut done = Vec::new();
    be.completed(&mut |id, how| done.push((id, how)));
    assert!(done.is_empty());
    assert_eq!(be.facts().ppgtt_bits, 48);
    assert_eq!(be.facts().max_pat_index, 3);
    assert_eq!(be.next_deadline(), None);
}

#[test]
fn a_batch_runs_and_completes_once() {
    let mut be = backend();
    let vm = be.vm_create().expect("vm");
    let out = store_batch(&mut be, vm, 0x1_0000_0000, 0x1_0000_1040, 0xbe_0001);
    let (engine, ctx) = be.context_create(vm, RENDER, 16 << 10).expect("context");
    be.submit(engine, ctx, 0x1_0000_0000, 7).expect("taken");
    assert!(be.next_deadline().is_some(), "work is outstanding");
    // A context made from the default is not restore-inhibited.
    let done = step(&mut be, 1_000);
    assert_eq!(be.platform().gpu.inhibited_runs, 1);
    assert_eq!(done, [(7, Completion::Ok)]);
    assert_eq!(be.platform().gpu.pages[&out[0]][0x10], 0xbe_0001);
    assert_eq!(step(&mut be, 2_000), []);
    assert_eq!(be.next_deadline(), None);
    be.unbind(vm, 0x1_0000_0000, 4096);
    be.context_destroy(engine, ctx);
    be.vm_destroy(vm);
}

#[test]
fn contexts_take_turns_and_requests_stay_in_order() {
    let mut be = backend();
    let vm = be.vm_create().expect("vm");
    let _ = store_batch(&mut be, vm, 0x1_0000_0000, 0x1_0000_1000, 1);
    let (e, a) = be.context_create(vm, RENDER, 16 << 10).expect("a");
    let (_, b) = be.context_create(vm, RENDER, 16 << 10).expect("b");
    be.submit(e, a, 0x1_0000_0000, 1).expect("a1");
    be.submit(e, b, 0x1_0000_0000, 2).expect("b1");
    be.submit(e, a, 0x1_0000_0000, 3).expect("a2");
    let mut done = Vec::new();
    for t in 1..10 {
        done.extend(step(&mut be, t * 1_000));
    }
    let order: Vec<u64> = done.iter().map(|(id, _)| *id).collect();
    assert_eq!(order.len(), 3, "{done:?}");
    assert!(done.iter().all(|(_, how)| *how == Completion::Ok));
    // a's requests in order.
    let pos = |id| order.iter().position(|&x| x == id).expect("done");
    assert!(pos(1) < pos(3));
}

#[test]
fn a_full_context_says_full() {
    let mut be = backend();
    let vm = be.vm_create().expect("vm");
    let (e, c) = be.context_create(vm, RENDER, 4 << 10).expect("context");
    let mut taken = 0;
    loop {
        match be.submit(e, c, 0x1_0000_0000, 100 + taken) {
            Ok(()) => taken += 1,
            Err(SubmitError::Full) => break,
            Err(other) => panic!("{other:?}"),
        }
        assert!(taken < 100, "never full");
    }
    // A 4 KiB ring holds a few requests, fewer than the in-flight table.
    assert!(taken > 1 && taken < 32, "{taken}");
}

#[test]
fn refusals() {
    let mut be = backend();
    let vm = be.vm_create().expect("vm");
    let memory = be.platform().create_memory(4096).expect("memory");
    assert_eq!(be.bind(vm, 0x1000, &memory, 4096, 4), Err(Error::Invalid));
    assert_eq!(be.bind(99, 0x1000, &memory, 4096, 0), Err(Error::Invalid));
    let video = UapiEngine {
        class: 2,
        instance: 0,
    };
    assert_eq!(be.context_create(vm, video, 16 << 10), Err(Error::Invalid));
    assert_eq!(be.context_create(vm, RENDER, 12 << 10), Err(Error::Invalid));
    assert_eq!(be.context_create(vm, RENDER, 4 << 20), Err(Error::Invalid));
    // The copy engine is not up in this back end of one engine.
    let copy = UapiEngine {
        class: 1,
        instance: 0,
    };
    assert_eq!(be.context_create(vm, copy, 16 << 10), Err(Error::Invalid));
}

/// No page for a table is i915's ENOMEM (`alloc_pt_dma`), not EINVAL: anv
/// retries an execbuffer's ENOMEM and takes EINVAL for a lost device, which
/// is how Endfield's world load lost the N150's device.
#[test]
fn no_page_for_a_table_is_no_memory() {
    let mut be = backend();
    let vm = be.vm_create().expect("vm");
    let memory = be.platform().create_memory(4 << 20).expect("memory");
    let live = be.platform().gpu.tables.pages.len();
    be.platform().gpu.tables.limit = Some(live);
    let refused = be.bind(vm, 0x40_0000_0000, &memory, 4 << 20, 0);
    assert_eq!(refused, Err(Error::NoMemory));
    assert_eq!(
        crate::backend::errno(Error::NoMemory),
        ferrix_linux_abi::errno::Errno::ENOMEM
    );
    be.platform().gpu.tables.limit = None;
    assert_eq!(be.bind(vm, 0x40_0000_0000, &memory, 4 << 20, 0), Ok(()));
}

#[test]
fn a_stalled_request_is_reset_hung_and_its_context_banned() {
    let mut be = backend();
    let vm = be.vm_create().expect("vm");
    // A spinner: an arbitration point and a jump back to itself.
    let spin = be.platform().create_memory(4096).expect("memory");
    be.platform().gpu.pages.get_mut(&spin[0]).expect("page")[..4].copy_from_slice(&[
        gpu::MI_ARB_CHECK,
        gpu::MI_BATCH_BUFFER_START_GEN8 | (1 << 8),
        0,
        3,
    ]);
    be.bind(vm, 0x3_0000_0000, &spin, 4096, 0).expect("bound");
    let out = store_batch(&mut be, vm, 0x1_0000_0000, 0x1_0000_1000, 0x600d);
    let (e, hung) = be.context_create(vm, RENDER, 16 << 10).expect("context");
    let (_, good) = be.context_create(vm, RENDER, 16 << 10).expect("context");
    be.submit(e, hung, 0x3_0000_0000, 1).expect("spinner");
    be.submit(e, hung, 0x1_0000_0000, 2).expect("behind it");
    be.submit(e, good, 0x1_0000_0000, 3)
        .expect("another context");
    // It spins: nothing completes, the watch starts.
    assert_eq!(step(&mut be, 1_000), []);
    assert_eq!(step(&mut be, 1_000 + HANG_NS / 2), []);
    assert_eq!(be.resets(), [0, 0]);
    // A heartbeat later: reset, the spinner hung, the one behind cancelled.
    let mut done = step(&mut be, 1_000 + HANG_NS);
    assert_eq!(be.resets(), [1, 0]);
    assert_eq!(
        done[..2],
        [(1, Completion::Hung), (2, Completion::Cancelled)]
    );
    // The banned context refuses more; the other one runs.
    assert_eq!(
        be.submit(e, hung, 0x1_0000_0000, 4),
        Err(SubmitError::Failed(Error::Banned))
    );
    for t in 2..8 {
        done.extend(step(&mut be, HANG_NS + t * 1_000));
    }
    assert!(done.contains(&(3, Completion::Ok)), "{done:?}");
    assert_eq!(
        done.iter().filter(|(id, _)| *id == 1 || *id == 2).count(),
        2,
        "reported once"
    );
    assert_eq!(be.platform().gpu.pages[&out[0]][0], 0x600d);
    assert_eq!(be.next_deadline(), None, "the replayed breadcrumbs retired");
}

#[test]
fn the_timestamp_is_read_whole() {
    let mut be = backend();
    be.platform().gpu.write32(0x2358, 0x1234_5678);
    be.platform().gpu.write32(0x235c, 0x9);
    assert_eq!(be.timestamp(), 0x9_1234_5678);
}

#[test]
fn the_front_ends_backend_trait_reaches_it() {
    use crate::frontend::{self, Backend};
    let mut be = backend();
    let facts = *Backend::facts(&be);
    assert_eq!(
        (facts.device_id, facts.ppgtt_bits, facts.max_pat_index),
        (0x46d4, 48, 3)
    );
    assert_eq!(
        facts.engines,
        [frontend::Engine {
            class: 0,
            instance: 0
        }]
    );
    let vm = Backend::vm_create(&mut be).expect("vm");
    let _ = store_batch(&mut be, vm.0 as usize, 0x1_0000_0000, 0x1_0000_1000, 5);
    let ctx = Backend::context_create(
        &mut be,
        vm,
        frontend::Engine {
            class: 0,
            instance: 0,
        },
        frontend::ContextParams::DEFAULT,
    )
    .expect("context");
    let copy = frontend::Engine {
        class: 1,
        instance: 0,
    };
    assert_eq!(
        Backend::context_create(&mut be, vm, copy, frontend::ContextParams::DEFAULT),
        Err(ferrix_linux_abi::errno::Errno::EINVAL)
    );
    Backend::submit(&mut be, ctx, 0x1_0000_0000, 64, 41).expect("taken");
    let _ = be.platform().gpu.run();
    Backend::service(&mut be, 1_000, false);
    let mut done = Vec::new();
    Backend::completed(&mut be, &mut |id, how| done.push((id, how)));
    assert_eq!(done, [(41, frontend::Completion::Ok)]);
    Backend::context_destroy(&mut be, ctx);
    Backend::vm_destroy(&mut be, vm);
}

/// The program's side of the chardev core, as little as the front end needs
/// for an open, objects and an execbuffer.
#[derive(Default)]
struct Program {
    memory: Vec<u8>,
    replies: std::collections::BTreeMap<u64, i32>,
}

const BASE: u64 = 0x7000_0000;

impl Program {
    fn alloc(&mut self, bytes: &[u8]) -> u64 {
        let at = self.memory.len().next_multiple_of(8);
        self.memory.resize(at + bytes.len().max(8), 0);
        self.memory[at..at + bytes.len()].copy_from_slice(bytes);
        BASE + at as u64
    }

    fn read(&self, address: u64, len: usize) -> Vec<u8> {
        let at = (address - BASE) as usize;
        self.memory[at..at + len].to_vec()
    }
}

impl crate::frontend::Host<Vec<u64>> for Program {
    fn copy_in(
        &mut self,
        _r: u64,
        address: u64,
        out: &mut [u8],
    ) -> Result<(), ferrix_linux_abi::errno::Errno> {
        let at = (address - BASE) as usize;
        out.copy_from_slice(&self.memory[at..at + out.len()]);
        Ok(())
    }

    fn copy_out(
        &mut self,
        _r: u64,
        address: u64,
        bytes: &[u8],
    ) -> Result<(), ferrix_linux_abi::errno::Errno> {
        let at = (address - BASE) as usize;
        self.memory[at..at + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn reply(&mut self, request: u64, status: i32) {
        assert!(
            self.replies.insert(request, status).is_none(),
            "answered twice"
        );
    }

    fn reply_map(
        &mut self,
        request: u64,
        _m: &Vec<u64>,
        _o: u64,
        _wc: bool,
    ) -> Result<(), ferrix_linux_abi::errno::Errno> {
        self.reply(request, 0);
        Ok(())
    }

    fn dmabuf_install(
        &mut self,
        _r: u64,
        _m: &Vec<u64>,
        _c: u64,
        _w: bool,
        _x: bool,
    ) -> Result<crate::frontend::Installed, ferrix_linux_abi::errno::Errno> {
        Err(ferrix_linux_abi::errno::Errno::ENODEV)
    }

    fn dmabuf_resolve(&mut self, _r: u64, _fd: i32) -> Result<u64, ferrix_linux_abi::errno::Errno> {
        Err(ferrix_linux_abi::errno::Errno::EBADF)
    }

    fn dmabuf_fence(&mut self, _d: u64, _f: u64) -> Result<(), ferrix_linux_abi::errno::Errno> {
        Ok(())
    }

    fn sync_install(
        &mut self,
        _r: u64,
        _f: u64,
        _x: bool,
    ) -> Result<i32, ferrix_linux_abi::errno::Errno> {
        Err(ferrix_linux_abi::errno::Errno::ENODEV)
    }

    fn sync_resolve(
        &mut self,
        _r: u64,
        _fd: i32,
    ) -> Result<(u64, bool), ferrix_linux_abi::errno::Errno> {
        Err(ferrix_linux_abi::errno::Errno::EINVAL)
    }

    fn sync_signal(&mut self, _f: u64, _s: i32) {}

    fn now_ns(&self) -> u64 {
        1_000
    }

    fn privileged(&self, _r: u64) -> bool {
        false
    }

    fn say(&mut self, _line: core::fmt::Arguments<'_>) {}
}

/// The front end over the real back end, on the model: a program opens the
/// node, makes two objects, writes a batch, and an execbuffer of both runs
/// it on RCS0 through the PPGTT the front end's VM is. What the driver
/// process does with the chardev core in place of [`Program`].
#[test]
fn the_front_end_runs_a_batch_on_the_real_back_end() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            use crate::uapi::{ExecObject2, Execbuffer2, GemCreate, exec, ioctl};
            let mut frontend = std::boxed::Box::new(crate::frontend::Frontend::new(backend()));
            let mut program = Program::default();
            let mut next = 1_u64;
            let mut request = || {
                next += 1;
                next
            };
            let r = request();
            frontend.open(&mut program, r, 1, 100);
            assert_eq!(program.replies[&r], 0);
            let create = |frontend: &mut crate::frontend::Frontend<XelpBackend<Model>>,
                          program: &mut Program,
                          size: u64,
                          r: u64| {
                let mut args = GemCreate {
                    size,
                    ..Default::default()
                }
                .to_bytes();
                let arg = program.alloc(&args);
                frontend.ioctl(program, r, 1, ioctl::GEM_CREATE, arg);
                assert_eq!(program.replies[&r], 0);
                let len = args.len();
                args.copy_from_slice(&program.read(arg, len));
                GemCreate::from_bytes(&args).expect("create").handle
            };
            let data = create(&mut frontend, &mut program, 8192, request());
            let batch = create(&mut frontend, &mut program, 4096, request());
            // The model's pages are handed out in order: the data object's
            // two, then the batch's.
            let (data_page, batch_page) = (0x4000_0000_u64, 0x4000_2000_u64);
            let mut words = [0_u32; 6];
            let mut cs = Cs::new(&mut words);
            emit_store_dword(&mut cs, 0x10_0040, 0xfe_0003);
            cs.push(gpu::MI_BATCH_BUFFER_END);
            frontend
                .backend
                .platform()
                .gpu
                .pages
                .get_mut(&batch_page)
                .expect("page")[..6]
                .copy_from_slice(&words);
            let pinned = exec::OBJECT_PINNED | exec::OBJECT_SUPPORTS_48B_ADDRESS;
            let mut list = Vec::new();
            for (handle, offset, flags) in [
                (data, 0x10_0000, pinned | exec::OBJECT_WRITE),
                (batch, 0x20_0000, pinned),
            ] {
                list.extend_from_slice(
                    &ExecObject2 {
                        handle,
                        offset,
                        flags,
                        ..Default::default()
                    }
                    .to_bytes(),
                );
            }
            let buffers_ptr = program.alloc(&list);
            let args = Execbuffer2 {
                buffers_ptr,
                buffer_count: 2,
                flags: exec::NO_RELOC | exec::HANDLE_LUT,
                ..Default::default()
            }
            .to_bytes();
            let arg = program.alloc(&args);
            let r = request();
            frontend.ioctl(&mut program, r, 1, ioctl::GEM_EXECBUFFER2, arg);
            assert_eq!(program.replies[&r], 0, "execbuffer taken");
            let _ = frontend.backend.platform().gpu.run();
            frontend.service(&mut program, false);
            assert_eq!(
                frontend.backend.platform().gpu.pages[&data_page][0x10],
                0xfe_0003,
                "the batch ran"
            );
            assert_eq!(frontend.backend.next_deadline(), None, "and retired");
        })
        .expect("thread")
        .join()
        .expect("passed");
}

/// The driver builds its front end and back end on a native process's 8 MiB
/// stack (`frontend`'s own test holds the front end under 3 MiB).
#[test]
fn the_back_end_fits_a_native_stack() {
    let bytes = size_of::<crate::frontend::Frontend<XelpBackend<Model>>>();
    assert!(bytes < 4 << 20, "{bytes} bytes");
    assert!(
        size_of::<XelpBackend<Model>>() < 1 << 20,
        "{}",
        size_of::<XelpBackend<Model>>()
    );
}

#[test]
fn rps_starts_at_rp1_takes_a_fixed_request_and_logs_each_second() {
    use crate::rps;
    let mut be = backend();
    // M0's ADL-N: RP0 1000 MHz (20 x 50), RPn 200 (4 x 50), RP1 300 (6).
    be.platform()
        .gpu
        .write32(rps::GEN6_RP_STATE_CAP.offset(), (4 << 16) | 20);
    be.platform()
        .gpu
        .write32(rps::GEN10_FREQ_INFO_REC.offset(), 6 << 8);
    let state = be.power_up(None, true);
    assert_eq!(state.cur, 18, "RP1 in 16.67 MHz units");
    let regs = &be.platform().gpu.regs;
    assert_eq!(regs[&0xa008], 18 << 23, "GEN6_RPNSWREQ");
    assert_eq!(
        regs[&0xa014],
        60 << 23,
        "GEN6_RP_INTERRUPT_LIMITS: RP0 only"
    );
    assert_eq!(regs[&0xa010], 50_000, "GEN6_RP_DOWN_TIMEOUT");
    assert_eq!(regs[&0xa024] & (1 << 7), 1 << 7, "GEN6_RP_ENABLE");
    assert_ne!(regs[&0xa090], 0, "GEN6_RC_CONTROL");
    // The intervals in 16-clock units of the CS clock: 16 ms, and 95 % of it
    // (19200 and 18240 at the box's 19.2 MHz).
    let hz = be.facts().cs_timestamp_hz;
    let regs = &be.platform().gpu.regs;
    assert_eq!(
        regs[&0xa068],
        rps::intel_gt_ns_to_pm_interval(hz, 16_000_000)
    );
    assert_eq!(
        regs[&0xa02c],
        rps::intel_gt_ns_to_pm_interval(hz, 15_200_000)
    );
    assert_eq!(
        rps::intel_gt_ns_to_pm_interval(19_200_000, 16_000_000),
        19_200
    );
    assert_eq!(
        rps::intel_gt_ns_to_pm_interval(19_200_000, 15_200_000),
        18_240
    );
    // A fixed request at RP0, and one past it clamped.
    assert_eq!(be.set_frequency_mhz(1000, true), Some(1000));
    assert_eq!(be.platform().gpu.regs[&0xa008], 60 << 23);
    assert_eq!(be.set_frequency_mhz(5000, true), Some(1000));
    assert_eq!(be.set_frequency_mhz(10, true), Some(200));
    assert_eq!(
        be.platform().gpu.regs[&0xa014],
        (60 << 23) | (12 << 14),
        "GEN6_RP_INTERRUPT_LIMITS: RP0 and, at RPn, RPn"
    );
    // The actual frequency read back from GEN12_RPSTAT1's CAGF (bits 19:11).
    be.platform().gpu.write32(0x13_81b4, 36 << 11);
    assert_eq!(be.frequency_sample().expect("up").actual_mhz, 600);
    // A second's log: power from the energy counter, RC6 from residency.
    be.platform()
        .gpu
        .write32(rps::PCU_PACKAGE_POWER_SKU_UNIT.offset(), 14 << 8);
    be.platform()
        .gpu
        .write32(rps::PCU_PACKAGE_ENERGY_STATUS.offset(), 1000);
    be.set_freqlog(true, 0);
    assert_eq!(be.next_deadline(), Some(crate::backend::FREQLOG_NS));
    // 3 J (3 << 14 counts) and 0.5 s of RC6 (390625 x 1.28 us) in a second.
    be.platform()
        .gpu
        .write32(rps::PCU_PACKAGE_ENERGY_STATUS.offset(), 1000 + (3 << 14));
    be.platform().gpu.write32(0x13_8108, 390_625);
    be.service(crate::backend::FREQLOG_NS / 2, false);
    assert!(be.platform().reports.is_empty(), "not yet due");
    be.service(crate::backend::FREQLOG_NS, false);
    let reports = be.platform().reports.clone();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (reports[0].milliwatts, reports[0].rc6_permille),
        (3000, 500),
        "3000 mW, 50.0 % in RC6"
    );
    assert_eq!(reports[0].sample.actual_mhz, 600);
    assert_eq!(reports[0].busy_permille, 0, "nothing ran");
    assert!(reports[0].pinned);
}

/// The back end with M0's ADL-N caps (RP0 1000, RP1 300, `RPn` 200 MHz) and
/// RPS up on the policy, at model time 0.
fn backend_with_rps() -> XelpBackend<Model> {
    use crate::rps;
    let mut be = backend();
    be.platform()
        .gpu
        .write32(rps::GEN6_RP_STATE_CAP.offset(), (4 << 16) | 20);
    be.platform()
        .gpu
        .write32(rps::GEN10_FREQ_INFO_REC.offset(), 6 << 8);
    let state = be.power_up(None, true);
    assert!(state.active && !state.pinned);
    be
}

/// `rps_timer`'s and `rps_work`'s rules, i915's numbers.
#[test]
fn the_policy_steps_as_i915_steps() {
    use crate::rps::{Event, combined_busy, park_step, rps_timer_event, rps_work};
    // The busiest engine in full, the next by half, the third by a quarter.
    assert_eq!(combined_busy(&[100, 400, 0, 200]), 400 + 100 + 25);
    assert_eq!(combined_busy(&[0, 0]), 0);
    // 95 % up, 85 % down, between them steady; never past the limits.
    assert_eq!(rps_timer_event(960, 1000, 18, 12, 60), Event::Up);
    assert_eq!(rps_timer_event(960, 1000, 60, 12, 60), Event::Steady);
    assert_eq!(rps_timer_event(900, 1000, 18, 12, 60), Event::Steady);
    assert_eq!(rps_timer_event(800, 1000, 18, 12, 60), Event::Down);
    assert_eq!(rps_timer_event(0, 1000, 12, 12, 60), Event::Steady);
    // Steps double while they continue, and clamp.
    assert_eq!(rps_work(Event::Up, 18, 0, 12, 60), (19, 1));
    assert_eq!(rps_work(Event::Up, 19, 1, 12, 60), (21, 2));
    assert_eq!(rps_work(Event::Up, 49, 16, 12, 60), (60, 32));
    assert_eq!(rps_work(Event::Up, 60, 32, 12, 60), (60, 0));
    assert_eq!(rps_work(Event::Down, 60, 32, 12, 60), (59, -1));
    assert_eq!(rps_work(Event::Down, 13, -4, 12, 60), (12, -8));
    assert_eq!(rps_work(Event::Down, 12, -8, 12, 60), (12, 0));
    assert_eq!(rps_work(Event::Steady, 30, 4, 12, 60), (30, 0));
    // A park is a downclock by an even bin, doubling; never below RPn.
    assert_eq!(park_step(30, 0, 12), (28, -2));
    assert_eq!(park_step(28, -2, 12), (24, -4));
    assert_eq!(park_step(13, -4, 12), (12, -8));
    assert_eq!(park_step(30, i32::MIN, 12), (12, i32::MIN));
}

/// Busy work clocks the GT up to RP0 within milliseconds; idle, it parks
/// at `RPn` after [`crate::backend::PARK_NS`]; the next work unparks it at the
/// last request less a step, and the per-second log says the busy share.
#[test]
fn the_gt_clocks_up_under_load_and_parks_when_idle() {
    use crate::backend::{FREQLOG_NS, PARK_NS};
    let mut be = backend_with_rps();
    let swreq = |be: &mut XelpBackend<Model>| be.platform().gpu.regs[&0xa008] >> 23;
    assert_eq!(swreq(&mut be), 18, "RP1 first");
    be.set_freqlog(true, 0);
    // Work that the model is not given time to run: the engine stays in
    // the ports, busy for every interval.
    let vm = be.vm_create().expect("vm");
    let _out = store_batch(&mut be, vm, 0x1_0000_0000, 0x1_0000_1040, 1);
    let (engine, ctx) = be.context_create(vm, RENDER, 16 << 10).expect("context");
    be.submit(engine, ctx, 0x1_0000_0000, 7).expect("taken");
    let mut now = 0;
    let mut climbed_at = None;
    while now < 200_000_000 {
        now += 500_000;
        be.service(now, false);
        if climbed_at.is_none() && swreq(&mut be) == 60 {
            climbed_at = Some(now);
        }
    }
    let climbed_at = climbed_at.expect("the request reaches RP0");
    assert!(
        climbed_at <= 20_000_000,
        "RP0 within 20 ms: {climbed_at} ns"
    );
    assert_eq!(be.rps().expect("up").cur, 60);
    // Let it finish: idle from here.
    let _ = be.platform().gpu.run();
    be.service(now, true);
    let mut done = Vec::new();
    be.completed(&mut |id, how| done.push((id, how)));
    assert_eq!(done, [(7, Completion::Ok)]);
    // Not parked before PARK_NS of idle; parked at RPn after.
    be.service(now + PARK_NS / 2, false);
    assert!(be.rps().expect("up").active);
    let deadline = be.next_deadline().expect("the park is a deadline");
    assert!(deadline <= now + PARK_NS, "{deadline} {now}");
    be.service(now + PARK_NS, false);
    let parked = be.rps().expect("up");
    assert!(!parked.active, "parked");
    assert_eq!(swreq(&mut be), 12, "RPn while parked");
    assert!(parked.cur < 60, "the unpark comes back a step lower");
    // The log saw a busy second.
    be.service(FREQLOG_NS, false);
    let reports = be.platform().reports.clone();
    assert!(!reports.is_empty());
    assert!(reports[0].busy_permille >= 150, "{:?}", reports[0]);
    // New work unparks it where it left off, less the park's step.
    be.platform().now = (now + PARK_NS * 2) / 1_000;
    be.submit(engine, ctx, 0x1_0000_0000, 8).expect("taken");
    let unparked = be.rps().expect("up");
    assert!(unparked.active);
    assert_eq!(swreq(&mut be), parked.cur);
}

/// A pinned request stays where it was put: no step, no park.
#[test]
fn a_pinned_request_is_left_alone() {
    let mut be = backend_with_rps();
    assert_eq!(be.set_frequency_mhz(1000, true), Some(1000));
    for t in 1..100 {
        be.service(t * 1_000_000, false);
    }
    let rps = be.rps().expect("up");
    assert!(rps.active && rps.pinned);
    assert_eq!(be.platform().gpu.regs[&0xa008] >> 23, 60);
    // Released to the policy, idle, it steps down and parks.
    assert_eq!(be.set_frequency_mhz(1000, false), Some(1000));
    for t in 100..200 {
        be.service(t * 1_000_000, false);
    }
    assert!(!be.rps().expect("up").active);
    assert_eq!(be.platform().gpu.regs[&0xa008] >> 23, 12);
}

/// The checkpoint's load: a spinner from the GGTT that runs until it is
/// ended, then completes and is given back without a completion for the
/// front end.
#[test]
fn the_drivers_own_load_spins_until_ended() {
    let mut be = backend_with_rps();
    be.platform().gpu.spin_resumes = true;
    let load = be.load_start().expect("a load");
    for t in 1..20 {
        let _ = be.platform().gpu.run();
        be.service(t * 1_000_000, false);
    }
    let busy = be.busy_totals(20_000_000);
    assert!(busy[0] >= 18_000_000, "RCS0 busy: {busy:?}");
    assert!(be.load_end(load, 100_000), "it ends");
    let mut done = Vec::new();
    be.completed(&mut |id, how| done.push((id, how)));
    assert_eq!(done, [], "the load's completion is the driver's");
    assert_eq!(be.resets(), [0, 0]);
}

/// Registers that answer forcewake as the hardware does: an ack follows
/// its request at once.
#[derive(Default)]
struct FwRegs {
    regs: std::collections::BTreeMap<u32, u32>,
    log: Vec<(u32, u32)>,
    now: u64,
}

impl Mmio for FwRegs {
    fn read32(&mut self, offset: u32) -> u32 {
        let ack = match offset {
            0xd84 => Some(0xa278),
            0x13_0044 => Some(0xa188),
            _ => None,
        };
        let at = ack.unwrap_or(offset);
        self.regs.get(&at).copied().unwrap_or(0)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.log.push((offset, value));
        let old = self.regs.get(&offset).copied().unwrap_or(0);
        let new = if matches!(offset, 0xa278 | 0xa188) {
            let mask = value >> 16;
            (old & !mask) | (value & mask & 0xffff)
        } else {
            value
        };
        let _ = self.regs.insert(offset, new);
    }
}

impl Clock for FwRegs {
    fn now_us(&mut self) -> u64 {
        self.now
    }

    fn delay_us(&mut self, us: u32) {
        self.now += u64::from(us);
    }
}

/// Forcewake per access: woken for a register that needs it, not for one
/// outside every domain nor for a shadowed write, kept while used, released
/// a millisecond after the last use.
#[test]
fn forcewake_is_taken_per_access_and_released_when_idle() {
    use crate::uncore::{
        AutoForcewake, FORCEWAKE_GT, FORCEWAKE_RELEASE_NS, FORCEWAKE_RENDER, fw_domains_init,
    };
    let domains = fw_domains_init(1 | 2);
    let mut regs = FwRegs::default();
    // The bring-up's hold handed over: released once unused for 1 ms.
    let mut fw = AutoForcewake::new(domains, FORCEWAKE_RENDER | FORCEWAKE_GT, 0);
    assert_eq!(fw.due(), Some(FORCEWAKE_RELEASE_NS));
    fw.release(&mut regs, FORCEWAKE_RELEASE_NS);
    assert_eq!(fw.awake(), 0);
    assert_eq!(regs.regs[&0xa278] & 1, 0, "render released");
    assert_eq!(fw.due(), None);
    // GEN12_RPSTAT1 (0x1381b4) needs GT; the PCU mailbox outside the GT
    // ranges needs none here either way; RCS0's RING_TIMESTAMP needs render.
    let before = regs.log.len();
    fw.access(&mut regs, &mut FwRegs::default(), 0x13_81b4, false);
    let gt = crate::uncore::read_fw_domains(crate::reg::Reg(0x13_81b4));
    assert_eq!(fw.awake(), gt & domains.mask());
    fw.access(&mut regs, &mut FwRegs::default(), 0x2358, false);
    assert_ne!(fw.awake() & FORCEWAKE_RENDER, 0);
    let woken = regs.log.len();
    assert!(woken > before);
    // Awake domains are not asked again.
    fw.access(&mut regs, &mut FwRegs::default(), 0x2358, false);
    assert_eq!(regs.log.len(), woken);
    // A shadowed write (RCS0's RING_TAIL) wakes nothing.
    let mut quiet = AutoForcewake::new(domains, 0, 0);
    quiet.access(&mut regs, &mut FwRegs::default(), 0x2030, true);
    assert_eq!(quiet.awake(), 0);
    // Used since the timer was armed: kept, re-armed; then released.
    let due = fw.due().expect("armed");
    fw.release(&mut regs, due);
    assert_ne!(fw.awake(), 0, "used, so kept");
    fw.release(&mut regs, due + FORCEWAKE_RELEASE_NS);
    assert_eq!(fw.awake(), 0);
    assert_eq!(regs.regs[&0xa278] & 1, 0);
    assert_eq!(fw.failures, 0);
}

/// A PCU that answers the mailbox: every request ends READY clear with
/// success, a read of the ring table answers its row.
#[derive(Default)]
struct Pcu {
    data: u32,
    data1: u32,
    mailbox: u32,
    requests: Vec<(u32, u32)>,
    now: u64,
}

impl Mmio for Pcu {
    fn read32(&mut self, offset: u32) -> u32 {
        match offset {
            0x13_8124 => self.mailbox,
            0x13_8128 => self.data,
            0x13_812c => self.data1,
            _ => 0,
        }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        match offset {
            0x13_8128 => self.data = value,
            0x13_812c => self.data1 = value,
            0x13_8124 => {
                self.requests.push((value & 0xff, self.data));
                if value & 0xff == 0x9 {
                    // The row for GT frequency N: ring 2N x 100 MHz.
                    self.data = (self.data * 2) << 8;
                }
                self.mailbox = 0;
            }
            _ => {}
        }
    }
}

impl Clock for Pcu {
    fn now_us(&mut self) -> u64 {
        self.now
    }

    fn delay_us(&mut self, us: u32) {
        self.now += u64::from(us);
    }
}

/// `gen6_update_ring_freq`'s requests for ADL-N: RP0 (20 x 50 MHz) down to
/// `RPn` (4), ring ratio = GT ratio, no IA ratio; the table read back as
/// `i915_ring_freq_table` reads it; the mailbox's errors.
#[test]
fn the_ring_frequency_table_is_written_as_i915_writes_it() {
    use crate::pcode::{PcodeError, gen6_update_ring_freq, gen7_check_mailbox_status};
    let caps = crate::rps::gen6_rps_get_freq_caps((4 << 16) | 20, 6 << 8);
    let mut pcu = Pcu::default();
    let mut clock = Pcu::default();
    assert_eq!(gen6_update_ring_freq(&mut pcu, &mut clock, caps), (0, None));
    assert_eq!(pcu.requests.len(), 17);
    assert_eq!(pcu.requests[0], (0x8, (20 << 16) | 20));
    assert_eq!(pcu.requests[16], (0x8, (4 << 16) | 4));
    let row = crate::pcode::read_ring_row(&mut pcu, &mut clock, 6).expect("read");
    assert_eq!((row.gpu_freq, row.ring_mhz, row.ia_mhz), (6, 1200, 0));
    assert_eq!(gen7_check_mailbox_status(0), Ok(()));
    assert_eq!(gen7_check_mailbox_status(0x2), Err(PcodeError::Timeout));
    assert_eq!(gen7_check_mailbox_status(0x11), Err(PcodeError::Rejected));
    assert_eq!(gen7_check_mailbox_status(0x10), Err(PcodeError::OutOfRange));
    // A busy mailbox is refused before anything is written.
    pcu.mailbox = 1 << 31;
    let n = pcu.requests.len();
    assert_eq!(
        crate::pcode::read_ring_row(&mut pcu, &mut clock, 6),
        Err(PcodeError::Busy)
    );
    assert_eq!(pcu.requests.len(), n);
}
