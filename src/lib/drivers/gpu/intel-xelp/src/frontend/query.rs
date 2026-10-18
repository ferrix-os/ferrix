// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/i915_query.c.

//! `I915_QUERY`: `i915_query_ioctl` and its items. The topology is the
//! fuses' (`crate::sseu::fill_topology_info`), the engines are those the
//! back end submits to, and memory is system memory alone. Gen12.0 has no
//! geometry-subslice item (12.55 and later) and ADL-N no hwconfig blob; no
//! OA unit is driven, and `GuC` does not submit: each of those is `ENODEV`,
//! as i915 answers them.

use ferrix_linux_abi::errno::Errno;

use super::drm::Call;
use super::{Backend, Frontend, Host};
use crate::sseu::{TOPOLOGY_LEN, fill_topology_info};
use crate::uapi::{
    EngineInfo, MemoryRegionInfo, Query, QueryEngineInfo, QueryItem, QueryMemoryRegions, query,
};

/// `i915_query_ioctl`: each item's answer (a length, or a negative errno)
/// goes back into its `length` when it differs.
pub fn query<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = Query::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags != 0 {
        return Err(Errno::EINVAL);
    }
    for index in 0..u64::from(args.num_items) {
        let at = args.items_ptr + index * QueryItem::SIZE as u64;
        let mut raw = [0_u8; QueryItem::SIZE];
        host.copy_in(call.request, at, &mut raw)
            .map_err(|_| Errno::EFAULT)?;
        let item = QueryItem::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if item.query_id == 0 {
            return Err(Errno::EINVAL);
        }
        let answer = match item.query_id {
            query::TOPOLOGY_INFO => topology(frontend, host, call, &item),
            query::ENGINE_INFO => engines(frontend, host, call, &item),
            query::MEMORY_REGIONS => memory_regions(frontend, host, call, &item),
            query::PERF_CONFIG
            | query::HWCONFIG_BLOB
            | query::GEOMETRY_SUBSLICES
            | query::GUC_SUBMISSION_VERSION => Err(Errno::ENODEV),
            _ => Err(Errno::EINVAL),
        };
        let length = match answer {
            Ok(length) => length,
            Err(errno) => -i32::from(errno.0),
        };
        if length != item.length {
            host.copy_out(call.request, at + 8, &length.to_le_bytes())
                .map_err(|_| Errno::EFAULT)?;
        }
    }
    Ok(())
}

/// `copy_query_item`: `Ok(Some(total))` asks for the size, `Ok(None)` goes
/// on to fill it in.
fn sized<M, H: Host<M>>(
    host: &mut H,
    call: &Call,
    item: &QueryItem,
    total: usize,
    header: &mut [u8],
) -> Result<Option<i32>, Errno> {
    let total = i32::try_from(total).map_err(|_| Errno::EINVAL)?;
    if item.length == 0 {
        return Ok(Some(total));
    }
    if item.length < total {
        return Err(Errno::EINVAL);
    }
    host.copy_in(call.request, item.data_ptr, header)
        .map_err(|_| Errno::EFAULT)?;
    Ok(None)
}

/// `query_topology_info`.
fn topology<B: Backend, H: Host<B::Memory>>(
    frontend: &Frontend<B>,
    host: &mut H,
    call: &Call,
    item: &QueryItem,
) -> Result<i32, Errno> {
    if item.flags != 0 {
        return Err(Errno::EINVAL);
    }
    let mut header = [0_u8; 16];
    if let Some(total) = sized(host, call, item, TOPOLOGY_LEN, &mut header)? {
        return Ok(total);
    }
    let bytes = fill_topology_info(&frontend.backend.facts().sseu);
    host.copy_out(call.request, item.data_ptr, &bytes)
        .map_err(|_| Errno::EFAULT)?;
    Ok(TOPOLOGY_LEN as i32)
}

/// `query_engine_info`: each engine with `HAS_LOGICAL_INSTANCE`, its
/// logical instance its instance, and no capabilities (render and copy on
/// Gen12.0 have none).
fn engines<B: Backend, H: Host<B::Memory>>(
    frontend: &Frontend<B>,
    host: &mut H,
    call: &Call,
    item: &QueryItem,
) -> Result<i32, Errno> {
    if item.flags != 0 {
        return Err(Errno::EINVAL);
    }
    let list = frontend.backend.facts().engines;
    let total = QueryEngineInfo::SIZE + list.len() * EngineInfo::SIZE;
    let mut header = [0_u8; QueryEngineInfo::SIZE];
    if let Some(total) = sized(host, call, item, total, &mut header)? {
        return Ok(total);
    }
    // `num_engines` and `rsvd[3]` must come in as zero.
    if header.iter().any(|byte| *byte != 0) {
        return Err(Errno::EINVAL);
    }
    for (index, engine) in list.iter().enumerate() {
        let info = EngineInfo {
            engine_class: engine.class,
            engine_instance: engine.instance,
            rsvd0: 0,
            flags: query::ENGINE_INFO_HAS_LOGICAL_INSTANCE,
            capabilities: 0,
            logical_instance: engine.instance,
        };
        host.copy_out(
            call.request,
            item.data_ptr + (QueryEngineInfo::SIZE + index * EngineInfo::SIZE) as u64,
            &info.to_bytes(),
        )
        .map_err(|_| Errno::EFAULT)?;
    }
    let head = QueryEngineInfo {
        num_engines: u32::try_from(list.len()).unwrap_or(0),
    };
    host.copy_out(call.request, item.data_ptr, &head.to_bytes())
        .map_err(|_| Errno::EFAULT)?;
    i32::try_from(total).map_err(|_| Errno::EINVAL)
}

/// `query_memregion_info`: system memory, instance 0, whole and (for a
/// program without `CAP_PERFMON`, as every one is here) all unallocated.
fn memory_regions<B: Backend, H: Host<B::Memory>>(
    frontend: &Frontend<B>,
    host: &mut H,
    call: &Call,
    item: &QueryItem,
) -> Result<i32, Errno> {
    if item.flags != 0 {
        return Err(Errno::EINVAL);
    }
    let total = QueryMemoryRegions::SIZE + MemoryRegionInfo::SIZE;
    let mut header = [0_u8; QueryMemoryRegions::SIZE];
    if let Some(total) = sized(host, call, item, total, &mut header)? {
        return Ok(total);
    }
    if header.iter().any(|byte| *byte != 0) {
        return Err(Errno::EINVAL);
    }
    let bytes = frontend.backend.facts().system_memory_bytes;
    let info = MemoryRegionInfo {
        memory_class: query::MEMORY_CLASS_SYSTEM,
        memory_instance: 0,
        probed_size: bytes,
        unallocated_size: bytes,
        probed_cpu_visible_size: bytes,
        unallocated_cpu_visible_size: bytes,
    };
    host.copy_out(
        call.request,
        item.data_ptr + QueryMemoryRegions::SIZE as u64,
        &info.to_bytes(),
    )
    .map_err(|_| Errno::EFAULT)?;
    host.copy_out(
        call.request,
        item.data_ptr,
        &QueryMemoryRegions { num_regions: 1 }.to_bytes(),
    )
    .map_err(|_| Errno::EFAULT)?;
    i32::try_from(total).map_err(|_| Errno::EINVAL)
}
