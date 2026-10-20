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
    next_ggtt: u32,
    hwsp_given: bool,
    now: u64,
    next_page: u64,
}

impl Model {
    fn new() -> Self {
        Model {
            gpu: Gpu::new(&ENGINES[0], HWSP),
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
