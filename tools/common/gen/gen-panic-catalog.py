#!/usr/bin/env python3
"""Render the panic catalog into a document, and check it on the way.

A kernel panic prints the site's own sentence and, when the site names one, an
entry from `src/kernel/src/panic/catalog.rs`: a stable code, a title, what the
failed check establishes, and the likely causes. That file is the source of
truth, because it is what the machine prints. This writes the same entries as
`docs/generated/PANICS.md`, so they can be read, searched and linked without a
machine that has just stopped.

Validated before anything is written, and failing either way:

  * every code is `FX-` and four digits, and no two entries share one;
  * every `pub` or `pub(crate)` static of type `Explanation` is listed in `ALL`, once,
    and `ALL` names nothing else and is in code order;
  * every title and meaning says something, and every entry has a cause;
  * every path `see` mentions exists in the repository. A pointer to a file
    that was renamed is worse than none: it sends the reader nowhere at the
    moment they most need to be sent somewhere.

The document is committed, and `--check` regenerates it into memory and
compares, in the manner of `tools/common/gen/gen-arch-doc.py`.

Usage:
    python3 tools/common/gen/gen-panic-catalog.py            # write the document
    python3 tools/common/gen/gen-panic-catalog.py --check    # fail if it is stale
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import textwrap

SOURCE = pathlib.Path("src/kernel/src/panic/catalog.rs")
OUTPUT = pathlib.Path("docs/generated/PANICS.md")
COMMAND = "python3 tools/common/gen/gen-panic-catalog.py"

CODE = re.compile(r"FX-\d{4}")
STATIC = re.compile(
    r"pub(?:\(crate\))?\s+static\s+(\w+)\s*:\s*Explanation\s*=\s*Explanation\s*\{(.*?)\n\};",
    re.DOTALL,
)
ALL_LIST = re.compile(
    r"pub(?:\(crate\))?\s+static\s+ALL\s*:\s*&\[&Explanation\]\s*=\s*&\[(.*?)\];",
    re.DOTALL,
)
STRING = r'"((?:[^"\\]|\\.)*)"'
FIELD = re.compile(rf"\b(code|title|meaning|see)\s*:\s*{STRING}", re.DOTALL)
CAUSES = re.compile(r"\bcauses\s*:\s*&\[(.*?)\]\s*,", re.DOTALL)
LITERAL = re.compile(STRING, re.DOTALL)

WIDTH = 80


class CatalogError(Exception):
    """Something in the catalog that the document must not be built from."""


def repository_root() -> pathlib.Path:
    return pathlib.Path(__file__).resolve().parent.parent.parent.parent


def unescape(body: str, where: str) -> str:
    """A Rust string literal's value, for the escapes the catalog may use.

    A backslash before a newline is Rust's continuation: it and the whitespace
    that follows vanish. Quotes and backslashes may be escaped. Anything else,
    `\\n` in particular, is refused, because an entry is printed into a report
    one wrapped line at a time and a newline of its own would break the shape.
    """
    out: list[str] = []
    index = 0
    while index < len(body):
        char = body[index]
        if char != "\\":
            if char == "\n":
                raise CatalogError(f"{where}: a raw newline inside a string")
            out.append(char)
            index += 1
            continue
        following = body[index + 1 : index + 2]
        if following == "\n":
            index += 2
            while index < len(body) and body[index] in " \t\n\r":
                index += 1
        elif following in ('"', "\\"):
            out.append(following)
            index += 2
        else:
            raise CatalogError(f"{where}: escape \\{following} is not allowed; keep strings plain")
    return "".join(out)


def parse(source: str) -> tuple[list[dict], list[str]]:
    entries = []
    for match in STATIC.finditer(source):
        name, body = match.group(1), match.group(2)
        line = source.count("\n", 0, match.start()) + 1
        where = f"{SOURCE}:{line} ({name})"
        entry: dict = {"name": name, "where": where}
        for field in FIELD.finditer(body):
            key = field.group(1)
            if key in entry:
                raise CatalogError(f"{where}: `{key}` given twice")
            entry[key] = unescape(field.group(2), where)
        causes = CAUSES.search(body)
        if causes is None:
            raise CatalogError(f"{where}: no `causes` list")
        entry["causes"] = [unescape(text, where) for text in LITERAL.findall(causes.group(1))]
        for key in ("code", "title", "meaning", "see"):
            if key not in entry:
                raise CatalogError(f"{where}: no `{key}`")
        entries.append(entry)

    listed = ALL_LIST.search(source)
    if listed is None:
        raise CatalogError(f"{SOURCE}: no `ALL` list")
    names = re.findall(r"&\s*(\w+)", listed.group(1))
    return entries, names


def validate(entries: list[dict], names: list[str], root: pathlib.Path) -> list[str]:
    problems: list[str] = []
    if not entries:
        problems.append(f"{SOURCE}: no entries found")

    seen_codes: dict[str, str] = {}
    for entry in entries:
        where = entry["where"]
        code = entry["code"]
        if not CODE.fullmatch(code):
            problems.append(f"{where}: code {code!r} is not FX- and four digits")
        if code in seen_codes:
            problems.append(f"{where}: code {code} is already {seen_codes[code]}'s")
        seen_codes.setdefault(code, entry["name"])
        if not entry["title"].strip():
            problems.append(f"{where}: empty title")
        if not entry["meaning"].strip():
            problems.append(f"{where}: empty meaning")
        if not entry["causes"]:
            problems.append(f"{where}: no causes")
        if any(not cause.strip() for cause in entry["causes"]):
            problems.append(f"{where}: an empty cause")
        for part in entry["see"].split(";"):
            words = part.split()
            if not words:
                problems.append(f"{where}: an empty reference in `see`")
                continue
            path = words[0]
            if pathlib.PurePath(path).is_absolute() or ".." in pathlib.PurePath(path).parts:
                problems.append(f"{where}: `see` path {path} is not inside the repository")
            elif not (root / path).exists():
                problems.append(f"{where}: `see` names {path}, which does not exist")

    declared = [entry["name"] for entry in entries]
    for name in declared:
        count = names.count(name)
        if count == 0:
            problems.append(f"{SOURCE}: {name} is not listed in ALL")
        elif count > 1:
            problems.append(f"{SOURCE}: {name} is listed in ALL {count} times")
    for name in names:
        if name not in declared:
            problems.append(f"{SOURCE}: ALL lists {name}, which is not an Explanation")

    by_name = {entry["name"]: entry["code"] for entry in entries}
    codes = [by_name[name] for name in names if name in by_name]
    if codes != sorted(codes):
        problems.append(f"{SOURCE}: ALL is not in code order")
    return problems


def wrap(text: str, first: str = "", rest: str = "") -> str:
    return textwrap.fill(
        text,
        width=WIDTH,
        initial_indent=first,
        subsequent_indent=rest,
        break_long_words=False,
        break_on_hyphens=False,
    )


def render(entries: list[dict], names: list[str]) -> str:
    by_name = {entry["name"]: entry for entry in entries}
    ordered = [by_name[name] for name in names]

    lines = [
        "# Ferrix — panic codes",
        "",
        f"> Generated from {SOURCE.as_posix()} by tools/common/gen/gen-panic-catalog.py. Do not edit: "
        f"change the catalog and regenerate with `{COMMAND}`.",
        "",
        wrap(
            "When the kernel stops on a fatal condition it prints a report beginning "
            "with a `FERRIX-PANIC` line that carries the failing site's own message. A "
            "site that names a catalog entry adds, below the trace, the entry's code and "
            "title on a `code` line, then `means`, `causes` and `see` lines with the text "
            "below. The message says what happened this time; the entry says what the "
            "check was for and where to look."
        ),
        "",
        wrap(
            "A code is `FX-SSNN`. `SS` is the stage of `docs/ROADMAP.md` whose check or "
            "bring-up failed: `00` for anything before stage 1 or outside any stage, `90` "
            "for the trap path's reports. `NN` numbers the entries within it. Codes are "
            "never reused, so a code in an old log still means what is written here."
        ),
        "",
        wrap("Causes are listed most likely first."),
        "",
        "| Code | What failed |",
        "| --- | --- |",
    ]
    for entry in ordered:
        anchor = entry["code"].lower()
        lines.append(f"| [{entry['code']}](#{anchor}) | {entry['title']} |")

    for entry in ordered:
        lines += [
            "",
            f'<a id="{entry["code"].lower()}"></a>',
            "",
            f"## {entry['code']} — {entry['title']}",
            "",
            wrap(entry["meaning"]),
            "",
        ]
        for number, cause in enumerate(entry["causes"], 1):
            marker = f"{number}. "
            lines.append(wrap(cause, marker, " " * len(marker)))
        lines += ["", wrap(f"See: {entry['see']}.")]
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(
        description=f"Render {SOURCE} into {OUTPUT}.",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="do not write; exit 1 if the committed document is stale",
    )
    arguments = parser.parse_args()

    root = repository_root()
    source_path = root / SOURCE
    try:
        source = source_path.read_text(encoding="utf-8")
    except OSError as error:
        print(f"gen-panic-catalog: cannot read {SOURCE}: {error}", file=sys.stderr)
        return 1

    try:
        entries, names = parse(source)
    except CatalogError as error:
        print(f"gen-panic-catalog: {error}", file=sys.stderr)
        return 1

    problems = validate(entries, names, root)
    if problems:
        print("gen-panic-catalog: the catalog is not fit to publish.\n", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1

    text = render(entries, names)
    output = root / OUTPUT

    if arguments.check:
        current = output.read_text(encoding="utf-8") if output.exists() else None
        if current != text:
            state = "missing" if current is None else "out of date"
            print(f"gen-panic-catalog: {OUTPUT} is {state}.")
            print(f"\nRun `{COMMAND}` and commit the result.")
            return 1
        return 0

    output.parent.mkdir(parents=True, exist_ok=True)
    # newline="\n" so a Windows checkout does not produce CRLF and fail the
    # line-endings gate on a file nobody typed.
    output.write_text(text, encoding="utf-8", newline="\n")
    print(f"gen-panic-catalog: {len(entries)} entries, wrote {OUTPUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
