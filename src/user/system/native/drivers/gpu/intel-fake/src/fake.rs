//! The fake GT both of this package's programs run: every submission done
//! at once, nothing bound, no register touched. `intel-fake` serves the
//! front end over it in one process; `intel-fake-gt` runs it as the
//! keeper's worker (`docs/INTEL-GPU.md` §12.8).

use ferrix_intel_xelp::frontend::{
    Backend, Completion, ContextParams, Engine, Facts, HwContext, RequestId, SubmitError, VmId,
};
use ferrix_intel_xelp::sseu::{Fuses, gen12_sseu_info_init};
use ferrix_linux_abi::errno::Errno;

/// The engines the fake answers for: the render and copy engines, those
/// the GT back end drives (M2).
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

/// The words every fake's facts line ends with, which
/// `xtask::intel_uapi::refuse_fake` looks for in a program called `intel`
/// or `intel-gt` that is not this gate's.
pub(crate) const MARKER: &str = "every batch completes at once, nothing runs";

/// M0's ADL-N (`docs/INTEL-GPU.md` §10): one slice, two DSS of twelve
/// EUs (pairs 3 and 7 fused off), the CS timestamp at 19.2 MHz.
pub(crate) fn facts(device_id: u16, revision: u8, system_memory_bytes: u64) -> Facts {
    Facts {
        device_id,
        revision,
        sseu: gen12_sseu_info_init(Fuses {
            slice_enable: 1,
            geometry_dss_enable: 0x3,
            eu_disable: 0x88,
        }),
        cs_timestamp_hz: 19_200_000,
        engines: &ENGINES,
        ggtt_bytes: 4 << 30,
        ppgtt_bits: 48,
        system_memory_bytes,
        max_pat_index: 3,
    }
}

/// The fake GT, over memory of type `M` that `make` makes.
pub(crate) struct Fake<M> {
    facts: Facts,
    make: fn(u64) -> Result<M, Errno>,
    next: u32,
    done: [RequestId; 64],
    pending: usize,
}

impl<M> Fake<M> {
    /// A fake answering with `facts`, making memory with `make`.
    pub(crate) fn new(facts: Facts, make: fn(u64) -> Result<M, Errno>) -> Self {
        Fake {
            facts,
            make,
            next: 0,
            done: [0; 64],
            pending: 0,
        }
    }
}

impl<M> Backend for Fake<M> {
    type Memory = M;

    fn facts(&self) -> &Facts {
        &self.facts
    }

    fn create_memory(&mut self, bytes: u64) -> Result<M, Errno> {
        (self.make)(bytes)
    }

    fn vm_create(&mut self) -> Result<VmId, Errno> {
        self.next += 1;
        Ok(VmId(self.next))
    }

    fn vm_destroy(&mut self, _vm: VmId) {}

    fn bind(
        &mut self,
        _vm: VmId,
        _address: u64,
        _memory: &M,
        _bytes: u64,
        pat_index: u8,
    ) -> Result<(), Errno> {
        if pat_index > self.facts.max_pat_index {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn unbind(&mut self, _vm: VmId, _address: u64, _bytes: u64) {}

    fn context_create(
        &mut self,
        _vm: VmId,
        _engine: Engine,
        params: ContextParams,
    ) -> Result<HwContext, Errno> {
        let ring = params.ring_bytes;
        if !ring.is_power_of_two() || !(4 << 10..=2 << 20).contains(&ring) {
            return Err(Errno::EINVAL);
        }
        self.next += 1;
        Ok(HwContext(self.next))
    }

    fn context_destroy(&mut self, _context: HwContext) {}

    fn submit(
        &mut self,
        _context: HwContext,
        _batch: u64,
        _length: u32,
        request: RequestId,
    ) -> Result<(), SubmitError> {
        let slot = self.done.get_mut(self.pending).ok_or(SubmitError::Full)?;
        *slot = request;
        self.pending += 1;
        Ok(())
    }

    fn service(&mut self, _now_ns: u64, _interrupt: bool) {}

    fn next_deadline(&self) -> Option<u64> {
        None
    }

    fn completed(&mut self, sink: &mut dyn FnMut(RequestId, Completion)) {
        for id in self.done.iter().take(self.pending) {
            sink(*id, Completion::Ok);
        }
        self.pending = 0;
    }

    fn timestamp(&mut self) -> u64 {
        // The command streamer's clock at 19.2 MHz, from the monotonic one.
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0) / 1000 * 192 / 10
    }
}
