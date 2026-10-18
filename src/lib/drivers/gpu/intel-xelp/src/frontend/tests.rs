//! Host tests of the front end: the probe's numbers and layouts, then each
//! ioctl as Mesa's anv and iris call it on Gen12 (`src/intel/common/i915`,
//! `src/intel/vulkan/i915`, `src/intel/dev/i915`), against a fake back end
//! and a fake chardev core holding a program's memory and descriptors.

#![expect(
    clippy::panic_in_result_fn,
    reason = "AUDIT: test code; the fake back end asserts the promises the front end makes it"
)]

extern crate std;

use std::boxed::Box;
use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use ferrix_linux_abi::errno::Errno;

use super::backend::{Completion, ContextParams, Engine, Facts, HwContext, SubmitError, VmId};
use super::host::{Host, Installed};
use super::{Backend, Frontend, RequestId};
use crate::sseu::{Fuses, gen12_sseu_info_init};
use crate::uapi::*;

// ---------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------

const PROBE: &str = include_str!("../../probe/i915-64.txt");

/// Every `name value` line of the probe that is not a layout is in
/// [`VALUES`] with that value, and every entry there is in the probe.
#[test]
fn values_match_the_probe() {
    let mut probed = BTreeMap::new();
    for line in PROBE.lines() {
        let (name, value) = line.split_once(' ').expect("name value");
        if name.starts_with("sizeof.") || name.starts_with("offsetof.") {
            continue;
        }
        let value: u64 = value.parse().expect("decimal");
        assert!(probed.insert(name, value).is_none(), "{name} twice");
    }
    for (name, value) in VALUES {
        assert_eq!(probed.get(name), Some(value), "{name}: ours {value:#x}");
    }
    for name in probed.keys() {
        assert!(
            VALUES.iter().any(|(ours, _)| ours == name),
            "the probe prints {name}, which VALUES lacks"
        );
    }
}

/// Every structure the probe sizes is one of [`LAYOUTS`] with that
/// size and those field offsets, or named in [`UNMODELLED`] with why;
/// and every field the crate names at a plain C name is in the probe.
#[test]
fn layouts_match_the_probe() {
    let mut sizes = BTreeMap::new();
    let mut offsets = BTreeMap::new();
    for line in PROBE.lines() {
        let (name, value) = line.split_once(' ').expect("name value");
        let value: usize = value.parse().expect("decimal");
        if let Some(name) = name.strip_prefix("sizeof.") {
            let _ = sizes.insert(name, value);
        } else if let Some(name) = name.strip_prefix("offsetof.") {
            let (structure, field) = name.split_once('.').expect("struct.field");
            let _ = offsets.insert((structure, field), value);
        }
    }
    for (name, size) in &sizes {
        match LAYOUTS.iter().find(|layout| layout.name == *name) {
            Some(layout) => assert_eq!(layout.size, *size, "sizeof {name}"),
            None => assert!(
                UNMODELLED.iter().any(|(unmodelled, _)| unmodelled == name),
                "the probe sizes {name}, which the crate neither models nor names as unmodelled"
            ),
        }
    }
    for ((structure, field), offset) in &offsets {
        let Some(layout) = LAYOUTS.iter().find(|layout| layout.name == *structure) else {
            assert!(
                UNMODELLED
                    .iter()
                    .any(|(unmodelled, _)| unmodelled == structure),
                "{structure} has offsets but no layout"
            );
            continue;
        };
        if let Some((_, _, at)) = FLEXIBLE
            .iter()
            .find(|(name, member, _)| name == structure && member == field)
        {
            assert_eq!(at, offset, "offsetof {structure}.{field}");
            continue;
        }
        let ours = layout
            .fields
            .iter()
            .find(|(name, _)| name == field)
            .unwrap_or_else(|| panic!("{structure}.{field} is not modelled"));
        assert_eq!(ours.1, *offset, "offsetof {structure}.{field}");
    }
    for layout in LAYOUTS {
        if layout.name == "drm_version" {
            continue;
        }
        assert!(
            sizes.contains_key(layout.name),
            "{} is not probed",
            layout.name
        );
        for (field, _) in layout.fields {
            if field.contains(['.', '[', ' ']) {
                continue;
            }
            assert!(
                offsets.contains_key(&(layout.name, *field)),
                "{}.{field} is not probed",
                layout.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The fakes
// ---------------------------------------------------------------------------

/// A fake object's memory.
#[derive(Debug)]
struct Memory {
    id: u32,
    size: u64,
}

/// ADL-N as M0 read it (`docs/INTEL-GPU.md` §10): one slice, two DSS of 12
/// EUs (pairs 3 and 7 fused off), 19.2 MHz.
fn facts() -> Facts {
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

#[derive(Debug, Default)]
struct FakeBackend {
    facts: Option<Facts>,
    next: u32,
    vms: Vec<u32>,
    destroyed_vms: Vec<u32>,
    binds: Vec<(u32, u64, u32, u64)>,
    unbinds: Vec<(u32, u64, u64)>,
    contexts: Vec<(u32, u32, Engine)>,
    destroyed_contexts: Vec<u32>,
    submits: Vec<(u32, u64, u32, RequestId)>,
    pending: Vec<(RequestId, Completion)>,
    /// Hold completions until [`FakeBackend::finish`].
    hold: bool,
    held: Vec<RequestId>,
    full: bool,
}

impl FakeBackend {
    fn finish(&mut self, completion: Completion) {
        for id in self.held.drain(..) {
            self.pending.push((id, completion));
        }
    }
}

impl Backend for FakeBackend {
    type Memory = Memory;

    fn facts(&self) -> &Facts {
        self.facts.as_ref().expect("facts")
    }

    fn create_memory(&mut self, bytes: u64) -> Result<Memory, Errno> {
        assert_eq!(bytes % 4096, 0);
        self.next += 1;
        Ok(Memory {
            id: self.next,
            size: bytes,
        })
    }

    fn vm_create(&mut self) -> Result<VmId, Errno> {
        self.next += 1;
        self.vms.push(self.next);
        Ok(VmId(self.next))
    }

    fn vm_destroy(&mut self, vm: VmId) {
        self.destroyed_vms.push(vm.0);
    }

    fn bind(
        &mut self,
        vm: VmId,
        address: u64,
        memory: &Memory,
        bytes: u64,
        pat_index: u8,
    ) -> Result<(), Errno> {
        assert_eq!(address % 4096, 0);
        assert!(address + bytes <= 1 << 48);
        assert!(pat_index <= 3);
        assert!(bytes <= memory.size);
        // The promise: no overlap with another live binding of the VM.
        for (other_vm, other, _, other_bytes) in &self.binds {
            if *other_vm == vm.0
                && !self
                    .unbinds
                    .iter()
                    .any(|(v, a, _)| *v == vm.0 && a == other)
                && address < other + other_bytes
                && *other < address + bytes
            {
                panic!("bind {address:#x} overlaps {other:#x}");
            }
        }
        self.binds.push((vm.0, address, memory.id, bytes));
        Ok(())
    }

    fn unbind(&mut self, vm: VmId, address: u64, bytes: u64) {
        self.unbinds.push((vm.0, address, bytes));
    }

    fn context_create(
        &mut self,
        vm: VmId,
        engine: Engine,
        params: ContextParams,
    ) -> Result<HwContext, Errno> {
        assert_eq!(params, ContextParams::DEFAULT);
        self.next += 1;
        self.contexts.push((self.next, vm.0, engine));
        Ok(HwContext(self.next))
    }

    fn context_destroy(&mut self, context: HwContext) {
        self.destroyed_contexts.push(context.0);
    }

    fn submit(
        &mut self,
        context: HwContext,
        batch: u64,
        length: u32,
        request: RequestId,
    ) -> Result<(), SubmitError> {
        if self.full {
            return Err(SubmitError::Full);
        }
        self.submits.push((context.0, batch, length, request));
        if self.hold {
            self.held.push(request);
        } else {
            self.pending.push((request, Completion::Ok));
        }
        Ok(())
    }

    fn service(&mut self, _now_ns: u64, _interrupt: bool) {}

    fn next_deadline(&self) -> Option<u64> {
        None
    }

    fn completed(&mut self, sink: &mut dyn FnMut(RequestId, Completion)) {
        for (id, completion) in self.pending.drain(..) {
            sink(id, completion);
        }
    }

    fn timestamp(&mut self) -> u64 {
        0x1234_5678_9abc
    }
}

/// What a program's descriptor is, in the fake core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fd {
    Dmabuf(u64),
    Sync(u64),
}

/// The base of the program's fake memory.
const BASE: u64 = 0x7000_0000;

#[derive(Debug, Default)]
struct FakeHost {
    memory: Vec<u8>,
    replies: BTreeMap<u64, i32>,
    maps: Vec<(u64, u32, u64, bool)>,
    fds: BTreeMap<i32, Fd>,
    next_fd: i32,
    dmabufs: BTreeMap<u64, u32>,
    signalled: BTreeMap<u64, i32>,
    slot_fences: Vec<(u64, u64)>,
    now: u64,
    lines: Vec<String>,
    privileged: bool,
}

impl FakeHost {
    fn range(&mut self, address: u64, length: usize) -> Result<core::ops::Range<usize>, Errno> {
        let start = usize::try_from(address.checked_sub(BASE).ok_or(Errno::EFAULT)?)
            .map_err(|_| Errno::EFAULT)?;
        let end = start + length;
        if end > self.memory.len() {
            return Err(Errno::EFAULT);
        }
        Ok(start..end)
    }

    /// Room for `bytes` of the program's memory, zeroed.
    fn alloc(&mut self, bytes: &[u8]) -> u64 {
        let at = self.memory.len().next_multiple_of(8);
        self.memory.resize(at + bytes.len().max(8), 0);
        self.memory[at..at + bytes.len()].copy_from_slice(bytes);
        BASE + at as u64
    }

    fn read(&self, address: u64, length: usize) -> Vec<u8> {
        let at = (address - BASE) as usize;
        self.memory[at..at + length].to_vec()
    }

    fn fd(&mut self, what: Fd) -> i32 {
        self.next_fd += 1;
        let fd = self.next_fd + 2;
        let _ = self.fds.insert(fd, what);
        fd
    }
}

impl Host<Memory> for FakeHost {
    fn copy_in(&mut self, _request: u64, address: u64, out: &mut [u8]) -> Result<(), Errno> {
        let range = self.range(address, out.len())?;
        out.copy_from_slice(&self.memory[range]);
        Ok(())
    }

    fn copy_out(&mut self, _request: u64, address: u64, bytes: &[u8]) -> Result<(), Errno> {
        let range = self.range(address, bytes.len())?;
        self.memory[range].copy_from_slice(bytes);
        Ok(())
    }

    fn reply(&mut self, request: u64, status: i32) {
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
        wc: bool,
    ) -> Result<(), Errno> {
        self.maps.push((request, memory.id, offset, wc));
        self.reply(request, 0);
        Ok(())
    }

    fn dmabuf_install(
        &mut self,
        _request: u64,
        memory: &Memory,
        cookie: u64,
        _writable: bool,
        _cloexec: bool,
    ) -> Result<Installed, Errno> {
        let made = !self.dmabufs.contains_key(&cookie);
        let _ = self.dmabufs.insert(cookie, memory.id);
        Ok(Installed {
            fd: self.fd(Fd::Dmabuf(cookie)),
            made,
        })
    }

    fn dmabuf_resolve(&mut self, _request: u64, fd: i32) -> Result<u64, Errno> {
        match self.fds.get(&fd) {
            Some(Fd::Dmabuf(cookie)) => Ok(*cookie),
            _ => Err(Errno::EBADF),
        }
    }

    fn dmabuf_fence(&mut self, dmabuf: u64, fence: u64) -> Result<(), Errno> {
        if !self.dmabufs.contains_key(&dmabuf) {
            return Err(Errno::EBADF);
        }
        self.slot_fences.push((dmabuf, fence));
        Ok(())
    }

    fn sync_install(&mut self, _request: u64, fence: u64, _cloexec: bool) -> Result<i32, Errno> {
        Ok(self.fd(Fd::Sync(fence)))
    }

    fn sync_resolve(&mut self, _request: u64, fd: i32) -> Result<(u64, bool), Errno> {
        match self.fds.get(&fd) {
            Some(Fd::Sync(cookie)) => Ok((*cookie, self.signalled.contains_key(cookie))),
            _ => Err(Errno::EINVAL),
        }
    }

    fn sync_signal(&mut self, fence: u64, status: i32) {
        assert!(
            self.signalled.insert(fence, status).is_none(),
            "fence {fence} signalled twice"
        );
    }

    fn now_ns(&self) -> u64 {
        self.now
    }

    fn privileged(&self, _request: u64) -> bool {
        self.privileged
    }

    fn say(&mut self, line: core::fmt::Arguments<'_>) {
        self.lines.push(format!("{line}"));
    }
}

/// A front end and a program, and the requests between them.
struct Rig {
    frontend: Box<Frontend<FakeBackend>>,
    host: FakeHost,
    next: u64,
}

/// What an ioctl came to: its status, or held.
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Done(i32),
    Held(u64),
}

const ENOENT: i32 = -2;
const EINVAL: i32 = -22;
const EACCES: i32 = -13;
const ENODEV: i32 = -19;
const ETIME: i32 = -62;
const EIO: i32 = -5;
const EPERM: i32 = -1;

impl Rig {
    fn new() -> Self {
        let backend = FakeBackend {
            facts: Some(facts()),
            ..FakeBackend::default()
        };
        let mut rig = Rig {
            frontend: Box::new(Frontend::new(backend)),
            host: FakeHost::default(),
            next: 1,
        };
        assert_eq!(rig.open(1), 0);
        rig
    }

    fn request(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn open(&mut self, file: u64) -> i32 {
        let request = self.request();
        self.frontend.open(&mut self.host, request, file, 100);
        self.host.replies[&request]
    }

    /// An ioctl with its argument in `bytes`, which is read back.
    fn ioctl_on(&mut self, file: u64, cmd: u32, bytes: &mut [u8]) -> Status {
        let request = self.request();
        let arg = self.host.alloc(bytes);
        self.frontend.ioctl(&mut self.host, request, file, cmd, arg);
        bytes.copy_from_slice(&self.host.read(arg, bytes.len()));
        match self.host.replies.get(&request) {
            Some(status) => Status::Done(*status),
            None => Status::Held(request),
        }
    }

    fn ioctl(&mut self, cmd: u32, bytes: &mut [u8]) -> i32 {
        match self.ioctl_on(1, cmd, bytes) {
            Status::Done(status) => status,
            Status::Held(request) => panic!("request {request} held"),
        }
    }

    fn getparam(&mut self, param: i32) -> (i32, i32) {
        let value = self.host.alloc(&[0; 4]);
        let mut args = GetParam { param, value }.to_bytes();
        let status = self.ioctl(ioctl::GETPARAM, &mut args);
        let raw = self.host.read(value, 4);
        (status, i32::from_le_bytes(raw.try_into().expect("4")))
    }

    fn create(&mut self, size: u64) -> u32 {
        let mut args = GemCreate {
            size,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(self.ioctl(ioctl::GEM_CREATE, &mut args), 0);
        GemCreate::from_bytes(&args).expect("create").handle
    }

    fn syncobj(&mut self, signalled: bool) -> u32 {
        let mut args = SyncobjCreate {
            handle: 0,
            flags: u32::from(signalled),
        }
        .to_bytes();
        assert_eq!(self.ioctl(ioctl::SYNCOBJ_CREATE, &mut args), 0);
        SyncobjCreate::from_bytes(&args).expect("syncobj").handle
    }

    /// An execbuffer of `objects` (handle, address, flags), the last the
    /// batch, with `flags` and the given fence array.
    fn exec(
        &mut self,
        cmd: u32,
        objects: &[(u32, u64, u64)],
        flags: u64,
        fences: &[(u32, u32)],
        rsvd2: u64,
    ) -> (i32, Execbuffer2) {
        let mut list = Vec::new();
        for (handle, offset, object_flags) in objects {
            list.extend_from_slice(
                &ExecObject2 {
                    handle: *handle,
                    offset: *offset,
                    flags: *object_flags,
                    ..Default::default()
                }
                .to_bytes(),
            );
        }
        let buffers_ptr = self.host.alloc(&list);
        let mut fence_bytes = Vec::new();
        for (handle, fence_flags) in fences {
            fence_bytes.extend_from_slice(
                &ExecFence {
                    handle: *handle,
                    flags: *fence_flags,
                }
                .to_bytes(),
            );
        }
        let (cliprects_ptr, num_cliprects, flags) = if fences.is_empty() {
            (0, 0, flags)
        } else {
            (
                self.host.alloc(&fence_bytes),
                fences.len() as u32,
                flags | exec::FENCE_ARRAY,
            )
        };
        let mut args = Execbuffer2 {
            buffers_ptr,
            buffer_count: objects.len() as u32,
            batch_len: 0,
            cliprects_ptr,
            num_cliprects,
            flags: flags | exec::NO_RELOC | exec::HANDLE_LUT,
            rsvd2,
            ..Default::default()
        }
        .to_bytes();
        let status = self.ioctl(cmd, &mut args);
        (status, Execbuffer2::from_bytes(&args).expect("execbuffer"))
    }
}

/// Run `test` on a thread with room for the tables.
fn big(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(test)
        .expect("thread")
        .join()
        .expect("test");
}

const PINNED: u64 = exec::OBJECT_PINNED | exec::OBJECT_SUPPORTS_48B_ADDRESS;

// ---------------------------------------------------------------------------
// The common ioctls
// ---------------------------------------------------------------------------

/// `DRM_IOCTL_VERSION` names `i915`, which is what makes Mesa choose its
/// i915 back end (`intel_kmd.c`), with the lengths told even when the
/// buffers are short.
#[test]
fn version_names_i915() {
    big(|| {
        let mut rig = Rig::new();
        let name = rig.host.alloc(&[0; 8]);
        let mut args = Version {
            name_len: 8,
            name,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::VERSION, &mut args), 0);
        let version = Version::from_bytes(&args).expect("version");
        assert_eq!((version.version_major, version.version_minor), (1, 6));
        assert_eq!(version.name_len, 4);
        assert_eq!(&rig.host.read(name, 4), b"i915");
        assert_eq!(version.date_len, 1);
        assert_eq!(version.desc_len, 14);
    });
}

/// The render node's permission rule (`drm_ioctl_permit`), the wrong type,
/// an unknown driver number, and the capabilities.
#[test]
fn render_node_permissions_and_caps() {
    big(|| {
        let mut rig = Rig::new();
        let mut cap = GetCap {
            capability: 1,
            value: 1,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::SET_CLIENT_CAP, &mut cap), EACCES);
        let mut flink = [0_u8; 8];
        assert_eq!(rig.ioctl(ioctl::GEM_FLINK, &mut flink), EACCES);
        let mut nothing = [0_u8; 8];
        assert_eq!(rig.ioctl(ioc(3, b'x', 1, 8), &mut nothing), -25);
        assert_eq!(rig.ioctl(drm_iowr(0x5f, 8), &mut nothing), EINVAL);
        for (capability, want) in [
            (drm::CAP_SYNCOBJ, 1),
            (drm::CAP_SYNCOBJ_TIMELINE, 1),
            (drm::CAP_PRIME, 3),
        ] {
            let mut args = GetCap {
                capability,
                value: 0,
            }
            .to_bytes();
            assert_eq!(rig.ioctl(ioctl::GET_CAP, &mut args), 0);
            assert_eq!(GetCap::from_bytes(&args).expect("cap").value, want);
        }
        let mut dumb = GetCap {
            capability: 1,
            value: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GET_CAP, &mut dumb), -95);
    });
}

// ---------------------------------------------------------------------------
// GETPARAM and QUERY
// ---------------------------------------------------------------------------

/// Every parameter anv refuses to start without (`anv_device.c`,
/// `anv_i915_physical_device_get_parameters`), and those Mesa's device
/// info reads.
#[test]
fn getparam_answers_what_anv_needs() {
    big(|| {
        let mut rig = Rig::new();
        for required in [
            param::HAS_WAIT_TIMEOUT,
            param::HAS_EXECBUF2,
            param::MMAP_VERSION,
            param::HAS_EXEC_SOFTPIN,
            param::HAS_EXEC_FENCE_ARRAY,
            param::HAS_EXEC_ASYNC,
            param::HAS_EXEC_CAPTURE,
            param::HAS_EXEC_TIMELINE_FENCES,
            param::HAS_CONTEXT_ISOLATION,
            param::HAS_LLC,
        ] {
            assert_eq!(rig.getparam(required), (0, 1), "param {required}");
        }
        assert_eq!(rig.getparam(param::CHIPSET_ID), (0, 0x46d4));
        assert_eq!(rig.getparam(param::CS_TIMESTAMP_FREQUENCY), (0, 19_200_000));
        assert_eq!(rig.getparam(param::EU_TOTAL), (0, 24));
        assert_eq!(rig.getparam(param::SUBSLICE_TOTAL), (0, 2));
        assert_eq!(rig.getparam(param::SLICE_MASK), (0, 1));
        assert_eq!(rig.getparam(param::SUBSLICE_MASK), (0, 3));
        assert_eq!(rig.getparam(param::MMAP_GTT_VERSION), (0, 4));
        assert_eq!(rig.getparam(param::HAS_USERPTR_PROBE), (0, 0));
        assert_eq!(rig.getparam(param::HAS_BLT), (0, 1));
        assert_eq!(rig.getparam(param::HAS_BSD), (0, 0));
        assert_eq!(rig.getparam(param::PXP_STATUS).0, ENODEV);
        assert_eq!(rig.getparam(param::IRQ_ACTIVE).0, ENODEV);
        assert_eq!(rig.getparam(param::PERF_REVISION).0, EINVAL);
        assert_eq!(rig.getparam(9999).0, EINVAL);
    });
}

/// `intel_i915_query_alloc`'s two calls: the length, then the data; and
/// the items Gen12.0 has not.
#[test]
fn query_items_in_two_calls() {
    big(|| {
        let mut rig = Rig::new();
        let query = |rig: &mut Rig, query_id: u64, length: i32, data_ptr: u64| -> i32 {
            let item = rig.host.alloc(
                &QueryItem {
                    query_id,
                    length,
                    flags: 0,
                    data_ptr,
                }
                .to_bytes(),
            );
            let mut args = Query {
                num_items: 1,
                flags: 0,
                items_ptr: item,
            }
            .to_bytes();
            assert_eq!(rig.ioctl(ioctl::QUERY, &mut args), 0);
            QueryItem::from_bytes(&rig.host.read(item, QueryItem::SIZE))
                .expect("item")
                .length
        };
        let length = query(&mut rig, query::TOPOLOGY_INFO, 0, 0);
        assert_eq!(length as usize, crate::sseu::TOPOLOGY_LEN);
        let data = rig.host.alloc(&vec![0; length as usize]);
        assert_eq!(query(&mut rig, query::TOPOLOGY_INFO, length, data), length);
        let topology = TopologyInfo::from_bytes(&rig.host.read(data, 16)).expect("topology");
        assert_eq!(
            (
                topology.max_slices,
                topology.max_subslices,
                topology.max_eus_per_subslice
            ),
            (1, 6, 16)
        );
        assert_eq!(rig.host.read(data + 16, 2), [1, 3]);
        assert_eq!(rig.host.read(data + 18, 4), [0x3f, 0x3f, 0x3f, 0x3f]);
        assert_eq!(
            query(&mut rig, query::TOPOLOGY_INFO, length - 1, data),
            EINVAL
        );

        let length = query(&mut rig, query::ENGINE_INFO, 0, 0);
        assert_eq!(length as usize, 16 + 2 * 56);
        let data = rig.host.alloc(&vec![0; length as usize]);
        assert_eq!(query(&mut rig, query::ENGINE_INFO, length, data), length);
        assert_eq!(
            QueryEngineInfo::from_bytes(&rig.host.read(data, 16))
                .expect("engines")
                .num_engines,
            2
        );
        let copy = EngineInfo::from_bytes(&rig.host.read(data + 16 + 56, 56)).expect("bcs");
        assert_eq!(
            (copy.engine_class, copy.flags),
            (1, query::ENGINE_INFO_HAS_LOGICAL_INSTANCE)
        );

        let length = query(&mut rig, query::MEMORY_REGIONS, 0, 0);
        let data = rig.host.alloc(&vec![0; length as usize]);
        assert_eq!(query(&mut rig, query::MEMORY_REGIONS, length, data), length);
        let region = MemoryRegionInfo::from_bytes(&rig.host.read(data + 16, 88)).expect("region");
        assert_eq!((region.memory_class, region.probed_size), (0, 16 << 30));

        assert_eq!(query(&mut rig, query::HWCONFIG_BLOB, 0, 0), ENODEV);
        assert_eq!(query(&mut rig, query::GEOMETRY_SUBSLICES, 0, 0), ENODEV);
        assert_eq!(query(&mut rig, query::GUC_SUBMISSION_VERSION, 0, 0), ENODEV);
        assert_eq!(query(&mut rig, 99, 0, 0), EINVAL);
    });
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

/// Create, the extension chain, and close.
#[test]
fn objects_are_created_and_closed() {
    big(|| {
        let mut rig = Rig::new();
        let mut args = GemCreate {
            size: 5000,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CREATE, &mut args), 0);
        let created = GemCreate::from_bytes(&args).expect("create");
        assert_eq!(created.size, 8192);
        let mut zero = GemCreate::default().to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CREATE, &mut zero), EINVAL);

        let regions = rig.host.alloc(&MemoryClassInstance::default().to_bytes());
        let mut ext = CreateExtMemoryRegions {
            pad: 0,
            num_regions: 1,
            regions,
        }
        .to_bytes();
        ext[8..12].copy_from_slice(&gem::CREATE_EXT_MEMORY_REGIONS.to_le_bytes());
        let ext_at = rig.host.alloc(&ext);
        let mut args = GemCreateExt {
            size: 4096,
            extensions: ext_at,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CREATE_EXT, &mut args), 0);
        let device = rig.host.alloc(
            &MemoryClassInstance {
                memory_class: 1,
                memory_instance: 0,
            }
            .to_bytes(),
        );
        let mut bad = CreateExtMemoryRegions {
            pad: 0,
            num_regions: 1,
            regions: device,
        }
        .to_bytes();
        bad[8..12].copy_from_slice(&gem::CREATE_EXT_MEMORY_REGIONS.to_le_bytes());
        let bad_at = rig.host.alloc(&bad);
        let mut args = GemCreateExt {
            size: 4096,
            extensions: bad_at,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CREATE_EXT, &mut args), EINVAL);
        let mut pat = [0_u8; 40];
        pat[8..12].copy_from_slice(&gem::CREATE_EXT_SET_PAT.to_le_bytes());
        let pat_at = rig.host.alloc(&pat);
        let mut args = GemCreateExt {
            size: 4096,
            extensions: pat_at,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CREATE_EXT, &mut args), ENODEV);

        let mut close = GemClose {
            handle: created.handle,
            pad: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CLOSE, &mut close), 0);
        assert_eq!(rig.ioctl(ioctl::GEM_CLOSE, &mut close), ENOENT);
        assert_eq!(
            rig.frontend.state.objects.len(),
            1,
            "the closed object is freed"
        );
    });
}

/// `MMAP_OFFSET` then `mmap`: the object's VMO, write-back or
/// write-combining; only a file that asked may map it; GTT is `ENODEV`.
#[test]
fn mmap_offsets_map_the_object() {
    big(|| {
        let mut rig = Rig::new();
        let handle = rig.create(16384);
        let offset = |rig: &mut Rig, flags: u64| -> (i32, u64) {
            let mut args = GemMmapOffset {
                handle,
                flags,
                ..Default::default()
            }
            .to_bytes();
            let status = rig.ioctl(ioctl::GEM_MMAP_OFFSET, &mut args);
            (
                status,
                GemMmapOffset::from_bytes(&args).expect("offset").offset,
            )
        };
        assert_eq!(offset(&mut rig, gem::MMAP_OFFSET_GTT).0, ENODEV);
        assert_eq!(offset(&mut rig, 9).0, EINVAL);
        let (status, wb) = offset(&mut rig, gem::MMAP_OFFSET_WB);
        assert_eq!(status, 0);
        let request = rig.request();
        rig.frontend.mmap(&mut rig.host, request, 1, wb + 4096, 2);
        assert_eq!(
            rig.host.maps.last().copied(),
            Some((request, 1, 4096, false))
        );
        let request = rig.request();
        rig.frontend.mmap(&mut rig.host, request, 1, wb, 5);
        assert_eq!(rig.host.replies[&request], EINVAL, "past the end");
        assert_eq!(rig.open(2), 0);
        let request = rig.request();
        rig.frontend.mmap(&mut rig.host, request, 2, wb, 1);
        assert_eq!(
            rig.host.replies[&request], EACCES,
            "another file did not ask"
        );
        let (_, wc) = offset(&mut rig, gem::MMAP_OFFSET_WC);
        let request = rig.request();
        rig.frontend.mmap(&mut rig.host, request, 1, wc, 1);
        assert_eq!(
            rig.host.replies[&request], EINVAL,
            "K6: WC only before any mapping"
        );
    });
}

/// `i915_gem_supports_dma_buf_sync_file`'s probe: create, export, export a
/// sync file from the dmabuf (the kernel's), import it back (passed to the
/// driver as data), close.
#[test]
fn dmabuf_sync_file_probe() {
    big(|| {
        let mut rig = Rig::new();
        let handle = rig.create(64 << 10);
        let mut args = PrimeHandle {
            handle,
            flags: drm::CLOEXEC | drm::RDWR,
            fd: -1,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::PRIME_HANDLE_TO_FD, &mut args), 0);
        let fd = PrimeHandle::from_bytes(&args).expect("prime").fd;
        let Some(Fd::Dmabuf(cookie)) = rig.host.fds.get(&fd).copied() else {
            panic!("no dmabuf");
        };
        let request = rig.request();
        rig.frontend
            .dmabuf_import(&mut rig.host, request, cookie, 3, 2, 0);
        assert_eq!(rig.host.replies[&request], 0);
        let request = rig.request();
        rig.frontend
            .dmabuf_import(&mut rig.host, request, cookie, 3, 3, 0);
        assert_eq!(
            rig.host.replies[&request], -95,
            "a foreign fence is refused"
        );
        let mut back = PrimeHandle {
            handle: 0,
            flags: 0,
            fd,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::PRIME_FD_TO_HANDLE, &mut back), 0);
        assert_eq!(
            PrimeHandle::from_bytes(&back).expect("prime").handle,
            handle,
            "the same handle"
        );
        let mut close = GemClose { handle, pad: 0 }.to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CLOSE, &mut close), 0);
        assert_eq!(rig.frontend.state.objects.len(), 1, "held by its dmabuf");
        rig.frontend.dmabuf_release(&mut rig.host, cookie);
        assert_eq!(rig.frontend.state.objects.len(), 0, "freed with its dmabuf");
    });
}

// ---------------------------------------------------------------------------
// Contexts and VMs
// ---------------------------------------------------------------------------

/// `i915_gem_create_context_engines` as anv calls it: engines, recoverable
/// and a VM, as extensions; then the parameters.
#[test]
fn contexts_with_engines_and_vms() {
    big(|| {
        let mut rig = Rig::new();
        let mut vm = VmControl::default().to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_VM_CREATE, &mut vm), 0);
        let vm_id = VmControl::from_bytes(&vm).expect("vm").vm_id;
        assert_ne!(vm_id, 0);
        let mut engines = 0_u64.to_le_bytes().to_vec();
        engines.extend_from_slice(
            &EngineClassInstance {
                engine_class: 0,
                engine_instance: 0,
            }
            .to_bytes(),
        );
        engines.extend_from_slice(
            &EngineClassInstance {
                engine_class: 1,
                engine_instance: 0,
            }
            .to_bytes(),
        );
        let engines_at = rig.host.alloc(&engines);
        let setparam = |rig: &mut Rig, next: u64, param: u64, value: u64, size: u32| -> u64 {
            let ext = ContextCreateExtSetparam {
                next_extension: next,
                ctx_id: 0,
                size,
                param,
                value,
            };
            rig.host.alloc(&ext.to_bytes())
        };
        let vm_ext = setparam(&mut rig, 0, context::PARAM_VM, u64::from(vm_id), 0);
        let recoverable = setparam(&mut rig, vm_ext, context::PARAM_RECOVERABLE, 0, 0);
        let engines_ext = setparam(
            &mut rig,
            recoverable,
            context::PARAM_ENGINES,
            engines_at,
            16,
        );
        let mut args = ContextCreateExt {
            ctx_id: 0,
            flags: context::CREATE_FLAGS_USE_EXTENSIONS,
            extensions: engines_ext,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CONTEXT_CREATE_EXT, &mut args), 0);
        let ctx_id = ContextCreateExt::from_bytes(&args).expect("context").ctx_id;
        assert_ne!(ctx_id, 0);
        let get = |rig: &mut Rig, ctx_id: u32, param: u64| -> (i32, u64) {
            let mut args = ContextParam {
                ctx_id,
                param,
                ..Default::default()
            }
            .to_bytes();
            let status = rig.ioctl(ioctl::GEM_CONTEXT_GETPARAM, &mut args);
            (
                status,
                ContextParam::from_bytes(&args).expect("param").value,
            )
        };
        assert_eq!(get(&mut rig, ctx_id, context::PARAM_RECOVERABLE), (0, 0));
        assert_eq!(get(&mut rig, ctx_id, context::PARAM_GTT_SIZE), (0, 1 << 48));
        let (status, default_vm) = get(&mut rig, 0, context::PARAM_VM);
        assert_eq!(status, 0);
        assert_ne!(default_vm, 0, "has_vm_control");
        let set = |rig: &mut Rig, ctx_id: u32, param: u64, value: u64| -> i32 {
            let mut args = ContextParam {
                ctx_id,
                param,
                value,
                size: 0,
            }
            .to_bytes();
            rig.ioctl(ioctl::GEM_CONTEXT_SETPARAM, &mut args)
        };
        assert_eq!(
            set(&mut rig, ctx_id, context::PARAM_VM, u64::from(vm_id)),
            EINVAL,
            "finalized"
        );
        assert_eq!(set(&mut rig, 0, context::PARAM_PRIORITY, 0), 0);
        assert_eq!(set(&mut rig, 0, context::PARAM_PRIORITY, 512), EPERM);
        assert_eq!(set(&mut rig, 0, context::PARAM_PRIORITY, 2000), EINVAL);
        assert_eq!(set(&mut rig, 0, context::PARAM_PERSISTENCE, 0), ENODEV);
        assert_eq!(set(&mut rig, 0, context::PARAM_LOW_LATENCY, 1), EINVAL);
        let mut destroy = [0_u8; 8];
        destroy[..4].copy_from_slice(&ctx_id.to_le_bytes());
        assert_eq!(rig.ioctl(ioctl::GEM_CONTEXT_DESTROY, &mut destroy), 0);
        assert_eq!(rig.ioctl(ioctl::GEM_CONTEXT_DESTROY, &mut destroy), ENOENT);
        let mut default = [0_u8; 8];
        assert_eq!(rig.ioctl(ioctl::GEM_CONTEXT_DESTROY, &mut default), ENOENT);
    });
}

// ---------------------------------------------------------------------------
// Execbuffer, fences and waits
// ---------------------------------------------------------------------------

/// anv's submission: pinned objects, the batch last, a syncobj signalled;
/// the binding made once and kept; the batch at its address.
#[test]
fn execbuffer_binds_and_submits() {
    big(|| {
        let mut rig = Rig::new();
        let data = rig.create(8192);
        let batch = rig.create(4096);
        let done = rig.syncobj(false);
        let objects = [
            (data, 0x10_0000, PINNED | exec::OBJECT_WRITE),
            (batch, 0x20_0000, PINNED),
        ];
        let (status, _) = rig.exec(
            ioctl::GEM_EXECBUFFER2,
            &objects,
            0,
            &[(done, exec::FENCE_SIGNAL)],
            0,
        );
        assert_eq!(status, 0);
        let backend = &rig.frontend.backend;
        assert_eq!(backend.binds.len(), 2);
        assert_eq!(backend.submits.len(), 1);
        assert_eq!(backend.submits[0].1, 0x20_0000);
        assert_eq!(backend.submits[0].2, 4096);
        assert_eq!(
            backend.contexts[0].2,
            Engine {
                class: 0,
                instance: 0
            }
        );
        let (status, _) = rig.exec(
            ioctl::GEM_EXECBUFFER2,
            &objects,
            0,
            &[(done, exec::FENCE_WAIT)],
            0,
        );
        assert_eq!(status, 0);
        assert_eq!(rig.frontend.backend.binds.len(), 2, "bound once");
        assert_eq!(rig.frontend.backend.submits.len(), 2);
        assert_eq!(
            rig.frontend.backend.contexts.len(),
            1,
            "one hardware context"
        );
    });
}

/// What i915 refuses on Gen12 past TGL, and what this driver does not
/// serve, each by its check.
#[test]
#[allow(clippy::too_many_lines, reason = "one scenario, step by step")]
fn execbuffer_refusals() {
    big(|| {
        let mut rig = Rig::new();
        let batch = rig.create(4096);
        let mut relocs = ExecObject2 {
            handle: batch,
            relocation_count: 1,
            flags: PINNED,
            ..Default::default()
        }
        .to_bytes()
        .to_vec();
        let ptr = rig.host.alloc(&relocs);
        let mut args = Execbuffer2 {
            buffers_ptr: ptr,
            buffer_count: 1,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(
            rig.ioctl(ioctl::GEM_EXECBUFFER2, &mut args),
            EINVAL,
            "relocations"
        );
        relocs.clear();
        assert_eq!(
            rig.exec(ioctl::GEM_EXECBUFFER2, &[(batch, 0x1000, 0)], 0, &[], 0)
                .0,
            EINVAL,
            "unpinned"
        );
        assert!(
            rig.host
                .lines
                .iter()
                .any(|line| line.contains("not EXEC_OBJECT_PINNED"))
        );
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1001, PINNED)],
                0,
                &[],
                0
            )
            .0,
            EINVAL,
            "unaligned"
        );
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1_0000_0000, exec::OBJECT_PINNED)],
                0,
                &[],
                0
            )
            .0,
            EINVAL,
            "past 4 GiB without 48B"
        );
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED | exec::OBJECT_WRITE)],
                0,
                &[],
                0
            )
            .0,
            EINVAL,
            "a written batch"
        );
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                exec::VEBOX,
                &[],
                0
            )
            .0,
            EINVAL,
            "no VECS"
        );
        assert_eq!(
            rig.exec(ioctl::GEM_EXECBUFFER2, &[(77, 0x1000, PINNED)], 0, &[], 0)
                .0,
            ENOENT
        );
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                exec::SECURE,
                &[],
                0
            )
            .0,
            ENODEV
        );
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[(99, exec::FENCE_WAIT)],
                0
            )
            .0,
            ENOENT
        );
        let empty = rig.syncobj(false);
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[(empty, exec::FENCE_WAIT)],
                0
            )
            .0,
            EINVAL,
            "no fence to wait for"
        );
        assert!(rig.frontend.backend.submits.is_empty());
    });
}

/// `GEM_WAIT` and `SYNCOBJ_WAIT` held while the GPU runs, answered when it
/// finishes; a poll answers `ETIME`; the deadline answers `ETIME` too.
#[test]
fn waits_are_held_and_answered() {
    big(|| {
        let mut rig = Rig::new();
        rig.frontend.backend.hold = true;
        let batch = rig.create(4096);
        let done = rig.syncobj(false);
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[(done, exec::FENCE_SIGNAL)],
                0
            )
            .0,
            0
        );
        let mut poll = GemWait {
            bo_handle: batch,
            flags: 0,
            timeout_ns: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_WAIT, &mut poll), ETIME);
        let mut wait = GemWait {
            bo_handle: batch,
            flags: 0,
            timeout_ns: 1_000_000,
        }
        .to_bytes();
        let Status::Held(gem_wait) = rig.ioctl_on(1, ioctl::GEM_WAIT, &mut wait) else {
            panic!("not held");
        };
        let handles = rig.host.alloc(&done.to_le_bytes());
        let mut sync = SyncobjWait {
            handles,
            timeout_nsec: i64::MAX,
            count_handles: 1,
            flags: drm::SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
            ..Default::default()
        }
        .to_bytes();
        let Status::Held(sync_wait) = rig.ioctl_on(1, ioctl::SYNCOBJ_WAIT, &mut sync) else {
            panic!("not held");
        };
        rig.host.now = 500_000;
        rig.frontend.backend.finish(Completion::Ok);
        rig.frontend.service(&mut rig.host, true);
        assert_eq!(rig.host.replies[&gem_wait], 0);
        assert_eq!(rig.host.replies[&sync_wait], 0);

        // A wait that runs out.
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        let mut wait = GemWait {
            bo_handle: batch,
            flags: 0,
            timeout_ns: 1000,
        }
        .to_bytes();
        let Status::Held(late) = rig.ioctl_on(1, ioctl::GEM_WAIT, &mut wait) else {
            panic!("not held");
        };
        assert_eq!(rig.frontend.next_deadline(), Some(501_000));
        rig.host.now = 600_000;
        rig.frontend.service(&mut rig.host, false);
        assert_eq!(rig.host.replies[&late], ETIME);
    });
}

/// A syncobj wait of the 32-byte layout Mesa built against older headers
/// sends (`deadline_nsec` came later): `drm_ioctl` zero-extends it.
#[test]
fn older_syncobj_wait_layout_is_accepted() {
    big(|| {
        let mut rig = Rig::new();
        let signalled = rig.syncobj(true);
        let handles = rig.host.alloc(&signalled.to_le_bytes());
        let full = SyncobjWait {
            handles,
            timeout_nsec: 0,
            count_handles: 1,
            flags: drm::SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
            ..Default::default()
        }
        .to_bytes();
        let mut short = full[..32].to_vec();
        assert_eq!(rig.ioctl(ioc(3, b'd', 0xc3, 32), &mut short), 0);
        // vk_drm_syncobj's probe: a wait of 0 on a signalled syncobj succeeds.
        let unsignalled = rig.syncobj(false).to_le_bytes();
        let handles = rig.host.alloc(&unsignalled);
        let mut wait = SyncobjWait {
            handles,
            timeout_nsec: 0,
            count_handles: 1,
            flags: 0,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(
            rig.ioctl(ioctl::SYNCOBJ_WAIT, &mut wait),
            EINVAL,
            "no fence, not waiting for submit"
        );
    });
}

/// Timelines: signal points, query, wait for a point not yet submitted
/// (held), submit it through an execbuffer's timeline extension.
#[test]
fn timeline_points() {
    big(|| {
        let mut rig = Rig::new();
        let timeline = rig.syncobj(false);
        let handles = rig.host.alloc(&timeline.to_le_bytes());
        let points = rig.host.alloc(&2_u64.to_le_bytes());
        let mut signal = SyncobjTimelineArray {
            handles,
            points,
            count_handles: 1,
            flags: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::SYNCOBJ_TIMELINE_SIGNAL, &mut signal), 0);
        let out = rig.host.alloc(&[0; 8]);
        let mut query = SyncobjTimelineArray {
            handles,
            points: out,
            count_handles: 1,
            flags: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::SYNCOBJ_QUERY, &mut query), 0);
        assert_eq!(rig.host.read(out, 8), 2_u64.to_le_bytes());
        let three = rig.host.alloc(&3_u64.to_le_bytes());
        let mut wait = SyncobjTimelineWait {
            handles,
            points: three,
            timeout_nsec: i64::MAX,
            count_handles: 1,
            flags: drm::SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
            ..Default::default()
        }
        .to_bytes();
        let Status::Held(held) = rig.ioctl_on(1, ioctl::SYNCOBJ_TIMELINE_WAIT, &mut wait) else {
            panic!("not held");
        };
        let mut not_submitted = SyncobjTimelineWait {
            handles,
            points: three,
            timeout_nsec: 0,
            count_handles: 1,
            flags: 0,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(
            rig.ioctl(ioctl::SYNCOBJ_TIMELINE_WAIT, &mut not_submitted),
            EINVAL
        );
        // An execbuffer signalling point 3 through the extension.
        let batch = rig.create(4096);
        let fence = rig.host.alloc(
            &ExecFence {
                handle: timeline,
                flags: exec::FENCE_SIGNAL,
            }
            .to_bytes(),
        );
        let values = rig.host.alloc(&3_u64.to_le_bytes());
        let ext = ExecbufferExtTimelineFences {
            next_extension: 0,
            name: exec::EXT_TIMELINE_FENCES,
            flags: 0,
            fence_count: 1,
            handles_ptr: fence,
            values_ptr: values,
        };
        let ext_at = rig.host.alloc(&ext.to_bytes());
        let list = rig.host.alloc(
            &ExecObject2 {
                handle: batch,
                offset: 0x1000,
                flags: PINNED,
                ..Default::default()
            }
            .to_bytes(),
        );
        let mut args = Execbuffer2 {
            buffers_ptr: list,
            buffer_count: 1,
            cliprects_ptr: ext_at,
            flags: exec::USE_EXTENSIONS,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_EXECBUFFER2, &mut args), 0);
        assert_eq!(rig.host.replies[&held], 0, "submitted and done");
        assert_eq!(rig.ioctl(ioctl::SYNCOBJ_QUERY, &mut query), 0);
        assert_eq!(rig.host.read(out, 8), 3_u64.to_le_bytes());
    });
}

/// Sync files: an out fence, signalled when the request completes; and a
/// syncobj's fence exported and imported back.
#[test]
fn sync_files_round_trip() {
    big(|| {
        let mut rig = Rig::new();
        rig.frontend.backend.hold = true;
        let batch = rig.create(4096);
        let (status, args) = rig.exec(
            ioctl::GEM_EXECBUFFER2_WR,
            &[(batch, 0x1000, PINNED)],
            exec::FENCE_OUT,
            &[],
            0,
        );
        assert_eq!(status, 0);
        let fd = (args.rsvd2 >> 32) as i32;
        let Some(Fd::Sync(cookie)) = rig.host.fds.get(&fd).copied() else {
            panic!("no out fence");
        };
        assert!(!rig.host.signalled.contains_key(&cookie));
        // That fence, as another execbuffer's in fence.
        let (status, _) = rig.exec(
            ioctl::GEM_EXECBUFFER2,
            &[(batch, 0x1000, PINNED)],
            exec::FENCE_IN,
            &[],
            u64::from(fd as u32),
        );
        assert_eq!(status, 0);
        rig.frontend.backend.finish(Completion::Ok);
        rig.frontend.service(&mut rig.host, true);
        assert_eq!(rig.host.signalled.get(&cookie), Some(&0));
        assert_eq!(
            rig.frontend.backend.submits.len(),
            2,
            "the second ran after the first"
        );

        let sync = rig.syncobj(true);
        let mut export = SyncobjHandle {
            handle: sync,
            flags: drm::SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE,
            fd: -1,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::SYNCOBJ_HANDLE_TO_FD, &mut export), 0);
        let exported = SyncobjHandle::from_bytes(&export).expect("export").fd;
        let other = rig.syncobj(false);
        let mut import = SyncobjHandle {
            handle: other,
            flags: drm::SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE,
            fd: exported,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::SYNCOBJ_FD_TO_HANDLE, &mut import), 0);
        let mut opaque = SyncobjHandle {
            handle: sync,
            ..Default::default()
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::SYNCOBJ_HANDLE_TO_FD, &mut opaque), -95);
    });
}

/// K5's contract (ledger 2360): an execbuffer that reads or writes a shared
/// object sets its dmabuf's slot, and the slot's fence signals with the
/// request.
#[test]
fn shared_objects_set_their_dmabuf_slot() {
    big(|| {
        let mut rig = Rig::new();
        rig.frontend.backend.hold = true;
        let shared = rig.create(4096);
        let batch = rig.create(4096);
        let mut prime = PrimeHandle {
            handle: shared,
            flags: drm::RDWR,
            fd: -1,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::PRIME_HANDLE_TO_FD, &mut prime), 0);
        // Read only: the slot is set for reads too.
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(shared, 0x10000, PINNED), (batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        assert_eq!(rig.host.slot_fences.len(), 1);
        let (_, fence) = rig.host.slot_fences[0];
        assert!(!rig.host.signalled.contains_key(&fence));
        rig.frontend.backend.finish(Completion::Ok);
        rig.frontend.service(&mut rig.host, true);
        assert_eq!(rig.host.signalled.get(&fence), Some(&0));
        // An unshared object sets nothing.
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        assert_eq!(rig.host.slot_fences.len(), 1);
    });
}

/// A hang: the guilty request's fence carries `EIO`, `GET_RESET_STATS`
/// counts it, and a context that is not recoverable is banned.
#[test]
fn hangs_are_counted_and_ban() {
    big(|| {
        let mut rig = Rig::new();
        let set = |rig: &mut Rig, param: u64, value: u64| {
            let mut args = ContextParam {
                ctx_id: 0,
                param,
                value,
                size: 0,
            }
            .to_bytes();
            assert_eq!(rig.ioctl(ioctl::GEM_CONTEXT_SETPARAM, &mut args), 0);
        };
        set(&mut rig, context::PARAM_RECOVERABLE, 0);
        rig.frontend.backend.hold = true;
        let batch = rig.create(4096);
        let (status, args) = rig.exec(
            ioctl::GEM_EXECBUFFER2_WR,
            &[(batch, 0x1000, PINNED)],
            exec::FENCE_OUT,
            &[],
            0,
        );
        assert_eq!(status, 0);
        let Some(Fd::Sync(cookie)) = rig.host.fds.get(&((args.rsvd2 >> 32) as i32)).copied() else {
            panic!("no out fence");
        };
        rig.frontend.backend.finish(Completion::Hung);
        rig.frontend.service(&mut rig.host, true);
        assert_eq!(rig.host.signalled.get(&cookie), Some(&EIO));
        let mut stats = ResetStats::default().to_bytes();
        assert_eq!(rig.ioctl(ioctl::GET_RESET_STATS, &mut stats), 0);
        let stats = ResetStats::from_bytes(&stats).expect("stats");
        assert_eq!((stats.batch_active, stats.batch_pending), (1, 0));
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            EIO,
            "banned"
        );
    });
}

/// A full back end holds the request and offers it again after a
/// completion; requests of one context keep their order.
#[test]
fn a_full_backend_keeps_order() {
    big(|| {
        let mut rig = Rig::new();
        let batch = rig.create(4096);
        rig.frontend.backend.full = true;
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        assert!(rig.frontend.backend.submits.is_empty());
        rig.frontend.backend.full = false;
        rig.frontend.service(&mut rig.host, true);
        let ids: Vec<_> = rig
            .frontend
            .backend
            .submits
            .iter()
            .map(|submit| submit.3)
            .collect();
        assert_eq!(ids, [1, 2]);
    });
}

/// A released file's objects are unbound and freed once idle, its contexts
/// destroyed, its VMs too.
#[test]
fn release_frees_everything_when_idle() {
    big(|| {
        let mut rig = Rig::new();
        rig.frontend.backend.hold = true;
        let batch = rig.create(4096);
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        rig.frontend.release(&mut rig.host, 1);
        assert!(rig.frontend.backend.unbinds.is_empty(), "still in use");
        assert_eq!(rig.frontend.state.objects.len(), 1);
        rig.frontend.backend.finish(Completion::Ok);
        rig.frontend.service(&mut rig.host, true);
        assert_eq!(rig.frontend.backend.unbinds.len(), 1);
        assert_eq!(rig.frontend.state.objects.len(), 0);
        assert_eq!(rig.frontend.backend.destroyed_contexts.len(), 1);
        assert_eq!(rig.frontend.backend.destroyed_vms.len(), 1);
        assert_eq!(rig.frontend.state.files.len(), 0);
    });
}

/// A binding over one still in use answers `EAGAIN`; over an idle one, the
/// idle one is unbound first (i915 evicts it).
#[test]
fn overlapping_bindings() {
    big(|| {
        let mut rig = Rig::new();
        rig.frontend.backend.hold = true;
        let first = rig.create(8192);
        let second = rig.create(4096);
        let batch = rig.create(4096);
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(first, 0x10000, PINNED), (batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        let mut close = GemClose {
            handle: first,
            pad: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::GEM_CLOSE, &mut close), 0);
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(second, 0x11000, PINNED), (batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            -11
        );
        rig.frontend.backend.finish(Completion::Ok);
        rig.frontend.service(&mut rig.host, true);
        assert_eq!(
            rig.exec(
                ioctl::GEM_EXECBUFFER2,
                &[(second, 0x11000, PINNED), (batch, 0x1000, PINNED)],
                0,
                &[],
                0
            )
            .0,
            0
        );
        assert!(
            rig.frontend
                .backend
                .unbinds
                .iter()
                .any(|(_, address, _)| *address == 0x10000)
        );
    });
}

/// `I915_REG_READ`: the render timestamp alone.
#[test]
fn reg_read_reads_the_timestamp_alone() {
    big(|| {
        let mut rig = Rig::new();
        let mut args = RegRead {
            offset: 0x2358 | gem::REG_READ_8B_WA,
            val: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::REG_READ, &mut args), 0);
        assert_eq!(
            RegRead::from_bytes(&args).expect("reg").val,
            0x1234_5678_9abc
        );
        let mut other = RegRead {
            offset: 0x2000,
            val: 0,
        }
        .to_bytes();
        assert_eq!(rig.ioctl(ioctl::REG_READ, &mut other), EINVAL);
    });
}

/// The tables fit the 8 MiB stack a native process starts on with room to
/// spare (`syscall/exec.rs`, `STACK_SIZE`): the driver builds its front end
/// there.
#[test]
fn the_tables_fit_a_native_stack() {
    let bytes = core::mem::size_of::<Frontend<FakeBackend>>();
    assert!(bytes < 3 << 20, "{bytes} bytes");
}
