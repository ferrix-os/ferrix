#!/usr/bin/env python3
"""A boot check on a worker reaches no machine-wide count or wait (ledger 7044 Q2).

`stages_check.rs`'s WORKER_CHECKS runs checks beside the boot task. A check that reads a
count of the whole machine, or waits for it to be quiet, would read another check's work,
so none in the table may reach one: from each entry's function, through the functions
of the kernel's check files that it names, helpers included, none of FORBIDDEN appears.
A name followed by file and function, not by type: it errs toward refusing. A tool of
the same class as check-first-rows, and separate code from what it reads.
Usage: check-boot-workers.py [--self-test]
"""
import re
import sys
sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))
import kernel_roots  # noqa: E402  (docs/microkernel/CRATE-SPLIT.md S0)
from pathlib import Path

ROOT = Path("src/kernel/src")
TABLE = ROOT / "stages_check.rs"
FORBIDDEN = (
    "FrameWindow",
    "free_frames",
    "held_frames",
    "live_slots",
    "wait_until_reaper_quiet",
    "bandwidth_walks",
    "sleepers_here",
    "PLACED_ELSEWHERE",
    "audit::read",
    "high_next",
    "low_next",
    "log::read",
    "log::line_from",
    "lines_begun",
    "LINE_STARTS",
)
FUNCTION = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s+([a-zA-Z_0-9]+)", re.M
)
CALL = re.compile(r"\b(?:([a-z_][a-z0-9_]*)::)?([a-z_][a-z0-9_]*)\s*(?:::<[^>]*>)?\(")


def functions(path: str, text: str) -> list[tuple[str, str, str]]:
    """Each function of a file: its name, the file, its body up to the next one."""
    found = list(FUNCTION.finditer(text))
    out = []
    for index, match in enumerate(found):
        end = found[index + 1].start() if index + 1 < len(found) else len(text)
        body = re.sub(r"//[^\n]*", "", text[match.end() : end])
        out.append((match.group(1), path, body))
    return out


def reaches(start: str, files: dict[str, str]) -> set[str]:
    """What of FORBIDDEN function `start`, in the check files, reaches."""
    known: dict[str, list[tuple[str, str]]] = {}
    for path, text in files.items():
        for name, where, body in functions(path, text):
            known.setdefault(name, []).append((where, body))
    seen: set[tuple[str, str]] = set()
    found: set[str] = set()
    queue = [(where, body, start) for where, body in known.get(start, [])]
    while queue:
        where, body, name = queue.pop()
        if (where, name) in seen:
            continue
        seen.add((where, name))
        found.update(word for word in FORBIDDEN if word in body)
        for module, called in CALL.findall(body):
            for other, other_body in known.get(called, []):
                same = other == where
                named = module and (f"/{module}." in "/" + other or f"/{module}/" in "/" + other)
                if same or named:
                    queue.append((other, other_body, called))
    return found


def entries(text: str) -> list[str]:
    """The functions WORKER_CHECKS names."""
    at = text.find("static WORKER_CHECKS")
    if at < 0:
        return []
    end = text.find("];", at)
    return re.findall(r"run:\s*([a-z_][a-z0-9_]*)", text[at:end])


def check_files() -> dict[str, str]:
    """The kernel's check files."""
    out = {}
    paths = [p for d in kernel_roots.roots() for p in Path(d).rglob("*.rs")]
    kernel_roots.note_walked("check-boot-workers", [p.resolve() for p in paths])
    for path in paths:
        if "check" in path.name:
            out[str(path)] = path.read_text(encoding="utf-8")
    return out


def self_test() -> None:
    bad = {"a_check.rs": "fn top() { helper(); }\nfn helper() { let _ = mm::FrameWindow::open(); }\n"}
    assert reaches("top", bad) == {"FrameWindow"}, "a helper's window was not found"
    other = {
        "a_check.rs": "fn top() { other_check::run(); }\n",
        "x/other_check.rs": "pub fn run() { quota::live_slots(); }\n",
    }
    assert reaches("top", other) == {"live_slots"}, "another file's count was not found"
    good = {"a_check.rs": "fn top() { helper(); }\nfn helper() { let _ = 1; }\n"}
    assert not reaches("top", good), "a clean check was refused"
    table = "static WORKER_CHECKS: [W; 1] = [\n    W { name: \"x\", run: a_fn },\n];\n"
    assert entries(table) == ["a_fn"], "the table was not read"
    print("check-boot-workers: self-test: a helper's window and another file's count FIRED, clean passed")


def main() -> int:
    if "--self-test" in sys.argv:
        self_test()
        return 0
    text = TABLE.read_text(encoding="utf-8")
    table = entries(text)
    if not table:
        print("check-boot-workers: no WORKER_CHECKS read")
        return 1
    files = check_files()
    failed = 0
    for entry in table:
        found = reaches(entry, files)
        if found:
            print(f"check-boot-workers: {entry} runs on a worker and reaches {sorted(found)}")
            failed += 1
    if failed:
        return 1
    print(f"check-boot-workers: {len(table)} checks on workers, none reaches a machine-wide count or wait")
    return 0


if __name__ == "__main__":
    sys.exit(main())
