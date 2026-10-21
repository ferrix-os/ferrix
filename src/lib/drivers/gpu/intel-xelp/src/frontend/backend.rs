//! What the front end asks of the GPU: the interface agreed with M2's back
//! end on 2026-10-18 (see the module above for what it may rely on).

use ferrix_linux_abi::errno::Errno;

use super::RequestId;
use crate::sseu::SseuInfo;

/// A PPGTT, as the back end names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VmId(pub u32);

/// A hardware context (an LRC and its ring on one engine), as the back end
/// names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HwContext(pub u32);

/// An engine, as the uAPI names it: `I915_ENGINE_CLASS_*` and an instance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Engine {
    /// The uAPI class.
    pub class: u16,
    /// The instance within the class.
    pub instance: u16,
}

/// A hardware context's parameters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ContextParams {
    /// The ring's size: a power of two from 4 KiB to 2 MiB; 16 KiB unless
    /// the program set `I915_CONTEXT_PARAM_RINGSIZE`.
    pub ring_bytes: u32,
}

impl ContextParams {
    /// i915's default ring (`SZ_16K`, `intel_context_set_gem`'s default for
    /// a user context).
    pub const DEFAULT: Self = Self {
        ring_bytes: 16 << 10,
    };
}

/// How a request ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Completion {
    /// It ran to its end.
    Ok,
    /// It was executing when its engine was reset: the guilty one
    /// (`__i915_request_reset`, `guilty = true`); its fence's error is EIO.
    Hung,
    /// It was queued behind the guilty one in the same context and was
    /// skipped, never run; its fence's error is EIO.
    Cancelled,
}

/// Why a submission was not taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// The context's ring or the submit queue is full: the front end holds
    /// the request and offers it again after the next completion.
    Full,
    /// Refused for good: the request completes with this error.
    Failed(Errno),
}

/// The facts `GETPARAM` and `QUERY` answer from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Facts {
    /// The PCI device id (`I915_PARAM_CHIPSET_ID`).
    pub device_id: u16,
    /// The PCI revision (`I915_PARAM_REVISION`).
    pub revision: u8,
    /// The slices, dual-subslices and EUs.
    pub sseu: SseuInfo,
    /// The command streamer's timestamp frequency, in Hz.
    pub cs_timestamp_hz: u32,
    /// The engines the back end submits to, in uAPI order (class, then
    /// instance). Only these are told to programs.
    pub engines: &'static [Engine],
    /// The GGTT's size in bytes (`GET_APERTURE`'s `aper_size`).
    pub ggtt_bytes: u64,
    /// The bits of a PPGTT address: 48 on Gen12.
    pub ppgtt_bits: u8,
    /// System memory, in bytes, for `MEMORY_REGIONS`.
    pub system_memory_bytes: u64,
    /// The largest PAT index a binding may name.
    pub max_pat_index: u8,
}

/// The GPU, as the front end sees it.
pub trait Backend {
    /// One GEM object's memory: a VMO the process owns. The real back end
    /// pins it into the device's IOMMU domain at its first bind
    /// (`docs/INTEL-GPU.md` §4.4). Dropping it frees it.
    type Memory;

    /// The facts the uAPI tells.
    fn facts(&self) -> &Facts;

    /// Memory for a new object of `bytes` (a multiple of 4 KiB).
    ///
    /// # Errors
    ///
    /// `ENOMEM`, or the pin budget's `EDQUOT`.
    fn create_memory(&mut self, bytes: u64) -> Result<Self::Memory, Errno>;

    /// A new 4-level PPGTT.
    ///
    /// # Errors
    ///
    /// `ENOMEM`.
    fn vm_create(&mut self) -> Result<VmId, Errno>;

    /// Destroy `vm`: no context and no binding uses it any more.
    fn vm_destroy(&mut self, vm: VmId);

    /// Map `bytes` of `memory` at `address` in `vm` with `pat_index`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a PAT index past [`Facts::max_pat_index`], `ENOMEM`,
    /// `EDQUOT`.
    fn bind(
        &mut self,
        vm: VmId,
        address: u64,
        memory: &Self::Memory,
        bytes: u64,
        pat_index: u8,
    ) -> Result<(), Errno>;

    /// Unmap `bytes` at `address` in `vm`; every request that used it has
    /// completed. No TLB flush is asked: every request starts with i915's
    /// unconditional invalidation.
    fn unbind(&mut self, vm: VmId, address: u64, bytes: u64);

    /// A hardware context on `engine` in `vm`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a ring size out of range, `ENOMEM`.
    fn context_create(
        &mut self,
        vm: VmId,
        engine: Engine,
        params: ContextParams,
    ) -> Result<HwContext, Errno>;

    /// Destroy `context`: it is idle and nothing will be submitted to it.
    fn context_destroy(&mut self, context: HwContext);

    /// Run the batch at `batch` (`length` bytes, in the context's VM) as
    /// `request`; it completes later, through [`Backend::completed`].
    ///
    /// # Errors
    ///
    /// [`SubmitError::Full`] to be offered again, or
    /// [`SubmitError::Failed`].
    fn submit(
        &mut self,
        context: HwContext,
        batch: u64,
        length: u32,
        request: RequestId,
    ) -> Result<(), SubmitError>;

    /// Let the GPU's state machines run: after an interrupt packet
    /// (`interrupt`) or a deadline. `now_ns` is monotonic.
    fn service(&mut self, now_ns: u64, interrupt: bool);

    /// When [`Backend::service`] must next be called, in monotonic
    /// nanoseconds: the bounded poll and the hang heartbeat.
    fn next_deadline(&self) -> Option<u64>;

    /// Hand each finished request to `sink`, once.
    fn completed(&mut self, sink: &mut dyn FnMut(RequestId, Completion));

    /// The render engine's `RING_TIMESTAMP`, for `I915_REG_READ`.
    fn timestamp(&mut self) -> u64;

    /// Whether the front end holds requests it has not submitted yet,
    /// waiting on their fences: work in flight, so the GT's frequency is
    /// not parked under it.
    fn set_held(&mut self, held: bool) {
        let _ = held;
    }

    /// Whether a kernel fence (a sync file, a dmabuf's fence) is still to
    /// be signalled: a program outside the driver may be blocked on it, so
    /// completions should be seen promptly.
    fn set_watched(&mut self, watched: bool) {
        let _ = watched;
    }

    /// `intel_rps_boost`: a program waits on `request`, which has not
    /// signalled. The GT goes to its boost frequency until the request
    /// retires; once per request, however often this is called. A request
    /// the back end has not been given yet is ignored (asked again later).
    fn boost(&mut self, request: RequestId) {
        let _ = request;
    }
}
