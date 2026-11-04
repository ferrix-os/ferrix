"""The parsed model: one element tree, plus the relations laid over it.

This is deliberately *not* a SysML v2 metamodel. A faithful one would need the
whole standard library and the whole type system, and the document this package
generates needs neither. What it needs is what the model actually writes down:
an element with a kind, a name, a maturity keyword, a stage, a doc comment, a
type, and children -- plus the handful of relations (`satisfy`, `verify`,
`allocate`, `dependency`) the roadmap and assurance packages draw between them.

So `Element` is one shape for every declaration, with the fields that are
absent left empty, and `Model` is the index over them. An element whose header
the parser did not recognise still lands here with its raw text intact and
`kind` left empty: the generator counts those and can fail on them, which is
how drift in the notation announces itself rather than silently dropping a
section from the document.

Names are qualified the way the model cites them -- `FerrixStructure::Kernel`
-- and every lookup and every iteration is sorted or insertion-ordered, because
the generated document has to be byte-identical for the same input.
"""

from __future__ import annotations

import dataclasses
import re
from typing import Iterator

# The four maturity keywords `00-lifecycle.sysml` defines, in the order a
# reader should meet them: what runs, what is half-landed, what is written but
# unreached, what is only designed.
MATURITIES = ("implemented", "inProgress", "writtenAhead", "planned")

# How each one reads in a document. The model's own README is the source of
# these sentences; they are repeated here because the document has to explain
# its vocabulary before it uses it, and `00-lifecycle.sysml` states them in a
# doc comment the parser does keep -- see `Model.lifecycle_doc`.
MATURITY_LABEL = {
    "implemented": "implemented",
    "inProgress": "in progress",
    "writtenAhead": "written ahead",
    "planned": "planned",
}


@dataclasses.dataclass
class Element:
    """One declaration: a package, a definition, a usage, or an enum literal."""

    kind: str = ""
    """`part`, `action`, `requirement`, `package`, `enum-literal`, ... Empty
    when the parser did not recognise the header; `raw` still holds it."""

    name: str = ""
    short_name: str = ""
    """The stable id in angle brackets: `G.4`, `P.6`, `S12`."""

    is_definition: bool = False
    """True for `part def X`, false for the usage `part x : X`."""

    modifiers: list[str] = dataclasses.field(default_factory=list)
    """`abstract`, `variation`, `variant`, `ref`, `perform`, `exhibit`, ..."""

    typed_by: str = ""
    """The type after `:`."""
    specializes: str = ""
    """The supertype after `:>`."""
    redefines: str = ""
    """The feature after `:>>`."""
    multiplicity: str = ""
    value: str = ""
    """The right-hand side of `=`, as written."""

    keywords: list[str] = dataclasses.field(default_factory=list)
    """Maturity keywords: the `#implemented` family."""

    stage: int | None = None
    """`@stage { number = n; }`, the roadmap stage that owns this element."""
    deferred_reason: str = ""
    """`@deferred { reason = "..."; }`. Non-empty means the element is deferred."""

    doc: str = ""
    """The `doc /* ... */` comment, reflowed to one paragraph per blank line."""

    children: list[Element] = dataclasses.field(default_factory=list)
    flow: list[str] = dataclasses.field(default_factory=list)
    """For an action body: the `first`/`then` step names, in order."""

    successions: list[tuple[str, str, str]] = dataclasses.field(default_factory=list)
    """For an action body: `(from, to, guard)` for every step that follows
    another, including the branches of a `decide`.

    `flow` is the same body read as a straight line, which is what a numbered
    list wants. It is not enough to draw with: an action that decides has two
    ways out, and one that re-chains -- `first mapDemandPage then done` --
    rejoins a step the straight reading has already passed. A guard is the
    condition as the model wrote it, or `else`, or empty for a plain
    succession."""

    parent: Element | None = dataclasses.field(default=None, repr=False)
    source: str = ""
    """The file this came from, repository-relative."""
    line: int = 0
    raw: str = ""
    """The declaration header exactly as written, comments stripped."""

    # -- identity ---------------------------------------------------------

    @property
    def qualified_name(self) -> str:
        """`FerrixStructure::Kernel::mm`, skipping anonymous ancestors."""
        parts: list[str] = []
        node: Element | None = self
        while node is not None:
            if node.name:
                parts.append(node.name)
            node = node.parent
        return "::".join(reversed(parts))

    @property
    def package(self) -> str:
        """The outermost named package this element sits in."""
        node: Element | None = self
        last = ""
        while node is not None:
            if node.kind == "package" and node.name:
                last = node.name
            node = node.parent
        return last

    @property
    def maturity(self) -> str:
        """The first maturity keyword, or "" when the element carries none.

        An element with no keyword of its own is *not* given its parent's: the
        model marks what it means to mark, and inventing inheritance here would
        report a planned field inside an implemented part as implemented.
        """
        for keyword in self.keywords:
            if keyword in MATURITIES:
                return keyword
        return ""

    @property
    def is_deferred(self) -> bool:
        return bool(self.deferred_reason)

    # -- traversal --------------------------------------------------------

    def walk(self) -> Iterator[Element]:
        """This element, then every descendant, depth first, in source order."""
        yield self
        for child in self.children:
            yield from child.walk()

    def of_kind(self, *kinds: str) -> list[Element]:
        return [child for child in self.children if child.kind in kinds]

    def child(self, name: str) -> Element | None:
        for candidate in self.children:
            if candidate.name == name:
                return candidate
        return None

    def attribute_value(self, name: str) -> str:
        """The value of a child attribute, following `:>>` redefinitions.

        `attribute :>> status = StageStatus::Done;` has no name of its own --
        it redefines one -- so both spellings have to be looked at.
        """
        for candidate in self.children:
            if candidate.name == name or candidate.redefines == name:
                return candidate.value
        return ""


@dataclasses.dataclass
class Relation:
    """A `satisfy`, `verify`, `allocate`, `dependency` or `connect` edge."""

    kind: str
    source: str
    targets: list[str]
    origin: str = ""
    """The qualified name of the element the statement was written inside."""
    file: str = ""
    line: int = 0


@dataclasses.dataclass
class Model:
    """Every file, loaded as one model."""

    roots: list[Element] = dataclasses.field(default_factory=list)
    relations: list[Relation] = dataclasses.field(default_factory=list)
    files: list[str] = dataclasses.field(default_factory=list)
    unparsed: list[Element] = dataclasses.field(default_factory=list)
    """Headers the parser did not recognise. The generator reports these."""

    synopses: dict[str, str] = dataclasses.field(default_factory=dict)
    """Each file's header comment: the one-paragraph account of what it holds."""

    digest: str = ""
    """SHA-256 over the inputs, so a document can name what produced it."""

    _named: tuple[list[Element], list[tuple[Element, str]], dict[str, Element]] | None = (
        dataclasses.field(default=None, repr=False, compare=False)
    )
    """The roots this was made from, every element with its qualified name in
    source order, and the first element of each name. `resolve` runs some 8,000
    times over 8,500 elements, and each pass used to rebuild every name from
    its parents: 90% of `sysml/tests.py`'s 25 s (docs/TEST-TIME.md, mk-gpu)."""

    # -- traversal --------------------------------------------------------

    def walk(self) -> Iterator[Element]:
        for root in self.roots:
            yield from root.walk()

    def _names(self) -> tuple[list[tuple[Element, str]], dict[str, Element]]:
        """The names `find` and `resolve` search, in source order.

        Made once for these roots: the parser finishes a tree before anything
        looks a name up, and the one later change, a test's `model.roots = ...`
        over a tree it just parsed, replaces the list and so makes them again.

        Replacing the list is all this notices. After a lookup, `roots.extend`
        leaves `find` saying None for what a walk finds, and `element.name = ...`
        leaves it answering by the old name, while `by_short_name` and the last
        pass of `resolve` still walk the live tree: mixed answers and no error.
        No caller does either today; one that must, replaces `roots`.
        """
        if self._named is None or self._named[0] is not self.roots:
            ordered = [(element, element.qualified_name) for element in self.walk()]
            first: dict[str, Element] = {}
            for element, name in ordered:
                first.setdefault(name, element)
            self._named = (self.roots, ordered, first)
        return self._named[1], self._named[2]

    def packages(self) -> list[Element]:
        return [element for element in self.walk() if element.kind == "package"]

    def package(self, name: str) -> Element | None:
        for element in self.walk():
            if element.kind == "package" and element.name == name:
                return element
        return None

    def find(self, qualified: str) -> Element | None:
        return self._names()[1].get(qualified)

    def by_short_name(self, short: str) -> Element | None:
        for element in self.walk():
            if element.short_name == short:
                return element
        return None

    def resolve(self, reference: str, scope: str = "") -> Element | None:
        """Best-effort lookup of a name as the model writes it.

        References are written unqualified (`stage5Scheduler`), partly
        qualified (`Principles::DesignRule`) or as a feature chain
        (`ferrix.kernel.sched`). Exact qualified names win; then a match inside
        `scope`, then inside each scope containing it; then a suffix match
        anywhere; then the last segment on its own.

        `scope` is the qualified name of the element the reference was written
        inside, and passing it is what makes a name mean what its author meant.
        The roadmap writes `dependency from stage6UserMode to stage5Scheduler`
        and means the requirement two lines up -- but a step of the kernel's
        bring-up action is also called `stage5Scheduler`, and it is declared in
        an earlier file. Without the scope the search finds the step, and the
        roadmap quietly gains an edge into the boot sequence.

        Ambiguity that survives the scope resolves to the first match in source
        order, which is stable for a given input.
        """
        reference = reference.strip()
        if not reference:
            return None
        direct = self.find(reference)
        if direct is not None:
            return direct
        chain = reference.replace("::", ".").split(".")
        tail = chain[-1]
        suffix = "::".join(chain)

        # Outwards from where the reference was written: the element itself,
        # then each element containing it, then its package. `dependency from
        # kernelCrate to acpi` is written inside the workspace and means the
        # crate; a part of the kernel is also called `acpi`, and is declared
        # earlier. Searching the enclosing scopes first is what tells them
        # apart -- and it is what the notation means by an unqualified name.
        prefixes: list[str] = []
        if scope:
            segments = scope.split("::")
            while segments:
                prefixes.append("::".join(segments))
                segments.pop()
        for prefix in prefixes:
            local = self.find(f"{prefix}::{suffix}")
            if local is not None:
                return local
            for element, name in self._names()[0]:
                if name.startswith(prefix + "::") and name.endswith("::" + suffix):
                    return element
            for element, name in self._names()[0]:
                if name.startswith(prefix + "::") and (
                    element.name == tail or element.short_name == tail
                ):
                    return element

        for element, name in self._names()[0]:
            if name.endswith("::" + suffix):
                return element
        for element in self.walk():
            if element.name == tail or element.short_name == tail:
                return element
        return None

    # -- queries the document asks ----------------------------------------

    def with_maturity(self, maturity: str) -> list[Element]:
        return [element for element in self.walk() if element.maturity == maturity]

    def maturity_counts(self) -> dict[str, int]:
        counts = {keyword: 0 for keyword in MATURITIES}
        for element in self.walk():
            if element.maturity:
                counts[element.maturity] += 1
        return counts

    def deferred(self) -> list[Element]:
        return [element for element in self.walk() if element.is_deferred]

    def by_stage(self) -> dict[int, list[Element]]:
        stages: dict[int, list[Element]] = {}
        for element in self.walk():
            if element.stage is not None:
                stages.setdefault(element.stage, []).append(element)
        return dict(sorted(stages.items()))

    def relations_of(self, kind: str) -> list[Relation]:
        return [relation for relation in self.relations if relation.kind == kind]

    def lifecycle_doc(self) -> str:
        """The doc comment `FerrixLifecycle` uses to define its vocabulary."""
        package = self.package("FerrixLifecycle")
        if package is None:
            return ""
        if package.doc:
            return package.doc
        for child in package.children:
            if child.doc:
                return child.doc
        return ""


# Words the model spells as one lower-case token but that a sentence should
# capitalise. Presentation only: nothing here changes what an element *is*, and
# a word missing from this table merely reads as ordinary prose.
ACRONYMS = {
    "abi": "ABI",
    "acpi": "ACPI",
    "apic": "APIC",
    "bpf": "BPF",
    "cbs": "CBS",
    "cpu": "CPU",
    "dma": "DMA",
    "edf": "EDF",
    "eevdf": "EEVDF",
    "elf": "ELF",
    "fdt": "FDT",
    "fifo": "FIFO",
    "gic": "GIC",
    "gicv2": "GICv2",
    "gicv3": "GICv3",
    "io": "I/O",
    "iommu": "IOMMU",
    "ipc": "IPC",
    "irq": "IRQ",
    "lru": "LRU",
    "mmio": "MMIO",
    "os": "OS",
    "pid": "PID",
    "posix": "POSIX",
    "psci": "PSCI",
    "rt": "RT",
    "smp": "SMP",
    "tlb": "TLB",
    "uefi": "UEFI",
    "vfs": "VFS",
    "vma": "VMA",
    "vmo": "VMO",
    "wcet": "WCET",
    "x2apic": "x2APIC",
    "armv7a": "ARMv7-A",
    # Proper nouns and names the project spells a particular way. camelCase
    # capitalises every word, so there is no structural way to tell `Linux`
    # from `Interrupts`; the ones that matter are listed rather than guessed.
    "linux": "Linux",
    "ferrix": "Ferrix",
    "rust": "Rust",
    "rustc": "rustc",
    "btrfs": "btrfs",
    "qemu": "QEMU",
    "miri": "Miri",
    "clippy": "clippy",
    "procfs": "procfs",
    "tmpfs": "tmpfs",
    "devfs": "devfs",
    "cgroupfs": "cgroupfs",
    "sysfs": "sysfs",
    "seccomp": "seccomp",
    "cpio": "cpio",
    "virtio": "virtio",
}

_STAGE_PREFIX = re.compile(r"^stage(\d+)(?=[A-Z]|$)")


def humanise(name: str) -> str:
    """`stage3TrapsInterruptsTime` -> `Stage 3 traps interrupts time`.

    Used only where the model gives no better label than the element's own
    name. Sentence case, not title case: a heading is a phrase, and Title Case
    On Every Word reads like a slide deck.
    """
    if not name:
        return ""
    stage = _STAGE_PREFIX.match(name)
    prefix = ""
    if stage:
        prefix = f"Stage {stage.group(1)} "
        name = name[stage.end() :]
        if not name:
            return prefix.strip()
    spaced = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", " ", name).replace("_", " ")
    words = [word for word in spaced.split() if word]
    out: list[str] = []
    for index, word in enumerate(words):
        lowered = word.lower()
        if lowered in ACRONYMS:
            out.append(ACRONYMS[lowered])
        elif index == 0 and not prefix:
            out.append(word[:1].upper() + word[1:])
        else:
            out.append(lowered)
    text = prefix + " ".join(out)
    return text[:1].upper() + text[1:]
