#!/usr/bin/env python3
"""Every function's frame against a kernel stack's guard (F-100, ledger 7033 C1-C7).

    check-frames.py <kernel ELF> <x86_64|aarch64|armv7a> <present|desktop|...> <dev|release>
                    [--record] [--self-test]

A frame reaches past a guard only if the stack pointer can move past the guard's size with nothing
touching the memory in between.

x86-64 and AArch64: rustc's targets carry inline stack probes, and this says they are there: no
function moves the stack pointer more than a page without a touch at it (a probe's store, a push, a
pre-indexed store), in either form LLVM writes, the steps or the loop. So there is no baseline, and
the size of a frame is not limited. A frame under a page needs no probe of its own: the guard is a
page, and every call leaves a touch of the callee's own, so two touches are never further apart than
one page and the 1,024 bytes LLVM's contract lets a caller leave. The largest unprobed frame is
printed, for that argument to be read against.

ARMv7-A: no probes exist. Below a stack the guard is `STACK_LOWER_GUARD_PAGES` pages, so a frame
larger than that is refused off the boot stack, which is 64 KiB and has no guard to step over, with no
baseline. A frame over a page is in tools/common/data/frame-baseline-armv7a.json with its figure,
whether it is product code or a check, and its stack; a new one or a larger one is refused.
"""
import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path

PAGE = 4096
BASELINE = Path("tools/common/data/frame-baseline-armv7a.json")
# Not a function: the vector table's sixteen entries, each its own `sub sp, sp, #304`, and each one
# followed at once by a store at the new stack pointer.
NOT_FUNCTIONS = {"ferrix_vectors"}
CHECK_NAME = re.compile(r"_check::|::check::|::check_|_check\b|check::")


def read_stack_depth():
    """The frame reader's other half: `check-stack-depth`'s objdump."""
    path = Path(__file__).with_name("check-stack-depth.py")
    spec = importlib.util.spec_from_file_location("check_stack_depth", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def walk(text):
    """Each function's frame, and the most the stack pointer moves with no touch.

    Returns {name: (frame, largest_gap)}.
    """
    found = {}
    name, frame, gap, largest, pending = None, 0, 0, 0, {}

    def close():
        if name is not None:
            found[name] = (frame, largest)

    for line in text.splitlines():
        head = re.match(r"^[0-9a-f]+ <(.*)>:", line)
        if head:
            close()
            name, frame, gap, largest, pending = head.group(1), 0, 0, 0, {}
            continue
        if name is None:
            continue
        moved, touched = 0, False
        # AArch64.
        sub = re.search(r"\bsub\s+sp, sp, #(0x[0-9a-f]+|\d+)(, lsl #12)?", line)
        if sub:
            moved = int(sub.group(1), 0) << (12 if sub.group(2) else 0)
        far = re.search(r"\bsub\s+(x\d+), sp, #(0x[0-9a-f]+|\d+)(, lsl #12)?", line)
        if far:
            pending[far.group(1)] = int(far.group(2), 0) << (12 if far.group(3) else 0)
        more = re.search(r"\bsub\s+(x\d+), (x\d+), #(0x[0-9a-f]+|\d+)(, lsl #12)?", line)
        if more and more.group(1) == more.group(2) and more.group(1) in pending:
            pending[more.group(1)] += int(more.group(3), 0) << (12 if more.group(4) else 0)
        # The loop form's end is its compare, or a move to the limit after it.
        compared = re.search(r"\bcmp\s+sp, (x\d+)", line)
        if compared and compared.group(1) in pending:
            frame += pending.pop(compared.group(1)) - PAGE
            gap = 0
        landed = re.search(r"\bmov\s+sp, (x\d+)", line)
        if landed and landed.group(1) in pending:
            # The end of the loop form: its steps were read one each, and each touches.
            frame += pending.pop(landed.group(1)) - PAGE
            gap = 0
        if re.search(r"\b(?:str|ldr)\s+xzr, \[sp\]", line):
            touched = True
        pair = re.search(r"\b(?:stp|str)\s+\S+(?:, \S+)?, \[sp, #-(0x[0-9a-f]+|\d+)\]!", line)
        if pair:
            moved = int(pair.group(1), 0)
            touched = True
        # x86-64.
        rsp = re.search(r"\bsubq?\s+\$(0x[0-9a-f]+|\d+), %rsp", line)
        if rsp:
            moved = int(rsp.group(1), 0)
        limit = re.search(r"\bsubq?\s+\$(0x[0-9a-f]+|\d+), %r(\d+)\b", line)
        if limit:
            pending["r" + limit.group(2)] = int(limit.group(1), 0)
        loop = re.search(r"\bcmpq?\s+%r(\d+), %rsp", line)
        if loop and "r" + loop.group(1) in pending:
            frame += pending.pop("r" + loop.group(1)) - PAGE
            gap = 0
        if re.search(r"\b(?:movq|orq)\s+\$0x0, \(%rsp\)", line):
            touched = True
        if re.search(r"\bpushq?\s+%", line):
            moved = 8
            touched = True
        # ARMv7-A, which has only the frame.
        arm = re.search(r"\bsub\s+sp, sp, #(\d+)$", line)
        if arm and not sub:
            moved = int(arm.group(1))
        push = re.search(r"\b(?:push|stmdb\s+sp!,)\s*\{([^}]*)\}", line)
        if push:
            moved = 4 * (push.group(1).count(",") + 1)
        vpush = re.search(r"\bvpush\s*\{([^}]*)\}", line)
        if vpush:
            moved = 8 * (vpush.group(1).count(",") + 1)
        frame += moved
        gap += moved
        largest = max(largest, gap)
        if touched:
            gap = 0
    close()
    return found


def judge_probed(found):
    """x86-64 and AArch64: lines, problems."""
    problems = [
        f"{name}: the stack pointer moves {gap} bytes with no touch, past a page's guard: "
        "stack probes are not in this image (F-100)"
        for name, (_, gap) in sorted(found.items())
        if gap > PAGE and name not in NOT_FUNCTIONS
    ]
    over = [frame for name, (frame, _) in found.items() if frame > PAGE and name not in NOT_FUNCTIONS]
    unprobed = max(
        ((gap, name) for name, (_, gap) in found.items() if gap <= PAGE and name not in NOT_FUNCTIONS),
        default=(0, "none"),
    )
    lines = [
        f"{len(found)} functions, {len(over)} frames over a page, every one probed; "
        f"largest frame {max(over, default=0)}; "
        f"largest run with no touch {unprobed[0]}, {unprobed[1][:80]}"
    ]
    return lines, problems


def lower_guard_pages():
    source = Path("src/kernel/src/arch/armv7a/mod.rs").read_text(encoding="utf-8")
    return int(re.search(r"const KERNEL_STACK_LOWER_GUARD_PAGES: u64 = (\d+);", source).group(1))


# A baseline entry's stack and what limits a frame on it. `boot` is the 64 KiB boot stack with no
# guard below it (`BOOT_STACK_SIZE`): its frames are judged against the baseline only. `task` is a
# task's kernel stack and `worker` a boot check's worker stack (`stages_check::WORKER_CHECKS`), both
# guarded: a frame there may not exceed the guard. No entry is `worker` today (ledger 7415 C).
STACKS = {"boot": None, "task": "guard", "worker": "guard"}


def judge_baseline(found, build, baseline, guard_bytes):
    """ARMv7-A: lines, problems, and what a record would write for this build."""
    problems, record = [], {}
    held = baseline.get(build, {})
    over = {name: frame for name, (frame, _) in found.items() if frame > PAGE and name not in NOT_FUNCTIONS}
    for name, frame in sorted(over.items(), key=lambda item: -item[1]):
        was = held.get(name)
        kind = "check" if CHECK_NAME.search(name) else "product"
        stack = was["stack"] if was else "task"
        record[name] = {"frame": frame, "kind": kind, "stack": stack}
        if was is None:
            problems.append(f"{name}: a new frame of {frame} bytes over a page on ARMv7-A, which has no probes (F-100)")
        elif frame > was["frame"]:
            problems.append(f"{name}: its frame grew from {was['frame']} to {frame} bytes on ARMv7-A (F-100)")
        if stack not in STACKS:
            problems.append(f"{name}: its baseline names stack {stack!r}, which is none of {sorted(STACKS)} (F-100)")
        elif STACKS[stack] == "guard" and frame > guard_bytes:
            problems.append(
                f"{name}: a frame of {frame} bytes on a {stack} stack steps over ARMv7-A's guard of {guard_bytes} (F-100)"
            )
    gone = sorted(set(held) - set(over))
    lines = [
        f"{len(found)} functions, {len(over)} frames over a page against the baseline, "
        f"{sum(1 for entry in record.values() if entry['kind'] == 'product')} of them product, guard {guard_bytes}"
    ]
    if gone:
        lines.append(f"{len(gone)} gone from the baseline since it was recorded: --record drops them")
    return lines, problems, record


def self_test():
    probed = (
        "0000 <a>:\n  stp x29, x30, [sp, #-16]!\n  sub sp, sp, #0x1, lsl #12\n  str xzr, [sp]\n  sub sp, sp, #0x40\n"
        "0020 <bare>:\n  sub sp, sp, #0x2, lsl #12\n"
        "0040 <small>:\n  sub sp, sp, #0xf00\n"
        "0060 <loop>:\n  sub x9, sp, #0x8, lsl #12\n  sub sp, sp, #0x1, lsl #12\n  cmp sp, x9\n  str xzr, [sp]\n"
        "  mov sp, x9\n  ldr xzr, [sp]\n"
        "0080 <x>:\n  pushq %rbx\n  subq $0x1000, %rsp\n  movq $0x0, (%rsp)\n  subq $0x10, %rsp\n"
        "00a0 <y>:\n  subq $0x2000, %rsp\n"
        "00c0 <z>:\n  movq %rsp, %r11\n  subq $0x8000, %r11\n  subq $0x1000, %rsp\n  movq $0x0, (%rsp)\n  cmpq %r11, %rsp\n"
    )
    found = walk(probed)
    assert found["a"] == (16 + 4096 + 64, 4096), found["a"]
    assert found["bare"][1] == 8192, "an unprobed move is seen"
    assert found["small"] == (0xf00, 0xf00)
    assert found["loop"][0] == 0x8000 and found["loop"][1] <= PAGE, found["loop"]
    assert found["x"][1] <= PAGE + 8 and found["x"][0] == 8 + 4096 + 16, found["x"]
    assert found["y"][1] == 8192
    assert found["z"][0] == 0x8000 and found["z"][1] <= PAGE, found["z"]
    _, problems = judge_probed(found)
    assert len(problems) == 2 and "bare" in problems[0] and "y:" in problems[1], problems
    armed = walk("0 <f>:\n  push {r4, r5, lr}\n  sub sp, sp, #292\n  sub sp, sp, #9216\n1 <g>:\n  sub sp, sp, #8192\n")
    held = {"b": {"f": {"frame": 9520, "stack": "task"}, "gone": {"frame": 5000, "stack": "task"}}}
    _, problems, _ = judge_baseline(armed, "b", held, 12288)
    assert len(problems) == 1 and "g: a new frame" in problems[0], problems
    held["b"]["f"]["frame"] = 9000
    held["b"]["g"] = {"frame": 8192, "stack": "task"}
    _, problems, _ = judge_baseline(armed, "b", held, 12288)
    assert len(problems) == 1 and "grew" in problems[0], problems
    held["b"]["f"]["frame"] = 9520
    _, problems, _ = judge_baseline(armed, "b", held, 8000)
    assert len(problems) == 2 and all("steps over" in problem for problem in problems), problems
    held["b"]["f"]["stack"] = "boot"
    _, problems, _ = judge_baseline(armed, "b", held, 8000)
    assert len(problems) == 1 and "g: a frame of 8192 bytes on a task stack" in problems[0], problems
    held["b"]["g"]["stack"] = "worker"
    _, problems, _ = judge_baseline(armed, "b", held, 8000)
    assert len(problems) == 1 and "on a worker stack steps over" in problems[0], problems
    _, problems, _ = judge_baseline(armed, "b", held, 12288)
    assert not problems, problems
    held["b"]["g"]["stack"] = "elsewhere"
    _, problems, _ = judge_baseline(armed, "b", held, 12288)
    assert len(problems) == 1 and "none of" in problems[0], problems


def main():
    self_test()
    if "--self-test" in sys.argv:
        print("check-frames: self-test passed")
        return 0
    elf, arch, profile, build_kind = sys.argv[1:5]
    depth = read_stack_depth()
    text = subprocess.run(
        [depth.objdump(), "-d", "-C", "--no-show-raw-insn", elf], capture_output=True, text=True, check=True
    ).stdout
    found = walk(text)
    build = f"{profile}/{build_kind}"
    if arch == "armv7a":
        baseline = json.loads(BASELINE.read_text(encoding="utf-8")) if BASELINE.exists() else {}
        lines, problems, record = judge_baseline(found, build, baseline, lower_guard_pages() * PAGE)
        if "--record" in sys.argv:
            baseline[build] = record
            BASELINE.write_text(json.dumps(baseline, indent=1, sort_keys=True) + "\n", encoding="utf-8")
            lines.append(f"recorded {len(record)} entries for {build}: read `stack` of each over the guard")
            problems = [problem for problem in problems if "steps over" in problem]
    else:
        lines, problems = judge_probed(found)
    for line in lines:
        print(f"check-frames: {arch} {build}: {line}")
    for problem in problems:
        print(f"check-frames: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
