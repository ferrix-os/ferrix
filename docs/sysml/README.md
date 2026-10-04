# The SysML v2 model

`docs/ARCHITECTURE.md` says what is being built and `docs/ROADMAP.md` in what
order. This directory says the same thing as a model: one set of SysML v2
packages, in the textual notation, covering both what runs today and what the
roadmap still owes, with every element marked as one or the other.

The prose documents remain the source of truth. The model is a structured
index over them — requirements with stable ids, parts with the connections
between them, the boot sequence as an action flow, the scheduling-domain mode
switch as a state machine, the gates as verification cases — for the reader
who wants the shape of the system without the argument for it, and for
tooling that can check a model for consistency where it cannot check prose.

## Files

| File | Package | What it holds |
|---|---|---|
| `00-lifecycle.sysml` | `FerrixLifecycle` | The keywords every other element carries: `#implemented`, `#inProgress`, `#writtenAhead`, `#planned`, `@deferred { reason = "..."; }`, and `@stage { number = n; }`. |
| `01-requirements.sysml` | `FerrixRequirements` | The goal read as a specification (`G`, `G.1`–`G.8`), the design rules (`P.1`–`P.17`, including the commit conventions), and the promises deliberately not made (`N.1`–`N.3`). |
| `02-structure.sysml` | `FerrixStructure` | The machine context, the loader, the kernel and its parts, the userland, the architecture facade with its three variants, and the workspace crates with their dependency edges. |
| `03-boot.sysml` | `FerrixBoot` | The hand-off structure, both address layouts, the loader's sequence, the kernel's bring-up with every stage's self-check through stage 12, and the trap path. |
| `04-memory.sysml` | `FerrixMemory` | Frames, heap, paging, the vmap arena, the VMA map, VMOs, process address spaces, demand paging, copy-on-write and file mappings (today); reclaim (ahead). |
| `05-scheduling.sysml` | `FerrixScheduling` | Locks, interrupts, time, SMP, tasks, the fair class, placement and balancing, one Throughput scheduling domain, and futex (today); the real-time classes and the mode-switch state machine (ahead). |
| `06-objects.sysml` | `FerrixObjects` | The nine kernel objects, clone's sharing set, the Linux ABI layer and the native ABI. |
| `07-isolation.sysml` | `FerrixIsolation` | Credentials (today); cgroups v2 and the namespaces' mount groundwork (in progress); the namespaces themselves and seccomp (ahead). |
| `08-drivers.sysml` | `FerrixDrivers` | Table access and MMIO, device nodes, IOMMU domains, `devmgr`, the driver processes, the shared rings and the bootstrap sequence. |
| `09-storage.sysml` | `FerrixStorage` | Block core, VFS, page cache, the small filesystems, btrfs in three stages (the third, subvolumes, ahead), and the net core. |
| `10-roadmap.sysml` | `FerrixRoadmap` | Stages 0–22, the ARMv7-A port, networking and dynamic linking as requirements with status and exit criteria, their ordering, what satisfies each, and the boot tests that verify the done ones. |
| `11-assurance.sysml` | `FerrixAssurance` | Every gate `cargo xtask check` and CI run, the xtask boot gates, the commit-authorship check, the assembly budget, what each layer's tests can reach, and the verification later stages owe. |
| `12-views.sysml` | `FerrixViews` | Views filtering the one model into current, written-ahead, future and deferred. |
| `13-item-requirements.sysml` | `FerrixItemRequirements` | The certified item's high-level requirements (`H.<AREA>.<n>`), each with a statement, a pass/fail criterion and its parent objective; the definitions the low-level ones (`L.<module>.<n>`) use. `tools/common/check/check-traceability.py` holds them to their checks. |
| `14-object-requirements.sysml` | `FerrixObjectRequirements` | The low-level requirements of `src/kernel/src/object/` (`L.object.<n>`), the pilot of W-8: each names the functions that carry it as its `unit` and the `H.*` it refines. One file per subsystem follows it. |
| `15-sched-requirements.sysml` | `FerrixSchedRequirements` | The low-level requirements of `src/kernel/src/sched/` (`L.sched.<n>`), started with the two FX-0502's fix created: a decision within a slice while anything waits, and a lone yield that asks for nothing. |
| `16-iommu-requirements.sysml` | `FerrixIommuRequirements` | The low-level requirements of `src/kernel/src/iommu.rs` and `src/kernel/src/iommu/` (`L.iommu.<n>`): where firmware puts each PCI function's DMA, the units translation is turned on for, the domains a driver pins pages into, the gate a wait on a unit goes through, and the faults a unit records, in the pilot's format. |
| `17-memory-requirements.sysml` | `FerrixMemoryRequirements` | The low-level requirements of the item's memory management: `L.mm.<n>` for the kernel's own (`mm.rs`, `mm/`, `vmap.rs`, `early.rs`) and `L.user.<n>` for a program's (`user/vmo.rs`, `user/space.rs`), in the pilot's format. |
| `18-x86-64-requirements.sysml` | `FerrixX8664Requirements` | The low-level requirements of `src/kernel/src/arch/x86_64/` (`L.x86_64.<n>`), `src/kernel/src/trap.rs` (`L.trap.<n>`) and the system call dispatcher (`L.syscall.<n>`): descriptors and user selectors, the context switch, starting processors, the paranoid entries, speculation, timers, both ABIs' signal frames, SYSCALL and int $0x80, exceptions and dispatch, in the pilot's format. |
| `19-aarch64-requirements.sysml` | `FerrixAarch64Requirements` | The low-level requirements of `src/kernel/src/arch/aarch64/` (`L.aarch64.<n>`): the trap path and signal frames, the context switch, the GICv3 and its ITS, the generic timer, the boot console, translation, starting the processors, the speculation defences and what firmware is asked for, in the pilot's format. |
| `20-device-requirements.sysml` | `FerrixDeviceRequirements` | The low-level requirements of a device's claim, its node and its quiesce: `src/kernel/src/claim.rs`, `src/kernel/src/device.rs` and the quiesce in `src/kernel/src/syscall/native.rs`, in the pilot's format. |
| `21-armv7a-requirements.sysml` | `FerrixArmv7aRequirements` | The low-level requirements of `src/kernel/src/arch/armv7a/` written so far (`L.armv7a.<n>`): what the supervisor-call path does for the filter the core asks about every call (seccomp), in the pilot's format. |
| `22-smp-requirements.sysml` | `FerrixSmpRequirements` | The low-level requirements of `src/kernel/src/smp.rs` (`L.smp.<n>`): the processors and their records, bring-up, the whole-TLB and scoped shootdowns and the bounds on their waits, grace periods, stopping for a panic and the scheduler's kick, in the pilot's format. |
| `23-console-requirements.sysml` | `FerrixConsoleRequirements` | The low-level requirements of `src/kernel/src/console.rs` and `src/kernel/src/console/` (`L.console.<n>`): whole lines to the port, the failure report's way past a held lock, the recent-output ring, the transmit and receive rings, the kernel log and what it keeps out, and the screen console; with the two console-choice parsers of the Arm ports, in the pilot's format. |
| `24-btrfs-requirements.sysml` | `FerrixBtrfsRequirements` | The low-level requirements of the two btrfs crates in the item, `ferrix-btrfs` and `ferrix-btrfs-write` (`L.btrfs.<n>`), units named from each crate's src/ led by the crate's name: the reader's parsers and their totality over any image, nodes and data sectors read from a copy that passes its checks, the commit's order and copy-on-write, the log, the allocator and its bookkeeping, and a full volume's refusals, in the pilot's format. |
| `25-init-requirements.sysml` | `FerrixInitRequirements` | The low-level requirements of pid 1's inputs (`L.init.<n>`): the order init starts pid 1 in, the program, script and commands taken once from the boot initramfs, the refusals of an input that cannot be one, and the unpacker that never creates them, in the pilot's format. |

## Reading it

Every definition and usage carries a maturity keyword:

* `#implemented` — the code exists and a QEMU boot exercises it: the boot
  test itself, or a named `cargo xtask test-*` gate.
* `#inProgress` — the owning stage has started; part of the element runs.
* `#writtenAhead` — a `src/lib/` crate exists and passes host tests, but no
  gate exercises it yet through the kernel or a program in the image.
* `#planned` — the design exists in `docs/ARCHITECTURE.md` and nothing else.
* `@deferred { reason = "..."; }` — a finished stage explicitly left it behind; the attribute
  carries the stage's reason.

`@stage { number = n; }` names the roadmap stage that owns the element. The
short names in angle brackets — `<'G.4'>`, `<'P.6'>`, `<'S12'>` — are stable
ids the packages cite across files.

Hexadecimal addresses are `String` attributes because the notation has no
hexadecimal literal; the two layouts in `03-boot.sysml` are the same
constants `src/lib/proto/bootinfo` checks at compile time.

## The generated document

`cargo xtask model-doc` reads these files and writes four kinds of output,
all committed:

| Output | What it is |
|---|---|
| `docs/generated/ARCHITECTURE.md` | The document, readable on GitHub, diagrams included. |
| `docs/generated/architecture.html` | The same, standalone — no network, no CDN, opens from a `file://` path. |
| `docs/generated/model.json` | The parsed model: every element with its kind, maturity, stage, doc, successions and children, plus every relation and every figure as a graph. |
| `docs/generated/diagrams/*.svg` | One file per figure, for embedding anywhere else. |

They are generated, so **do not edit them**. `cargo xtask check` and the CI job
*Architecture document* regenerate and compare; a model edited without
regenerating fails, and the fix is to run the command and commit the result.
The outputs are byte-identical for the same input — nothing writes a timestamp,
a hostname or a tool version — so a difference is always a real difference.

`model.json` exists so that nothing else has to learn the notation. A tool that
wants to assert something about the model — that no `#implemented` element
belongs to a stage still marked `Planned`, say — reads that file. It carries
the figures too, as nodes and edges with the qualified names they came from, so
a tool that wants to draw the model its own way needs neither the notation nor
the layout.

## The diagrams

The document draws what the model is already shaped like: an action's
successions as a flow, a state definition's transitions as a state machine,
`:>` as a generalisation, `connect` as an interface diagram, `dependency` as a
graph, and `satisfy` / `allocate` / `verify` as traceability. Nothing is
positioned by hand and no figure is written down: `diagrams.py` looks for the
shape, so a stage added to the roadmap, a crate added to the workspace or a
subtype added to `KernelObject` turns up in its figure with no code change.

Each figure is drawn twice from one graph:

* **Markdown** carries it as a ```mermaid fence, which GitHub renders itself —
  theme-aware, zoomable, and selectable as text in the raw file.
* **HTML** carries it as inline SVG, laid out by `layout.py`, because that page
  has to open from a `file://` path with no network. The same SVG is written to
  `docs/generated/diagrams/` for use on its own.

Colour says one thing and only one: the maturity keyword the element carries.
Shape says what kind of element it is, and the arrowheads are UML's — a hollow
triangle for `:>`, a filled diamond for composition, a dashed open arrow for
the trace relations.

There is no Graphviz and no Mermaid CLI in the pipeline, for the same reason
there is no SysML parser: a gate that needs an `apt` or a `pip` is a gate that
does not run on a stock checkout. `layout.py` is a layered layout in stdlib
Python — break cycles, assign layers, insert a waypoint per layer an edge
crosses, order by neighbour averages, then place — with every iteration count
fixed and every sort stable, so the same model always produces the same
coordinates and the committed output is comparable byte for byte.

## The generator

```
tools/common/gen/gen-arch-doc.py       the command
tools/common/gen/sysml/parser.py       SysML v2 text  -> model.Model
tools/common/gen/sysml/model.py        the element tree and the queries over it
tools/common/gen/sysml/sections.py     model.Model    -> document.Doc      <- start here
tools/common/gen/sysml/document.py     the format-neutral document
tools/common/gen/sysml/diagrams.py     model.Model    -> figure.Figure     <- and here
tools/common/gen/sysml/figure.py       the format-neutral graph
tools/common/gen/sysml/layout.py       figure.Figure  -> coordinates
tools/common/gen/sysml/render_*.py     document.Doc   -> Markdown / HTML / SVG / Mermaid
tools/common/gen/sysml/emit_json.py    model.Model    -> JSON
tools/common/gen/sysml/tests.py        all of the above
```

Stdlib Python, no dependencies: a gate that needs a `pip install` is a gate
that does not run on a stock checkout, which is the argument `P.17` makes.

**To add a part of the document**, write a function in `sections.py` that takes
the model and the document and appends blocks, and append it to `SECTIONS` at
the bottom. Markdown, HTML and the table of contents all gain it; no renderer
changes. Two rules the existing sections keep:

* Say what the model says. Prose that is neither in the model nor a caption
  explaining how to read a table belongs in `docs/ARCHITECTURE.md`.
* Never write a count by hand. Every number in the document comes from walking
  the model, because a generated document is exactly where nobody thinks to
  check one.

**To add a figure**, write a builder in `diagrams.py` that returns a
`figure.Figure` — or none, when the model does not hold enough to draw one —
and call it from the section that should carry it with `place(doc, ...)`. Both
renderings and the standalone file follow; no renderer and no layout code needs
to know what a state machine is. Two rules the existing builders keep:

* Every node is an element the model declares and every edge is a relation it
  writes down. A box that is not in the model is a picture of nothing, and the
  tests fail on one.
* Find figures by shape, not by name. `every action with a succession` keeps
  working as the model grows; a list of names goes stale the first time
  somebody adds a package.

**To support new notation**, teach `parser.py`. It reads the subset these files
use, not the whole language, and it fails loudly rather than quietly: a
declaration it does not recognise is collected in `Model.unparsed`, and the
generator refuses to write a document while any exist. If that fires, the
message names the file and line. `--lenient` generates anyway, for when you
want to see how far it gets.

## Other SysML v2 tooling

The files are plain SysML v2 textual notation and load together as one model in
any implementation of the language. None of these is part of this tree's
toolchain or runs in CI:

* **`sysmlpy`** (PyPI, pure Python). Every file parses. Its `analyze` pass
  reports false positives on valid notation it does not implement — wildcard
  imports across packages, port conjugation (`~Port`), enum literals in
  expressions, sequence literals, and `satisfy ... by a.b` feature chains — so
  only `parse` is meaningful here.

  ```
  python3 -m venv .venv && .venv/bin/pip install sysmlpy
  for f in docs/sysml/*.sysml; do .venv/bin/sysmlpy parse "$f" >/dev/null || echo "FAIL $f"; done
  ```

* **`syside`** (PyPI, Sensmetry's Automator) is a full implementation but needs
  a paid licence key; the free Syside Editor is a VS Code extension.
* The **pilot implementation** (Systems-Modeling on GitHub) ships a Jupyter
  kernel that needs Java 21 and is installed from its release zip with
  `python3 install.py`.

## Keeping it current

The model changes when the documents it indexes change, in the same commit:

* a stage finishing flips its requirement in `10-roadmap.sysml` to `Done`,
  moves its parts from `#planned` to `#implemented`, and records anything the
  stage deferred with the reason the roadmap gives;
* a new `src/lib/` crate is a new `#writtenAhead` part in
  `FerrixStructure::Workspace`, with its dependency edge;
* a new architecture is a fourth variant of `ArchLayer` and a fourth boot
  test, and nothing else — which is the point of the facade.

Whatever the change, run `cargo xtask model-doc` in the same commit so
`docs/generated/` moves with it. `cargo xtask check` refuses otherwise.
