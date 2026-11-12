#!/usr/bin/env python3
"""Write the seed corpus for the `intel_uapi` fuzz target.

The target reads its input as a program of ops (`src/tests/fuzz/fuzz_targets/
intel_uapi.rs`, `run_op`): each op is a byte, and the bytes after it say how
to build its call. Every seed here is a short program as Mesa's anv runs it
on Gen12, one per ioctl family, with the layouts the front end's host tests
pin (`src/lib/drivers/gpu/intel-xelp/src/frontend/tests.rs`): a softpinned
batch and its execbuffer, a fence array and the timeline-fences extension,
syncobjs exported and imported as sync files, a context with a VM and an
engine map, a dmabuf shared between two processes, the queries anv starts
with. A few walk to a table's limit with the repeat bit, and one is the
input that found a bug, kept as its regression.

Each builder below reads its op's fields in the order the target does; the
comments name the field. `INTEL_UAPI_TRACE=1` on the fuzz binary with a seed
as its argument prints every call and its answer, which is how these were
checked.
"""

import pathlib
import struct

OUT = (
    pathlib.Path(__file__).resolve().parent.parent.parent.parent
    / "src"
    / "tests"
    / "fuzz"
    / "corpus"
    / "intel_uapi"
)

# The ops' numbers (`run_op`).
OPEN, RELEASE, GEM_CREATE, GEM_CREATE_EXT, GEM_CLOSE, GEM_SMALL, GEM_WAIT = range(7)
MMAP_OFFSET, MMAP, PRIME_EXPORT, PRIME_IMPORT, EXECBUFFER = 7, 8, 9, 10, 11
CONTEXT_CREATE, CONTEXT_DESTROY, CONTEXT_PARAM, VM_CREATE, VM_DESTROY = 13, 14, 15, 16, 17
SYNCOBJ_CREATE, SYNCOBJ_DESTROY, SYNCOBJ_FD, SYNCOBJ_TRANSFER = 18, 19, 20, 21
SYNCOBJ_ARRAY, SYNCOBJ_WAIT, INFO, RAW, DESCRIPTORS, DMABUF_IMPORT = 22, 23, 24, 25, 26, 27
COMPLETE, SERVICE, HELD, FAULTS = 28, 29, 30, 31

# `gem_small`'s calls.
BUSY, SET_TILING, GET_TILING, SET_CACHING, GET_CACHING = range(5)
SET_DOMAIN, SW_FINISH, MADVISE, GET_APERTURE, THROTTLE = range(5, 10)

# Execbuffer fence flags.
FENCE_WAIT, FENCE_SIGNAL = 0, 3


def u32(value):
    return list(struct.pack("<I", value))


def u64(value):
    return list(struct.pack("<Q", value))


def word(value):
    """`Input::word`."""
    if value in (0, 1, 2):
        return [value]
    return [6] + u32(value)


def point(value):
    """`point`."""
    return {0: [0], 1: [3], 2: [4]}.get(value, [6, value])


def own(index):
    """`Rig::name`: the file's own name of that kind, by the order it was
    told them."""
    return [0, index]


def others(index):
    """`Rig::name`: another open file's name."""
    return [5, index]


DEFAULT_CONTEXT = [7, 0]  # `Rig::name` at an edge: id 0, the default context
VALID = [0]  # `Rig::ptr`: the pointer to what was built
ARG = [0]  # `Rig::ioctl`: the argument where it was built


class Program:
    def __init__(self, auto=False, trace=False):
        self.bytes = [int(auto) | int(trace) << 2]

    def add(self, *parts):
        for part in parts:
            self.bytes += part if isinstance(part, list) else [part]
        return self

    # Files and descriptors.
    def open(self, proc):
        return self.add(OPEN, proc)

    def release(self, file):
        return self.add(RELEASE, file)

    def close_fd(self, proc, fd):
        return self.add(DESCRIPTORS, proc, 0, fd, 0, 1)

    def pass_fd(self, proc, fd, to):
        return self.add(DESCRIPTORS, proc, 0, fd, 2, to)

    def export_sync(self, proc, fd):
        """A sync file of a dmabuf's fences, as the core serves it."""
        return self.add(DESCRIPTORS, proc, 0, fd, 3)

    # GEM.
    def create(self, file, pages):
        # file, size (13: pages x 4096), handle (word 0), argument
        return self.add(GEM_CREATE, file, 13, pages, word(0), ARG)

    def create_ext_system(self, file, pages):
        # file, size, flags (not 0 mod 8: none), MEMORY_REGIONS (2), one
        # region, system memory, its pointer, no next, num_regions as built,
        # the extension's pointer, argument
        return self.add(GEM_CREATE_EXT, file, 13, pages, 1, 2, 1, 0, VALID, 1, 1, VALID, ARG)

    def create_2m(self, file):
        # file, size (7: 2 MiB, two windows from a page in), handle, argument
        return self.add(GEM_CREATE, file, 7, word(0), ARG)

    def close(self, file, handle):
        return self.add(GEM_CLOSE, file, handle, 0, ARG)

    def small(self, file, handle, which, a=0, b=0):
        return self.add(GEM_SMALL, file, handle, word(a), word(b), which, ARG)

    def wait(self, file, handle, timeout):
        # timeout: `Input::timeout`'s selector (0 poll, 3 a millisecond,
        # 5 for ever)
        return self.add(GEM_WAIT, file, handle, 0, timeout, ARG)

    def mmap_offset(self, file, handle, wc=False):
        return self.add(MMAP_OFFSET, file, handle, 3 if wc else 0, 1, ARG)

    def mmap(self, file, offset_index, pages=1):
        # an offset a file was told, no page added, then `pages` (selector
        # 0: one page; 6: the next byte)
        return self.add(MMAP, file, 0, offset_index, 1, [0] if pages == 1 else [6, pages])

    def prime_export(self, file, handle):
        return self.add(PRIME_EXPORT, file, handle, 0, ARG)

    def prime_import(self, file, fd):
        return self.add(PRIME_IMPORT, file, 0, fd, ARG)

    def execbuffer(
        self,
        file,
        objects,
        context=DEFAULT_CONTEXT,
        ring=0,
        fence_out=False,
        fence_in=None,
        fence_array=None,
        timeline=None,
    ):
        """objects: (handle selector, page, write); the last is the batch.
        fence_array: (syncobj selector, FENCE_WAIT or FENCE_SIGNAL).
        timeline: (syncobj selector, flags, point)."""
        self.add(EXECBUFFER, file, len(objects))
        for handle, page, write in objects:
            # handle, its flag bits (1: WRITE), address (selector 1: page x 4 KiB)
            self.add(handle, 1 if write else 0, 1, page)
        self.add(VALID, ring)
        bits = 0
        if fence_out:
            bits |= 0x02
        if fence_in is not None:
            bits |= 0x04
        if fence_array is not None:
            bits |= 0x08
        if timeline is not None:
            bits |= 0x10
        self.add(bits)
        if fence_in is not None:
            self.add(1, 0, fence_in)  # FENCE_IN, a descriptor of the process
        if fence_array is not None:
            self.add(len(fence_array))
            for syncobj, flags in fence_array:
                self.add(syncobj, flags)
            self.add(VALID, 1)  # the array, num_cliprects as built
        if timeline is not None:
            self.add(len(timeline))
            for syncobj, flags, value in timeline:
                self.add(syncobj, flags, point(value))
            # no next extension, its name, fence_count as built, the
            # handles, the values, the extension
            self.add(1, 1, 1, VALID, VALID, VALID)
        # the context, buffer_count as built, dr4 0, EXECBUFFER2, argument
        return self.add(context, 1, 1, 0, ARG)

    # Contexts and VMs.
    def context_create(self, file, params=()):
        """params: lists of a param's bytes (`vm_param`, `engines_param`,
        `priority_param`)."""
        if not params:
            return self.add(CONTEXT_CREATE, file, 0, 1, 1, ARG)
        self.add(CONTEXT_CREATE, file, 2, len(params))
        for param in params:
            self.add(param, 0, 1)  # the param, ctx_id 0, its name 0
        return self.add(VALID, 1, ARG)

    def context_destroy(self, file, context):
        return self.add(CONTEXT_DESTROY, file, context, 0, ARG)

    def context_param(self, file, context, set_it, param):
        return self.add(CONTEXT_PARAM, file, context, int(set_it), param, ARG)

    def vm_create(self, file):
        return self.add(VM_CREATE, file, 0, ARG)

    def vm_destroy(self, file, vm):
        return self.add(VM_DESTROY, file, vm, 0, ARG)

    # Syncobjs.
    def syncobj(self, file, signalled=False):
        return self.add(SYNCOBJ_CREATE, file, 2 if signalled else 0, ARG)

    def syncobj_destroy(self, file, syncobj):
        return self.add(SYNCOBJ_DESTROY, file, syncobj, 0, ARG)

    def syncobj_export(self, file, syncobj, timeline_point=None):
        """`HANDLE_TO_FD` as a sync file, of a timeline point if given."""
        if timeline_point is None:
            return self.add(SYNCOBJ_FD, file, syncobj, 1, 0, 0, ARG)
        return self.add(SYNCOBJ_FD, file, syncobj, 1, 4, 0, point(timeline_point), ARG)

    def syncobj_import(self, file, syncobj, fd):
        """`FD_TO_HANDLE` of a sync file into a syncobj."""
        return self.add(SYNCOBJ_FD, file, syncobj, 0, 0, 0, fd, 0, ARG)

    def transfer(self, file, source, destination, source_point=0, destination_point=0):
        return self.add(
            SYNCOBJ_TRANSFER,
            file,
            source,
            destination,
            point(source_point),
            point(destination_point),
            1,
            0,
            ARG,
        )

    def syncobj_array(self, file, syncobjs, which, points=()):
        """which: 0 RESET, 1 SIGNAL, 2 TIMELINE_SIGNAL, 3 QUERY."""
        self.add(SYNCOBJ_ARRAY, file, len(syncobjs))
        for syncobj in syncobjs:
            self.add(syncobj)
        self.add(VALID, 1, which)
        if which in (2, 3):
            for value in points:
                self.add(point(value))
            self.add(1, VALID, 1 if which == 2 else 0)
        return self.add(ARG)

    def syncobj_wait(self, file, syncobjs, flags=0, timeline_points=None, deadline=True):
        """Held until signalled or a millisecond from now (`deadline`), or a
        poll."""
        self.add(SYNCOBJ_WAIT, file, len(syncobjs))
        for syncobj in syncobjs:
            self.add(syncobj)
        self.add(VALID, 1, flags, 0, 1 if deadline else 0)
        if timeline_points is None:
            self.add(0)
        else:
            self.add(1)
            for value in timeline_points:
                self.add(point(value))
            self.add(1, VALID)
        return self.add(ARG)

    # GETPARAM, QUERY and the rest.
    def getparam(self, file, param):
        return self.add(INFO, file, 0, 0, param, VALID, ARG)

    def query(self, file, items):
        """items: (query_id selector, length selector): 0 ENGINE_INFO,
        3 TOPOLOGY_INFO, 5 MEMORY_REGIONS; lengths 0 (ask), 2 (128 bytes,
        engine info's), 3 (104)."""
        self.add(INFO, file, 1, len(items))
        for query_id, length in items:
            self.add(query_id, length, 1, VALID)
        return self.add(1, 1, VALID, ARG)

    def version(self, file):
        return self.add(INFO, file, 2, 2, VALID, 2, VALID, 2, VALID, ARG)

    def get_cap(self, file, capability):
        return self.add(INFO, file, 3, 5, capability, ARG)

    def reset_stats(self, file, context):
        return self.add(INFO, file, 4, context, 1, ARG)

    def reg_read(self, file):
        return self.add(INFO, file, 5, 0, ARG)

    # The GPU and time.
    def complete(self, count=1, hung=False, context=0):
        return self.add(COMPLETE, context, count, 0 if hung else 1)

    def service(self, later=1):
        """later: 0 a nanosecond, 1 a millisecond, 2 three seconds."""
        return self.add(SERVICE, later, 1)

    def interrupt(self, held=0):
        return self.add(HELD, held, 1)

    def faults(self, mask):
        """Arm the model's failures (`fault` in the target), each once."""
        return self.add(FAULTS, u32(mask)[:2])

    def repeat(self, times, build):
        """The op `build` adds, with the repeat bit: run `times` times (odd)."""
        at = len(self.bytes)
        build(self)
        self.bytes[at] |= 0x80
        self.bytes.insert(at + 1, (times - 1) // 2)
        return self

    def data(self):
        return bytes(self.bytes)


# The model's failures (`fault`).
FAULT_BIND_PARTWAY, FAULT_PT_ALLOC = 1 << 11, 1 << 12

# The two files every run starts with: A in process 0, B in process 1.
A, B = 0, 1
CHIPSET_ID, HAS_EXEC_SOFTPIN = 4, 37
CAP_SYNCOBJ = 19
VM_PARAM = [0, 0, 0, 1]  # PARAM_VM: the file's first VM id, as told
ENGINES_PARAM = [2, 2, 1, 0, 3, 0, 0, 1]  # PARAM_ENGINES: render, copy
PRIORITY_PARAM = [4, 0]  # PARAM_PRIORITY 0
VM_GETPARAM = [0, 7, 0, 1]  # PARAM_VM of the default context's (id 0)


def seeds():
    yield "gem-lifecycle", (
        Program()
        .create(A, 2)
        .mmap_offset(A, own(0))
        .mmap(A, 0)
        .small(A, own(0), SET_TILING, a=2, b=512)
        .small(A, own(0), GET_TILING)
        .small(A, own(0), SET_CACHING, a=1)
        .small(A, own(0), GET_CACHING)
        .small(A, own(0), SET_DOMAIN, a=0, b=1)
        .small(A, own(0), SW_FINISH)
        .small(A, own(0), MADVISE, a=1)
        .small(A, own(0), BUSY)
        .small(A, own(0), GET_APERTURE)
        .small(A, own(0), THROTTLE)
        .close(A, own(0))
    )
    yield "gem-create-ext-system-memory", (
        Program().create_ext_system(A, 4).mmap_offset(A, own(0), wc=True).mmap(A, 0, pages=4)
    )
    yield "exec-wait", (
        Program()
        .create(A, 1)
        .execbuffer(A, [(own(0), 1, False)])
        .small(A, own(0), BUSY)
        .wait(A, own(0), 5)
        .complete()
        .wait(A, own(0), 0)
    )
    yield "exec-fence-array-out-fence", (
        Program()
        .syncobj(A)
        .create(A, 1)
        .execbuffer(A, [(own(0), 1, False)], fence_out=True, fence_array=[(own(0), FENCE_SIGNAL)])
        .syncobj_wait(A, [own(0)])
        .syncobj(A)
        .syncobj_import(A, own(1), 0)
        .complete()
        .syncobj_wait(A, [own(0), own(1)], flags=1, deadline=False)
    )
    yield "exec-timeline-fences", (
        Program()
        .syncobj(A)
        .create(A, 1)
        .create(A, 1)
        .execbuffer(A, [(own(0), 2, True), (own(1), 1, False)], timeline=[(own(0), FENCE_SIGNAL, 1)])
        .syncobj_wait(A, [own(0)], flags=2, timeline_points=[1])
        .syncobj_array(A, [own(0)], 3, points=[0])
        .complete()
        .execbuffer(A, [(own(1), 1, False)], timeline=[(own(0), FENCE_WAIT, 1)])
        .complete()
    )
    yield "syncobj-ops", (
        Program()
        .syncobj(A, signalled=True)
        .syncobj(A)
        .syncobj_export(A, own(0))
        .syncobj_import(A, own(1), 0)
        .transfer(A, own(0), own(1), 0, 1)
        .syncobj_array(A, [own(0), own(1)], 0)
        .syncobj_array(A, [own(0), own(1)], 1)
        .syncobj_array(A, [own(1)], 2, points=[2])
        .syncobj_array(A, [own(1)], 3, points=[0])
        .syncobj_export(A, own(1), timeline_point=2)
        .syncobj_destroy(A, own(1))
        .syncobj_destroy(A, own(0))
    )
    yield "context-vm-engines", (
        Program()
        .vm_create(A)
        .context_create(A, [VM_PARAM, ENGINES_PARAM])
        .create(A, 1)
        .execbuffer(A, [(own(0), 1, False)], context=own(0), ring=0)
        .context_param(A, own(0), True, PRIORITY_PARAM)
        .context_param(A, DEFAULT_CONTEXT, False, VM_GETPARAM)
        .context_destroy(A, own(0))
        .vm_destroy(A, own(0))
        .complete()
    )
    yield "prime-shared-between-processes", (
        Program()
        .create(A, 1)
        .create(A, 1)
        .prime_export(A, own(0))
        .pass_fd(0, 0, 1)
        .prime_import(B, 0)
        .create(B, 1)
        .execbuffer(B, [(own(0), 1, True), (own(1), 2, False)])
        .export_sync(1, 0)
        .add(DMABUF_IMPORT, 1, 0, 0, 3, 1, 0, 1)
        .execbuffer(A, [(own(0), 3, False), (own(1), 1, False)])
        .complete()
        .close_fd(1, 0)
        .close_fd(1, 0)
        .close_fd(0, 0)
        .release(B)
        .service()
    )
    yield "info", (
        Program()
        .getparam(A, CHIPSET_ID)
        .getparam(A, HAS_EXEC_SOFTPIN)
        .query(A, [(0, 0), (3, 0), (5, 0)])
        .query(A, [(0, 2), (5, 3)])
        .version(A)
        .get_cap(A, CAP_SYNCOBJ)
        .reset_stats(A, DEFAULT_CONTEXT)
        .reg_read(A)
    )
    yield "hang-and-reset-stats", (
        Program()
        .context_create(A)
        .create(A, 1)
        .execbuffer(A, [(own(0), 1, False)], context=own(0))
        .execbuffer(A, [(own(0), 1, False)], context=own(0))
        .complete(count=1, hung=True)
        .reset_stats(A, own(0))
        .execbuffer(A, [(own(0), 1, False)], context=own(0))
        .complete()
    )
    yield "softpin-over-a-busy-binding", (
        Program()
        .create(A, 2)
        .create(A, 2)
        .execbuffer(A, [(own(0), 1, False)])
        .execbuffer(A, [(own(1), 2, False)])
        .complete()
        .complete()
    )
    yield "vm-destroyed-while-bound", (
        Program()
        .vm_create(A)
        .context_create(A, [VM_PARAM])
        .create(A, 1)
        .execbuffer(A, [(own(0), 1, False)], context=own(0))
        .vm_destroy(A, own(0))
        .context_destroy(A, own(0))
        .close(A, own(0))
        .complete()
    )
    yield "another-files-handles", (
        Program()
        .create(A, 1)
        .syncobj(A)
        .mmap_offset(A, own(0))
        .small(B, others(0), GET_TILING)
        .mmap(B, 0)
        .execbuffer(B, [(others(0), 1, False)])
        .syncobj_destroy(B, others(0))
        .release(A)
        .create(A, 1)
    )
    yield "release-with-work-in-flight", (
        Program()
        .create(A, 1)
        .syncobj(A)
        .execbuffer(A, [(own(0), 1, False)], fence_out=True, fence_array=[(own(0), FENCE_SIGNAL)])
        .syncobj_wait(A, [own(0)])
        .release(A)
        .complete()
    )
    # A bind that fails part-way (D1): a 2 MiB object one page into the VM
    # spans two windows, the first written before the second fails, on its
    # last page's address, then on a page table (the first window's table
    # is there already, the page at 0's). Nothing of either may stay mapped.
    yield "bind-fails-part-way", (
        Program()
        .create(A, 1)
        .execbuffer(A, [(own(0), 0, False)])
        .create_2m(A)
        .faults(FAULT_BIND_PARTWAY)
        .execbuffer(A, [(own(0), 0, False), (own(1), 1, False)])
        .faults(FAULT_PT_ALLOC)
        .execbuffer(A, [(own(0), 0, False), (own(1), 1, False)])
        .execbuffer(A, [(own(0), 0, False), (own(1), 1, False)])
        .complete(count=4)
        .close(A, own(1))
        .service()
    )
    yield "limit-files", Program().repeat(41, lambda p: p.open(0))
    yield "limit-held-waits", (
        Program()
        .syncobj(A)
        .repeat(71, lambda p: p.syncobj_wait(A, [own(0)], flags=2))
        .interrupt()
        .service(2)
    )
    yield "limit-vms", Program().repeat(301, lambda p: p.vm_create(A)).release(A)
    yield "raw-gem-create", (
        Program().add(RAW, A, 0, 22, 16, u64(4096), u64(0), 0, ARG)
    )
    # The input the target found first (2026-11-12): 499 GEM_VM_CREATEs and
    # the end of the run. Every VM past the first 64 outlived the release
    # (`vm::free_idle`; `release_frees_more_than_64_contexts_and_vms`).
    yield "regression-vms-outlive-release", bytes([0xE4, 0xF0, 0xF9, 0x00])


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for name, program in seeds():
        data = program if isinstance(program, bytes) else program.data()
        (OUT / name).write_bytes(data)
        print(f"  {name}: {len(data)} bytes")


if __name__ == "__main__":
    main()
