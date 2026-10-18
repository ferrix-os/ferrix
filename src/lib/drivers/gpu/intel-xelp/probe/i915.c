/*
 * The i915 uAPI numbers and layouts `src/lib/drivers/gpu/intel-xelp/src/uapi.rs`
 * writes down, printed from Linux v7.0's UAPI headers themselves
 * (docs/INTEL-GPU.md §2.1, Appendix A).
 *
 * Run by `probe/i915.sh` against the v7.0 tag's `include/uapi` into
 * `probe/i915-64.txt` beside it. The crate's tests read the file and require
 * every line to match what the crate says, so a number changed on either side
 * fails `cargo test`. Only the 64-bit file is kept: every structure below
 * carries its pointers as `__u64`, and anv and iris are x86-64 programs here.
 *
 * Each line is `name value`, in decimal. Layouts are `sizeof.<struct>` and
 * `offsetof.<struct>.<field>`.
 */

#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <drm/drm.h>
#include <drm/i915_drm.h>
#include <linux/dma-buf.h>

#define VALUE(name) printf("%s %llu\n", #name, (unsigned long long)(name))
#define SIZE(type) printf("sizeof.%s %zu\n", #type, sizeof(struct type))
#define FIELD(type, field) \
	printf("offsetof.%s.%s %zu\n", #type, #field, offsetof(struct type, field))

int main(void)
{
	/* The common ioctls a render node answers. */
	VALUE(DRM_IOCTL_VERSION);
	VALUE(DRM_IOCTL_GET_CAP);
	VALUE(DRM_IOCTL_SET_CLIENT_CAP);
	VALUE(DRM_IOCTL_GEM_CLOSE);
	VALUE(DRM_IOCTL_GEM_FLINK);
	VALUE(DRM_IOCTL_GEM_OPEN);
	VALUE(DRM_IOCTL_PRIME_HANDLE_TO_FD);
	VALUE(DRM_IOCTL_PRIME_FD_TO_HANDLE);
	VALUE(DRM_IOCTL_SYNCOBJ_CREATE);
	VALUE(DRM_IOCTL_SYNCOBJ_DESTROY);
	VALUE(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD);
	VALUE(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE);
	VALUE(DRM_IOCTL_SYNCOBJ_WAIT);
	VALUE(DRM_IOCTL_SYNCOBJ_RESET);
	VALUE(DRM_IOCTL_SYNCOBJ_SIGNAL);
	VALUE(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT);
	VALUE(DRM_IOCTL_SYNCOBJ_QUERY);
	VALUE(DRM_IOCTL_SYNCOBJ_TRANSFER);
	VALUE(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL);
	VALUE(DRM_IOCTL_SYNCOBJ_EVENTFD);

	/* The i915 ioctls. */
	VALUE(DRM_IOCTL_I915_GETPARAM);
	VALUE(DRM_IOCTL_I915_GEM_EXECBUFFER2);
	VALUE(DRM_IOCTL_I915_GEM_EXECBUFFER2_WR);
	VALUE(DRM_IOCTL_I915_GEM_BUSY);
	VALUE(DRM_IOCTL_I915_GEM_SET_CACHING);
	VALUE(DRM_IOCTL_I915_GEM_GET_CACHING);
	VALUE(DRM_IOCTL_I915_GEM_THROTTLE);
	VALUE(DRM_IOCTL_I915_GEM_CREATE);
	VALUE(DRM_IOCTL_I915_GEM_CREATE_EXT);
	VALUE(DRM_IOCTL_I915_GEM_PREAD);
	VALUE(DRM_IOCTL_I915_GEM_PWRITE);
	VALUE(DRM_IOCTL_I915_GEM_MMAP);
	VALUE(DRM_IOCTL_I915_GEM_MMAP_OFFSET);
	VALUE(DRM_IOCTL_I915_GEM_SET_DOMAIN);
	VALUE(DRM_IOCTL_I915_GEM_SW_FINISH);
	VALUE(DRM_IOCTL_I915_GEM_SET_TILING);
	VALUE(DRM_IOCTL_I915_GEM_GET_TILING);
	VALUE(DRM_IOCTL_I915_GEM_GET_APERTURE);
	VALUE(DRM_IOCTL_I915_GEM_MADVISE);
	VALUE(DRM_IOCTL_I915_GEM_WAIT);
	VALUE(DRM_IOCTL_I915_GEM_CONTEXT_CREATE);
	VALUE(DRM_IOCTL_I915_GEM_CONTEXT_CREATE_EXT);
	VALUE(DRM_IOCTL_I915_GEM_CONTEXT_DESTROY);
	VALUE(DRM_IOCTL_I915_REG_READ);
	VALUE(DRM_IOCTL_I915_GET_RESET_STATS);
	VALUE(DRM_IOCTL_I915_GEM_USERPTR);
	VALUE(DRM_IOCTL_I915_GEM_CONTEXT_GETPARAM);
	VALUE(DRM_IOCTL_I915_GEM_CONTEXT_SETPARAM);
	VALUE(DRM_IOCTL_I915_PERF_OPEN);
	VALUE(DRM_IOCTL_I915_QUERY);
	VALUE(DRM_IOCTL_I915_GEM_VM_CREATE);
	VALUE(DRM_IOCTL_I915_GEM_VM_DESTROY);

	/* On a dmabuf. */
	VALUE(DMA_BUF_IOCTL_SYNC);
	VALUE(DMA_BUF_IOCTL_EXPORT_SYNC_FILE);
	VALUE(DMA_BUF_IOCTL_IMPORT_SYNC_FILE);

	/* Capabilities and flags of the common ioctls. */
	VALUE(DRM_CAP_PRIME);
	VALUE(DRM_PRIME_CAP_IMPORT);
	VALUE(DRM_PRIME_CAP_EXPORT);
	VALUE(DRM_CAP_TIMESTAMP_MONOTONIC);
	VALUE(DRM_CAP_SYNCOBJ);
	VALUE(DRM_CAP_SYNCOBJ_TIMELINE);
	VALUE(DRM_CLOEXEC);
	VALUE(DRM_RDWR);
	VALUE(DRM_SYNCOBJ_CREATE_SIGNALED);
	VALUE(DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE);
	VALUE(DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE);
	VALUE(DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE);
	VALUE(DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE);
	VALUE(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL);
	VALUE(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT);
	VALUE(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE);
	VALUE(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE);
	VALUE(DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED);

	/* GETPARAM's parameters. */
	VALUE(I915_PARAM_IRQ_ACTIVE);
	VALUE(I915_PARAM_ALLOW_BATCHBUFFER);
	VALUE(I915_PARAM_LAST_DISPATCH);
	VALUE(I915_PARAM_CHIPSET_ID);
	VALUE(I915_PARAM_HAS_GEM);
	VALUE(I915_PARAM_NUM_FENCES_AVAIL);
	VALUE(I915_PARAM_HAS_OVERLAY);
	VALUE(I915_PARAM_HAS_PAGEFLIPPING);
	VALUE(I915_PARAM_HAS_EXECBUF2);
	VALUE(I915_PARAM_HAS_BSD);
	VALUE(I915_PARAM_HAS_BLT);
	VALUE(I915_PARAM_HAS_RELAXED_FENCING);
	VALUE(I915_PARAM_HAS_COHERENT_RINGS);
	VALUE(I915_PARAM_HAS_EXEC_CONSTANTS);
	VALUE(I915_PARAM_HAS_RELAXED_DELTA);
	VALUE(I915_PARAM_HAS_GEN7_SOL_RESET);
	VALUE(I915_PARAM_HAS_LLC);
	VALUE(I915_PARAM_HAS_ALIASING_PPGTT);
	VALUE(I915_PARAM_HAS_WAIT_TIMEOUT);
	VALUE(I915_PARAM_HAS_SEMAPHORES);
	VALUE(I915_PARAM_HAS_PRIME_VMAP_FLUSH);
	VALUE(I915_PARAM_HAS_VEBOX);
	VALUE(I915_PARAM_HAS_SECURE_BATCHES);
	VALUE(I915_PARAM_HAS_PINNED_BATCHES);
	VALUE(I915_PARAM_HAS_EXEC_NO_RELOC);
	VALUE(I915_PARAM_HAS_EXEC_HANDLE_LUT);
	VALUE(I915_PARAM_HAS_WT);
	VALUE(I915_PARAM_CMD_PARSER_VERSION);
	VALUE(I915_PARAM_HAS_COHERENT_PHYS_GTT);
	VALUE(I915_PARAM_MMAP_VERSION);
	VALUE(I915_PARAM_HAS_BSD2);
	VALUE(I915_PARAM_REVISION);
	VALUE(I915_PARAM_SUBSLICE_TOTAL);
	VALUE(I915_PARAM_EU_TOTAL);
	VALUE(I915_PARAM_HAS_GPU_RESET);
	VALUE(I915_PARAM_HAS_RESOURCE_STREAMER);
	VALUE(I915_PARAM_HAS_EXEC_SOFTPIN);
	VALUE(I915_PARAM_HAS_POOLED_EU);
	VALUE(I915_PARAM_MIN_EU_IN_POOL);
	VALUE(I915_PARAM_MMAP_GTT_VERSION);
	VALUE(I915_PARAM_HAS_SCHEDULER);
	VALUE(I915_PARAM_HUC_STATUS);
	VALUE(I915_PARAM_HAS_EXEC_ASYNC);
	VALUE(I915_PARAM_HAS_EXEC_FENCE);
	VALUE(I915_PARAM_HAS_EXEC_CAPTURE);
	VALUE(I915_PARAM_SLICE_MASK);
	VALUE(I915_PARAM_SUBSLICE_MASK);
	VALUE(I915_PARAM_HAS_EXEC_BATCH_FIRST);
	VALUE(I915_PARAM_HAS_EXEC_FENCE_ARRAY);
	VALUE(I915_PARAM_HAS_CONTEXT_ISOLATION);
	VALUE(I915_PARAM_CS_TIMESTAMP_FREQUENCY);
	VALUE(I915_PARAM_MMAP_GTT_COHERENT);
	VALUE(I915_PARAM_HAS_EXEC_SUBMIT_FENCE);
	VALUE(I915_PARAM_PERF_REVISION);
	VALUE(I915_PARAM_HAS_EXEC_TIMELINE_FENCES);
	VALUE(I915_PARAM_HAS_USERPTR_PROBE);
	VALUE(I915_PARAM_OA_TIMESTAMP_FREQUENCY);
	VALUE(I915_PARAM_PXP_STATUS);
	VALUE(I915_PARAM_HAS_CONTEXT_FREQ_HINT);

	/* QUERY's items, engines and memory classes. */
	VALUE(DRM_I915_QUERY_TOPOLOGY_INFO);
	VALUE(DRM_I915_QUERY_ENGINE_INFO);
	VALUE(DRM_I915_QUERY_PERF_CONFIG);
	VALUE(DRM_I915_QUERY_MEMORY_REGIONS);
	VALUE(DRM_I915_QUERY_HWCONFIG_BLOB);
	VALUE(DRM_I915_QUERY_GEOMETRY_SUBSLICES);
	VALUE(DRM_I915_QUERY_GUC_SUBMISSION_VERSION);
	VALUE(I915_ENGINE_CLASS_RENDER);
	VALUE(I915_ENGINE_CLASS_COPY);
	VALUE(I915_ENGINE_CLASS_VIDEO);
	VALUE(I915_ENGINE_CLASS_VIDEO_ENHANCE);
	VALUE(I915_ENGINE_CLASS_COMPUTE);
	VALUE(I915_ENGINE_CLASS_INVALID);
	VALUE(I915_ENGINE_CLASS_INVALID_NONE);
	VALUE(I915_ENGINE_CLASS_INVALID_VIRTUAL);
	VALUE(I915_ENGINE_INFO_HAS_LOGICAL_INSTANCE);
	VALUE(I915_MEMORY_CLASS_SYSTEM);
	VALUE(I915_MEMORY_CLASS_DEVICE);

	/* GEM objects. */
	VALUE(I915_GEM_CREATE_EXT_MEMORY_REGIONS);
	VALUE(I915_GEM_CREATE_EXT_PROTECTED_CONTENT);
	VALUE(I915_GEM_CREATE_EXT_SET_PAT);
	VALUE(I915_GEM_CREATE_EXT_FLAG_NEEDS_CPU_ACCESS);
	VALUE(I915_MMAP_WC);
	VALUE(I915_MMAP_OFFSET_GTT);
	VALUE(I915_MMAP_OFFSET_WC);
	VALUE(I915_MMAP_OFFSET_WB);
	VALUE(I915_MMAP_OFFSET_UC);
	VALUE(I915_MMAP_OFFSET_FIXED);
	VALUE(I915_GEM_DOMAIN_CPU);
	VALUE(I915_GEM_DOMAIN_RENDER);
	VALUE(I915_GEM_DOMAIN_GTT);
	VALUE(I915_GEM_DOMAIN_WC);
	VALUE(I915_CACHING_NONE);
	VALUE(I915_CACHING_CACHED);
	VALUE(I915_CACHING_DISPLAY);
	VALUE(I915_TILING_NONE);
	VALUE(I915_TILING_X);
	VALUE(I915_TILING_Y);
	VALUE(I915_TILING_LAST);
	VALUE(I915_BIT_6_SWIZZLE_NONE);
	VALUE(I915_MADV_WILLNEED);
	VALUE(I915_MADV_DONTNEED);
	VALUE(I915_USERPTR_READ_ONLY);
	VALUE(I915_USERPTR_PROBE);
	VALUE(I915_USERPTR_UNSYNCHRONIZED);
	VALUE(I915_REG_READ_8B_WA);

	/* EXECBUFFER2. */
	VALUE(EXEC_OBJECT_NEEDS_FENCE);
	VALUE(EXEC_OBJECT_NEEDS_GTT);
	VALUE(EXEC_OBJECT_WRITE);
	VALUE(EXEC_OBJECT_SUPPORTS_48B_ADDRESS);
	VALUE(EXEC_OBJECT_PINNED);
	VALUE(EXEC_OBJECT_PAD_TO_SIZE);
	VALUE(EXEC_OBJECT_ASYNC);
	VALUE(EXEC_OBJECT_CAPTURE);
	VALUE(__EXEC_OBJECT_UNKNOWN_FLAGS);
	VALUE(I915_EXEC_RING_MASK);
	VALUE(I915_EXEC_DEFAULT);
	VALUE(I915_EXEC_RENDER);
	VALUE(I915_EXEC_BSD);
	VALUE(I915_EXEC_BLT);
	VALUE(I915_EXEC_VEBOX);
	VALUE(I915_EXEC_CONSTANTS_MASK);
	VALUE(I915_EXEC_GEN7_SOL_RESET);
	VALUE(I915_EXEC_SECURE);
	VALUE(I915_EXEC_IS_PINNED);
	VALUE(I915_EXEC_NO_RELOC);
	VALUE(I915_EXEC_HANDLE_LUT);
	VALUE(I915_EXEC_BSD_MASK);
	VALUE(I915_EXEC_RESOURCE_STREAMER);
	VALUE(I915_EXEC_FENCE_IN);
	VALUE(I915_EXEC_FENCE_OUT);
	VALUE(I915_EXEC_BATCH_FIRST);
	VALUE(I915_EXEC_FENCE_ARRAY);
	VALUE(I915_EXEC_FENCE_SUBMIT);
	VALUE(I915_EXEC_USE_EXTENSIONS);
	VALUE(__I915_EXEC_UNKNOWN_FLAGS);
	VALUE(I915_EXEC_CONTEXT_ID_MASK);
	VALUE(I915_EXEC_FENCE_WAIT);
	VALUE(I915_EXEC_FENCE_SIGNAL);
	VALUE(__I915_EXEC_FENCE_UNKNOWN_FLAGS);
	VALUE(DRM_I915_GEM_EXECBUFFER_EXT_TIMELINE_FENCES);

	/* Contexts. */
	VALUE(I915_CONTEXT_CREATE_FLAGS_USE_EXTENSIONS);
	VALUE(I915_CONTEXT_CREATE_FLAGS_SINGLE_TIMELINE);
	VALUE(I915_CONTEXT_CREATE_FLAGS_UNKNOWN);
	VALUE(I915_CONTEXT_CREATE_EXT_SETPARAM);
	VALUE(I915_CONTEXT_CREATE_EXT_CLONE);
	VALUE(I915_CONTEXT_PARAM_BAN_PERIOD);
	VALUE(I915_CONTEXT_PARAM_NO_ZEROMAP);
	VALUE(I915_CONTEXT_PARAM_GTT_SIZE);
	VALUE(I915_CONTEXT_PARAM_NO_ERROR_CAPTURE);
	VALUE(I915_CONTEXT_PARAM_BANNABLE);
	VALUE(I915_CONTEXT_PARAM_PRIORITY);
	VALUE(I915_CONTEXT_PARAM_SSEU);
	VALUE(I915_CONTEXT_PARAM_RECOVERABLE);
	VALUE(I915_CONTEXT_PARAM_VM);
	VALUE(I915_CONTEXT_PARAM_ENGINES);
	VALUE(I915_CONTEXT_PARAM_PERSISTENCE);
	VALUE(I915_CONTEXT_PARAM_RINGSIZE);
	VALUE(I915_CONTEXT_PARAM_PROTECTED_CONTENT);
	VALUE(I915_CONTEXT_PARAM_LOW_LATENCY);
	VALUE(I915_CONTEXT_PARAM_CONTEXT_IMAGE);
	VALUE(I915_CONTEXT_MAX_USER_PRIORITY);
	VALUE(I915_CONTEXT_DEFAULT_PRIORITY);
	VALUE(I915_CONTEXT_MIN_USER_PRIORITY);
	VALUE(I915_CONTEXT_ENGINES_EXT_LOAD_BALANCE);
	VALUE(I915_CONTEXT_ENGINES_EXT_BOND);
	VALUE(I915_CONTEXT_ENGINES_EXT_PARALLEL_SUBMIT);

	/* The layouts. */
	SIZE(drm_get_cap);
	FIELD(drm_get_cap, capability);
	FIELD(drm_get_cap, value);
	SIZE(drm_set_client_cap);
	SIZE(drm_gem_close);
	FIELD(drm_gem_close, handle);
	FIELD(drm_gem_close, pad);
	SIZE(drm_prime_handle);
	FIELD(drm_prime_handle, handle);
	FIELD(drm_prime_handle, flags);
	FIELD(drm_prime_handle, fd);
	SIZE(drm_syncobj_create);
	FIELD(drm_syncobj_create, handle);
	FIELD(drm_syncobj_create, flags);
	SIZE(drm_syncobj_destroy);
	FIELD(drm_syncobj_destroy, handle);
	FIELD(drm_syncobj_destroy, pad);
	SIZE(drm_syncobj_handle);
	FIELD(drm_syncobj_handle, handle);
	FIELD(drm_syncobj_handle, flags);
	FIELD(drm_syncobj_handle, fd);
	FIELD(drm_syncobj_handle, pad);
	FIELD(drm_syncobj_handle, point);
	SIZE(drm_syncobj_wait);
	FIELD(drm_syncobj_wait, handles);
	FIELD(drm_syncobj_wait, timeout_nsec);
	FIELD(drm_syncobj_wait, count_handles);
	FIELD(drm_syncobj_wait, flags);
	FIELD(drm_syncobj_wait, first_signaled);
	FIELD(drm_syncobj_wait, pad);
	FIELD(drm_syncobj_wait, deadline_nsec);
	SIZE(drm_syncobj_timeline_wait);
	FIELD(drm_syncobj_timeline_wait, handles);
	FIELD(drm_syncobj_timeline_wait, points);
	FIELD(drm_syncobj_timeline_wait, timeout_nsec);
	FIELD(drm_syncobj_timeline_wait, count_handles);
	FIELD(drm_syncobj_timeline_wait, flags);
	FIELD(drm_syncobj_timeline_wait, first_signaled);
	FIELD(drm_syncobj_timeline_wait, pad);
	FIELD(drm_syncobj_timeline_wait, deadline_nsec);
	SIZE(drm_syncobj_array);
	FIELD(drm_syncobj_array, handles);
	FIELD(drm_syncobj_array, count_handles);
	FIELD(drm_syncobj_array, pad);
	SIZE(drm_syncobj_timeline_array);
	FIELD(drm_syncobj_timeline_array, handles);
	FIELD(drm_syncobj_timeline_array, points);
	FIELD(drm_syncobj_timeline_array, count_handles);
	FIELD(drm_syncobj_timeline_array, flags);
	SIZE(drm_syncobj_transfer);
	FIELD(drm_syncobj_transfer, src_handle);
	FIELD(drm_syncobj_transfer, dst_handle);
	FIELD(drm_syncobj_transfer, src_point);
	FIELD(drm_syncobj_transfer, dst_point);
	FIELD(drm_syncobj_transfer, flags);
	FIELD(drm_syncobj_transfer, pad);

	SIZE(drm_i915_getparam);
	FIELD(drm_i915_getparam, param);
	FIELD(drm_i915_getparam, value);
	SIZE(drm_i915_query);
	FIELD(drm_i915_query, num_items);
	FIELD(drm_i915_query, flags);
	FIELD(drm_i915_query, items_ptr);
	SIZE(drm_i915_query_item);
	FIELD(drm_i915_query_item, query_id);
	FIELD(drm_i915_query_item, length);
	FIELD(drm_i915_query_item, flags);
	FIELD(drm_i915_query_item, data_ptr);
	SIZE(drm_i915_query_topology_info);
	FIELD(drm_i915_query_topology_info, flags);
	FIELD(drm_i915_query_topology_info, max_slices);
	FIELD(drm_i915_query_topology_info, max_subslices);
	FIELD(drm_i915_query_topology_info, max_eus_per_subslice);
	FIELD(drm_i915_query_topology_info, subslice_offset);
	FIELD(drm_i915_query_topology_info, subslice_stride);
	FIELD(drm_i915_query_topology_info, eu_offset);
	FIELD(drm_i915_query_topology_info, eu_stride);
	FIELD(drm_i915_query_topology_info, data);
	SIZE(i915_engine_class_instance);
	FIELD(i915_engine_class_instance, engine_class);
	FIELD(i915_engine_class_instance, engine_instance);
	SIZE(drm_i915_engine_info);
	FIELD(drm_i915_engine_info, engine);
	FIELD(drm_i915_engine_info, rsvd0);
	FIELD(drm_i915_engine_info, flags);
	FIELD(drm_i915_engine_info, capabilities);
	FIELD(drm_i915_engine_info, logical_instance);
	SIZE(drm_i915_query_engine_info);
	FIELD(drm_i915_query_engine_info, num_engines);
	FIELD(drm_i915_query_engine_info, engines);
	SIZE(drm_i915_gem_memory_class_instance);
	FIELD(drm_i915_gem_memory_class_instance, memory_class);
	FIELD(drm_i915_gem_memory_class_instance, memory_instance);
	SIZE(drm_i915_memory_region_info);
	FIELD(drm_i915_memory_region_info, region);
	FIELD(drm_i915_memory_region_info, probed_size);
	FIELD(drm_i915_memory_region_info, unallocated_size);
	FIELD(drm_i915_memory_region_info, probed_cpu_visible_size);
	FIELD(drm_i915_memory_region_info, unallocated_cpu_visible_size);
	SIZE(drm_i915_query_memory_regions);
	FIELD(drm_i915_query_memory_regions, num_regions);
	FIELD(drm_i915_query_memory_regions, regions);

	SIZE(i915_user_extension);
	FIELD(i915_user_extension, next_extension);
	FIELD(i915_user_extension, name);
	FIELD(i915_user_extension, flags);
	FIELD(i915_user_extension, rsvd);
	SIZE(drm_i915_gem_create);
	FIELD(drm_i915_gem_create, size);
	FIELD(drm_i915_gem_create, handle);
	FIELD(drm_i915_gem_create, pad);
	SIZE(drm_i915_gem_create_ext);
	FIELD(drm_i915_gem_create_ext, size);
	FIELD(drm_i915_gem_create_ext, handle);
	FIELD(drm_i915_gem_create_ext, flags);
	FIELD(drm_i915_gem_create_ext, extensions);
	SIZE(drm_i915_gem_create_ext_memory_regions);
	FIELD(drm_i915_gem_create_ext_memory_regions, pad);
	FIELD(drm_i915_gem_create_ext_memory_regions, num_regions);
	FIELD(drm_i915_gem_create_ext_memory_regions, regions);
	SIZE(drm_i915_gem_create_ext_protected_content);
	SIZE(drm_i915_gem_create_ext_set_pat);
	FIELD(drm_i915_gem_create_ext_set_pat, pat_index);
	SIZE(drm_i915_gem_mmap);
	SIZE(drm_i915_gem_mmap_offset);
	FIELD(drm_i915_gem_mmap_offset, handle);
	FIELD(drm_i915_gem_mmap_offset, pad);
	FIELD(drm_i915_gem_mmap_offset, offset);
	FIELD(drm_i915_gem_mmap_offset, flags);
	FIELD(drm_i915_gem_mmap_offset, extensions);
	SIZE(drm_i915_gem_pread);
	SIZE(drm_i915_gem_pwrite);
	SIZE(drm_i915_gem_exec_object2);
	FIELD(drm_i915_gem_exec_object2, handle);
	FIELD(drm_i915_gem_exec_object2, relocation_count);
	FIELD(drm_i915_gem_exec_object2, relocs_ptr);
	FIELD(drm_i915_gem_exec_object2, alignment);
	FIELD(drm_i915_gem_exec_object2, offset);
	FIELD(drm_i915_gem_exec_object2, flags);
	FIELD(drm_i915_gem_exec_object2, rsvd1);
	FIELD(drm_i915_gem_exec_object2, rsvd2);
	SIZE(drm_i915_gem_execbuffer2);
	FIELD(drm_i915_gem_execbuffer2, buffers_ptr);
	FIELD(drm_i915_gem_execbuffer2, buffer_count);
	FIELD(drm_i915_gem_execbuffer2, batch_start_offset);
	FIELD(drm_i915_gem_execbuffer2, batch_len);
	FIELD(drm_i915_gem_execbuffer2, DR1);
	FIELD(drm_i915_gem_execbuffer2, DR4);
	FIELD(drm_i915_gem_execbuffer2, num_cliprects);
	FIELD(drm_i915_gem_execbuffer2, cliprects_ptr);
	FIELD(drm_i915_gem_execbuffer2, flags);
	FIELD(drm_i915_gem_execbuffer2, rsvd1);
	FIELD(drm_i915_gem_execbuffer2, rsvd2);
	SIZE(drm_i915_gem_exec_fence);
	FIELD(drm_i915_gem_exec_fence, handle);
	FIELD(drm_i915_gem_exec_fence, flags);
	SIZE(drm_i915_gem_execbuffer_ext_timeline_fences);
	FIELD(drm_i915_gem_execbuffer_ext_timeline_fences, base);
	FIELD(drm_i915_gem_execbuffer_ext_timeline_fences, fence_count);
	FIELD(drm_i915_gem_execbuffer_ext_timeline_fences, handles_ptr);
	FIELD(drm_i915_gem_execbuffer_ext_timeline_fences, values_ptr);
	SIZE(drm_i915_gem_wait);
	FIELD(drm_i915_gem_wait, bo_handle);
	FIELD(drm_i915_gem_wait, flags);
	FIELD(drm_i915_gem_wait, timeout_ns);
	SIZE(drm_i915_gem_busy);
	FIELD(drm_i915_gem_busy, handle);
	FIELD(drm_i915_gem_busy, busy);
	SIZE(drm_i915_gem_set_tiling);
	FIELD(drm_i915_gem_set_tiling, handle);
	FIELD(drm_i915_gem_set_tiling, tiling_mode);
	FIELD(drm_i915_gem_set_tiling, stride);
	FIELD(drm_i915_gem_set_tiling, swizzle_mode);
	SIZE(drm_i915_gem_get_tiling);
	FIELD(drm_i915_gem_get_tiling, handle);
	FIELD(drm_i915_gem_get_tiling, tiling_mode);
	FIELD(drm_i915_gem_get_tiling, swizzle_mode);
	FIELD(drm_i915_gem_get_tiling, phys_swizzle_mode);
	SIZE(drm_i915_gem_caching);
	FIELD(drm_i915_gem_caching, handle);
	FIELD(drm_i915_gem_caching, caching);
	SIZE(drm_i915_gem_set_domain);
	FIELD(drm_i915_gem_set_domain, handle);
	FIELD(drm_i915_gem_set_domain, read_domains);
	FIELD(drm_i915_gem_set_domain, write_domain);
	SIZE(drm_i915_gem_sw_finish);
	SIZE(drm_i915_gem_madvise);
	FIELD(drm_i915_gem_madvise, handle);
	FIELD(drm_i915_gem_madvise, madv);
	FIELD(drm_i915_gem_madvise, retained);
	SIZE(drm_i915_gem_userptr);
	SIZE(drm_i915_gem_get_aperture);
	FIELD(drm_i915_gem_get_aperture, aper_size);
	FIELD(drm_i915_gem_get_aperture, aper_available_size);
	SIZE(drm_i915_gem_context_create);
	FIELD(drm_i915_gem_context_create, ctx_id);
	SIZE(drm_i915_gem_context_create_ext);
	FIELD(drm_i915_gem_context_create_ext, ctx_id);
	FIELD(drm_i915_gem_context_create_ext, flags);
	FIELD(drm_i915_gem_context_create_ext, extensions);
	SIZE(drm_i915_gem_context_create_ext_setparam);
	FIELD(drm_i915_gem_context_create_ext_setparam, base);
	FIELD(drm_i915_gem_context_create_ext_setparam, param);
	SIZE(drm_i915_gem_context_destroy);
	SIZE(drm_i915_gem_context_param);
	FIELD(drm_i915_gem_context_param, ctx_id);
	FIELD(drm_i915_gem_context_param, size);
	FIELD(drm_i915_gem_context_param, param);
	FIELD(drm_i915_gem_context_param, value);
	SIZE(i915_context_param_engines);
	FIELD(i915_context_param_engines, extensions);
	FIELD(i915_context_param_engines, engines);
	SIZE(drm_i915_gem_vm_control);
	FIELD(drm_i915_gem_vm_control, extensions);
	FIELD(drm_i915_gem_vm_control, flags);
	FIELD(drm_i915_gem_vm_control, vm_id);
	SIZE(drm_i915_reset_stats);
	FIELD(drm_i915_reset_stats, ctx_id);
	FIELD(drm_i915_reset_stats, flags);
	FIELD(drm_i915_reset_stats, reset_count);
	FIELD(drm_i915_reset_stats, batch_active);
	FIELD(drm_i915_reset_stats, batch_pending);
	FIELD(drm_i915_reset_stats, pad);
	SIZE(drm_i915_reg_read);
	FIELD(drm_i915_reg_read, offset);
	FIELD(drm_i915_reg_read, val);

	SIZE(dma_buf_export_sync_file);
	FIELD(dma_buf_export_sync_file, flags);
	FIELD(dma_buf_export_sync_file, fd);
	SIZE(dma_buf_import_sync_file);
	return 0;
}
