//! Explanations for the fatal conditions the kernel knows how to name.
//!
//! A panic's own message says what went wrong at the site, in one sentence
//! with the particulars filled in. That is enough for whoever wrote the check
//! and not for anyone else: "stage 4 self-check failed: a processor did not
//! finish its share of the work" does not say what stage 4 is for, why the
//! kernel cannot carry on without it, or where to start looking. This catalog
//! is that second half, written once per failure rather than once per site, so
//! a site names an entry and the report prints it below the trace.
//!
//! # Codes
//!
//! Every entry has a stable code, `FX-SSNN`. `SS` is the stage of
//! `docs/ROADMAP.md` whose check or bring-up failed: `01` to `07` for those
//! stages, `00` for anything before stage 1 or outside any stage — a runtime
//! mechanism timing out, say — and `90` for the trap path's reports. `NN`
//! numbers the entries within it. A code is never reused once published, so a
//! log line from an old build still means what it meant.
//!
//! # The document
//!
//! `tools/common/gen/gen-panic-catalog.py` reads this file, checks it — codes well
//! formed and unique, every entry listed in [`ALL`], every path in `see` real —
//! and renders it into `docs/generated/PANICS.md`, so the explanations can be
//! read without a machine that has just stopped. Its `--check` fails when the
//! document is stale.

/// A fatal condition, explained for someone who has not read the source.
#[derive(Debug)]
pub(crate) struct Explanation {
    /// Stable code, `FX-SSNN`: `SS` the roadmap stage whose check or bring-up
    /// failed (`00` for anything before stage 1 or outside any stage, `90` for
    /// traps), `NN` a number within it. Never reused once published.
    pub(crate) code: &'static str,
    /// What failed, in a few words.
    pub(crate) title: &'static str,
    /// What the check or step establishes, and why the kernel cannot continue
    /// without it. Two to five sentences.
    pub(crate) meaning: &'static str,
    /// The likely causes, most likely first. One sentence each.
    pub(crate) causes: &'static [&'static str],
    /// Where to read more: repository paths, optionally with a function name.
    pub(crate) see: &'static str,
}

/// For `wait_for` in `smp.rs`, when a TLB shootdown times out.
pub(crate) static SHOOTDOWN_TIMEOUT: Explanation = Explanation {
    code: "FX-0001",
    title: "a processor never flushed its TLB for a shootdown",
    meaning: "When a kernel mapping is removed or made less permissive on x86-64, \
              `flush_tlb_everywhere` flushes this processor's TLB, interrupts every other \
              online processor, and waits for each to flush its own: at least a second, and \
              until it has asked 16,777,216 times, a count that takes longer the slower the \
              machine runs (about 1.8 s under KVM, 5 s under QEMU's tcg, 32 s under its \
              coverage plugin). A processor that has not answered may still translate \
              through the old entry, so the memory behind it cannot safely be freed or the \
              narrowed permission relied on, and the kernel stops instead. AArch64 and \
              ARMv7-A invalidate every processor's TLB in hardware and never wait here.",
    causes: &[
        "The named processor was spinning with interrupts masked on a lock this one held \
         when it asked for the shootdown, so it could not take the interrupt; \
         `flush_tlb_everywhere` must not be called holding such a lock.",
        "The named processor is halted or stuck with interrupts masked somewhere other than \
         the shootdown wait, which is the only place that answers without the interrupt.",
        "Inter-processor interrupts sent through the local APIC are not reaching the named \
         processor; the sends here ignore the APIC's own refusal.",
        "A host that stopped running the named virtual processor, and kept running this one, \
         for longer than the count takes, which is the host's fault and not the kernel's.",
    ],
    see: "src/kernel/src/smp.rs flush_tlb_everywhere; src/kernel/src/smp.rs patience; \
          docs/ROADMAP.md stage 4",
};

/// For `wait_for` in `smp.rs`, when a grace period times out.
pub(crate) static GRACE_PERIOD_TIMEOUT: Explanation = Explanation {
    code: "FX-0002",
    title: "a processor never left a read-side section",
    meaning: "`synchronize` waits for a grace period by interrupting every other online \
              processor and waiting for each to take the interrupt -- at least five seconds, \
              and until it has asked five times as often as a shootdown does -- which none \
              can do inside a read-side section, because a section masks \
              interrupts. A writer frees what it unpublished only after that wait, so a \
              processor that never answers may still be reading it, and the kernel stops \
              rather than free memory that is in use.",
    causes: &[
        "A read-side section on the named processor ran for more than five seconds and \
         that many polls, or waited for something, which a section must never do.",
        "The named processor was spinning with interrupts masked on a lock this one held \
         when it called `synchronize`.",
        "The named processor is halted or stuck with interrupts masked, so it takes no \
         interrupt at all.",
    ],
    see: "src/kernel/src/smp.rs synchronize; src/kernel/src/smp.rs read_section; \
          docs/ROADMAP.md stage 4",
};

/// For `flush_tlb_everywhere` in `smp.rs`, when the shootdown turn is never
/// released.
pub(crate) static SHOOTDOWN_TURN_TIMEOUT: Explanation = Explanation {
    code: "FX-0003",
    title: "a TLB shootdown never got its turn",
    meaning: "On x86-64 one processor at a time runs a TLB shootdown, and \
              `flush_tlb_everywhere` waits for the turn while answering the shootdowns ahead of \
              it. Every holder takes a new generation as soon as it has the turn, and gives up \
              on the machine after a second and its count of polls waiting for the other \
              processors, so a generation that stands still for four times both while the \
              turn is held means its holder is no longer running. Nothing would ever release \
              the turn, and the memory this processor's caller is about to free could never be \
              made safe, so the kernel stops instead of waiting forever.",
    causes: &[
        "The task holding the turn was preempted while holding it and has not run since, \
         because its processor stopped taking interrupts or never gives it a slice.",
        "The processor holding the turn is halted or hung with interrupts masked, part-way \
         through its own shootdown.",
        "A host so overcommitted that a virtual processor went unscheduled for seconds, which \
         is the host's fault and not the kernel's.",
    ],
    see: "src/kernel/src/smp.rs flush_tlb_everywhere; docs/ROADMAP.md stage 4",
};

/// For `this_logical_cpu` in `user/space.rs`, when an address space is
/// installed or uninstalled on a processor with no per-CPU record.
pub(crate) static SPACE_SET_WITHOUT_RECORD: Explanation = Explanation {
    code: "FX-0004",
    title: "an address space was switched on a processor that cannot name itself",
    meaning: "Every address space keeps the set of processors whose TLB may still hold its \
              translations, and a TLB shootdown for the space reaches exactly those. A \
              processor joins the set before it loads the space's root and leaves it after the \
              root write that flushed it, by its logical number, read from its per-CPU record. \
              With no record there is no number, and any guess would leave a processor that \
              caches the space out of the set, so a later shootdown would free memory it can \
              still reach. Nothing installs a user space before every processor has its record, \
              so the kernel stops instead.",
    causes: &[
        "Something installed or uninstalled a user address space before `smp::discover` set \
         the boot processor's record, or on a secondary before `install_secondary_record`.",
        "The per-CPU register was overwritten, so `smp::this_cpu` no longer finds the record.",
    ],
    see: "src/kernel/src/user/space.rs install; src/kernel/src/smp.rs this_cpu; \
          docs/ROADMAP.md stage 6",
};

/// For `remap` in `user/space.rs`, when the object behind a private region
/// has more than one mapper.
pub(crate) static PRIVATE_OBJECT_SHARED: Explanation = Explanation {
    code: "FX-0005",
    title: "a private region's object was mapped by more than one address space",
    meaning: "An object backing a private region has exactly one mapper: `fork` gives the \
              child an object of its own, and `vmo_map` maps objects shared. `mremap` relies on \
              it when it moves a private region's pages to a fresh object, because it tells \
              only its own address space to forget them. A second mapper would keep \
              translations to pages that have left the object, and reach frames given back to \
              the allocator, so the kernel stops before anything moves.",
    causes: &[
        "Something attached a second address space to a private region's object, bypassing \
         `Vmo::attach`'s own check.",
        "A region's sharing flag changed after its object was attached, so an object attached \
         as shared now backs a private region.",
    ],
    see: "src/kernel/src/user/space.rs remap; src/kernel/src/user/vmo.rs attach; \
          docs/ROADMAP.md stage 6",
};

/// For `register_load` in `main.rs`, when the load ring did not register
/// what the certified item reaches it through.
pub(crate) static LOAD_REGISTRATION: Explanation = Explanation {
    code: "FX-0006",
    title: "the load ring did not register what the item needs from it",
    meaning: "The certified item may not name what is above it (`docs/certification/ITEM.md`), so \
              the filesystem, the Linux personality and board support register what the item \
              must reach at bring-up: the commits power makes before the machine stops, the \
              launcher init starts pid 1 with, the reader `devmgr` reads its drivers with, \
              board support's device bindings and its boot mode. A registration missing here \
              is a machine that would power off without committing its disks, start no init, \
              or publish no board devices, and each of those would pass unnoticed until it \
              mattered, so the kernel stops at the first line that can say so.",
    causes: &[
        "An `install` call was removed from `register_load` in `main.rs`.",
        "A registration list is full: another registration was added without raising the \
         bound its list declares (`power::FLUSHES`, `device::BOARD`).",
    ],
    see: "src/kernel/src/main.rs register_load; src/kernel/src/hooks.rs; src/kernel/src/power.rs; \
          src/kernel/src/init.rs; docs/certification/FINDINGS.md F-04 and F-08",
};

/// For an allocation the kernel makes while it comes up, before the first
/// program runs, when memory has already run out.
pub(crate) static BOOT_OUT_OF_MEMORY: Explanation = Explanation {
    code: "FX-0007",
    title: "memory ran out while the kernel was coming up",
    meaning: "After the kernel has booted, an allocation that fails is reported to whoever \
              asked -- a program gets `ENOMEM` or `NO_MEMORY`, and nothing stops \
              (docs/certification/MEMORY-AND-TIMING.md section 1). While it is still coming \
              up there is nobody to report to: the structure being built is one every later \
              step depends on, such as the root job, a processor's run queue or the table of \
              processors. Those allocations are fatal by design and marked `FATAL-ALLOC` \
              where they are made, and `tools/common/check/check-fallible-alloc.py` lists them; one \
              that stops through the standard library's allocation error handler before the \
              boot marker is reported here too. Reaching one means the machine does not have \
              the memory to run the kernel at all.",
    causes: &[
        "The machine has far less memory than the kernel needs, or firmware reported \
         almost none of it as usable.",
        "An earlier boot step leaked or reserved most of memory, so a later one found none.",
    ],
    see: "docs/certification/MEMORY-AND-TIMING.md; tools/common/check/check-fallible-alloc.py; \
          src/kernel/src/fallible.rs",
};

/// For the panic the standard library's allocation error handler raises,
/// recognised by `panic.rs`.
pub(crate) static ALLOCATION_ABORTED: Explanation = Explanation {
    code: "FX-0008",
    title: "an allocation that cannot report failure found the heap empty",
    meaning: "The kernel's allocator reports an empty heap by returning null. A caller in \
              the certified item turns that into an error it returns (`ENOMEM`, \
              `NO_MEMORY`) or, while the kernel is still coming up, a stop with its own \
              code (FX-0007). An ordinary `Box`, `Vec` or map cannot: the standard library \
              calls its allocation error handler instead, which panics, and this is that \
              panic. After the boot it is reached only from code that has not been \
              converted to report failure -- the uncertified load, whose allocations share \
              the heap -- when memory really has run out. The item has none: \
              `tools/common/check/check-fallible-alloc.py` counts every allocating call in it and fails \
              the build on one that does not report failure.",
    causes: &[
        "Memory ran out, and the allocation that found it so was one of the infallible \
         ones: the trace names it.",
        "A leak elsewhere consumed memory until an unrelated allocation failed; the frame \
         and heap counts in the boot report and /proc/meminfo say whether memory was \
         being lost.",
    ],
    see: "docs/certification/MEMORY-AND-TIMING.md; tools/common/check/check-fallible-alloc.py; \
          src/kernel/src/mm.rs",
};

/// For `check_allocation_failure` in `stages_check.rs`.
pub(crate) static STAGE9_ALLOCATION: Explanation = Explanation {
    code: "FX-0902",
    title: "an allocation failure was not survived",
    meaning: "`object::alloc_check::run` makes allocations fail on purpose and requires the \
              kernel to carry on (finding F-23). An `Arc` and a map insert must complete on \
              the reserve their section filled when the heap refuses inside it, and must \
              fail before they start when the reserve cannot be filled. Then one process \
              drives rounds of native calls that allocate while every `n`th fallible \
              allocation of its task fails: each call must succeed or answer `NO_MEMORY`, \
              some must do each, no frame may leak, and a round with nothing failing must \
              succeed whole. Last, a decommit of a mapped object with every allocation \
              failing must give back every page and leave no translation to one. A kernel \
              failing this has an allocation failure that corrupts state, leaks, or is \
              reported as something it is not.",
    causes: &[
        "A caller of `crate::fallible` turned an `AllocError` into a status other than \
         `NO_MEMORY`, or dropped it and carried on with state half changed.",
        "An error path freed nothing it had taken, so the frame window saw a leak.",
        "The reserve in `mm/reserve.rs` was not drawn on when the heap refused inside a \
         section, or a section went ahead with its reserve unfilled.",
        "Taking pages out of an object needed memory it could not get, and kept a page, \
         lost one, or left a space still translating it (`user/vmo.rs`'s chunked and \
         one-at-a-time fallbacks).",
    ],
    see: "src/kernel/src/object/alloc_check.rs; src/kernel/src/fallible.rs; src/kernel/src/mm/reserve.rs; \
          docs/certification/MEMORY-AND-TIMING.md",
};

/// For `check_native_refusals` in `stages_check.rs`.
pub(crate) static STAGE9_REFUSALS: Explanation = Explanation {
    code: "FX-0903",
    title: "a native call accepted what the ABI says it refuses",
    meaning: "`syscall::native_check::run` drives the native calls from a process of its \
              own with what each must refuse: a handle of another kind (`WRONG_TYPE`), one \
              without the right the call needs (`ACCESS_DENIED`), one named twice in a \
              message (`INVALID_ARGS`), a send whose cycle check would walk past its bound \
              (`TOO_BIG`), a copy through a VMO a device reads past the caches \
              (`BAD_STATE`), a clock asked for with an unknown option or zero hertz, and a \
              handle table with no room (`NO_HANDLES`) from a duplicate, a create and a \
              read. A refused call must leave what it was given where it was: the handle \
              named twice still open, the endpoint a refused send carried still the \
              sender's, a packet whose buffer faulted and a message a full table could not \
              take still queued for the next attempt.",
    causes: &[
        "A handler checked rights before the object's kind, or not at all, or turned a \
         handle table's refusal into another status.",
        "A refused call took a handle, a packet or a message it then had nowhere to put.",
        "The cycle check's walk bound in `object/channel.rs` changed without the check.",
    ],
    see: "src/kernel/src/syscall/native_check.rs; src/kernel/src/syscall/native.rs; \
          src/kernel/src/object/channel.rs; src/lib/kernel/objects",
};

/// For `check_services` in `stages_check.rs`.
pub(crate) static SERVICES: Explanation = Explanation {
    code: "FX-0904",
    title: "a service the item leans on failed its self-check",
    meaning: "`service_check::run` drives the small services the rest of the item leans on \
              in the shapes a passing boot does not: a registration list past its bound, a \
              device's claim while its driver lives and after it has gone, the lowest free \
              node number, a boot-mode word on a machine that keeps none, `devmgr` asked to \
              bind a device that does not exist and one that already has its driver, the \
              IOMMU gate's waits past their deadline and past their patience, and the \
              sentences the interrupt table, init, the copy layer and the DMA fault audit \
              report failures with.",
    causes: &[
        "A list took a registration past its bound, or gave them back out of order.",
        "A quiesce went ahead under a live driver, or a cancelled one was not told so.",
        "`devmgr` answered a request it should refuse, or answered with the wrong error.",
        "A failure's wording changed without its documentation.",
    ],
    see: "src/kernel/src/service_check.rs; src/kernel/src/hooks.rs; src/kernel/src/claim.rs; \
          src/kernel/src/discovery/devmgr.rs; src/kernel/src/iommu/check.rs; src/kernel/src/iommu/gate.rs",
};

/// For `check_quotas` in `stages_check.rs`.
pub(crate) static STAGE9_QUOTAS: Explanation = Explanation {
    code: "FX-0905",
    title: "a job quota did not bound what it claims to",
    meaning: "`object::quota_check::run` drives the quotas the Security Target claims as \
              FRU_RSA.1 through the paths a program's use takes: a fork loop in a job with a \
              task limit, a user space faulted in as a task of a job with a memory limit, \
              objects made in a job with an object limit, and spinning tasks in two jobs on \
              one processor. Each limit must refuse at exactly its value, anywhere above the \
              job, while a sibling job goes on; one task alone in its job must keep about \
              half a processor against eight in another; and every counter must read zero \
              and every quota slot be given back once the jobs are empty and gone. A kernel \
              failing this lets one job take memory, tasks, objects or processor time from \
              the rest (T.EXHAUST).",
    causes: &[
        "A charge site stopped charging, or a free path stopped uncharging: a frame, a task \
         or an object freed without `object::quota` hearing of it.",
        "A charge walked up the tree wrongly, so a limit above the job did not refuse, or a \
         refused charge left part of itself counted.",
        "A task's weight stopped following its job's share (`quota::effective`, \
         `CpuQueue::follow_group_share`), or its job's load was not kept as it came and went.",
        "A slot was not given back as its last hold went, or was freed while still held.",
    ],
    see: "src/kernel/src/object/quota.rs; src/kernel/src/object/quota_check.rs; src/kernel/src/mm.rs; \
          docs/certification/IMPLEMENTATION.md",
};

/// For `check_kernel_memory` in `stages_check.rs`, when `fs::kmem_check::run` fails.
pub(crate) static STAGE9_KMEM: Explanation = Explanation {
    code: "FX-0906",
    title: "the kernel heap a job drove through the Linux calls was not bounded by its job",
    meaning: "`fs::kmem_check::run` fills a job with a small memory limit with each kind of \
              object a program can make and keep through the Linux calls -- files in a \
              tmpfs, pipes, socket pairs, descriptors in flight, epoll registrations, \
              eventfds, regions of an address space, record locks, semaphore sets, and mount \
              namespaces copied from one of 64 mounts -- until one is refused. Each must be \
              refused with ENOMEM by the job's limit and not by something else, with the \
              job's memory never past its limit and its kernel-memory count above zero; a \
              sibling job must still make one; and once the objects are gone every byte \
              charged must have come back. A kernel failing this lets one job take the \
              kernel heap from the rest through a path its memory limit does not see \
              (certification finding F-37, T.EXHAUST).",
    causes: &[
        "An allocation site stopped carrying a `ferrix_kmem::Charge`, or made the object \
         before charging it, so a refusal left something behind.",
        "A charge was made to the wrong job: a buffer's growth to the running task where it \
         belongs to the object's maker, or the other way round.",
        "An object outlived what held it -- a cached dentry, a weak reference, a queue that \
         kept its room -- so its charge never came back.",
        "The kernel's account was not installed as the first job was made \
         (`quota::install_kernel_heap`), so nothing was charged at all.",
    ],
    see: "src/kernel/src/fs/kmem_check.rs; src/lib/kernel/kmem/src/lib.rs; src/kernel/src/object/quota.rs; \
          docs/certification/IMPLEMENTATION.md W-15",
};

/// For `object::process::Process::leave_speculation_domain`, when a member
/// leaves its domain where it may not wait for the grace period that leaving
/// takes.
pub(crate) static SPECULATION_DOMAIN_LEAVE_MAY_NOT_WAIT: Explanation = Explanation {
    code: "FX-0907",
    title: "a process left its speculation domain where it may not wait",
    meaning: "A member of a speculation domain that leaves it -- a move between jobs, a set-id \
              `execve`, a change of credentials, `PR_SET_DUMPABLE` -- makes every processor \
              whose last space was in the domain issue the predictor barrier before it goes \
              on, and waits for them in a grace period (`docs/OPAQUE-KERNEL.md` §9.3b, F1). \
              A grace period waits on every processor, so it may not be waited for holding a \
              spin lock another processor may spin on with interrupts masked, nor with \
              interrupts masked, which is a read-side section here: either can deadlock the \
              machine at random. The leave checks first and stops the machine instead, \
              naming the site.",
    causes: &[
        "A new caller of `syscall::attributes::update`, `Process::move_to` or \
         `Process::leave_speculation_domain` holds a `sync::SpinLock`, or masks interrupts, \
         when a member reaches it. Take the leave out from under the lock: decide under it, \
         leave after it.",
        "A path that used to reach `update` without a lock now holds one across it, such as \
         a change of credentials made inside `with_credentials`.",
    ],
    see: "src/kernel/src/object/process.rs leave_speculation_domain; src/kernel/src/smp.rs \
          synchronize; docs/OPAQUE-KERNEL.md §9.3b",
};

/// For `kmain` in `main.rs`, when `self_check` fails.
pub(crate) static STAGE1_HANDOFF: Explanation = Explanation {
    code: "FX-0101",
    title: "the loader's hand-off is not what the kernel needs",
    meaning: "Stage 1 checks what the UEFI loader handed over before anything depends on it. \
              The memory map must be non-empty, sorted, free of overlaps, report usable RAM, \
              and describe the loader's own allocations: the kernel image, the page tables \
              and the boot information. The kernel must run where the loader says it put \
              it, and a kernel built to move (KASLR) must have moved, or the loader must \
              say honestly why not. The kernel's first bytes must read the same through \
              the direct map as through the image mapping, walking the page tables must find \
              the image where the loader said it put it, and a framebuffer, where there is \
              one, must be mappable. The frame allocator hands out frames from this map and \
              everything after it reads physical memory through these mappings, so a wrong \
              hand-off would surface much later as corruption with no visible source.",
    causes: &[
        "The loader did not mark its own allocations in the memory map with their own \
         kinds, so the kernel would hand out the frames holding its own page tables.",
        "The loader mapped the direct map at a different physical origin from the one it \
         recorded, so one physical address reads different bytes through the two mappings.",
        "The loader passed on a memory map whose regions are out of order or overlap.",
        "Firmware reported a framebuffer at an address the early mapper could not map.",
        "The kernel was built `--mitigations on` and arrived as a fixed-address image \
         without its fixups, because a copy was stripped with `--strip-all` rather than \
         `--strip-debug`; or a loader reported a move it did not make.",
    ],
    see: "src/kernel/src/main.rs self_check and check_layout; src/kernel/src/early.rs; \
          docs/certification/SPECULATION.md section 6; docs/ROADMAP.md stage 1",
};

/// For `kmain` in `main.rs`, when `mm::init` fails.
pub(crate) static MEMORY_BRING_UP: Explanation = Explanation {
    code: "FX-0201",
    title: "the frame allocator could not be built",
    meaning: "`mm::init` builds the buddy allocator that every physical frame is handed out \
              by. It needs a per-frame array covering RAM from the lowest frame to the \
              highest the direct map reaches, and carves that array, which has to be \
              contiguous, from the front of the largest usable region inside the direct map. \
              Without the allocator \
              there are no page tables, no stacks and no heap, so nothing after this point \
              can run.",
    causes: &[
        "The memory map describes no usable region, or no RAM below the limit of the \
         direct map.",
        "No single usable region, clipped to the direct map, is large enough for the \
         per-frame array, which spans \
         from the lowest RAM frame to the highest and so grows with the gaps between \
         banks of RAM; the message gives the bytes it needed.",
    ],
    see: "src/kernel/src/mm.rs init; src/lib/kernel/frame; docs/ROADMAP.md stage 2",
};

/// For `kmain` in `main.rs`, when `vmap::init` fails.
pub(crate) static VMAP_ARENA_BRING_UP: Explanation = Explanation {
    code: "FX-0202",
    title: "the kernel address arena could not be created",
    meaning: "`vmap::init` creates the arena that hands out kernel virtual ranges, from which \
              every device window and every guard-paged kernel stack is later taken. It \
              fails only if `ferrix_vma` rejects the arena's bounds as misaligned or empty. \
              Nothing that needs a device register or a new stack can run without it.",
    causes: &[
        "`ARENA_BASE` and `ARENA_END`, or the `KERNEL_VMAP_BASE`, `KERNEL_VMAP_RESERVED` \
         and `KERNEL_VMAP_SIZE` constants they are built from, are not page aligned or \
         leave no room; this is a bug in those constants, not something a machine can cause.",
    ],
    see: "src/kernel/src/vmap.rs init; src/lib/proto/bootinfo; src/lib/kernel/vma",
};

/// For `check_allocators_and_traps` in `stages_check.rs`, when `mm::check::memory_check` fails.
pub(crate) static STAGE2_ALLOCATORS: Explanation = Explanation {
    code: "FX-0203",
    title: "the memory allocators failed their self-check",
    meaning: "`memory_check` exercises what every later subsystem assumes about memory without \
              checking. The frame allocator must hand out distinct, aligned blocks and get \
              back exactly the frames it gave; the heap must hold what a `Box`, a `Vec` and a \
              `BTreeMap` put in it, give everything back, and keep at most one slab page per \
              size class. The vmap arena must hand out distinct, zeroed, guard-paged ranges, \
              change their permissions in the page tables, map device windows at the right \
              offset within the page and refuse one over the kernel's own image, and free all \
              of it without leaking a frame, and a \
              kernel stack must be aligned, writable at both ends and guarded at both ends. \
              A broken property here would otherwise show up as corruption in whichever \
              subsystem first relied on it.",
    causes: &[
        "The frame allocator was given too little memory for the check: `no frame \
         available`, `no sixteen-frame block available` and `handed out nothing` mean it \
         ran out, not that it is wrong.",
        "A change to `src/lib/kernel/frame` or `src/lib/kernel/heap` broke their bookkeeping, so the free frame \
         count or the heap's balance does not return to where it started.",
        "A change to the page table code left a mapping behind on unmap, or made a \
         permission change the descriptors do not show.",
        "`vmap::map_device` mapped a range touching the kernel image (`a device window over \
         the kernel image was mapped`): the refusal `mm::overlaps_image` makes is gone, or \
         memory bring-up did not record where the image is.",
    ],
    see: "src/kernel/src/mm/check.rs memory_check; src/lib/kernel/frame; src/lib/kernel/heap; \
          src/lib/kernel/paging; src/kernel/src/vmap.rs; docs/ROADMAP.md stage 2",
};

/// For `kmain` in `main.rs`, when `finish_memory` fails.
pub(crate) static STAGE2_FINISH_MEMORY: Explanation = Explanation {
    code: "FX-0204",
    title: "the identity map, the W^X sweep or the reclaim failed",
    meaning: "`finish_memory` runs last in boot. It first requires the W^X sweep to see the \
              loader's identity map as a violation, which proves the sweep can find one; \
              then it drops the identity map and requires address zero to translate to \
              nothing, sweeps every live page table root for a mapping that is both writable \
              and executable, requires no mapping of the frames holding the kernel's text and \
              read-only data to be writable and the direct map to alias every one of them, \
              and gives the loader's memory and the ACPI tables back to the frame allocator, \
              requiring something to come back. A kernel still running with a writable and \
              executable mapping, with a writable alias of its own code, or still depending \
              on the identity map, is not the kernel the boot marker describes.",
    causes: &[
        "A mapping was installed writable and executable; the `w^x` line printed before the \
         panic gives its address and length.",
        "A loader mapped the direct map writable over the kernel's text or read-only data, \
         or something mapped those frames writable later; the `sealed` line gives the \
         mapping's address. Both loaders cut the direct map around the span \
         `ferrix_bootinfo::read_only_span` computes.",
        "The direct map does not alias all of the image's text, so the sealed sweep checked \
         nothing; the `sealed` line gives where the alias should have been.",
        "The architecture's `drop_identity_map` did not remove the lower half, so address \
         zero still translates.",
        "On AArch64 and ARMv7-A the identity map is a root of its own, and \
         `arch::identity_root` did not return it, so the sweep could not see the violation \
         it is required to see.",
        "The memory map marks no loader or ACPI-reclaimable region inside the range the \
         per-frame array covers, so nothing was reclaimed.",
    ],
    see: "src/kernel/src/main.rs finish_memory; src/kernel/src/mm/check.rs check_w_xor_x and \
          check_sealed_image; src/kernel/src/mm.rs reclaim_boot_memory; docs/ROADMAP.md stage 2",
};

/// For `kmain` in `main.rs`, when `trap_check` fails.
pub(crate) static STAGE3_TRAPS: Explanation = Explanation {
    code: "FX-0301",
    title: "a deliberate trap did not come back correctly",
    meaning: "`trap_check` raises two breakpoints and requires both to reach the handler and \
              return to the next instruction with a canary register intact, which proves the \
              entry path restores what it saved. It then writes to three unmapped pages in \
              the on-demand window, requiring the fault handler to map each faulting page, \
              the write to retry and read back, the rest of each page to be zero, and the \
              frames consumed to be one per page plus at most one per table level. Every \
              fault the kernel resolves rather than reports goes through this path, which is \
              how demand paging works from stage 6. On x86-64, `check_exception_entry` runs \
              after stage 7's checks and requires three more traps to come back: an NMI sent \
              to the processor itself, hardware breakpoints on the `SYSCALL` trampoline's \
              first instruction and its `sysretq` while a program makes calls, and a \
              breakpoint on code the `#DB` handler runs, which must not fire inside it.",
    causes: &[
        "The architecture's trap entry or exit does not save or restore a register the frame \
         carries, or `arch::advance_past_breakpoint` returns to the wrong instruction.",
        "On x86-64, a paranoid entry that decides `swapgs` from the saved CS rather than from \
         `GS_BASE`, so a breakpoint in the trampoline runs its handler with the program's GS \
         (the message says so), or one that does not clear `DR7`, so the nesting check \
         stops with FX-9006.",
        "On x86-64 under an accelerator or emulator that does not deliver an NMI sent through \
         the local APIC, or does not implement debug registers, the check says the NMI never \
         arrived or a breakpoint did not fire.",
        "Something mapped an address inside `mm::DEMAND_WINDOW` before the check ran, so the \
         window was already mapped.",
        "Mapping a page in a region that already had its tables cost more than one frame, \
         which points at the page table code allocating a table it already had.",
    ],
    see: "src/kernel/src/main.rs trap_check; src/kernel/src/trap.rs handle_page_fault; \
          docs/ROADMAP.md stage 3",
};

/// For `kmain` in `main.rs`, when `timer_check` fails.
pub(crate) static STAGE3_TIMER: Explanation = Explanation {
    code: "FX-0302",
    title: "the timer interrupt did not arrive as programmed",
    meaning: "`timer_check` proves time works before anything is built on it. A one-shot \
              timer must fire exactly once; then a periodic timer must deliver its \
              ticks, every interrupt must reach a registered handler, and the rate measured \
              against the counter must be within 25 percent of the 1000 Hz asked for. A \
              scheduler slice, a sleep and every later timeout is this rate multiplied by \
              something, so a timer that is silent, repeats, or runs at the wrong rate makes \
              all of them wrong.",
    causes: &[
        "The timer's interrupt is not reaching the processor: firmware named the wrong \
         interrupt for the virtual timer (the GTDT on AArch64, the device tree on ARMv7-A), \
         or the controller was not left delivering it.",
        "On AArch64 and ARMv7-A the timer interrupt is level triggered, and a handler that \
         acknowledges it without disarming the timer is re-entered at once; this check is \
         there to catch that, though it can equally show as a hang with nothing printed.",
        "On x86-64 the local APIC timer was calibrated against a counter that was itself \
         wrong, so it was programmed from a wrong frequency; the line printed before the \
         panic gives the measured rate, and a rate that is too high points here.",
        "The host running an emulator was too loaded to deliver the ticks at a kilohertz, \
         so the measured rate is low.",
        "An interrupt arrived on a line nothing registered for: a device firmware left \
         enabled, or a controller programmed to deliver somewhere unexpected.",
    ],
    see: "src/kernel/src/main.rs timer_check; src/kernel/src/timer.rs; docs/ROADMAP.md stage 3",
};

/// For `kmain` in `main.rs`, when `arch::init_interrupts` fails.
pub(crate) static INTERRUPT_BRING_UP: Explanation = Explanation {
    code: "FX-0303",
    title: "the interrupt controller or the clocks could not be brought up",
    meaning: "`arch::init_interrupts` brings up, on the boot processor, the interrupt \
              controller, the counter and the timer that firmware describes. On x86-64 that \
              is the local APIC from the MADT, a counter from the HPET or from the TSC \
              calibrated against the PIT, the local APIC timer calibrated against that \
              counter, and every I/O APIC masked; on AArch64 a GICv2 from the MADT and the \
              generic timer; on ARMv7-A the same two from the device tree. Without them \
              nothing arrives that the kernel did not cause itself, so there is no timer, no \
              preemption and no second processor.",
    causes: &[
        "The interrupt controller is a GICv3, which the kernel refuses rather than drive \
         with GICv2 register layouts; QEMU's `virt` machine gives a GICv2 unless asked \
         for another.",
        "Firmware does not describe what the kernel needs: no readable ACPI tables or MADT \
         on x86-64 and AArch64, or no interrupt controller, distributor or CPU interface in \
         the device tree on ARMv7-A.",
        "On x86-64 there is no HPET and the PIT did not answer, so there is no counter; or \
         the HPET is described but its counter does not advance, or reports a period \
         outside the specification; or its counter is 32 bits wide and the TSC, \
         which is measured against it instead, did not count.",
        "Firmware left the generic timer's frequency register, `CNTFRQ_EL0` or `CNTFRQ`, \
         at zero.",
        "The kernel address arena refused a device window for the controller, the HPET or \
         an I/O APIC, for want of address space or of frames for page tables.",
    ],
    see: "src/kernel/src/arch/x86_64/mod.rs init_interrupts; src/kernel/src/arch/x86_64/apic.rs; \
          src/kernel/src/arch/x86_64/clock.rs; src/kernel/src/arch/aarch64/gic.rs; \
          src/kernel/src/arch/armv7a/mod.rs init_interrupts; docs/ROADMAP.md stage 3",
};

/// For `kmain` in `main.rs`, when `timer::init` fails.
pub(crate) static TIMER_REGISTRATION: Explanation = Explanation {
    code: "FX-0304",
    title: "the timer's interrupt could not be registered",
    meaning: "`timer::init` attaches the tick handler to the interrupt number \
              `arch::timer_irq` reports. Registration fails only if that number is past the \
              end of the handler table or already has a handler. Without the handler every \
              tick is counted as unclaimed, and the timer self-check that follows cannot pass.",
    causes: &[
        "Code added to `kmain` before this point registered a handler on the same number, \
         or `timer::init` was called twice; nothing else registers a handler this early.",
    ],
    see: "src/kernel/src/timer.rs init; src/kernel/src/irq.rs register",
};

/// For `kmain` in `main.rs`, when `console::input::check` or
/// `console::input::init` fails.
pub(crate) static CONSOLE_INPUT: Explanation = Explanation {
    code: "FX-0305",
    title: "console input could not be set up",
    meaning: "Before interrupts are first enabled, `console::input::check` fills the ring the \
              console's receive interrupt will fill, and requires every byte back in order, the \
              bytes past its capacity counted rather than kept, and a wait for input to return \
              at once while input is there. Then `console::input::init` registers the port's \
              receive interrupt, if the architecture names one. Nothing else touches the ring \
              while the check runs, so a check failure is the ring's own logic; a registration \
              failure means the interrupt number already has a handler.",
    causes: &[
        "A change to the ring's arithmetic in `console/input.rs`: the wrap-around, the held \
         count `has_input` reads without the lock, or the overrun count.",
        "The architecture named a receive interrupt for the console port that the timer or \
         another early handler already holds, as a device tree giving two devices one \
         interrupt would.",
    ],
    see: "src/kernel/src/console/input.rs check; src/kernel/src/irq.rs register",
};

/// For `kmain` in `main.rs`, when `console::output::check` fails.
pub(crate) static CONSOLE_OUTPUT: Explanation = Explanation {
    code: "FX-0504",
    title: "console output did not go out by interrupt",
    meaning: "Once the scheduler runs, `console::output::check` writes one line to the console \
              from a task, as a program's `write(2)` does, and requires the writer to have left \
              the port's transmit interrupt everything past the first burst it put into the \
              port itself, the ring to have emptied within two seconds, and no writer to have \
              found the port stalled and polled the ring out. A failure means program output \
              is again being polled out of the port with interrupts masked, or is waiting on \
              an interrupt that does not come.",
    causes: &[
        "The transmit interrupt is never raised or never reaches the handler: the port's \
         enable bit in `arch::console::transmit_interrupt` is wrong for the port (on an \
         STM32 USART it depends on whether firmware enabled the FIFO), or the interrupt the \
         receive side installed is not the one the port raises.",
        "`console::emit` stopped choosing `output::write_waiting` for a task that may sleep, \
         so a task's write is polled out under the port's lock again.",
        "The check ran from a context that cannot sleep, which `sched::may_block` reports; \
         `kmain` calls it straight after the scheduler is started.",
    ],
    see: "src/kernel/src/console/output.rs check; src/kernel/src/console.rs emit",
};

/// For `kmain` in `main.rs`, when `logctl::check::run` fails.
pub(crate) static LOG_CONTROL: Explanation = Explanation {
    code: "FX-1008",
    title: "the log core did not serve the kernel log to a driver",
    meaning: "A driver whose device may carry the kernel log off the machine -- the Pixel 7's \
              USB serial port -- reads it over a log control channel (`src/lib/proto/logctl`). The check \
              claims the log as `log_control_create` does and plays the driver: a READ must be \
              answered with DATA holding the log's oldest bytes, the next READ with the bytes \
              after them, a second reader must be refused while the first holds the log, a \
              driver that sends DATA must be refused and its claim ended, and a claim must end \
              when its channel closes. A failure means the driver on the phone would get \
              nothing, get the log out of order, or keep the log from the next driver after it \
              went.",
    causes: &[
        "The log core's task does not answer: it waits for the console to wake it, which the \
         console never does, instead of looking again every `logctl::POLL_NANOS`.",
        "The reader's cursor does not start at the log's beginning, or `Ring::read` does not \
         move it past what it copied.",
        "The claim is not let go when the task ends, so `CLAIMED` stays set and every later \
         `log_control_create` answers `ALREADY_BOUND`.",
    ],
    see: "src/kernel/src/interfaces/logctl/check.rs; src/kernel/src/interfaces/logctl/mod.rs; src/lib/proto/logctl",
};

/// For `kmain` in `main.rs`, when `console::log_check::run` fails.
pub(crate) static CONSOLE_LOG: Explanation = Explanation {
    code: "FX-0505",
    title: "the kernel log lost track of what it keeps",
    meaning: "The kernel log is a ring of every byte the console sends, which `syslog(2)` and a \
              log control channel read. Once the scheduler runs, `console::log_check` drives \
              small rings of its own past their length and requires the last bytes kept in \
              order, a reader that fell behind to be told exactly how many it lost, a partial \
              read to resume where it stopped, the unread count to match, and two writers \
              racing on two processors to lose nothing of the count while a reader keeps up. \
              Then it requires a task's write and a line written as a failure report is to be \
              in the kernel log, and a line sent unlogged not to be. A failure means a reader \
              of the log can be handed stale bytes without being told, or the log keeps what \
              was meant to stay on the port: the kernel's layout.",
    causes: &[
        "`Ring::read` stopped moving a cursor behind the oldest byte forward, or stopped \
         counting what it skipped or what a writer replaced during the copy.",
        "`Ring::record_byte` claims its place before storing the byte, or with a plain store \
         rather than a compare-and-exchange, so racing writers share a place.",
        "The console stopped recording on one of its paths: `Writer::bytes`, `Unlocked`, or \
         `output::try_queue` for a task's write; or `Kind::Unlogged` is recorded after all.",
    ],
    see: "src/kernel/src/console/log_check.rs; src/kernel/src/console/log.rs; src/kernel/src/console.rs",
};

/// For `kmain` in `main.rs`, when `random::check` fails.
pub(crate) static RANDOM_GENERATOR: Explanation = Explanation {
    code: "FX-0306",
    title: "the random number generator repeated itself",
    meaning: "`random::check` reads the generator twice, after `random::init` seeded it from \
              firmware, the CPU and timer jitter, and requires the two reads to differ and \
              neither to be all zeros. `getrandom`, `/dev/urandom` and every program's \
              `AT_RANDOM` read this generator: one that repeats hands every program the same \
              stack canary and every TLS session the same key.",
    causes: &[
        "A change to `src/lib/kernel/crng` that stopped `fill` from replacing the key, so each read \
         starts from the same block; its host tests check the construction.",
        "A change to `random::fill` that no longer takes the lock around the generator, so \
         two reads copied one state.",
    ],
    see: "src/kernel/src/random.rs; src/lib/kernel/crng",
};

/// For `check_speculation` in `stages_check.rs`, when `arch::check_speculation` fails.
pub(crate) static SPECULATION_DEFENCES: Explanation = Explanation {
    code: "FX-0307",
    title: "the side-channel defences did not hold",
    meaning: "`arch::check_speculation` runs after stage 7's programs and requires the clamp \
              that bounds a program-chosen index to return an index inside its bound unchanged \
              and one outside it as zero (unchanged in a build made with `--mitigations off`), \
              every running processor to have recorded what it applied, nothing written to a \
              processor -- `IA32_SPEC_CTRL`, `EFER`, `SCTLR_EL1` -- to have failed to read back, \
              a switch barrier to have been issued where any processor's plan has one, since \
              programs in different address spaces have run by then, and none by a processor \
              whose plan has none. The defences stand between a \
              program and a speculative read of another's memory or the kernel's.",
    causes: &[
        "A hypervisor that advertises a speculation control in `CPUID` and drops writes to it, \
         which is what a read-back failure on one processor usually is.",
        "A secondary start path that stopped calling the architecture's `apply_this_cpu`, so \
         a processor runs programs without the boot processor's defences.",
        "On `AArch64`, a switch barrier issued from another processor's decision: each core \
         decides for itself, and the barrier is read from the switching core's own bit.",
        "A change to `install_user_root` that stopped calling `speculation::entered_space`, \
         which is where every switch barrier is issued.",
        "A change to an architecture's `clamp_index` that lets an out-of-bound index through.",
    ],
    see: "src/kernel/src/arch/speculation.rs; src/kernel/src/arch/speculation_check.rs; \
          docs/certification/SPECULATION.md",
};

/// For `check_timer_and_start_clocks` in `main.rs`, when `arch::check_machine`
/// fails.
pub(crate) static STAGE3_MACHINE: Explanation = Explanation {
    code: "FX-0308",
    title: "the architecture decoded or masked something wrongly",
    meaning: "`arch::check_machine` drives what the architecture decides on its own from \
              values the machine hands it with inputs built for the purpose, since an \
              ordinary boot raises only some of them: trap frames carrying every exception \
              class or vector entry and fault status the decoder names, each required to \
              become the trap and the signal Linux raises; the descriptions a console or \
              interrupt controller can be given; and one idle shared line and one idle \
              private line masked and let through at the interrupt controller, read back \
              from its registers. A program that faults, and a driver that holds its \
              interrupt, rely on exactly these answers. On the Arm architectures only.",
    causes: &[
        "A change to the architecture's trap decoding that moved an exception class or \
         fault status to another trap or signal; the message says which kind of case.",
        "An interrupt controller whose enable registers do not read back what was written, \
         or a driver that writes the wrong register or bit for a line.",
        "A change to how the console port or GIC version is chosen from the machine's \
         description.",
    ],
    see: "src/kernel/src/arch/aarch64/check.rs; src/kernel/src/arch/armv7a/check.rs",
};

/// For `start_audit` in `main.rs`, when `audit::check::run` fails.
pub(crate) static AUDIT_STORE: Explanation = Explanation {
    code: "FX-0309",
    title: "the audit record lost or misplaced what it keeps",
    meaning: "The audit store keeps a record of each security decision the kernel makes, in \
              two rings of static storage (`docs/certification/AUDIT.md`). Right after the \
              random generator, bring-up starts it with the boot's audit id and records the \
              boot's configuration, and `audit::check` drives small stores of its own: the \
              start-up record first and carrying the whole id, a ring written past its \
              length numbering its last records without a gap and telling a reader exactly \
              how many it lost, a partial read resuming, refusals kept apart from every other \
              class, one job's refusals past the per-second limit counted in a single \
              suppressed record once its second ends while another job's are kept, and a \
              fairness window closed to make room writing its count out. Then it requires \
              the kernel's store to begin with its start-up record and to hold the \
              configuration bring-up read. A failure means a reader of the record could be \
              handed a gap it is not told of, a flood of refusals could push out what \
              matters, or the boot's configuration -- a boot that skipped its checks among \
              it -- goes unrecorded.",
    causes: &[
        "`Ring::push` stopped numbering a record before keeping it, or `Ring::read` stopped \
         moving a reader past what was overwritten and counting it.",
        "`Store::record_at` sends a class to the wrong ring, or `Refusals::keep` keeps a \
         refusal past `PER_JOB_PER_SECOND` in its window.",
        "`Refusals::close` frees a window without writing its suppressed count, or \
         `close_ended` stopped closing windows whose second has ended.",
        "Bring-up's `start_audit` stopped recording one of the configuration items, or \
         `Store::start` stopped writing the whole id into the start-up record.",
    ],
    see: "src/kernel/src/audit/check.rs; src/kernel/src/audit.rs; docs/certification/AUDIT.md",
};

/// For `bring_up_processors` in `main.rs`, when `smp::discover` fails.
pub(crate) static PROCESSOR_DISCOVERY: Explanation = Explanation {
    code: "FX-0401",
    title: "the processor list could not be read",
    meaning: "`smp::discover` finds which processors firmware says can be started, and which \
              one is running this code, before any other is started. The list comes from the \
              MADT on x86-64 and AArch64 and from the device tree's `/cpus` on ARMv7-A, and \
              must be non-empty, have no two processors with one identifier, and include the \
              boot processor. The boot processor's per-CPU record is then installed and must \
              lead back to itself and name the hardware reading it. Every later step \
              addresses processors by these identifiers, and a wrong one sends a start \
              request or an interrupt to the wrong processor.",
    causes: &[
        "The boot processor's identifier was read in a form the table does not store, so it \
         is not in the list; on AArch64 and ARMv7-A only the affinity fields of `MPIDR` \
         count.",
        "The machine has no readable ACPI tables or MADT (x86-64, AArch64), or no device \
         tree (ARMv7-A).",
        "Firmware marks every processor disabled, or lists two with the same identifier.",
        "The per-CPU register (`GS` base on x86-64, `TPIDR_EL1` on AArch64, `TPIDRPRW` on \
         ARMv7-A) did not keep the address written to it.",
    ],
    see: "src/kernel/src/smp.rs discover; src/kernel/src/arch/x86_64/smp.rs describe_cpus; \
          src/kernel/src/arch/aarch64/smp.rs describe_cpus; \
          src/kernel/src/arch/armv7a/smp.rs describe_cpus; docs/ROADMAP.md stage 4",
};

/// For `bring_up_processors` in `main.rs`, when `smp::start_secondaries` fails.
pub(crate) static SECONDARY_START: Explanation = Explanation {
    code: "FX-0402",
    title: "a secondary processor could not be started",
    meaning: "`smp::start_secondaries` starts every processor but the boot one, one at a \
              time, and waits up to a second for each to mark itself online before starting \
              the next. x86-64 starts one with INIT and two start-up IPIs into a real-mode \
              trampoline below 1 MiB; AArch64 and ARMv7-A ask PSCI's `CPU_ON` to start it at \
              an identity-mapped entry sequence. A processor that was started and never \
              reported in may still be about to read the shared start block, so the kernel \
              stops rather than go on without it or free the block under it.",
    causes: &[
        "A processor was started and never reported in: it faulted or hung in its entry \
         sequence before reaching `smp::secondary_main`. On a new board this is the usual \
         first failure, and ARMv7-A's `nosmp` boot argument keeps to one core to rule \
         everything else out.",
        "PSCI refused `CPU_ON`: firmware does not provide PSCI (the FADT on AArch64, the \
         device tree on ARMv7-A), the core is already on, or the entry address was \
         rejected; the message says which.",
        "On x86-64 there is no free frame below 1 MiB for the trampoline or its root table, \
         or a processor's APIC ID is above 255, which needs x2APIC mode the kernel does not \
         have.",
        "On x86-64 the local APIC never accepted the INIT or start-up IPI.",
        "On ARMv7-A the entry sequence, a stack, a per-CPU record or the start block lies \
         above 4 GiB and cannot be passed in a 32-bit register.",
    ],
    see: "src/kernel/src/smp.rs start_secondaries; src/kernel/src/arch/x86_64/smp.rs CpuStarter; \
          src/kernel/src/arch/aarch64/smp.rs CpuStarter; src/kernel/src/arch/armv7a/smp.rs CpuStarter; \
          docs/ROADMAP.md stage 4",
};

/// For `secondary_start` in `arch/x86_64/smp.rs`, when `gdt::init_secondary` fails.
#[allow(
    dead_code,
    reason = "only x86-64 builds a GDT for each secondary processor"
)]
pub(crate) static SECONDARY_GDT: Explanation = Explanation {
    code: "FX-0403",
    title: "a secondary processor could not build its GDT",
    meaning: "On x86-64 every processor needs a GDT and TSS of its own, because loading a TSS \
              marks its descriptor busy and so one cannot be shared, and the TSS carries the \
              stack a double fault runs on. `gdt::init_secondary` runs on the new processor \
              before it enables interrupts and takes that stack from the vmap arena. Without \
              it a double fault on this processor would have no stack to run on, which is a \
              triple fault and a silent reset.",
    causes: &[
        "The vmap arena had no address space or frames left for another guard-paged stack; \
         every processor started also takes a kernel stack from the same arena.",
    ],
    see: "src/kernel/src/arch/x86_64/gdt.rs init_secondary; src/kernel/src/arch/x86_64/smp.rs \
          secondary_start; docs/ROADMAP.md stage 4",
};

/// For `secondary_main` in `smp.rs`, when no record has the address handed over.
pub(crate) static SECONDARY_NO_RECORD: Explanation = Explanation {
    code: "FX-0404",
    title: "a secondary processor arrived with no record of its own",
    meaning: "Each secondary processor is handed the address of its per-CPU record through \
              the architecture's start sequence, and `secondary_main` looks that address up \
              among the records rather than trusting it. With no record the processor has no \
              identity: it cannot install its per-CPU register or say it is online. This is \
              raised on the secondary processor itself.",
    causes: &[
        "The block the start sequence loads its argument from does not match what the \
         assembly reads: `Header` on x86-64, `StartBlock` on AArch64 and ARMv7-A.",
        "On AArch64 or ARMv7-A the start block written by the boot core had not reached \
         memory when the new core, whose caches are off, read it; `clean_to_poc` after the \
         write is what prevents that.",
    ],
    see: "src/kernel/src/smp.rs secondary_main; src/kernel/src/arch/x86_64/smp.rs; \
          src/kernel/src/arch/aarch64/smp.rs; src/kernel/src/arch/armv7a/smp.rs",
};

/// For `secondary_main` in `smp.rs`, when `check_this_cpu` fails.
pub(crate) static SECONDARY_RECORD_MISMATCH: Explanation = Explanation {
    code: "FX-0405",
    title: "a secondary processor's record does not describe it",
    meaning: "Having found its record, a secondary processor installs it in its per-CPU \
              register and checks it: the register must lead back to that record, the record \
              must hold its own address and sit at its logical number's index, and the \
              hardware identifier in it must be the one this processor's hardware reports. A \
              register pointing at another record makes two processors share per-CPU state, \
              and a record naming other hardware sends this processor's interrupts elsewhere.",
    causes: &[
        "The identifier firmware listed for this processor is not the one the processor \
         reads from its own hardware, so the start request reached a different core from \
         the one the record describes.",
        "The per-CPU register (`GS` base on x86-64, `TPIDR_EL1` on AArch64, `TPIDRPRW` on \
         ARMv7-A) did not keep the address written to it.",
    ],
    see: "src/kernel/src/smp.rs check_this_cpu; src/kernel/src/smp.rs secondary_main",
};

/// For `bring_up_processors` in `main.rs`, when fewer processors are online than described.
pub(crate) static PROCESSORS_MISSING: Explanation = Explanation {
    code: "FX-0406",
    title: "not every processor firmware described came online",
    meaning: "Once `smp::start_secondaries` returns, every processor in the list must have \
              marked itself online. That function waits for each processor it starts and \
              fails if one does not arrive, and nothing ever marks a processor offline, so \
              this checks that promise rather than a separate step. Stage 4's checks hand \
              work to every online processor and expect all of them.",
    causes: &[
        "`start_secondaries` returned success without waiting for every processor after the \
         boot one, which its loop is written to prevent.",
    ],
    see: "src/kernel/src/main.rs bring_up_processors; src/kernel/src/smp.rs start_secondaries",
};

/// For `bring_up_processors` in `main.rs`, when `smp::check::run` fails.
pub(crate) static STAGE4_SMP: Explanation = Explanation {
    code: "FX-0407",
    title: "the processors failed to work together",
    meaning: "`smp::check::run` is stage 4's exit criterion, run on every processor at once. \
              Every processor must run a hundred rounds of work, each secondary woken for it \
              by an inter-processor interrupt; a page moved to another frame twenty times \
              must be read at its new frame by every processor; a hundred grace periods must \
              never end while a reader still holds what they retire; and a counter \
              incremented 25,000 times by each processor under one lock must come out exact, \
              with the processors' shares overlapping in time. The scheduler and everything \
              after it rely on each of these whenever more than one processor runs.",
    causes: &[
        "A TLB flush does not drop every entry it should on every processor, so one reads a \
         moved page through a stale translation; an emulated MMU keeps no TLB, so this shows \
         under a hardware accelerator and not under tcg.",
        "A secondary processor takes no inter-processor interrupts, because its own \
         interrupt controller interface was not brought up or its copy of the interrupt is \
         not enabled.",
        "`synchronize` returned before every processor had taken its interrupt, so a grace \
         period ended early and a reader found the object it poisoned.",
        "A processor did not finish its share of the work within thirty seconds, because it \
         missed the interrupt meant to wake it or sat with interrupts masked.",
        "The spin lock let two processors in at once, or the processors never ran their \
         increments at the same time in any of five rounds, so the lock was never contended. \
         A loaded host can run the processors one after another for a round; each round that \
         did not overlap is printed with its shares.",
        "Run again once the scheduler is up: a task that moved to another processor while \
         waiting for a shootdown recorded its flushes for the processor it left, because the \
         wait used a per-CPU record read before the move rather than the processor it was on.",
    ],
    see: "src/kernel/src/smp/check.rs run; src/kernel/src/smp/check.rs migrating_shootdown; \
          src/kernel/src/smp.rs run_everywhere; docs/ROADMAP.md stage 4",
};

/// For `start_scheduler` in `main.rs`, when `sched::init` fails.
pub(crate) static SCHEDULER_BRING_UP: Explanation = Explanation {
    code: "FX-0501",
    title: "the scheduler could not be started",
    meaning: "`sched::init` builds one Throughput scheduling domain over every processor, gives \
              each processor a run queue, makes the boot context the boot processor's first \
              task with an idle task beside it, and wakes the secondaries so each joins the \
              scheduler from its idle loop. It waits up to five seconds for all of them, \
              re-sending the wake-up interrupt every millisecond. Nothing can be spawned, \
              slept or preempted until every processor is running tasks.",
    causes: &[
        "A secondary processor never reached `sched::enter_idle`: it missed the wake-up \
         interrupt, or was interrupted so often it never ran the instructions between waking \
         and joining.",
        "The machine has more processors than a scheduling domain's `CpuSet` can hold.",
        "The vmap arena had no stack left for the boot processor's idle task.",
        "`ferrix_sched` rejected the scheduler's slice constant when a run queue was built.",
    ],
    see: "src/kernel/src/sched/mod.rs init; src/kernel/src/sched/mod.rs wait_for_processors; \
          docs/ROADMAP.md stage 5",
};

/// For `sched::schedule`, when asked to switch with the preemption count
/// raised.
pub(crate) static SCHEDULE_WITH_PREEMPTION_HELD: Explanation = Explanation {
    code: "FX-0503",
    title: "a task tried to block while holding a lock that disables preemption",
    meaning: "The kernel's `sync::SpinLock` keeps its holder on its processor by raising a \
              per-processor count that the scheduler will not switch a task out under. The \
              count belongs to the processor: a task that blocked, yielded or slept with it \
              raised would leave that processor unable to preempt whatever ran next, and \
              would lower the count on whichever processor it woke on. So `schedule` stops \
              the machine instead, and the message says how many such locks were held.",
    causes: &[
        "A path takes a `sync::SpinLock` and, with the guard still alive, calls something \
         that blocks: a wait queue, `sleep_for`, `yield_now`, a pipe or channel wait. The \
         guard has to be dropped before the wait and the data re-read after it.",
        "A guard was stored somewhere that outlives the critical section -- in a struct, or \
         returned from a function -- and dropped much later.",
        "The count was raised on one processor and lowered on another. Before the read of \
         the processor's number and the increment were done under masked interrupts, a \
         preemption between them let the task be stolen, the increment landed on the \
         processor it had left, and the next task to decide there stopped for a lock it never \
         held; the message names the file and line that last raised the count. An enable \
         that finds nothing to lower now stops the machine itself, naming the same site.",
    ],
    see: "src/kernel/src/sync.rs; src/kernel/src/sched/mod.rs PREEMPT_OFF; src/lib/kernel/sync/src/lib.rs \
          PreemptSpinLock",
};

/// For `start_scheduler` in `main.rs`, when `sched::run_checks` fails.
pub(crate) static STAGE5_SCHEDULER: Explanation = Explanation {
    code: "FX-0502",
    title: "the scheduler failed its self-check",
    meaning: "`sched::run_checks` is stage 5's exit criterion and the checks added after it. A \
              task must run, be switched to and be reaped; a sleep must end neither early nor \
              twenty times late; a thousand tasks spawned on one processor must finish, run \
              on more than one processor and give every stack back; spinners of different \
              weights must each get their weighted share within EEVDF's bound; and placement, \
              affinity, load tracking, balancing and slice scaling must each do what they \
              claim, before every run queue's own bookkeeping is checked. Every later task, a \
              user process included, is scheduled by the code this measures.",
    causes: &[
        "A task became runnable without its processor being woken or its timer re-armed, so \
         it never ran and a wait gave up after twenty seconds (the `never started`, `never \
         finished` and `never stopped` messages).",
        "An exited task's stack was never given back to the vmap arena; the arena's count and \
         the expected count are printed before the panic.",
        "The emulator's host descheduled virtual processors for long stretches, delaying a \
         sleep or distorting measured service; the fairness bound grows with the overruns a \
         processor served inside the window, but a sleep twenty times late fails regardless.",
        "Stealing, placement or balancing moved nothing between processors, or a task ran \
         on a processor its affinity excluded.",
        "A processor running a task read as idle: `choose_next` does not clear `IDLE` for a \
         switch away from the idle task, so an idle task preempted on its way out of a halt \
         leaves its processor marked idle while another task runs.",
    ],
    see: "src/kernel/src/sched/check.rs run; src/kernel/src/sched/mod.rs; docs/ROADMAP.md stage 5",
};

/// For `check_user_memory` in `stages_check.rs`, when `user::check::run` fails.
pub(crate) static STAGE6_USER_MEMORY: Explanation = Explanation {
    code: "FX-0601",
    title: "the memory a process is built from failed its self-check",
    meaning: "`user::check::run` tests the objects stage 6 builds processes from. A VMO must cost \
              nothing until a page is committed, commit each page once and zeroed, and give \
              every frame back when dropped; an address space must refuse mappings outside the \
              user half, resolve faults inside its regions and refuse the rest; the processor, \
              with the space installed, must write through a user address into the frame the \
              tables name; fork must share pages and a write must copy exactly one; and two \
              tasks in two address spaces must each read their own page at one address. \
              Across all of it the free frame count must end where it started.",
    causes: &[
        "A frame reference taken by a VMO's page list or by fork's sharing was not released, \
         so the free frame count did not come back (the `leaked` messages).",
        "`arch::install_user_root` wrote the root without re-enabling the lower-half \
         translation regime, which on AArch64 and ARMv7-A passes the first installation and \
         fails the second.",
        "The scheduler did not swap address spaces when it switched tasks, so a task read \
         another task's page.",
        "The copy-on-write path let a write through to a frame still shared with the other \
         address space.",
    ],
    see: "src/kernel/src/user/check.rs run; src/kernel/src/user/space.rs; src/kernel/src/user/vmo.rs; \
          docs/ROADMAP.md stage 6",
};

/// For `check_reverse_map` in `stages_check.rs`, when `user::rmap_check::run` fails.
pub(crate) static STAGE6_REVERSE_MAP: Explanation = Explanation {
    code: "FX-0602",
    title: "a page taken from a mapped object stayed reachable, or was not taken as it should be",
    meaning: "`user::rmap_check::run` maps one shared anonymous object in two processes pinned to \
              two processors and has them touch its page from user mode. The kernel then \
              decommits the page, replaces it, and holds it for a device. After a decommit or a \
              replace both address spaces' tables must have lost the translation, and the \
              process on the other processor must fault in the new page: it must never read the \
              poison the kernel wrote into the frame it gave back, and its write must never land \
              there. A held page must keep its frame and its mappings through both. Across the \
              measured run every frame must come back.",
    causes: &[
        "A VMO change released a frame without asking every address space attached to the \
         object to forget its pages first (`Vmo::retire`, `AddressSpace::forget_pages`), or \
         an address space inserted an object id without attaching to it.",
        "The scoped shootdown missed a processor holding a stale entry: a processor joined an \
         address space's set after loading its root, left before the root write that flushed \
         it, or answered a shootdown for a processor it was no longer running on.",
        "The page-scoped invalidation is wrong for the architecture: `invlpg` not reaching the \
         entry, or `TLBI VAAE1IS` / `TLBIMVAAIS` given the wrong page number.",
        "A decommit, replace or move touched a held page, or backed off after already \
         invalidating it.",
    ],
    see: "src/kernel/src/user/rmap_check.rs run; src/kernel/src/user/vmo.rs retire; \
          src/kernel/src/user/space.rs forget_pages; src/kernel/src/smp.rs flush_tlb_pages; \
          docs/ROADMAP.md stage 6",
};

/// For `check_devices` in `main.rs`, when the PCI finder, `pci::Enumeration`,
/// fails.
pub(crate) static STAGE10_PCI: Explanation = Explanation {
    code: "FX-1001",
    title: "PCI enumeration failed its self-check",
    meaning: "`pci::Enumeration` reads where firmware put PCI Express configuration space — the MCFG \
              table, or a `pci-host-ecam-generic` device tree node — maps it a bus at a time, and \
              walks every function reachable from each root bus, sizing and restoring every BAR \
              and walking every capability list to its end. Stage 10 hands user-mode drivers \
              exactly the apertures this finds, so a walk that misses a device, sizes a BAR wrong \
              or follows a broken list would give a driver the wrong memory or none.",
    causes: &[
        "A host was described but nothing answered on its root bus, so the window was mapped at \
         the wrong address: an MCFG base taken as the first bus's rather than bus zero's, or a \
         device tree `reg` taken the other way.",
        "A bus's window could not be mapped, because the vmap arena is exhausted or the physical \
         address is beyond what this architecture's page tables can express.",
        "`src/lib/platform/pci` refused a BAR or a capability list. On QEMU that means the accessor read the \
         wrong width or offset, not that the device is malformed.",
        "The virtio-rng self-check in `pci/virtio.rs` saw a completion that cannot be right: a \
         request nobody made, a length it was not given, or bytes it never wrote — which is what \
         a wrong DMA address looks like. A device that merely refuses or stalls is skipped and \
         reported instead, because on a hypervisor somebody else configured that is not a fault \
         in the kernel.",
        "The out-of-domain probe saw the device complete a write into a page its IOMMU domain \
         does not map, and the unit recorded no fault for it within the deadline: the unit let \
         through DMA it should have faulted, or the domain maps more than was pinned into it. A \
         completion alone is not this — QEMU's device completes a refused write through a bounce \
         buffer it then drops, and the unit's record is the answer.",
    ],
    see: "src/kernel/src/discovery/pci.rs check; src/lib/platform/pci; src/lib/platform/acpi Mcfg; src/lib/platform/fdt ecam_hosts; \
          docs/ROADMAP.md stage 10",
};

/// For `check_btrfs_disk` in `stages_check.rs`, when `fs::btrfs_check::run` fails.
pub(crate) static STAGE11_MOUNT: Explanation = Explanation {
    code: "FX-1101",
    title: "the btrfs disk did not mount and read back as the host wrote it",
    meaning: "Stage 11's exit: xtask attaches the `none` fixture, an image made by real \
              mkfs.btrfs, as the second virtio-blk disk; the boot check's second driver serves \
              it as vdb; `fs::btrfs_check::run` mounts it read-only at /mnt through the kernel's \
              own mount path and reads every file, directory and link of the fixture's manifest \
              back, comparing each file's size and CRC-32C with what the host computed from the \
              bytes it gave mkfs.btrfs. The failing manifest line is printed before the report.",
    causes: &[
        "The mount failed: the disk is not a btrfs volume this reader reads (a log tree, an \
         unknown feature, a checksum type other than CRC-32C), or its superblock could not be \
         read through the ring.",
        "A file's size or CRC-32C differs: a byte was wrong somewhere in the stack, from the \
         driver's request layout and the pin's device addresses through the ring's data copy \
         to the volume reader and the page source, or the fixture and the manifest disagree.",
        "A directory or a link is missing or of the wrong kind: lookup, readdir or readlink \
         in src/lib/fs/btrfs-vfs changed what it answers.",
    ],
    see: "src/kernel/src/fs/btrfs_check.rs; src/kernel/src/fs/btrfs.rs; src/lib/fs/btrfs-vfs; \
          tools/common/xtask/src/btrfs_disk.rs; docs/ROADMAP.md stage 11",
};

/// For `check_cgroupfs` in `stages_check.rs`, when `fs::cgroupfs::check` fails.
pub(crate) static STAGE13_CGROUPFS: Explanation = Explanation {
    code: "FX-1301",
    title: "cgroupfs did not show the job tree as cgroup v2",
    meaning: "Stage 13's landing G2 (docs/CGROUPS.md): `fs::cgroupfs::check` mounts cgroup2 at \
              /tmp/cgroup-check. The root must list a new process in cgroup.procs and have no \
              cgroup.kill. mkdir must make a job whose cgroup.events says `populated 0`, and \
              mkdir over it or over an interface file must be EEXIST. A pid written to \
              cgroup.procs must move that process: the cgroup lists it, /proc/<pid>/cgroup says \
              `0::/check-a`, cgroup.events says `populated 1`, and rmdir is EBUSY. cgroup.kill \
              must refuse 0 with ERANGE and, given 1, end the process and leave the cgroup empty \
              and removable. cgroup.max.descendants 1 must allow one child and refuse a second \
              with EAGAIN, and cgroup.stat must count it. A controller not built, a negative \
              depth, `threaded` and a negative pid are refused as Linux refuses them. Landing \
              G3: an epoll set asking EPOLLPRI of a two-member cgroup's cgroup.events must keep \
              a waiting task asleep through the first member's release and wake it, by the \
              job's event queue and with its cookie, at the last; the file must then poll \
              POLLPRI and POLLERR and be in select's exception set until read again from its \
              start, and not after. Landing G4: a program's child started by clone3 with \
              CLONE_INTO_CGROUP must read `0::/check-g` from /proc/self/cgroup first thing, and \
              the call must be EBADF for a descriptor not open and for one of /tmp; chown of a \
              cgroup directory and its cgroup.procs must last to the next lookup; uid 1000 must \
              be able to mkdir in the cgroup it was given and not in root's, open its own \
              cgroup.procs for writing and not root's, move a process within its subtree and \
              back, and be refused EACCES moving it out, even to a cgroup.procs it owns; a \
              cgroup rmdir removed must refuse a move and a mkdir with ENODEV. Landing G5: \
              native job_for_cgroup must answer the job behind a cgroup directory, with every \
              right a job carries for root and WAIT without MANAGE for uid 1000, and refuse a \
              descriptor not open, one of /tmp and an unknown right; the job must assert EMPTY \
              while nothing is in it and not while it has members, and a port registration for \
              EMPTY must stay quiet through the first member's release and fire at the last, \
              at once, with cgroup.events saying `populated 0`; one made on an empty job fires \
              as it is made; a native job made inside shows as `job-<id>`, keeps the cgroup \
              from rmdir, and goes when its handle closes. docs/AUTH.md §7, P0: a process \
              running as uid 1000, given MANAGE on /check-p's job because the cgroup and its \
              cgroup.procs were chowned to it, must be able to make a native process there with \
              process_create, and that process must have every id its creator has before it \
              starts, and started, exit with 232, the low byte of what getuid answered it; a \
              child made as root would exit 0. docs/CGROUPS.md §5: a cgroup /check-l \
              delegated to uid 1000 by chown, under a 16 MiB memory.max root set, must give \
              its delegatee MANAGE without SET_LIMIT through job_for_cgroup; refuse with \
              ACCESS_DENIED a request for SET_LIMIT, job_set_limit for memory, tasks and \
              processor weight, a duplicate asking for SET_LIMIT, and job_set_limit through a \
              plain duplicate; refuse EACCES on opening its own memory.max, pids.max and \
              cpu.weight for writing; still read 16 MiB; and accept a limit on a job the \
              delegatee made with job_create.",
    causes: &[
        "`job_set_limit` in src/kernel/src/syscall/native.rs asks for MANAGE rather than \
         SET_LIMIT, or `job_for_cgroup` in src/kernel/src/fs/cgroupfs.rs grants SET_LIMIT without \
         judging the limit files' write permission (`limit_metadata`), so whoever may fill a \
         delegated cgroup may also lift its limits.",
        "`load_native` in src/kernel/src/syscall/launch.rs did not take the creator's \
         credentials, or `process_create` in src/kernel/src/syscall/native.rs did not pass the \
         caller as the creator, so a native process started as root \
         (`Credentials::root()`, what `Process::new` gives) whoever made it.",
        "`forked_into` in src/kernel/src/syscall/process.rs or `clone_with`/`cgroup_target` in \
         src/kernel/src/syscall/family.rs put the child in its parent's job, or \
         `cgroupfs::clone_target` did not recognise a cgroupfs directory.",
        "`attach_permissions` in src/kernel/src/fs/cgroupfs.rs did not find the common ancestor, \
         or judged the move as someone other than the file's opener; or `set_node`/`node` \
         lost an owner, so a lookup reports root's.",
        "`Job::remove_named_child` did not mark the job removed, or `count_in_checked` and \
         `new_named_child` did not look.",
        "`EventsFile` in src/kernel/src/fs/cgroupfs.rs does not name the job's `events` queue in \
         `poll_queues`, or does not compare the queue's wake count with the one it last \
         rendered at; or `job::notify` did not wake the queue at the flip.",
        "`poll::revents`, `poll::select_sets` or epoll's `bits` lost `Readiness::priority`.",
        "`Job::new_named_child`, `remove_named_child` or `children` in src/kernel/src/object/job.rs \
         lost a named child, or `room_for_a_child` reads the limits wrongly.",
        "`Process::move_to` did not move the process, or its job's counts, so cgroup.procs or \
         cgroup.events disagree with where the process is.",
        "`Job::kill_members` did not find the member through the registry, or sealed the job.",
        "`job::notify` did not fire the job's EMPTY registrations at the flip, `Job::observe` \
         did not fire one on an empty job at once, or `Job::signals` does not report EMPTY; or \
         `job_for_cgroup` in src/kernel/src/syscall/native.rs judged the rights by someone other \
         than the caller, or `cgroupfs::directory_job` did not recognise a cgroup directory.",
        "A write's text is parsed differently from Linux's: src/lib/fs/cgroupfs, whose host tests \
         pin each parse.",
    ],
    see: "src/kernel/src/fs/cgroupfs.rs; src/kernel/src/object/job.rs; src/lib/fs/cgroupfs; \
          docs/CGROUPS.md",
};

/// For `check_seccomp` in `stages_check.rs`, when `syscall::seccomp_check::run`
/// fails.
pub(crate) static STAGE13_SECCOMP: Explanation = Explanation {
    code: "FX-1302",
    title: "seccomp's filter was not asked first at every entry, or not as the entry's own",
    meaning: "Stage 13's seccomp landing S2 (docs/SECCOMP.md §3.3): the core's system call \
              entries ask a registered filter about every call before anything else answers \
              it. `syscall::seccomp_check::run` drives each entry this architecture has -- \
              SYSCALL and `int $0x80` on x86-64, `svc` on the Arm pair -- with frames of its \
              own and a test-only rule in the filter's place. Every call an entry keeps for \
              itself (`arch_prctl`, `set_tls`, `sigreturn`, `rt_sigreturn`) must reach the \
              rule once and be answered with the rule's value, with the number, the \
              instruction pointer and the first argument as the frame held them. A call must \
              carry its entry's `arch` token: an `int $0x80` call is i386's and not x86-64's, \
              whatever the image. A number with bits above the 32nd set must be judged as its \
              low half and dispatched as no call. A call in the native range must carry a \
              token of its own, so that a filter refusing every foreign `arch` refuses it and \
              one allowing that token by name does not.",
    causes: &[
        "An entry in src/kernel/src/arch calls `trap::filter_system_call` after one of its \
         early answers, or not at all, so a filter that denies `arch_prctl` or \
         `rt_sigreturn` is not obeyed.",
        "`arch::audit_arch` answers the image's token and not the entry's, or \
         `syscall::seccomp::data` takes the token from the process.",
        "`syscall::seccomp::data` cuts the number at another width than the filter's 32 \
         bits, or judges the native range under a Linux token.",
        "The entry builds `SyscallArgs::ip` from another register than the saved program \
         counter.",
    ],
    see: "src/kernel/src/syscall/seccomp_check.rs; src/kernel/src/syscall/seccomp.rs; \
          src/kernel/src/trap.rs filter_system_call; docs/SECCOMP.md §3.2, §3.3",
};

/// For `check_sysfs` in `stages_check.rs`, when `fs::sysfs::check::run` fails.
pub(crate) static SYSFS: Explanation = Explanation {
    code: "FX-0890",
    title: "sysfs did not show the machine's devices as Linux shows them",
    meaning: "`fs::sysfs::check::run` (docs/SYSFS.md §7), the last boot check, after devmgr has \
              started its drivers, mounts a sysfs at /tmp/sysfs-check and \
              walks all of it through the VFS: every directory must list names that each look \
              up to the kind the listing said, every file must open and read to its end, and \
              every link must lead, relative to where it is, to a directory in the mount. Then \
              the facts must be the owners': every device node has a directory under \
              /sys/bus/pci/devices or /sys/bus/platform/devices, and a PCI function's vendor, \
              device and class read what enumeration found; a device devmgr said a driver \
              drives has a driver link to that driver's directory, which links back, and a \
              uevent beginning DRIVER=; devices/system/cpu/online lists the \
              processors that are running and possible all of them; every disk devfs has is in \
              /sys/block with its size, ro and dev, and /sys/dev/block names it; lo is in \
              /sys/class/net with ifindex 1; /sys/dev/char/1:3 leads to devices/virtual/mem/null. \
              A read-only attribute opened for writing, mkdir and unlink are refused as kernfs \
              refuses them, statfs says SYSFS_MAGIC, and cgroup2 mounts on fs/cgroup.",
    causes: &[
        "A directory lists a name its lookup does not find, or the other way round: \
         `entries` in src/kernel/src/fs/sysfs.rs and a core's list disagree.",
        "A link's target is spelt wrongly: `path_of` and the directories disagree about where \
         something is, or src/lib/fs/sysfs's `path::relative` climbs to the wrong ancestor.",
        "A core stopped recording the device node its driver serves (`Origin` in devfs, \
         `net_ring::node_of`, `Card::node`), so a device is shown in the wrong directory.",
        "A format in src/lib/fs/sysfs changed; its host tests pin each against Linux's.",
    ],
    see: "src/kernel/src/fs/sysfs.rs; src/kernel/src/fs/sysfs/check.rs; src/lib/fs/sysfs; docs/SYSFS.md",
};

/// For `check_btrfs_write` in `main.rs`, when `fs::btrfs_write_check::run`
/// fails, or `fs::btrfs_powerfail`'s churn or replay under `ferrix.btrfs=`.
pub(crate) static STAGE12_WRITE: Explanation = Explanation {
    code: "FX-1201",
    title: "a btrfs volume Ferrix wrote did not read back as it was written",
    meaning: "Stage 12's exit, the guest's half: xtask attaches a fresh copy of the `blank` \
              fixture, an empty volume made by real mkfs.btrfs, as the third virtio-blk disk; \
              `fs::btrfs_write_check::run` mounts it writable at /mnt-rw, builds a tree on it — \
              files of every size, a hole, a link, a symlink, an overwrite, a truncation, a \
              rename and an unlink — syncs, unmounts, mounts again and reads everything back. \
              After the unmount nothing is cached, so every byte compared came off the disk. \
              Then one file fills the volume until a write answers ENOSPC, and the volume must \
              still sync, read back and take a new file once that one is deleted. Under `ferrix.btrfs=churn` or `=replay`, which only `cargo xtask test-powerfail` \
              sets, the same disk is instead rewritten until QEMU is killed, and then mounted \
              again — replaying any log the kill left — and every file whose trailer says its \
              body was made durable is checked against that trailer.",
    causes: &[
        "The mount failed: the disk takes no writes, or the volume is one src/lib/fs/btrfs-write will \
         not maintain (a subvolume, quotas), which is EROFS, or a log left by a crash would not \
         replay.",
        "A file read back short or with the wrong CRC-32C: the write path put an extent, a \
         checksum or an inode's size somewhere the read path does not look, or the block ring's \
         write copied the wrong bytes.",
        "The volume would not mount the second time: the commit wrote a superblock or a tree \
         the reader refuses, which is the write path's own consistency, not the disk's.",
        "Something removed is still there, or something renamed is not: the directory items, \
         the back-references or the orphan bookkeeping disagree.",
        "Filling the volume ended in something other than ENOSPC, or left it unable to sync \
         or take a new file: src/lib/fs/btrfs-vfs's reservation (rw.rs, \"Room is taken at the \
         write\") promised room the writeback did not find, or an allocation that ran out \
         aborted the transaction instead of answering ENOSPC before it changed anything.",
        "After a power failure, a file's bytes are not the ones its trailer promised: a log \
         or a commit that completed was rolled back, or replay put older extents under newer \
         stat data. src/lib/fs/btrfs-write's powerfail tests reproduce this on the host, faster.",
    ],
    see: "src/kernel/src/fs/btrfs_write_check.rs; src/kernel/src/fs/btrfs_powerfail.rs; \
          src/kernel/src/fs/btrfs.rs; src/lib/fs/btrfs-vfs rw; src/lib/fs/btrfs-write; tools/common/xtask/src/btrfs_disk.rs; \
          tools/common/xtask/src/powerfail.rs; docs/ROADMAP.md stage 12",
};

/// For `check_net` in `main.rs`, when the net core's self-check fails.
pub(crate) static NET_CORE: Explanation = Explanation {
    code: "FX-1150",
    title: "the net core did not carry a packet round its own loopback",
    meaning: "The net core is `src/lib/network/net` behind one lock, driven by a kernel task. Its check \
              uses the loopback and nothing else, so it passes on a machine with no network \
              device: a datagram sent to a bound port must arrive with its sender's address, a \
              datagram to an empty port must earn ECONNREFUSED from the unreachable this host \
              sends itself, a connection to a listening port must be made, accepted, carry \
              bytes both ways and end as a clean close, and a connection to a port nobody \
              listens on must be refused rather than left to time out — all of it over IPv4 \
              and again over IPv6. A port a socket listens on must be its own: a second \
              socket binding an overlapping address on it, or listening beside it having bound \
              first, must be EADDRINUSE though both set SO_REUSEADDR.",
    causes: &[
        "`Stack::port_taken` in src/lib/network/net let SO_REUSEADDR lift the conflict with a \
         socket already listening, or `Stack::listen` no longer checks for a listener on an \
         overlapping address and port (`listener_on`).",
        "The loopback interface is not up, or does not own 127.0.0.1 and ::1: `Stack::new` no \
         longer adds it, or `add_local_routes` no longer gives it its two routes.",
        "A packet routed to the loopback was handed out instead of going back up the input \
         path: `Stack::poll_transmit`'s loopback turn, or `NetCore::take_frames`, changed.",
        "A socket call reached the stack and the stack did not move: the net core's task is \
         not running, or `NetCore::with` stopped draining the egress after the call.",
        "A blocking call waited for ever: the wait's condition and what wakes it disagree, or \
         `NetCore::progress` is no longer woken after the stack moves.",
        "A connection was refused that should have been made, or made that should have been \
         refused: the listener lookup in `src/lib/network/net`'s TCP input, or the reset it sends a \
         segment with nowhere to go.",
    ],
    see: "src/kernel/src/net/check.rs; src/kernel/src/net/mod.rs; src/lib/network/net; src/lib/network/nettcp; \
          docs/ROADMAP.md",
};

/// For `check_net_ring` in `stages_check.rs`, when the net ring's self-check fails.
pub(crate) static NET_RING: Explanation = Explanation {
    code: "FX-1151",
    title: "the net ring did not carry a frame between the kernel and a driver",
    meaning: "`docs/NET-RING.md` is the memory the kernel shares with a ring-3 network driver. \
              The check plays the driver: it makes the two VMOs and the port a driver makes, \
              writes the ring header, sends HELLO, and answers submissions by hand, so the whole \
              kernel side is exercised on a machine with no network adapter. It requires a HELLO \
              whose handles carry the wrong rights to be refused, one as specified to be answered \
              with READY and its completion port, the interface to appear in the net core with \
              the name, address and MTU the HELLO gave it, every free slot to be posted for the \
              driver to fill, an ARP request written into a slot to be answered with an ARP \
              reply in a slot the kernel submits, and the interface to go when the driver does.",
    causes: &[
        "A HELLO with duplicable VMO handles was accepted: the exact-rights check in \
         `net_ring::decode_hello` was loosened, and the kernel's handle to a driver's memory \
         can now be copied.",
        "No slot was posted: `Serving::post_receives` stopped filling the ring, which is a \
         driver with no buffers and an interface that silently drops every packet.",
        "The ARP request was not answered: it never reached `src/lib/network/net`'s input path, the \
         interface has no address, or the reply was queued for an interface nobody drains.",
        "The interface outlived its driver: `Serving::finish` no longer takes it out of the net \
         core, so a route can still point at a device that is gone.",
        "The ring's task did not stop when the control channel closed, which a frame count \
         taken after the check would then see as a leak.",
    ],
    see: "src/kernel/src/interfaces/net_ring/check.rs; src/kernel/src/interfaces/net_ring/mod.rs; src/lib/proto/netring; \
          docs/NET-RING.md",
};

/// For `check_netlink` in `stages_check.rs`, when the netlink self-check fails.
pub(crate) static NETLINK: Explanation = Explanation {
    code: "FX-1152",
    title: "AF_NETLINK did not answer the requests `ip` makes",
    meaning: "`AF_NETLINK` is how a program configures an interface: `ip` uses nothing else, and \
              `ifconfig`, `route`, `udhcpc` and `getifaddrs` all end at the same socket. The \
              check opens one, binds it, and requires a dump of the links to hold the loopback \
              with its name and its up and loopback flags, an address and a route added through \
              it to appear in the next dump and to be gone after they are removed, a request \
              nothing answers to earn NLMSG_ERROR with EOPNOTSUPP, and a message too short for \
              its fixed header to earn EINVAL -- with every reply addressed to the port \
              getsockname reported, which is what libnetlink checks before it believes any of \
              it.",
    causes: &[
        "A reply could not be walked back: `src/lib/network/netlink`'s builder and its walk disagree about \
         a length or the padding between messages, which no host test covers if the two changed \
         together.",
        "A dump answered with nothing, or without the loopback: `RTM_GETLINK` no longer reaches \
         the net core's interface list, or `Stack::new` stopped adding the loopback.",
        "An address or a route was accepted and did not appear, or was removed and stayed: the \
         handler read the wrong attribute, or acted on a different interface from the one the \
         message named.",
        "A refusal came back as something else: the order of the checks in `src/kernel/src/net/\
         netlink/route.rs` changed, so an unknown type is answered before it is refused.",
        "A reply was addressed to another port or another sequence number: the socket's port \
         identifier is not what `getsockname` reports, or a reply no longer echoes the request's \
         sequence number -- which is silent breakage, because a program filters those replies \
         out and then waits for ever.",
    ],
    see: "src/kernel/src/net/netlink/check.rs; src/kernel/src/net/netlink/route.rs; src/lib/network/netlink; \
          docs/ROADMAP.md",
};

/// For `devmgr::devmgr_start`, when a start pid 1 asked for does not leave
/// the DEVICES channel with `devmgr` alone.
pub(crate) static DEVMGR_BY_INIT: Explanation = Explanation {
    code: "FX-1009",
    title: "a devmgr pid 1 started did not keep device authority to itself",
    meaning: "Under ferrix.devmgr=init (docs/INIT.md §7.3, L12) pid 1 asks the kernel to start \
              devmgr with the starter it was given, and gets back a handle to the process. The \
              kernel makes the DEVICES channel -- MANAGE on every device node, and through them \
              DMA -- and puts one end in devmgr's table and keeps the other. Every start checks, \
              as the caller: devmgr holds its end, the caller holds no handle to it, \
              process_start on the caller's handle to the started devmgr is BAD_STATE and gives \
              it nothing, and a second devmgr_start while devmgr lives is ALREADY_BOUND.",
    causes: &[
        "`devmgr_start` in src/kernel/src/discovery/devmgr.rs put the channel's end in the caller's table, \
         or `start_program` did not put it in devmgr's.",
        "A call on a process handle reaches into the process: `process_start` in \
         src/kernel/src/syscall/native.rs started or gave something to a process already started.",
        "`may_start_again` did not see the devmgr it started as living: its exit record was \
         not kept, or was closed early.",
    ],
    see: "src/kernel/src/discovery/devmgr.rs; src/kernel/src/object/mod.rs; src/kernel/src/syscall/native.rs; \
          docs/INIT.md §7.3; docs/certification/ITEM.md §5",
};

/// For `fs::root_disk`'s switch under `ferrix.devmgr=init`, when pid 1 did
/// not move onto the volume with it.
pub(crate) static ROOT_PID1: Explanation = Explanation {
    code: "FX-1202",
    title: "pid 1 did not move onto the root volume with the switch",
    meaning: "Under ferrix.devmgr=init pid 1 starts on the tmpfs, since the root disk's driver \
              comes from the devmgr it starts. When devmgr has reported, the kernel switches / \
              to the btrfs root volume and, in the same step, moves pid 1's root and working \
              directory onto it, under pid 1's filesystem lock. After the switch pid 1's root \
              and working directory must be the volume's, and a fork of pid 1 must see the \
              volume as /.",
    causes: &[
        "`switch_to` in src/kernel/src/fs/root_disk.rs published the root without moving pid 1, \
         or moved another process.",
        "A fork does not copy its parent's root (`Process::forked` in \
         src/kernel/src/syscall/process.rs).",
    ],
    see: "src/kernel/src/fs/root_disk.rs; docs/INIT.md §7.3; docs/certification/ITEM.md",
};

/// For `check_devmgr` in `main.rs`, when `devmgr::start` fails.
pub(crate) static STAGE10_DEVMGR: Explanation = Explanation {
    code: "FX-1006",
    title: "devmgr could not be started, or did not report",
    meaning: "The kernel starts `/sbin/devmgr` from the initramfs with a bootstrap channel \
              holding one DEVICES message (a job, every device node twice, and every driver \
              image `/lib/drivers/MANIFEST` names, as VMOs the kernel filled), then waits for \
              devmgr's REPORT of what it started (`docs/DEVMGR.md`). devmgr matches devices to \
              drivers, makes each driver's ring, starts it with START, and quiesces a device \
              whose driver died. The boot's block drivers come from it from then on.",
    causes: &[
        "A driver the manifest names is not in the image, or its image does not fit a VMO: \
         `tools/common/xtask/src/native.rs` and the initramfs disagree.",
        "`/sbin/devmgr` does not load as a native program, or could not be claimed to start.",
        "devmgr exited before reporting: its exit status names the step (see `src/user/system/native/devmgr`).",
        "devmgr reported nothing within twenty seconds: a driver did not bring its device up, \
         or the kernel never sent PUBLISHED for a disk it accepted.",
    ],
    see: "src/kernel/src/discovery/devmgr.rs; src/user/system/native/devmgr/src/main.rs; docs/DEVMGR.md; docs/ROADMAP.md \
          stage 10",
};

/// For `check_driver` in `main.rs`, when `block_ring::driver_check::run`
/// fails.
pub(crate) static STAGE10_DRIVER: Explanation = Explanation {
    code: "FX-1005",
    title: "the ring-3 virtio-blk driver did not serve the test disk",
    meaning: "The boot check starts `/sbin/blk` from the initramfs the way `devmgr` will: a \
              process given the device with `MANAGE` and the driver's end of a ring's control \
              channel over its bootstrap channel, in the START message `block_ring::start_for` \
              fills from the device node. The driver brings the device up over its `IoMapping`s, \
              pins its memory into the device's domain, sends HELLO, and serves; the check \
              requires its disk in the registry within ten seconds and sectors read through it \
              to match what `xtask` wrote into the test disk. The driver is left running.",
    causes: &[
        "The driver exited before publishing: the line above this report gives its exit \
         status, which is the step `src/user/system/native/drivers/block/virtio-blk` stopped at (1 START, 2 identity, 3 registers, \
         4 memory, 5 device bring-up, 6 ring or HELLO, 7 events), or from 20 up the fault that \
         stopped it serving (its `fault_status`: 23 the device refused a request, 30 up a \
         virtqueue check).",
        "No disk appeared in time: HELLO was refused (the ring's task prints why), the ring's \
         task did not publish, or the device never came up under TCG within the patience.",
        "A sector came back wrong or failed: the driver's request layout, the pin's device \
         addresses, the IOMMU domain, or the ring's data copy disagree with the device.",
        "The initramfs carries no `/sbin/blk`: `tools/common/xtask/src/native.rs` no longer lists it.",
    ],
    see: "src/kernel/src/interfaces/block_ring/driver_check.rs; src/user/system/native/drivers/block/virtio-blk/src/main.rs; docs/BLOCK-RING.md; \
          docs/ROADMAP.md stage 10",
};

/// For `check_block_ring` in `main.rs`, when `block_ring::check::run` fails.
pub(crate) static STAGE10_RING: Explanation = Explanation {
    code: "FX-1004",
    title: "the block ring's control plane answered a driver wrongly",
    meaning: "A ring-3 block driver is given its device and asks the kernel for a ring with \
              `block_ring_create`, then introduces its disk over the ring's control channel with \
              HELLO, and the kernel answers READY with a completion port or REFUSED with the \
              reason `docs/BLOCK-RING.md` fixes. `block_ring::check::run` drives that from a \
              process the way a driver will, every call through the native dispatcher, and \
              requires each refusal the protocol specifies, an accepted HELLO's disk to appear \
              in the registry under the name and numbers HELLO's name gives it, and the disk to \
              be gone once the driver's end of the channel closes. The kernel serves nothing \
              here; a request on the ring is the ring-3 driver's check.",
    causes: &[
        "`block_ring_create` accepted a handle without `MANAGE`, a handle to something other \
         than a device, or a device that already has a ring.",
        "The ring's task answered a HELLO with the wrong refusal, or none: the order of checks \
         in `block_ring::check_hello` and `take_up` no longer matches BLOCK-RING.md section 6.2, \
         or `ferrix-blkring`'s validation changed.",
        "An accepted HELLO published no disk, or one with another geometry or name: the \
         registration in `take_up`, or `DiskName::minor`, disagrees with section 6.1.",
        "The disk stayed published after the control channel closed: the ring's task did not \
         see `PEER_CLOSED`, or `Serving::finish` did not drop the registration.",
        "A quiesce the instant the driver's channel closed was refused as still served: \
         `object::dispose` queued the closed end behind a drain on another processor instead \
         of closing it at once, so the close returned with the channel open. The message says \
         which of `BAD_STATE` and `TIMED_OUT` came back.",
        "The second round did not give every frame back: the ring's VMO holds, its task's \
         stack or the registry leak.",
    ],
    see: "src/kernel/src/interfaces/block_ring/mod.rs; src/kernel/src/interfaces/block_ring/check.rs; docs/BLOCK-RING.md; \
          docs/ROADMAP.md stage 10",
};

/// For `check_iommu` in `iommu/check.rs`, when its domain check or the
/// quarantine's fails.
pub(crate) static STAGE10_IOMMU: Explanation = Explanation {
    code: "FX-1003",
    title: "an IOMMU domain gave a device the wrong addresses",
    meaning: "A driver in ring 3 pins the pages it gives a device into that device's IOMMU domain \
              and hands the device the addresses the pin returns, so the domain decides what the \
              device's DMA can reach. `iommu/check.rs`'s check_domains pins two frames through a \
              device node's domain and requires one domain per node, an address for each frame, a \
              count of the pages the domain holds, and a pin refused by every domain but the one \
              that took it. A domain that gets any of these wrong sends a device's writes \
              somewhere its driver did not choose.",
    causes: &[
        "`DeviceNode::domain` built a second domain for a node instead of handing back the first.",
        "An untranslated domain gave an address other than the frame's physical address, which is \
         the address a device must be given while no IOMMU domain is programmed.",
        "`Domain::unpin` accepted a pin another domain took, or kept counting pages it had \
         unpinned.",
        "No frame could be allocated for the check.",
        "On a VT-d unit whose walk does not snoop the caches (`ECAP.C` clear, as QEMU's reports), \
         a table entry or a fresh table was written and not cleaned to memory before the \
         invalidation or map that published it, so the unit could walk a stale entry (finding \
         F-58; `iommu/check.rs` check_cleaning).",
    ],
    see: "src/kernel/src/iommu.rs Domain; src/kernel/src/iommu/check.rs check_domains; \
          src/kernel/src/device.rs DeviceNode::domain; \
          docs/ARCHITECTURE.md section 7; docs/ROADMAP.md stage 10",
};

/// For `check_dma_faults` in `iommu/check.rs`, when an IOMMU recorded a fault
/// no check provoked.
pub(crate) static STAGE10_DMA_FAULT: Explanation = Explanation {
    code: "FX-1007",
    title: "an IOMMU faulted DMA that no check provoked",
    meaning: "Every PCI function sits in an IOMMU domain that maps only what its driver pinned, \
              so a device that reaches for anything else is stopped by its unit, which records \
              the stream, the page and whether it was a write. The one fault a boot provokes on \
              purpose is the out-of-domain probe's, in `pci/virtio.rs`, which registers its \
              stream and page with `Domain::provoke` before ringing the doorbell. \
              `iommu::audit_faults`, last before the success marker and after every driver the \
              boot starts has run, reads every unit's records and counts every other fault since \
              translation went on: the isolation held, but something tried DMA it was not \
              given, and a unit whose faults nobody reads would hide it.",
    causes: &[
        "A driver gave its device an address it never pinned, or one it had already unpinned: \
         a buffer handed out before `Domain::pin` returned, or a descriptor left in a ring \
         after the pin behind it was given back.",
        "A device was left running across a reset or a driver's restart and completed a stale \
         descriptor into a domain that no longer maps its buffer.",
        "A domain lost a mapping it should hold: `Domain::unpin` or the unit's tables removed \
         more than was asked, or an IOTLB invalidation was missed on a pin, so a unit in \
         caching mode still answers from a stale not-present entry.",
        "The probe's fault arrived at a stream or page other than the ones it registered, \
         which is what FX-1001 once was: a fault record read out of order.",
        "An SMMUv3 recorded an event other than a refused access, which no check provokes and \
         the audit line counts apart: C_BAD_STREAMID for a device whose stream is past the \
         256 entries of the table, C_BAD_STE or F_WALK_EABT for tables the kernel wrote that \
         the unit would not use. The `first read here` line names the event.",
        "An SMMUv3's event queue overflowed and the unit dropped events it had to record: \
         what they were is unknown, so the audit counts the overflow as one stray event. The \
         queue holds 128 records, and nothing but a device's DMA faulting over and over, or \
         tables the unit keeps refusing, fills it.",
        "A VT-d unit set its primary fault overflow: a fault found the record full and was \
         dropped. It counts as one stray event unless the full record held the probe's own \
         fault, when the lost one may have been the probe's next; the `first read here` line \
         names the record that was full. QEMU's unit never overflows on one device's faults, \
         so there it means a second device faulted.",
    ],
    see: "src/kernel/src/iommu.rs audit_faults; src/kernel/src/iommu/check.rs check_dma_faults; \
          src/kernel/src/discovery/pci/virtio.rs probe_out_of_domain; \
          src/kernel/src/iommu/vtd.rs Unit::take_fault; src/kernel/src/iommu/smmuv3.rs \
          Unit::take_fault; tools/common/xtask/src/dma_faults.rs; \
          docs/certification/VULNERABILITY-ANALYSIS.md T.DMA",
};

/// For `check_devices` in `main.rs`, when `device::publish` finds a node that
/// breaks the rule, or a finder other than the PCI walk fails.
pub(crate) static STAGE10_DEVICES: Explanation = Explanation {
    code: "FX-1002",
    title: "a device node handed out memory or an interrupt it does not have",
    meaning: "A driver in ring 3 is given an `IoMapping` for each of its device's apertures and \
              nothing outside them, and an `Interrupt` for each of its vectors. Those objects are \
              built only from `Aperture` and `Vector` values that `device.rs` mints from what \
              enumeration found, so the rule holds if and only if a device node refuses every \
              request that is not inside one of its own apertures. `device::publish` asks every \
              node for each aperture, its last byte, a range past each edge, an empty range and \
              one that wraps the address space, and requires exactly the right answers before \
              any node is published.",
    causes: &[
        "`DeviceNode::aperture` was changed to accept a range spanning two apertures or \
         running past one, which is the change that would let a driver map a neighbour's \
         registers.",
        "An aperture overlapping memory the kernel owns — RAM, an ECAM window, a controller the \
         kernel drives, the boot framebuffer — or another device's aperture was minted, so a \
         driver could be given memory that is not its device's.",
        "`DeviceNode::pci` minted an aperture from a BAR whose size or address enumeration had \
         not checked.",
    ],
    see: "src/kernel/src/device.rs publish; src/kernel/src/discovery/pci.rs; docs/ROADMAP.md stage 10",
};

/// For `check_devices` in `main.rs`, when the GICv2 distributor check fails.
pub(crate) static STAGE10_DISTRIBUTOR: Explanation = Explanation {
    code: "FX-1010",
    title: "two cores enabling neighbouring interrupt lines lost one's setting",
    meaning: "A GICv2 keeps four lines' priorities or targets in one register and sixteen \
              lines' trigger configuration in another, so enabling a line or making it \
              edge-triggered reads a register other lines share, changes its own part and writes \
              it back. `arm_common::gicv2::rmw` holds a lock across the three, and the check \
              shows it: two cores make neighbouring idle SPIs edge-triggered and enable them at \
              the same moment, held between each read and write so that the race happens every \
              round, and both lines must end with their priority, a target and the edge bit. \
              A lost target byte is a shared interrupt delivered to no core, silently; a lost \
              edge bit is an MSI that never arrives (F-50).",
    causes: &[
        "A read-modify-write of a distributor word was added or changed outside `rmw`, so \
         it no longer holds `DISTRIBUTOR_RMW` across its read and its write.",
        "`rmw` was changed to take its lock after the read, or to drop it before the write.",
        "The check's lines were not idle: a device or the `GICv2m` frame raised or changed one \
         of them while the check ran, which `idle_word` should have ruled out.",
        "The check could not put its lines back as it found them, or a core never reached \
         its rendezvous.",
    ],
    see: "src/kernel/src/arch/arm_common/gicv2.rs rmw; src/kernel/src/arch/arm_common/gicv2/check.rs; \
          docs/certification/FINDINGS.md F-50",
};

/// For `check_path_calls` in `stages_check.rs`, when `syscall::check::run_paths` fails.
pub(crate) static STAGE8_PATH_CALLS: Explanation = Explanation {
    code: "FX-0820",
    title: "the system calls that take a path failed their self-check",
    meaning: "`syscall::check::run_paths` makes the calls a shell makes on files -- mkdirat, \
              mknodat, symlinkat, readlinkat, renameat2, the stat family and statx, getdents64, \
              chdir, fchdir and getcwd, faccessat, chmod, chown, utimensat and unlinkat -- by \
              their numbers, through the dispatch table, against the real namespace under /tmp, \
              and decodes every stat record back out of the user buffer in this architecture's \
              layout. It also makes device nodes with mknodat and opens them, expecting 1:3 and \
              1:5 to act as /dev/null and /dev/zero and a number devfs lacks, or a block device, \
              to be ENXIO. A failure means a program would be told something false about a file: \
              a size read out of padding, a listing that skips or repeats a name, a working \
              directory that is not where chdir put it, or a device node reaching the wrong \
              device.",
    causes: &[
        "`arch::STAT_LAYOUT` names the wrong `struct stat` for this architecture, or a layout in \
         `src/lib/proto/linux-abi` moved a field.",
        "An arm of `syscall::path::dispatch` reads its arguments in the wrong order or at the \
         wrong width.",
        "The namespace in `src/lib/fs/vfs` changed what a walk, a rename or a directory cursor does.",
        "/tmp is not mounted, or a previous run left /tmp/pathcheck behind.",
        "`fs::devfs::attach_device` is not called from `openat`, or `devfs::open_char_device` \
         matches numbers in a different encoding from the one `mknodat` stores.",
    ],
    see: "src/kernel/src/syscall/check.rs run_paths; src/kernel/src/syscall/path.rs; \
          src/kernel/src/fs/devfs.rs; \
          src/kernel/src/syscall/stat.rs; src/lib/fs/vfs; docs/ROADMAP.md stage 8",
};

/// For `check_native_objects` in `stages_check.rs`, when `object::check::run` fails.
pub(crate) static STAGE9_OBJECTS: Explanation = Explanation {
    code: "FX-0901",
    title: "the native ABI's objects failed their self-check",
    meaning: "`object::check::run` builds two processes and drives the native system call \
              handlers between them, before any program can make a native call. A channel must \
              carry bytes and a VMO handle from one process's table to the other's, removing it \
              from the sender, and the handle that arrives must name the same object. A read \
              that does not fit must report the sizes and leave the message queued; rights must \
              only shrink; a closed handle must be refused; a refused send must leave every \
              handle with its sender; a send closing a cycle of channels must be refused; and closing a channel must free \
              what was queued in it. A wait must be woken by what it waits for rather than \
              by its deadline, and a job kill must end every process in and beneath the job \
              and none above it. `object::check::run_devices`, once the device nodes are \
              published, requires an I/O mapping of a device's own aperture to translate to \
              its physical pages, in a forked child too, and nothing past it to be granted, \
              and an interrupt to be held pending from delivery to acknowledgement; \
              `object::device_copy_check::run` then requires a writev from and a readv into a \
              page of a device's mapping each to be EFAULT before the page is faulted in \
              (F-55). Two \
              programs in user mode must exchange a message and a VMO handle over a channel \
              and both exit 0. A \
              port must give back what was queued, a registration must fire once when its \
              signal comes true or at once if it already is, and a port wait must be woken \
              by the message a registration watched. A bound interrupt must queue exactly one \
              packet on its port per delivery before acknowledgement. A \
              kernel failing any of these would give userspace drivers a capability system that \
              confines nothing.",
    causes: &[
        "The handle table in `src/lib/kernel/objects` or the rights rule in `src/lib/proto/native-abi` changed, so \
         a closed handle resolves again or a duplicate gains a right.",
        "`Endpoint::write` took the sender's handles before the peer's queue had accepted the \
         message, so a refused send lost them.",
        "`channel::check_carry` missed an edge, or a send carrying an endpoint skipped the \
         topology lock, so two channels were queued in each other and the VMO riding in one \
         was never freed.",
        "A channel end, or a job, was not woken when its signals changed, so a wait slept \
         until its deadline; or `Job::kill` missed a process added to a job beneath the \
         one killed.",
        "`AddressSpace::map_device` or the device branch of `fault` translated a device \
         region to the wrong physical page, or `fork` copied one; or `gicv2::disable` \
         cleared the wrong enable bit, so an interrupt stayed masked after its \
         acknowledgement.",
        "A copy to or from user memory in `syscall::native` used the wrong length or width.",
        "`object::dispose` stopped draining, or an object was dropped under a lock its drop \
         needs, so the frames behind a VMO queued in a closed channel were never freed.",
    ],
    see: "src/kernel/src/object/check.rs run; src/kernel/src/syscall/native.rs; \
          src/kernel/src/object/channel.rs; docs/ROADMAP.md stage 9",
};

/// For `check_syscalls` in `stages_check.rs`, when `syscall::check::run` fails.
pub(crate) static STAGE7_SYSCALLS: Explanation = Explanation {
    code: "FX-0701",
    title: "the system call dispatch path failed its self-check",
    meaning: "`syscall::check::run` proves the kernel decodes system calls with its own \
              architecture's table before any program can make one. Exactly one of the three \
              architectures' `getpid` numbers must decode to getpid on this build and answer; \
              the credential calls must report root, and getpid and gettid must agree; an \
              unknown number must be refused with ENOSYS, encoded as -38; and every number \
              from 0 to 600 must return a value. A build using another architecture's table \
              would pass every host test and answer a program's `write` with a different call.",
    causes: &[
        "The kernel was built with another architecture's number table behind \
         `arch::decode_syscall`.",
        "The number tables or the errno encoding in `src/lib/proto/linux-abi` changed, so a \
         credential call has no number on this architecture or ENOSYS no longer encodes \
         as -38.",
        "A handler added to the dispatch table returns `Outcome::Enter` for an ordinary call \
         or answers a credential call with something other than root.",
        "Two spinning programs on one processor were never both preempted twice in three \
         attempts, a preemption being a switch out of a still-runnable program at the exit of \
         an interrupt that arrived in user mode. One attempt can be the host: a stall while one \
         program runs is charged to it as service, and the scheduler then lets the other run \
         its whole loop. Each attempt prints both preemption and switch counts, the processor's \
         worst overrun and its preemption-lock count. Being switched to twice is not evidence: \
         a program never preempted shows that too, from its start and its first write; nor is \
         every switch away while runnable, since a lock released inside a system call with a \
         reschedule pending switches too.",
        "A program's task, process or address space outlives it: a reference to one is left \
         on a frame that never returns, in a table nothing empties, or in a cycle, so the \
         frames of programs that exited never come back (the `give every frame back` \
         messages).",
        "An orphan is handed to nobody, to the wrong process, or to a list it is not in: \
         `Process::end` no longer looks for a reaping ancestor or init, changes the orphan's \
         parent before the new parent's list holds it, or keeps a reaper that has ended \
         (the `orphan` messages).",
    ],
    see: "src/kernel/src/syscall/check.rs run; src/kernel/src/syscall/mod.rs dispatch; src/lib/proto/linux-abi; \
          docs/ROADMAP.md stage 7",
};

/// For `check_filesystems` in `main.rs`, when `fs::init` fails.
pub(crate) static STAGE8_ROOT: Explanation = Explanation {
    code: "FX-0801",
    title: "the root filesystem could not be built",
    meaning: "`fs::init` unpacks the initramfs the loader handed over into a tmpfs root and \
              mounts a second tmpfs on /tmp. Every program resolves its paths in that tree, \
              so a kernel that could not build it has nowhere to find a program or put a \
              file. The archive is read through the direct map from memory the loader \
              reserved as `Initrd`.",
    causes: &[
        "The archive is malformed or truncated: `tools/common/xtask/src/initramfs.rs` wrote something the \
         newc reader in `src/lib/fs/cpio` refuses, or the loader read less of FERRIX/INITRD.IMG \
         than it reported.",
        "The loader placed the archive outside the direct map, which it is meant to refuse \
         before the hand-off.",
        "An entry could not be created: tmpfs refused it, or memory for file contents ran \
         out.",
    ],
    see: "src/kernel/src/fs/mod.rs init; src/lib/fs/vfs/src/initramfs.rs; tools/common/xtask/src/initramfs.rs; \
          src/boot/common/uefi/src/main.rs load_initrd; docs/ROADMAP.md stage 8",
};

/// For `check_filesystems` in `main.rs`, when `fs::check::run` fails.
pub(crate) static STAGE8_FILESYSTEM: Explanation = Explanation {
    code: "FX-0802",
    title: "the root filesystem failed its self-check",
    meaning: "`fs::check::run` requires the unpacked archive to hold what the build wrote, its \
              hard link to be a second name for the marker rather than a copy, and its \
              symbolic link to lead there. It requires /tmp to be a filesystem of its own, \
              and a file there to read back across page boundaries, to read zeros past a \
              truncation rather than the bytes it cut off, to survive a rename, and to give \
              every frame back once it is gone.",
    causes: &[
        "The marker in `src/kernel/src/fs/check.rs` and the one in `tools/common/xtask/src/initramfs.rs` have \
         drifted apart.",
        "The VMO page store copies through the wrong frame or offset, or `Vmo::decommit_from` \
         does not release what a truncation discards.",
        "The dentry cache keeps an unlinked file's inode alive, so its pages outlive it and \
         show up as leaked frames.",
    ],
    see: "src/kernel/src/fs/check.rs run; src/kernel/src/fs/pages.rs; src/kernel/src/user/vmo.rs \
          decommit_from; src/lib/fs/vfs/src/namespace.rs; docs/ROADMAP.md stage 8",
};

/// For `standard_streams` in `syscall/fd.rs`, when a new process cannot be
/// given the console.
pub(crate) static CONSOLE_DESCRIPTORS: Explanation = Explanation {
    code: "FX-0810",
    title: "a new process could not be given the console as descriptors 0, 1 and 2",
    meaning: "Every process starts with one open description of the console installed at \
              descriptors 0, 1 and 2, opened through `/dev/console` when the namespace has \
              the console there and through a namespace of its own otherwise. Neither route \
              touches anything a program controls, so a failure is a kernel bug rather than \
              a condition to report to one: a process created without them would have its \
              first `open` land on descriptor 0 and its output written into that file. \
              Running out of memory for the table is not this: it is reported, and \
              process_create answers NO_MEMORY.",
    causes: &[
        "The console inode in `src/kernel/src/fs/console.rs` started refusing to be opened, or \
         `OpenFile::new` gained a check the console does not pass.",
        "A new descriptor table no longer starts empty or with room for three descriptors, \
         so installing them failed with EMFILE.",
    ],
    see: "src/kernel/src/syscall/fd.rs standard_streams; src/kernel/src/fs/console.rs open_console; \
          src/lib/fs/vfs/src/fd.rs; docs/ROADMAP.md stage 8",
};

/// For `check_filesystems` in `main.rs`, when `/dev` or `/proc` fails its check.
pub(crate) static STAGE8_PSEUDO_FILESYSTEMS: Explanation = Explanation {
    code: "FX-0830",
    title: "/dev or /proc failed its self-check",
    meaning: "`fs::procfs::check::run` opens the memory devices through the namespace and \
              requires each to do what it is for and to carry the number Linux gives it. It \
              registers a small in-memory disk and, by system call number, requires /dev to \
              list it after the character nodes without repeating or skipping a name across a \
              registration, stat to report a block device with its number and size, O_PATH to \
              open it, the disk opened as a file to answer pread inside sectors, lseek to its \
              end and the BLK requests as Linux does and to refuse O_RDWR with EACCES, a \
              writable disk in memory to take a pwrite patching two sectors in part, cut one \
              over its end, refuse one at it with ENOSPC and flush on fsync, block_device to \
              find it by number and read its sectors, /proc/partitions to hold exactly its row, a clashing name or number to \
              be refused, and, once the registration is dropped, the node, the lookup and the \
              row to be gone while the held device answers EIO; twice, with no frame and no \
              cached dentry left behind by the second run. It then runs a task in a process of its own and requires /proc/self to name that \
              process, every name a recursive listing of /proc reports to lead back to what \
              the listing said, /proc/self/fd to name a descriptor's path and say it was \
              deleted once it is gone, and /proc/self/maps, read a few bytes at a time while \
              the map changes, to be one line per region as it was at open, with the heap and \
              stack named. /proc/<pid>/stat and status must say T for that process while \
              it is stopped, R while it reads them, and Z once it has ended unreaped. It also \
              reads /proc/stat twice across a short sleep and requires \
              it to parse back with a cpuN line per online processor, the cpu line to be their \
              sum, no processor to have counted more time than has passed, and no counter to \
              have gone backwards while the total advanced. Programs read these files by fixed \
              columns, so a kernel that fails here hands them wrong numbers without an error.",
    causes: &[
        "A directory's lookup and its listing disagree about a name or an inode number, so a \
         recursive listing cannot walk back to what it listed.",
        "The VFS cached a procfs directory's names, so a process or descriptor that appeared \
         after a miss stays invisible; `Inode::caches_lookups` must answer false there.",
        "Reads of an open /proc file did not go to the snapshot taken at open, so a map that \
         changed mid-read shows up with extra lines.",
        "A /dev node carries the wrong major or minor number, or reaches the wrong device.",
        "devfs cached a lookup in /dev, so a disk registered after a miss stays invisible, or \
         a listing's cursor for a static node moved when a disk was registered or dropped.",
        "A dropped `BlockRegistration` left its disk in the registry, or the registry's lock \
         was held across a device read.",
        "A run queue's busy or idle count dropped the time since its last charge, or charged \
         one interval twice, so /proc/stat's times go backwards or outrun the clock.",
        "`render::state_of` reports a process by something other than what it is doing: an \
         ended process not as a zombie, a stopped one not as stopped, or its reader not as \
         running.",
    ],
    see: "src/kernel/src/fs/procfs/check.rs run; src/kernel/src/fs/procfs.rs; src/kernel/src/fs/devfs.rs; \
          src/kernel/src/fs/devfs/check.rs; src/kernel/src/fs/block.rs; src/lib/fs/procfs/src/maps.rs; src/lib/fs/procfs/src/kstat.rs; src/kernel/src/fs/procfs/render.rs state_of; \
          src/kernel/src/sched/queue.rs time_spent; docs/ROADMAP.md stage 8",
};

/// For `sysrq_trigger` in `fs/procfs.rs`, when a program writes `c` to
/// /proc/sysrq-trigger.
pub(crate) static SYSRQ_CRASH: Explanation = Explanation {
    code: "FX-0850",
    title: "a panic was requested through /proc/sysrq-trigger",
    meaning: "Nothing is wrong. Writing `c` to /proc/sysrq-trigger stops the kernel on purpose, \
              as it does on Linux, so the failure path -- the report, the backtrace, stopping \
              the other processors, the screen and its QR code -- can be reached from a shell \
              without building a kernel with a fault in it. The trace runs through the write \
              system call that carried the request.",
    causes: &[
        "A program wrote `c` to /proc/sysrq-trigger: from the shell, `echo c > \
         /proc/sysrq-trigger`.",
    ],
    see: "src/kernel/src/fs/procfs.rs sysrq_trigger; docs/RELIABILITY.md",
};

/// For `check_filesystems` in `main.rs`, when the pipe and filesystem call
/// checks fail.
pub(crate) static STAGE8_PIPES_AND_FILESYSTEM_CALLS: Explanation = Explanation {
    code: "FX-0860",
    title: "pipes, a FIFO or the filesystem calls failed their self-check",
    meaning: "`fs::check::run_calls` builds a process and drives the handlers with paths and \
              buffers in its own memory. A pipe must carry what is written into it, report \
              pipefs to fstatfs, read end of file once its writer closes, answer EAGAIN when \
              empty and non-blocking and EPIPE with no reader, and pipe2 must close both \
              descriptors again when it cannot hand them back. ioctl(FIONBIO) must set and clear \
              O_NONBLOCK on a pipe and on an AF_UNIX socket, and be EFAULT for an unreadable \
              argument; FIOCLEX and FIONCLEX must set and clear close-on-exec. A FIFO under \
              /tmp must be one \
              pipe for its openers. statfs of /tmp must decode TMPFS_MAGIC, and statfs64 must \
              take 84 and musl's 88 as its size. truncate and fallocate must grow a file and \
              fallocate never shrink one, a file written now must be dated by CLOCK_REALTIME, \
              and sendfile must copy a file with and without an \
              offset. splice must drain a pipe into /dev/null, fill a pipe from a file at an \
              offset and move bytes between two pipes, and copy_file_range must copy a file, \
              each refusing as Linux does. Then, by syscall number, mount -t proc and mount -t devtmpfs must each \
              make a new instance on a directory under /tmp: the check process must be found \
              through the procfs, zero must read zeros from the devtmpfs, /proc/mounts must \
              list both, and both must unmount; mount -t sysfs must mount and unmount, and \
              mount -t devpts, a type there is not, must be ENODEV. The whole run is done \
              twice and must leave no frame behind.",
    causes: &[
        "A pipe end's drop no longer counts it out of the buffer, so a reader never sees end \
         of file and the pipe outlives its descriptors as leaked frames.",
        "`attach_fifo` is not called from `openat`, or keys its table by something two opens \
         of one FIFO do not share, so each opener gets a pipe of its own.",
        "A `statfs` layout in `src/lib/proto/linux-abi` or its encoder in `src/lib/fs/vfs/src/statfs.rs` \
         moved a field, so the magic number is not where a program reads it.",
        "tmpfs's `grow_to` shrinks a file, or `sendfile` stopped putting its offset back.",
        "`splice` or `copy_file_range` in `src/kernel/src/syscall/pipe.rs` moved the wrong bytes \
         or offset, or `fs::pipe::splice_pipes` lost bytes between two pipes.",
    ],
    see: "src/kernel/src/fs/check.rs run_calls; src/kernel/src/fs/pipe.rs; src/kernel/src/syscall/pipe.rs; \
          src/kernel/src/syscall/fsctl.rs; src/lib/fs/vfs/src/pipe.rs; src/lib/fs/vfs/src/statfs.rs; \
          docs/ROADMAP.md stage 8",
};

/// For `check_filesystems` in `main.rs`, when the memfd check fails.
pub(crate) static STAGE8_MEMFD: Explanation = Explanation {
    code: "FX-0880",
    title: "memfd_create or its seals failed their self-check",
    meaning: "`fs::memfd_check::run` builds a process and makes memfds. An unknown flag and a \
              250-byte name must be EINVAL; a memfd made without MFD_ALLOW_SEALING must carry \
              F_SEAL_SEAL and refuse a seal with EPERM. A sealable memfd takes F_SEAL_SHRINK and \
              then refuses a truncation downwards, and F_SEAL_GROW and then refuses growing by \
              truncation or write, all with EPERM. F_SEAL_WRITE must be EBUSY while a shared \
              mapping may write the file, a fork child's copy of one included, and succeed once \
              none may; after it, a write and a shared writable mapping are EPERM, mprotect of a \
              shared read-only mapping to writable is EACCES, and a private mapping still maps \
              and keeps its writes from the file. An unknown seal is EINVAL, and any seal after \
              F_SEAL_SEAL is EPERM. A shared read-only mapping of a file opened read-only must \
              not become writable through mprotect. The whole run is done twice and must leave \
              no frame behind.",
    causes: &[
        "`map_file` does not raise the file object's may-write count for a shared mapping of a \
         writable file, or `give_back`, `Drop` or a fork child's attach does not balance it, so \
         F_SEAL_WRITE is granted beside a writable mapping or refused forever.",
        "`add_seals` reads the may-write count before it stores the seal, or `sys_mmap` reads \
         the seals before the mapping counts itself, which lets a race leave both standing.",
        "`AddressSpace::protect` does not look at a shared file region's may_write before it \
         makes the range writable.",
        "tmpfs's write_at, set_len or grow_to does not consult the node's seals under its lock.",
    ],
    see: "src/kernel/src/fs/memfd_check.rs; src/kernel/src/syscall/memfd.rs; src/kernel/src/syscall/fd.rs \
          sys_fcntl; src/kernel/src/syscall/memory.rs map_file; src/kernel/src/user/space.rs map_file, \
          protect; src/lib/fs/vfs/src/tmpfs.rs add_seals",
};

/// For `check_epoll` in `stages_check.rs`, when the epoll check fails.
pub(crate) static STAGE8_EPOLL: Explanation = Explanation {
    code: "FX-0881",
    title: "epoll failed its self-check",
    meaning: "`fs::epoll_check::run` builds a process, makes epoll sets by number and watches \
              pipes with them. A level-triggered registration must report an unread pipe at every \
              wait, with the cookie it was added with and only the events it asked for; an \
              edge-triggered one must report once, not again until more is written, and again \
              after the pipe was drained and written; a one-shot one must report once and not \
              again until EPOLL_CTL_MOD. With room for one event, two ready pipes must come by \
              turns. A registration must outlive its number while a dup keeps the file open and go \
              with the file. A set holding a set must poll readable and report the inner set's \
              cookie when the inner set has something; a set added to itself is EINVAL, to a set \
              it holds ELOOP, and a sixth set in a chain ELOOP. The refusals are Linux's, in \
              Linux's order, and the run is done twice and must leave no frame behind.",
    causes: &[
        "An inode's `poll_changes` does not move when its readiness changes, or a queue it reads \
         is woken without `WaitQueue::wake_all`, so an edge-triggered registration misses the data \
         written after a drain.",
        "`Epoll::delivered` does not clear `due`, disarm a one-shot registration or move a \
         level-triggered one behind the others.",
        "`Epoll::check_nesting` misses a loop through a set's registrations, or counts the depth \
         above or below one set too many or too few.",
        "`sys_epoll_ctl` or the wait checks in a different order from Linux's.",
    ],
    see: "src/kernel/src/fs/epoll_check.rs; src/kernel/src/fs/epoll.rs; src/kernel/src/fs/anon.rs; \
          src/kernel/src/syscall/epoll.rs; src/kernel/src/sched/wait.rs; src/lib/fs/vfs/src/node.rs \
          poll_changes",
};

/// For `check_mount_flags` in `stages_check.rs`, when `fs::mount_check::run`
/// fails.
pub(crate) static STAGE8_MOUNT_FLAGS: Explanation = Explanation {
    code: "FX-0885",
    title: "A mount's flags failed their self-check",
    meaning: "`fs::mount_check::run` mounts a tmpfs under /tmp by number, fills it and \
              remounts it ro,nosuid,nodev,noexec. Every change through a path or a descriptor \
              must then be EROFS -- open for writing, create, mkdir, symlink, unlink (of a \
              missing name too), rename, chmod, chown, utimensat, truncate, setxattr -- a device \
              on it EACCES, its program EACCES, an executable mapping of its file EPERM and \
              mprotect to executable EACCES, and a file opened for writing before the remount \
              must still write. nosuid alone must leave the program runnable without its \
              set-user-id bit; read-only alone must open the device for writing and answer \
              EEXIST for a name that exists. A remount inside a mount is EINVAL and one by uid \
              1000 EPERM. mountinfo, /proc/<pid>/mounts and statfs's f_flags must show the flags \
              after each remount, an access-time flag the remount did not name must be kept, \
              and mountinfo must name the mount as the O_PATH descriptor's /proc link does.",
    causes: &[
        "A `Namespace` method or a call in `syscall/` changes a file without asking \
         `Location::require_writable` first, or asks after a permission check.",
        "`Namespace::open` no longer refuses a device on a nodev mount, or refuses a device \
         or a pipe opened for writing on a read-only one.",
        "`fs::open_program` does not ask `Mount::no_exec` or `set_ids_on`, or `map_file` and \
         `maps_noexec_file` in `syscall/memory.rs` do not ask `no_exec`.",
        "`remount_at` in `syscall/fsctl.rs` drops the access-time flags, or accepts a place \
         that is not a mount's root.",
        "`render::mountinfo` prints the mount point from a different root than \
         `/proc/<pid>/fd` does, or `MountFlags::options` misorders the options.",
    ],
    see: "src/kernel/src/fs/mount_check.rs; src/kernel/src/syscall/fsctl.rs; \
          src/lib/fs/vfs/src/namespace.rs; src/kernel/src/fs/procfs/render.rs",
};

/// For `check_binds` in `stages_check.rs`, when `fs::bind_check::run` fails.
pub(crate) static STAGE8_BINDS: Explanation = Explanation {
    code: "FX-0886",
    title: "A bind mount failed its self-check",
    meaning: "`fs::bind_check::run` mounts a tmpfs under /tmp by number, with a file, a \
              directory holding a second tmpfs and a listening Unix socket, and binds them: a \
              directory without MS_REC must not show the submount, nor let its mount point be \
              removed or renamed (EBUSY), and with it must show it; a \
              subdirectory, a file, the socket (which connect must reach through the bind) and a \
              directory onto itself must bind, and a file onto a directory or the reverse be \
              ENOTDIR. mountinfo must name each bind as its O_PATH descriptor's /proc link does, \
              with its root inside the filesystem. MS_REMOUNT|MS_BIND|MS_RDONLY must make one \
              bind read-only and no other; a plain MS_REMOUNT|MS_RDONLY every mount of the \
              filesystem and none of another. MS_PRIVATE, MS_SLAVE and MS_UNBINDABLE must be \
              accepted on a mount's root; MS_SHARED, two types, a place inside a mount and \
              MS_MOVE EINVAL. umount2 of a mount with one inside must be EBUSY, and with \
              MNT_DETACH take both, leave `..` from inside the submount where it is, and make a \
              bind from it EINVAL.",
    causes: &[
        "`Namespace::bind` copies the wrong mounts for MS_REC, or roots the bind at the \
         filesystem's root rather than the source's dentry.",
        "`Namespace::remount` and `remount_filesystem` act on the wrong scope, or \
         `remount_at` in `syscall/fsctl.rs` calls the one for the other.",
        "`Namespace::unmount_with` leaves a mount inside the detached one in the table, or \
         leaves a detached mount its parent.",
        "`Namespace::owns` accepts a mount that is no longer in the tree.",
        "`sys_mount` in `syscall/fsctl.rs` takes the operations in another order than \
         Linux's `path_mount`.",
    ],
    see: "src/kernel/src/fs/bind_check.rs; src/kernel/src/syscall/fsctl.rs; \
          src/lib/fs/vfs/src/namespace.rs",
};

/// For `check_namespaces` in `stages_check.rs`, when
/// `fs::namespace_check::run` fails.
pub(crate) static STAGE13_MOUNT_NAMESPACES: Explanation = Explanation {
    code: "FX-0887",
    title: "A mount namespace failed its self-check",
    meaning: "`fs::namespace_check::run` first opens with openat2 from a descriptor of its \
              directory: an absolute link out of it must stay inside with RESOLVE_IN_ROOT \
              (ENOENT), be ELOOP with RESOLVE_NO_SYMLINKS, `..` EXDEV with RESOLVE_BENEATH, a \
              /proc/<pid>/fd link ELOOP with RESOLVE_NO_MAGICLINKS, and struct open_how's size \
              judged as Linux judges it. It then makes two processes. The second must be refused \
              unshare(CLONE_NEWNS) as uid 1000 (EPERM) and unshare of a pid namespace (EINVAL), \
              and granted it as root; /proc/<pid>/ns/mnt must then name the two apart, and a \
              tmpfs each mounts must show in its own mountinfo and not the other's. Then \
              bubblewrap's calls as root: a tmpfs base, newroot bound onto itself, \
              pivot_root(base, oldroot) (after a pivot_root whose put_old is not below the new \
              root is refused EINVAL), a file, a proc, a tmpfs and the stage 12 btrfs bound or \
              mounted into newroot, the old root detached, pivot_root(\".\", \".\") and \
              umount2(\".\", MNT_DETACH). After it / must be the new tree, `..` must stay in \
              it, nothing of the old tree may be reachable, and mountinfo must list the new \
              tree's mounts alone. A btrfs file written through the old root just before its \
              detach must be on the disk after it, as a second, read-only mount of the disk \
              reads it. A native child the second process makes must be in its namespace and \
              root (docs/NAMESPACES.md §2.5).",
    causes: &[
        "`Namespace::copy` shares a table or a mount with the namespace it copies, or leaves \
         a context's root or working directory on the old mounts.",
        "`Mount::tree` is not the copy's, so a walk crosses the first namespace's mount points.",
        "`Namespace::pivot_root` checks in another order than Linux's, or leaves a parent \
         pointer on the old mount.",
        "`chroot_fs_refs` in `syscall/fsctl.rs` does not move the caller's root.",
        "`sys_umount2` does not act on the mount on top of `.`, or does not write out every \
         filesystem of a detached subtree.",
        "`launch::load_native` starts a native child in the first namespace's root rather \
         than its creator's context, or in the first UTS, IPC and cgroup namespaces rather \
         than its creator's.",
    ],
    see: "src/kernel/src/fs/namespace_check.rs; src/kernel/src/syscall/namespace.rs; \
          src/kernel/src/syscall/fsctl.rs; src/kernel/src/syscall/launch.rs; \
          src/lib/fs/vfs/src/namespace.rs",
};

/// For `check_user_namespaces` in `stages_check.rs`, when
/// `fs::userns_check::run` fails.
pub(crate) static STAGE13_USER_NAMESPACES: Explanation = Explanation {
    code: "FX-0888",
    title: "A user namespace failed its self-check",
    meaning: "`fs::userns_check::run` makes a process that is uid 1000 make a user namespace. It must be named apart by /proc/<pid>/ns/user and read its ids as 65534 until mapped; be refused a gid_map before setgroups is denied, a uid_map naming kernel root or two ids, and a second write; be accepted its own id mapped to 0 and then read 0 from getuid and in status, while the first namespace reads 1000; and be refused sethostname, mount, setuid to an unmapped id (EINVAL) and setgroups, while holding every capability in its own namespace. A chrooted process must be refused a user namespace. On Credentials: a map opened by root and written by an unprivileged holder must be refused, a set-user-id bit must be ignored by execve in a child namespace, and the sets must go to the namespace's root alone. A read-only bind of /proc/sys must refuse a write with EROFS (docs/NAMESPACES.md §4, U1 to U9).",
    causes: &[
        "`Credentials::privileged` answers true for a process in a child user namespace.",
        "`userns::write_map` accepts a map the rules of Linux's `new_idmap_permitted` refuse, or judges only the opener or only the writer.",
        "`credentials::kernel_id` or `Credentials::shown` is skipped at an id boundary.",
        "`namespace::make_user_namespace` no longer compares the process's root with the namespace's.",
        "`Credentials::exec` honours a set-id bit, or gives the sets to a process that is not its namespace's root.",
    ],
    see: "src/kernel/src/fs/userns_check.rs; src/kernel/src/syscall/userns.rs; src/kernel/src/syscall/credentials.rs; src/kernel/src/syscall/namespace.rs",
};

/// For `check_small_namespaces` in `stages_check.rs`, when
/// `fs::smallns_check::run` fails.
pub(crate) static STAGE13_SMALL_NAMESPACES: Explanation = Explanation {
    code: "FX-0892",
    title: "A UTS, IPC or cgroup namespace, or setns, failed its self-check",
    meaning: "`fs::smallns_check::run` drives the small namespaces through the system-call layer. A UTS namespace made by clone or unshare starts with its creator's host name and then keeps its own, as uname and /proc/sys/kernel/hostname tell it; uid 1000 is refused one and refused sethostname; a maker of a user namespace may name the UTS namespace it made and never the first's. An IPC namespace shares no semaphore keys with the first, counts only its own sets in SEM_INFO and ends with its last holder. In a cgroup namespace /proc/<pid>/cgroup reads / at the root and `..` for what lies outside it, a cgroupfs mounted there has the root as its own, a writer moves processes only between cgroups inside it, and a clone is rooted at its creator's cgroup. A /proc/<pid>/ns link opens as a namespace file: two opens are one inode, readlink names it, NS_GET_USERNS, NS_GET_PARENT, NS_GET_NSTYPE and NS_GET_OWNER_UID answer, and another person's link is refused. setns refuses a closed descriptor, a file that is not a namespace, a type that is not the file's, an ancestor user namespace, the caller's own, a stranger, and a child user namespace's join of the first's UTS or mount namespace, and joins where it may. Pid and network namespaces, CLONE_NEWIPC with CLONE_SYSVSEM and a thread with a new namespace are EINVAL (docs/NAMESPACES.md §12).",
    causes: &[
        "`nsproxy::make` shares the creator's UTS names or IPC table, or skips its CAP_SYS_ADMIN test.",
        "`system::nameable` judges `privileged()` instead of `CAP_SYS_ADMIN` over the namespace's owner.",
        "`cgroupfs::relative_names` ignores the reader's root or leaves out `..`.",
        "`fsctl::filesystem_named` mounts the whole tree for a process in a cgroup namespace, or `cgroupfs::write_to` drops the move rule or judges the writer's namespace rather than the opener's, or `cgroupfs::clone_target` or `job_for_cgroup` has no namespace test.",
        "`nsfs::ioctl` or `nsfs::related` answers a namespace the caller is not inside of, or `nsfs::may_open` admits another person.",
        "`namespace::sys_setns` skips a capability, type or ownership test, or leaves the caller's root and working directory behind.",
    ],
    see: "src/kernel/src/fs/smallns_check.rs; src/kernel/src/syscall/nsproxy.rs; src/kernel/src/fs/nsfs.rs; src/kernel/src/syscall/namespace.rs; src/kernel/src/syscall/system.rs; src/kernel/src/syscall/sem.rs; src/kernel/src/fs/cgroupfs.rs",
};

/// For `check_pid_namespaces` in `stages_check.rs`, when
/// `fs::pidns_check::run` fails.
pub(crate) static STAGE13_PID_NAMESPACES: Explanation = Explanation {
    code: "FX-0891",
    title: "A pid namespace failed its self-check",
    meaning: "`fs::pidns_check::run` makes a namespace with unshare(CLONE_NEWPID) and forks into it. The first process must be pid 1 there and have another number outside, the next 2, and the caller that unshared must not have moved; kill, wait4, getppid, getpgid, getsid and setsid must speak the caller's numbers, refusing a process the namespace cannot see and reading 0 for a parent or group it cannot; an orphan must go to its namespace's init, and the init's end must end the namespace and shut it to new members; an init must ignore SIGTERM, SIGKILL and SIGSTOP from inside, catch a signal it has a handler for, and take SIGSTOP and SIGKILL from an ancestor namespace; si_pid, ssi_pid and SO_PEERCRED must be told in the reader's numbers and 0 for a sender it cannot see; a procfs mounted in a namespace must list only its processes by its numbers, with self, NSpid and ns/pid to match; cgroup.procs must list and take the reader's numbers; and CLONE_NEWPID must need CAP_SYS_ADMIN unless a user namespace comes with it, be refused with CLONE_THREAD or CLONE_PARENT, and stop at 32 levels, each made from a process in the one above; a native child must be numbered in its creator's namespace; and a namespace whose first fork failed must take no later fork (docs/PIDNS.md, P1 to P14).",
    causes: &[
        "`pidns::from_user` or `pidns::to_user` is skipped at a call that names or reports a pid.",
        "`Local::allocate` hands out a number already in use, or does not start at 1 in a namespace.",
        "`Process::reaper_for_orphans` sends an orphan to the machine's pid 1 instead of its namespace's init.",
        "`pidns::init_gone` does not shut the namespace or does not kill its members.",
        "`pidns::discards` lets a signal through to an init that it should not, or keeps SIGKILL from an ancestor namespace out.",
        "A procfs instance lists or finds processes by their kernel numbers.",
        "`namespaces_asked` or `sys_unshare` skips CAP_SYS_ADMIN, the CLONE_THREAD and CLONE_PARENT refusals, or the depth limit.",
        "`launch::load_native` leaves a native child out of its creator's namespace, or `Numbers::drop` leaves a namespace open after its pid 1 went.",
    ],
    see: "src/kernel/src/fs/pidns_check.rs; src/kernel/src/syscall/pidns.rs; src/kernel/src/syscall/family.rs; src/kernel/src/syscall/kill.rs; src/kernel/src/fs/procfs.rs",
};

/// For `check_semaphores` in `stages_check.rs`, when `syscall::sem_check::run`
/// fails.
pub(crate) static STAGE7_SEMAPHORES: Explanation = Explanation {
    code: "FX-0702",
    title: "System V semaphores failed their self-check",
    meaning: "`syscall::sem_check::run` makes semaphore sets through the functions `semget`, \
              `semop`, `semtimedop` and `semctl` reach. A key must find its set, and IPC_EXCL, a \
              missing key, a size past the set's or SEMMSL, and a stranger's access to a set of \
              mode 0600 are refused as Linux refuses them. SETVAL, GETVAL, SETALL, GETALL and \
              GETPID must agree; a call half of whose operations could go must leave none done; \
              IPC_NOWAIT, ERANGE, EFBIG and EACCES as Linux. IPC_STAT must put the mode and \
              sem_nsems where the UAPI headers do in the 32-bit, x86-64 and generic layouts, and \
              IPC_SET must hand the set over. A process that took one with SEM_UNDO and was \
              killed must give it back, and a keyed set it held must be found by its key, read and \
              taken by a new process in another job. A waiter task must be ended by an increment, by SETVAL \
              to zero, by an interruption (EINTR), by its deadline (EAGAIN, not before it) and by \
              IPC_RMID (EIDRM), and the job it ran in must hold no heap after each. A job must be \
              refused ENOSPC at its per-job bound while a sibling makes one, and every set the \
              check made must be gone.",
    causes: &[
        "`attempt` or `revert` leaves an operation of a refused call done, or `run_queue` does \
         not complete a waiter that can go now.",
        "A waiter's `Pending` record is not taken off the queue on the way out, so its charge \
         outlives it and GETNCNT still counts it.",
        "`Ticket::answer` stores the result without waking the task, or `wait` returns before \
         re-reading the ticket under the set's lock.",
        "`Process::release` no longer calls `sem::exit`, or `exit` looks for the undo record \
         under a different owner.",
        "`encode_semid` writes a field at an offset another layout uses.",
        "`create` counts the sets of every job, or of none, against the per-job bound.",
    ],
    see: "src/kernel/src/syscall/sem_check.rs; src/kernel/src/syscall/sem.rs; \
          src/kernel/src/syscall/process.rs Process::release",
};

/// For `check_shared_memory` in `stages_check.rs`, when
/// `syscall::shm_check::run` fails.
pub(crate) static STAGE7_SHARED_MEMORY: Explanation = Explanation {
    code: "FX-0703",
    title: "System V shared memory failed its self-check",
    meaning: "`syscall::shm_check::run` makes shared memory segments through the functions \
              `shmget`, `shmat` and `shmctl` reach. A key must find its segment, and IPC_EXCL, a \
              missing key, a size of zero, past SHMMAX or past the segment's, and a stranger's \
              access to a segment of mode 0600 are refused as Linux refuses them. IPC_STAT must \
              put the mode, shm_segsz, shm_cpid and shm_nattch where the UAPI headers do in the \
              32-bit, x86-64 and generic layouts, and IPC_INFO and SHM_INFO must report the \
              limits and the use. An attach must be refused without the mode's permission and \
              allowed to root over a stranger's mode-0600 segment. IPC_RMID while attached must \
              free the key and keep the segment, SHM_DEST, until its last detach, and then take \
              it out. A key and an id of the first IPC namespace must not be found from a new \
              one, whose SHM_INFO counts only its own segment. A job must be refused ENOSPC at \
              its per-job segment bound and at its per-job reserved-page bound while a sibling \
              makes one, and every segment the check made must be gone with its charge.",
    causes: &[
        "`attach` checks the permission after it counts the attach, or not at all.",
        "`Attachment::drop` does not take a removed segment out, or `remove` takes out one \
         still attached.",
        "`encode_shmid` writes a field at an offset another layout uses.",
        "`create` counts the segments or reserved pages of every job, or of none, against \
         the per-job bounds.",
        "A search by key or id looks in another namespace's table than the caller's.",
    ],
    see: "src/kernel/src/syscall/shm_check.rs; src/kernel/src/syscall/shm.rs",
};

/// For `check_eventfd` in `stages_check.rs`, when the eventfd check fails.
pub(crate) static STAGE8_EVENTFD: Explanation = Explanation {
    code: "FX-0882",
    title: "eventfd failed its self-check",
    meaning: "`fs::eventfd_check::run` builds a process and makes eventfds by number. One made \
              with an initial value must read it back and then be empty, EAGAIN when non-blocking; \
              two writes must add up to one read, and with EFD_SEMAPHORE each read takes one. The \
              counter stops at u64::MAX - 1: the write past it is EAGAIN, and poll stops answering \
              writable there. A blocking read must wait, and a write must end the wait by waking \
              it. Registered edge-triggered in an epoll set, the eventfd must be reported after \
              each write although it stays readable. A flag eventfd2 does not take, a buffer \
              shorter than eight bytes, a write of u64::MAX and lseek are refused as Linux refuses \
              them. A poll and an epoll_wait waiting on an eventfd in tasks of their own must be \
              ended by a write's wake -- the blocking read, the poll and the epoll_wait each on \
              one of three attempts, every miss printed -- and a 120 ms poll on a quiet eventfd \
              must look at most 12 times, sleeping on its queues rather than looking every 5 ms. \
              The run is done twice and must leave no frame behind.",
    causes: &[
        "A pollable inode's `poll_queues` names a queue other than the one its changes wake, or \
         `WaitQueue::wait_on_any` does not put the task on every queue, so a poll or an \
         epoll_wait is ended by its recheck.",
        "`Sources::recheck` does not trust queues every file vouched for, or an inode answers \
         `false` from `poll_queues`, so a quiet wait looks every 5 ms.",
        "`EventFd::write_stream` does not wake the readable queue, or `read_stream` the writable \
         one, so a waiting reader sleeps until its recheck and an edge-triggered registration \
         misses the second write.",
        "`fits` lets the counter reach u64::MAX, or `poll` measures writable against a different \
         limit than a write does.",
        "`sys_eventfd2` takes a flag Linux refuses, or does not pass EFD_NONBLOCK to the open \
         file.",
        "The host stopped the emulator for about the one-second recheck just before the write, \
         in all three attempts: on resuming, the recheck's overdue timer races the write and \
         can end the wait first. One stop spoils one attempt, and the attempt lines say which \
         wait each time.",
    ],
    see: "src/kernel/src/fs/eventfd_check.rs; src/kernel/src/fs/eventfd.rs; src/kernel/src/fs/wake.rs; \
          src/kernel/src/sched/wait.rs; src/kernel/src/syscall/eventfd.rs; \
          src/kernel/src/fs/anon.rs",
};

/// For `check_timerfd` in `stages_check.rs`, when the timerfd check fails.
pub(crate) static STAGE8_TIMERFD: Explanation = Explanation {
    code: "FX-0883",
    title: "timerfd failed its self-check",
    meaning: "`fs::timerfd_check::run` builds a process and makes timerfds by number. \
              timerfd_create must take TFD_NONBLOCK and TFD_CLOEXEC and refuse other flags and \
              clocks; a disarmed timer reads EAGAIN. A one-shot timer must not be readable before \
              its deadline and must read 1, once, after it. A periodic timer armed at an absolute \
              time in the past must be readable at once and read every interval that passed. \
              timerfd_gettime must report the time left and the interval in both itimerspec \
              layouts, timerfd_settime the setting it replaced, and a zero value must disarm. A \
              set of CLOCK_REALTIME must fire an absolute real-time timer it carried past, make \
              one armed with TFD_TIMER_CANCEL_ON_SET read ECANCELED once, and leave a monotonic \
              one alone. A blocked read, a poll and an epoll_wait, each waiting before the timer \
              is armed, must be ended by the timerfds thread's wake at the deadline and come back \
              within a quarter of the one-second recheck, on one of three attempts each: each \
              attempt that misses prints a line saying how late it was and what ended it. The run \
              is done twice and must leave no frame behind, the thread's stack included.",
    causes: &[
        "The `timerfds` thread did not start when a timer was armed, sleeps past the earliest \
         deadline, or counts an expiration without waking the timer's queue, so a waiter is \
         ended by its recheck a second late.",
        "The host stopped the emulator across a deadline in all three attempts: the guest's \
         clock runs on while it is stopped, so the timer fires as late as the stop was long. \
         One stop spoils one attempt; three in a row is a host too loaded to time anything on, \
         and the attempt lines show the lateness each time.",
        "`State::count` miscounts the intervals that passed, or moves the deadline to the wrong \
         side of now.",
        "`TimerFd::set` keeps the count of the setting it replaced, or keeps no interval when \
         disarming.",
        "`clock_was_set` is not called from `clock_settime` or `settimeofday`, or does not wake \
         the thread, so a real-time deadline does not move with the clock.",
        "The thread does not exit once nothing is armed, or a timer closed does not tell it, so \
         its stack is counted against the frame window.",
    ],
    see: "src/kernel/src/fs/timerfd_check.rs; src/kernel/src/fs/timerfd.rs; \
          src/kernel/src/syscall/timerfd.rs; src/kernel/src/syscall/time.rs; src/kernel/src/fs/wake.rs; \
          src/kernel/src/sched/wait.rs",
};

/// For `check_signalfd` in `stages_check.rs`, when the signalfd check fails.
pub(crate) static STAGE8_SIGNALFD: Explanation = Explanation {
    code: "FX-0884",
    title: "signalfd failed its self-check",
    meaning: "`fs::signalfd_check::run` builds a process, gives SIGUSR1, SIGUSR2 and SIGALRM \
              handlers so a signal sent to it stays pending, and makes signalfds by number. \
              signalfd4 must take SFD_NONBLOCK and SFD_CLOEXEC, refuse other flags, a mask that \
              is not eight bytes, a mask it cannot read and a descriptor that is not a signalfd, \
              and never keep SIGKILL or SIGSTOP in a mask. With nothing pending a read is EAGAIN \
              and poll is not readable. A signal sent and pending must poll readable and read \
              back as a signalfd_siginfo naming it, SI_USER and its sender, and be pending no \
              longer; two come back in one read, lowest first; one outside the mask stays \
              pending until signalfd4 gives the descriptor a mask holding it. A blocked read, a \
              poll and an epoll_wait, each waiting before the signal is sent, must be ended by \
              the wake its arrival makes and come back within a quarter of the one-second \
              recheck, on one of three attempts each: each attempt that misses prints a line \
              saying how late it was and what ended it. The run is done twice and must leave no \
              frame behind.",
    causes: &[
        "`Process::notify_signal` or `notify_signal_to` does not wake `signal_arrived`, so a \
         waiter on a signal it blocks is ended by its recheck a second late.",
        "`SignalFd::take` reads the thread's queue or the process's but not both, or takes a \
         signal outside the mask.",
        "`Origin::encode_signalfd` puts a field at the wrong offset of `signalfd_siginfo`.",
        "`sys_signalfd4` checks the flags before it reads the mask, or makes a new descriptor \
         when it was handed one to change.",
        "The host stopped the emulator between the send and the waiter's return in all three \
         attempts: the guest's clock runs on while it is stopped, so the waiter comes back as \
         late as the stop was long. One stop spoils one attempt, and the attempt lines show the \
         lateness each time.",
    ],
    see: "src/kernel/src/fs/signalfd_check.rs; src/kernel/src/fs/signalfd.rs; \
          src/kernel/src/syscall/signalfd.rs; src/kernel/src/syscall/signal.rs; \
          src/kernel/src/syscall/process.rs notify_signal; src/kernel/src/fs/wake.rs",
};

/// For `check_madvise` in `stages_check.rs`, when the madvise check fails.
pub(crate) static STAGE8_MADVISE: Explanation = Explanation {
    code: "FX-0872",
    title: "madvise failed its self-check",
    meaning: "`user::madvise_check::run` builds a process and calls madvise by number. Eight \
              written pages of private anonymous memory dropped with MADV_DONTNEED, and again \
              with MADV_FREE, must come back as exactly eight frames in a frame window, leave \
              the resident count eight lower, and read as zeros, while the page on either side \
              keeps what it held. A page a fork child shares copy-on-write must keep the child's \
              contents when its parent drops it. A private mapping of a memfd must give back the \
              two pages it copied and show the file there again; a shared one must still show \
              what it wrote; the file must keep every page. MADV_REMOVE must punch a hole in \
              shared anonymous memory that a fork child sees too, giving back two frames. Every \
              hint must be accepted and drop nothing, and madvise must refuse as Linux does: an \
              unaligned address, unknown advice, MADV_WIPEONFORK and MADV_MERGEABLE, a length \
              that wraps, MADV_FREE and MADV_REMOVE where they do not apply, and a hole, which \
              is ENOMEM once the mapped part is advised. The run is done twice and must leave \
              no frame behind.",
    causes: &[
        "`AddressSpace::advise_region` takes the translations down but not the pages out of \
         the region's object, or of a private file mapping's shadow, so no frame goes back \
         and the next touch shows the old contents.",
        "`advise` releases the frames before its shootdown, or holds its lock across it.",
        "A shared mapping's pages are taken out of its object, which loses what every other \
         mapping of it wrote.",
        "`sys_madvise` rounds the length in 64 bits on a 32-bit build, or checks it before the \
         advice and the alignment.",
    ],
    see: "src/kernel/src/user/madvise_check.rs; src/kernel/src/user/space.rs advise; \
          src/kernel/src/syscall/memory.rs sys_madvise; src/kernel/src/user/vmo.rs take_range, retire",
};

/// For `check_filesystems` in `main.rs`, when the shared file mapping check
/// fails.
pub(crate) static STAGE8_FILE_MAPPINGS: Explanation = Explanation {
    code: "FX-0870",
    title: "a shared file mapping failed its self-check",
    meaning: "`fs::mmap_check::run` builds a process, creates a file under /tmp and maps it \
              shared. The mapping must show the file's bytes and zeros for the rest of its \
              last page; a write through the mapping must be what the file reads back, and a \
              write to the file what the mapping shows, because both are the file's own VMO \
              pages. A loader's fixed file-backed RX and RW mappings must land exactly where \
              asked; it writes the latter as its RELRO span, then mprotect makes it read-only. \
              mmap must refuse a descriptor that names nothing with EBADF, a directory \
              with ENODEV, a writable shared mapping of a file opened read-only with EACCES, \
              while a writable private mapping of one maps. msync must answer a whole mapping and \
              refuse bad flags and an unmapped range as Linux does, and /proc/<pid>/maps must \
              name the mapping by its file. A truncation must take the pages past the new end \
              away from the mapping, and a grow must show zeros there. The whole run is done \
              twice and must leave no frame behind.",
    causes: &[
        "A file's VMO is not attached as a mapper of the space, so a truncation's retirement \
         never takes the mapping's translations down and a page past the end stays readable.",
        "`VmoPages::resize` is not called before `discard_from`, so a fault racing the cut \
         commits a page past the new end.",
        "`sys_mmap` checks the descriptor's access after it clears a MAP_FIXED range, or maps \
         a file whose inode has no mapping object.",
        "`give_back` decommits a file mapping's pages on munmap, which takes them away from \
         the file itself.",
    ],
    see: "src/kernel/src/fs/mmap_check.rs; src/kernel/src/syscall/memory.rs sys_mmap, sys_msync; \
          src/kernel/src/user/space.rs map_file, fault; src/kernel/src/fs/pages.rs; docs/ROADMAP.md stage 8",
};

/// For `check_program_files` in `stages_check.rs`, when the check of programs mapped
/// from their files fails.
pub(crate) static STAGE8_PROGRAM_FILES: Explanation = Explanation {
    code: "FX-0871",
    title: "a program mapped from its file failed its self-check",
    meaning: "`fs::exec_check::run` loads a 72 MiB program -- past the 64 MiB `fs::read_file` \
              reads -- from a file whose pages a counting page source fills, as btrfs fills \
              its page cache. Loading it must read at most four of its pages, and the large \
              segment's whole pages must be one private mapping of the file. A read 40 MiB in \
              must be the file's byte and fill one run of pages; a write 50 MiB in must read \
              back and leave the file's page as it was; the bytes past the segment's file \
              contents must be zero although the file's are not. The program must then run to \
              its status. Second, the same program as a sparse 72 MiB file under /tmp is opened \
              as execve opens one, and a fork of its process, with the file's name removed, \
              must execve /proc/self/exe, which leads to the program's file, and run it to its \
              status; /proc/<pid>/exe must read as the path with (deleted) after it.",
    causes: &[
        "`load::place` copies a segment rather than mapping it, or `ProgramFile::open` reads \
         more than the headers, so loading reads the whole file.",
        "`load::map_runs` maps a writable segment shared, or `writable_in_place` lets a write \
         through the file's page, so a write reaches the file.",
        "The page a segment's file contents end on is mapped from the file rather than \
         copied, so `.bss` shows the file's bytes.",
        "`Process::forked` does not copy the parent's identity, so a fork child's \
         /proc/self/exe is ENOENT; or procfs's `link_location` is not answered, so the link \
         is followed as text to a name that is gone.",
    ],
    see: "src/kernel/src/fs/exec_check.rs; src/kernel/src/syscall/load.rs; src/kernel/src/syscall/program.rs; \
          src/kernel/src/syscall/exec.rs; src/kernel/src/fs/procfs.rs; src/lib/fs/vfs/src/walk.rs; \
          docs/CHROME.md",
};

/// For `finish` in `power.rs`, when init exits under `ferrix.onexit=panic`.
pub(crate) static INIT_EXITED: Explanation = Explanation {
    code: "FX-1501",
    title: "init exited, and ferrix.onexit=panic asked for a panic",
    meaning: "The program the kernel started as pid 1 -- the one `ferrix.init=` named, or the \
              one built in -- has exited, and the command line carries `ferrix.onexit=panic`, \
              which asks for what Linux does when init dies. Without the option the machine \
              commits its disks and powers off, and with `ferrix.onexit=reset` it resets. \
              The disks were committed before this panic, as they are before a power-off.",
    causes: &[
        "Init ran to its end: a shell whose session was ended with `exit`, or a script that \
         finished.",
        "Init failed and exited: the `init ... exited with` line above this report gives its \
         status, and its own output above that says why.",
        "Nothing could be started at all: no program is built in, `ferrix.init=` named a file \
         that would not start, and the image carries no /sbin/init.",
    ],
    see: "src/kernel/src/power.rs finish; src/kernel/src/init.rs run; docs/INIT.md §8.3",
};

/// For `check_init_calls` in `stages_check.rs`, when `syscall::init_calls_check::run`
/// fails.
pub(crate) static INIT_CALLS: Explanation = Explanation {
    code: "FX-1502",
    title: "a kernel call init needs did not do what docs/INIT.md §11 says",
    meaning: "`syscall::init_calls_check::run` checks the kernel items of init's landing L8 \
              (docs/INIT.md §11). K2: the channel `init::bootstrap_channel` makes for pid \
              1, given as `exec::run_init` gives it, must hold exactly one message for \
              `process_bootstrap` to find -- the 8-byte hello `FXIN` version 1 with no \
              handle -- with the kernel's end open, and a program given it must take it by \
              number, close it and exit 0, the kernel's end hearing the close. K6: \
              `process_status` (0x1034) must read a process nothing ended as running, one a \
              SIGTERM ended as killed by 15, one its job's kill ended as killed by 9, and a \
              program that exited 42 as exited with 42; and refuse a handle without WAIT \
              (ACCESS_DENIED), a channel (WRONG_TYPE) and an unmapped answer (FAULT). K4: \
              `port_fd` (0x101B) must give a descriptor, close-on-exec when asked, that \
              polls readable only while the port has a packet; an epoll_wait of five \
              seconds on it must be woken, by the port's queue and within 900 ms, by a \
              packet a kernel task queues 50 ms in, and report EPOLLIN with its cookie; it \
              must go quiet once port_wait takes the packet, read as EINVAL, keep the port \
              after the handle is closed, and refuse an unknown flag, a channel and a handle \
              without WAIT. K3: \
              `process_give` (0x1032) must move a handle out of the \
              caller's table into its own child's bootstrap slot, and `process_bootstrap` \
              (0x1033) must answer that handle once, with its rights, and zero after and in a \
              process given nothing. A give must be refused with NOT_CHILD into another's \
              child, NO_PROCESS for a pid naming nothing, ACCESS_DENIED without TRANSFER, \
              BAD_HANDLE for handle zero, ALREADY_BOUND a second time, and BAD_STATE to a \
              child that completed an execve with nothing given or that has ended, each \
              leaving the handle with the caller. A handle given before an execve must be \
              there after it, and one never taken must be closed when the child ends. Then a \
              fork of a loaded program, given a channel end, must execve /tmp/k3-exec, take \
              its bootstrap by number, close it and exit 0, the kept end hearing the close; \
              its parent, given nothing, must exit with the close's EBADF (247).",
    causes: &[
        "`fs/portfd.rs` offers `poll` a queue the port does not wake, or none, so a wait \
         ends only at its own next look; or a way of queueing a packet in \
         `object/port.rs` no longer wakes the port's queue.",
        "`process_status` (`syscall/native.rs`) read the status without the signal the \
         personality records beside it (`Exit::record`), so a killed process reads as \
         exited with 128 plus the signal, or it reads an exit before the process ended.",
        "`init::bootstrap_channel` no longer writes the hello from `src/lib/proto/native-abi`'s \
         `bootstrap` module, or writes more than one message, or `exec::give_bootstrap` \
         does not put the program's end in the new process's slot.",
        "`process_give` (`syscall/launch.rs`) judged the parent by something other than the \
         child's own parent pointer, or `give_bootstrap` (`syscall/native.rs`) copied the \
         handle rather than removing it, or moved it before checking the slot.",
        "`execve` did not seal the slot (`Process::mark_execed` calling `seal_bootstrap`), \
         or sealed a slot already holding a handle, which the new program then cannot take.",
        "A process's release does not dispose of an untaken bootstrap \
         (`close_bootstrap`), so the peer never hears PEER_CLOSED.",
        "The program's machine code (`arch::USER_BOOTSTRAP_PROGRAM`) does not make the calls \
         by the numbers `src/lib/proto/native-abi` gives, or the dispatcher does not route a native \
         number from a Linux program.",
    ],
    see: "src/kernel/src/syscall/init_calls_check.rs; src/kernel/src/init.rs; \
          src/kernel/src/syscall/native.rs; src/kernel/src/syscall/launch.rs; \
          src/kernel/src/object/process.rs; src/lib/proto/native-abi/src/nr.rs; \
          src/kernel/src/fs/portfd.rs; src/lib/proto/native-abi/src/bootstrap.rs; \
          docs/INIT.md §6, §9, §11, §16",
};

/// For `handle_page_fault` in `trap.rs`, the `unhandled page fault` report.
pub(crate) static UNHANDLED_PAGE_FAULT: Explanation = Explanation {
    code: "FX-9001",
    title: "a page fault the kernel cannot resolve",
    meaning: "The trap path resolves one kind of page fault: a kernel access to an unmapped \
              page inside the on-demand window, which it maps with a fresh zeroed page before \
              letting the instruction retry. Every other fault, whether an unmapped address \
              elsewhere, a write to a read-only page, a fetch from a non-executable one, or \
              any fault from user mode, is a bug and stops the processor. The report's `page \
              fault at` line gives the address, the kind of access and whether a mapping \
              existed, and the architecture's saved registers follow.",
    causes: &[
        "Kernel code followed a null or otherwise invalid pointer; once the identity map is \
         dropped, address zero translates to nothing on purpose.",
        "A kernel stack overflowed into the unmapped guard page below it; on x86-64 the fault \
         cannot be pushed onto that stack and arrives as a double fault instead (FX-9004).",
        "Code used a vmap allocation after freeing it, or a device window after unmapping it.",
        "Kernel code touched a user address in an installed address space before faulting the \
         page in with `AddressSpace::fault`; the trap path consults no address space, only \
         the on-demand window.",
        "A fault in the on-demand window found no free frame to map.",
        "On AArch64 every data or instruction abort that is not a permission fault is \
         reported as not mapped, an external abort from a device address included; the \
         fault status in `esr` says which it was.",
    ],
    see: "src/kernel/src/trap.rs handle_page_fault; src/kernel/src/mm.rs map_demand_page; \
          src/kernel/src/vmap.rs; src/kernel/src/arch/aarch64/trap.rs abort",
};

/// For `dispatch` in `trap.rs`, the `system call before stage 7` report.
pub(crate) static SYSTEM_CALL_TRAP: Explanation = Explanation {
    code: "FX-9002",
    title: "a system call the trap path cannot carry out",
    meaning: "On AArch64 and ARMv7-A a system call is `svc`, an exception like any other, and \
              the trap path hands it to the architecture's `system_call`, which reads the \
              arguments from the saved registers, dispatches it and writes the answer back. \
              That fails in two ways, and either one ends here: the call came from the kernel \
              rather than from a program, or it was an `execve`, which that path cannot yet \
              carry out. The headline is the reason `system_call` gave. On x86-64 a system \
              call never comes this way, because `SYSCALL` has an entry of its own.",
    causes: &[
        "Kernel code executed `svc` (the headline says `from EL1` or `from SVC mode`); nothing \
         in the kernel is meant to.",
        "A user program called `execve` on an Arm architecture, and `execve` through the trap \
         path is not wired yet (the headline says so).",
    ],
    see: "src/kernel/src/trap.rs dispatch; src/kernel/src/arch/aarch64/trap.rs system_call; \
          src/kernel/src/arch/armv7a/trap.rs system_call; docs/ROADMAP.md stage 7",
};

/// For `dispatch` in `trap.rs`, the `illegal instruction` report.
pub(crate) static ILLEGAL_INSTRUCTION: Explanation = Explanation {
    code: "FX-9003",
    title: "the processor refused to execute an instruction",
    meaning: "The processor raised its undefined-instruction exception: invalid opcode on \
              x86-64, an exception of unknown reason on AArch64, undefined instruction on \
              ARMv7-A. The kernel emulates no instruction, so it stops. The saved program \
              counter in the report is the instruction that was refused.",
    causes: &[
        "Execution reached bytes that are not an instruction, through a corrupted return \
         address or function pointer; check whether the saved program counter lies inside \
         the kernel image.",
    ],
    see: "src/kernel/src/trap.rs dispatch; src/kernel/src/arch/x86_64/trap.rs classify; \
          src/kernel/src/arch/aarch64/trap.rs classify; src/kernel/src/arch/armv7a/trap.rs classify",
};

/// For `dispatch` in `trap.rs`, the report naming any other exception.
pub(crate) static UNEXPECTED_EXCEPTION: Explanation = Explanation {
    code: "FX-9004",
    title: "a processor exception the kernel has no handler for",
    meaning: "The trap path handles breakpoints, page faults and interrupts; every other \
              exception stops the kernel, with the architecture's own name for it on the \
              report's first line and the saved registers below. On x86-64 that is a vector \
              such as the double fault or the general protection fault; on AArch64 an \
              exception class such as an SError or a trapped system register access; on \
              ARMv7-A an alignment fault, an FIQ, or a data or prefetch abort that is not a \
              translation, access-flag or permission fault.",
    causes: &[
        "On x86-64, a double fault: a fault while delivering another, usually because the \
         kernel stack was unusable, as after an overflow into its guard page. It runs on a \
         stack of its own so that it can be reported at all.",
        "On ARMv7-A, a data abort with an external abort status, from an access to an \
         address with no device behind it; the `fsr` in the report gives the status.",
    ],
    see: "src/kernel/src/trap.rs dispatch; src/kernel/src/arch/x86_64/trap.rs vector_name; \
          src/kernel/src/arch/aarch64/trap.rs class_name; src/kernel/src/arch/armv7a/trap.rs abort",
};

/// For `ferrix_paranoid_entry` in `arch/x86_64/paranoid.rs`, a machine check.
pub(crate) static MACHINE_CHECK: Explanation = Explanation {
    code: "FX-9005",
    title: "the processor reported a hardware error",
    meaning: "x86-64 raises the machine check, vector 18, when the processor or its memory \
              finds an error it cannot correct: an uncorrectable ECC error, a bus or cache \
              failure. The kernel reads no machine-check banks and recovers nothing, so it \
              stops. The report is made on the machine check's own interrupt stack with the \
              kernel's GS, whatever the processor was running, the system call entry's \
              instructions on the program's stack included. `from` in the report says whether \
              a program or the kernel was interrupted; that says nothing about the cause.",
    causes: &[
        "Failing memory or a failing processor; on a virtual machine, a host that forwarded \
         an error, or a machine check injected through the monitor.",
        "Kernel code executed `int $18`, which enters through the same gate; the saved \
         instruction pointer then lies in the kernel image, just after it.",
    ],
    see: "src/kernel/src/arch/x86_64/paranoid.rs; src/kernel/src/arch/x86_64/trap.rs \
          ferrix_paranoid_common",
};

/// For `ferrix_paranoid_entry` in `arch/x86_64/paranoid.rs`, a second entry
/// onto an interrupt stack its first is still using.
pub(crate) static NESTED_INTERRUPT_STACK: Explanation = Explanation {
    code: "FX-9006",
    title: "an exception nested on its own interrupt stack",
    meaning: "On x86-64 the NMI, the debug exception, the machine check and the double fault \
              each switch to the top of a stack of their own, per processor, unconditionally. \
              A second one of the same kind taken while the first is still being handled \
              starts at that same top, so it has already overwritten the first one's saved \
              registers and part of its handler's stack, and the first could never return. \
              The entry counts the occupants of each stack and stops here on a second. The \
              registers in the report are the second exception's.",
    causes: &[
        "A handler on one of these stacks took an exception that returned, which lets the \
         processor deliver another NMI before the first one's `iretq`; an NMI handler must \
         not fault, not even on a page the demand window would map.",
        "A hardware breakpoint on the paranoid entry's own instructions, before they clear \
         `DR7`, or on an interrupt stack; nothing may arm one there.",
        "A double fault inside the double-fault handler: that handler's report itself \
         overflowed or faulted.",
    ],
    see: "src/kernel/src/arch/x86_64/paranoid.rs; src/kernel/src/arch/x86_64/trap.rs \
          ferrix_paranoid_common; src/kernel/src/arch/x86_64/gdt.rs",
};

/// Every entry, in code order.
#[expect(
    dead_code,
    reason = "tools/common/gen/gen-panic-catalog.py checks every entry is listed here; the kernel names entries directly"
)]
pub(crate) static ALL: &[&Explanation] = &[
    &SHOOTDOWN_TIMEOUT,
    &GRACE_PERIOD_TIMEOUT,
    &SHOOTDOWN_TURN_TIMEOUT,
    &SPACE_SET_WITHOUT_RECORD,
    &PRIVATE_OBJECT_SHARED,
    &LOAD_REGISTRATION,
    &BOOT_OUT_OF_MEMORY,
    &ALLOCATION_ABORTED,
    &STAGE1_HANDOFF,
    &MEMORY_BRING_UP,
    &VMAP_ARENA_BRING_UP,
    &STAGE2_ALLOCATORS,
    &STAGE2_FINISH_MEMORY,
    &STAGE3_TRAPS,
    &STAGE3_TIMER,
    &INTERRUPT_BRING_UP,
    &TIMER_REGISTRATION,
    &CONSOLE_INPUT,
    &RANDOM_GENERATOR,
    &SPECULATION_DEFENCES,
    &STAGE3_MACHINE,
    &AUDIT_STORE,
    &PROCESSOR_DISCOVERY,
    &SECONDARY_START,
    &SECONDARY_GDT,
    &SECONDARY_NO_RECORD,
    &SECONDARY_RECORD_MISMATCH,
    &PROCESSORS_MISSING,
    &STAGE4_SMP,
    &SCHEDULER_BRING_UP,
    &STAGE5_SCHEDULER,
    &SCHEDULE_WITH_PREEMPTION_HELD,
    &CONSOLE_OUTPUT,
    &CONSOLE_LOG,
    &STAGE6_USER_MEMORY,
    &STAGE6_REVERSE_MAP,
    &STAGE7_SYSCALLS,
    &STAGE7_SEMAPHORES,
    &STAGE7_SHARED_MEMORY,
    &STAGE8_ROOT,
    &STAGE8_FILESYSTEM,
    &CONSOLE_DESCRIPTORS,
    &STAGE8_PATH_CALLS,
    &STAGE8_PSEUDO_FILESYSTEMS,
    &SYSRQ_CRASH,
    &STAGE8_PIPES_AND_FILESYSTEM_CALLS,
    &STAGE8_FILE_MAPPINGS,
    &STAGE8_PROGRAM_FILES,
    &STAGE8_MADVISE,
    &STAGE8_MEMFD,
    &STAGE8_EPOLL,
    &STAGE8_EVENTFD,
    &STAGE8_TIMERFD,
    &STAGE8_SIGNALFD,
    &STAGE8_MOUNT_FLAGS,
    &STAGE8_BINDS,
    &STAGE13_MOUNT_NAMESPACES,
    &STAGE13_USER_NAMESPACES,
    &SYSFS,
    &STAGE13_PID_NAMESPACES,
    &STAGE13_SMALL_NAMESPACES,
    &STAGE9_OBJECTS,
    &STAGE9_ALLOCATION,
    &STAGE9_REFUSALS,
    &SERVICES,
    &STAGE9_QUOTAS,
    &STAGE9_KMEM,
    &SPECULATION_DOMAIN_LEAVE_MAY_NOT_WAIT,
    &STAGE10_PCI,
    &STAGE10_DEVICES,
    &STAGE10_IOMMU,
    &STAGE10_RING,
    &STAGE10_DRIVER,
    &STAGE10_DEVMGR,
    &STAGE10_DMA_FAULT,
    &LOG_CONTROL,
    &DEVMGR_BY_INIT,
    &STAGE10_DISTRIBUTOR,
    &STAGE11_MOUNT,
    &NET_CORE,
    &NET_RING,
    &NETLINK,
    &STAGE12_WRITE,
    &ROOT_PID1,
    &STAGE13_CGROUPFS,
    &STAGE13_SECCOMP,
    &INIT_EXITED,
    &INIT_CALLS,
    &UNHANDLED_PAGE_FAULT,
    &SYSTEM_CALL_TRAP,
    &ILLEGAL_INSTRUCTION,
    &UNEXPECTED_EXCEPTION,
    &MACHINE_CHECK,
    &NESTED_INTERRUPT_STACK,
];
