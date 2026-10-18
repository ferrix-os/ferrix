// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/i915_getparam.c.

//! `I915_GETPARAM`: `i915_getparam_ioctl`'s switch, in its order, for ADL-N
//! (Gen12.0, LLC, full PPGTT) driven by execlists with no `GuC`, no `HuC`, no
//! display, no PXP and no OA.
//!
//! Where an answer differs from what i915 gives on the same box, it says
//! why: `HAS_USERPTR_PROBE` is 0 because userptr is not served (K7), and
//! the scheduler has no preemption, so `HAS_SCHEDULER` says so.

use ferrix_linux_abi::errno::Errno;

use super::drm::{Call, put};
use super::{Backend, Frontend, Host};
use crate::uapi::{GetParam, param, query};

/// `I915_SCHEDULER_CAP_ENABLED`.
const SCHEDULER_ENABLED: i32 = 1 << 0;
/// `I915_SCHEDULER_CAP_PRIORITY`.
const SCHEDULER_PRIORITY: i32 = 1 << 1;

/// `i915_gem_mmap_gtt_version`'s answer as far as this driver serves it: 4,
/// "`MMAP_OFFSET`"; 5 would promise partial mmaps of the GTT mapping, which
/// is not served.
const MMAP_GTT_VERSION: i32 = 4;

/// The parameters i915 answers 1 for on every platform it drives ("For the
/// time being all of these are always true").
const fn always_one(param: i32) -> bool {
    matches!(
        param,
        param::MMAP_VERSION
            | param::HAS_GEM
            | param::HAS_PAGEFLIPPING
            | param::HAS_EXECBUF2
            | param::HAS_RELAXED_FENCING
            | param::HAS_COHERENT_RINGS
            | param::HAS_RELAXED_DELTA
            | param::HAS_GEN7_SOL_RESET
            | param::HAS_WAIT_TIMEOUT
            | param::HAS_PRIME_VMAP_FLUSH
            | param::HAS_PINNED_BATCHES
            | param::HAS_EXEC_NO_RELOC
            | param::HAS_EXEC_HANDLE_LUT
            | param::HAS_COHERENT_PHYS_GTT
            | param::HAS_EXEC_SOFTPIN
            | param::HAS_EXEC_ASYNC
            | param::HAS_EXEC_FENCE
            | param::HAS_EXEC_CAPTURE
            | param::HAS_EXEC_BATCH_FIRST
            | param::HAS_EXEC_FENCE_ARRAY
            | param::HAS_EXEC_SUBMIT_FENCE
            | param::HAS_EXEC_TIMELINE_FENCES
    )
}

/// `i915_getparam_ioctl`.
pub fn getparam<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = GetParam::from_bytes(data).ok_or(Errno::EFAULT)?;
    let facts = *frontend.backend.facts();
    let has = |class: u16, instance: u16| -> i32 {
        i32::from(
            facts
                .engines
                .iter()
                .any(|engine| engine.class == class && engine.instance == instance),
        )
    };
    let value: i32 = match args.param {
        param::IRQ_ACTIVE
        | param::ALLOW_BATCHBUFFER
        | param::LAST_DISPATCH
        | param::HAS_EXEC_CONSTANTS => {
            return Err(Errno::ENODEV);
        }
        param::CHIPSET_ID => i32::from(facts.device_id),
        param::REVISION => i32::from(facts.revision),
        // Gen12 has no fence registers for programs.
        param::NUM_FENCES_AVAIL => 0,
        param::HAS_OVERLAY => 0,
        param::HAS_BSD => has(query::ENGINE_CLASS_VIDEO, 0),
        param::HAS_BLT => has(query::ENGINE_CLASS_COPY, 0),
        param::HAS_VEBOX => has(query::ENGINE_CLASS_VIDEO_ENHANCE, 0),
        param::HAS_BSD2 => has(query::ENGINE_CLASS_VIDEO, 1),
        param::HAS_LLC => 1,
        param::HAS_WT => 0,
        // `INTEL_PPGTT_FULL`.
        param::HAS_ALIASING_PPGTT => 2,
        // Execlists without semaphores between engines.
        param::HAS_SEMAPHORES => 0,
        param::HAS_SECURE_BATCHES => 0,
        // Gen12 has no command parser (`i915_cmd_parser_get_version`).
        param::CMD_PARSER_VERSION => 0,
        param::SUBSLICE_TOTAL => {
            let total = i32::from(facts.sseu.subslice_mask.count_ones() as u8);
            if total == 0 {
                return Err(Errno::ENODEV);
            }
            total
        }
        param::EU_TOTAL => {
            let total = i32::try_from(facts.sseu.eu_total).unwrap_or(i32::MAX);
            if total == 0 {
                return Err(Errno::ENODEV);
            }
            total
        }
        // Engine resets, as the back end does on a hang (2: per engine).
        param::HAS_GPU_RESET => 2,
        param::HAS_RESOURCE_STREAMER => 0,
        param::HAS_POOLED_EU => 0,
        param::MIN_EU_IN_POOL => 0,
        // HuC is not loaded: `intel_huc_check_status` gives -ENODEV.
        param::HUC_STATUS => return Err(Errno::ENODEV),
        // `intel_pxp_get_readiness_status` with no PXP.
        param::PXP_STATUS => return Err(Errno::ENODEV),
        param::MMAP_GTT_VERSION => MMAP_GTT_VERSION,
        param::HAS_SCHEDULER => SCHEDULER_ENABLED | SCHEDULER_PRIORITY,
        always if always_one(always) => 1,
        // Userptr is not served until K7, so its probe flag is not either.
        param::HAS_USERPTR_PROBE => 0,
        // `HAS_CONTEXT_FREQ_HINT` is for GuC submission: i915 itself answers
        // the value -EINVAL.
        param::HAS_CONTEXT_FREQ_HINT => -i32::from(Errno::EINVAL.0),
        param::HAS_CONTEXT_ISOLATION => 1,
        param::SLICE_MASK => {
            let mask = i32::from(facts.sseu.slice_mask);
            if mask == 0 {
                return Err(Errno::ENODEV);
            }
            mask
        }
        param::SUBSLICE_MASK => {
            let mask = i32::from(facts.sseu.subslice_mask);
            if mask == 0 {
                return Err(Errno::ENODEV);
            }
            mask
        }
        param::CS_TIMESTAMP_FREQUENCY => i32::try_from(facts.cs_timestamp_hz).unwrap_or(i32::MAX),
        // `has_coherent_ggtt` is false from Gen9 LP on, and the GTT mmap
        // is not served anyway.
        param::MMAP_GTT_COHERENT => 0,
        // No OA unit: `i915_perf_ioctl_version` and the OA frequency
        // answer as for a device without perf.
        param::PERF_REVISION | param::OA_TIMESTAMP_FREQUENCY => return Err(Errno::EINVAL),
        _ => return Err(Errno::EINVAL),
    };
    if args.value == 0 {
        return Err(Errno::EFAULT);
    }
    host.copy_out(call.request, args.value, &value.to_le_bytes())
        .map_err(|_| Errno::EFAULT)?;
    put(data, &args.to_bytes());
    Ok(())
}
