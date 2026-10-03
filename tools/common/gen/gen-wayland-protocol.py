#!/usr/bin/env python3
"""Turn the Wayland protocol XML into the interface tables the server reads.

The wire format is untyped: the same eight bytes are a different message for a
different object. So `src/user/system/linux/compositor/wire`'s reader is handed a signature, and
something has to say which signature goes with which interface and opcode.
That something is the protocol XML, and writing it out by hand would be a
thousand lines of numbers nobody could review.

The XML files are vendored under `src/user/system/linux/compositor/protocol/protocols/`, not read
from the machine: a table generated from whatever `wayland-protocols` the
builder happened to have installed would change under the compositor without
a commit. Each carries its own permissive licence in a `<copyright>` block,
which is copied into the generated file.

The output is committed, and `--check` regenerates it into memory and
compares, in the manner of `tools/common/gen/gen-arch-doc.py` and
`tools/common/gen/gen-panic-catalog.py`.

Usage:
    python3 tools/common/gen/gen-wayland-protocol.py           # write the modules
    python3 tools/common/gen/gen-wayland-protocol.py --check   # fail if they are stale
"""

import pathlib
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ElementTree

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent.parent
PROTOCOLS = ROOT / "src" / "user" / "system" / "linux" / "compositor" / "protocol" / "protocols"
OUT = ROOT / "src" / "user" / "system" / "linux" / "compositor" / "protocol" / "src" / "generated"

# The protocols the compositor speaks, and the module each becomes. Adding one
# here and vendoring its XML is the whole of adding a protocol.
FILES = [
    ("wayland.xml", "core"),
    ("xdg-shell.xml", "xdg_shell"),
    ("xdg-decoration-unstable-v1.xml", "xdg_decoration"),
    ("wlr-layer-shell-unstable-v1.xml", "layer_shell"),
    ("wlr-foreign-toplevel-management-unstable-v1.xml", "foreign_toplevel"),
    ("wlr-screencopy-unstable-v1.xml", "screencopy"),
    ("ext-session-lock-v1.xml", "session_lock"),
    ("cursor-shape-v1.xml", "cursor_shape"),
    ("primary-selection-unstable-v1.xml", "primary_selection"),
    ("xdg-activation-v1.xml", "xdg_activation"),
    ("viewporter.xml", "viewporter"),
    ("fractional-scale-v1.xml", "fractional_scale"),
    ("xdg-toplevel-icon-v1.xml", "toplevel_icon"),
    ("text-input-unstable-v3.xml", "text_input"),
    ("input-method-unstable-v2.xml", "input_method"),
    ("xdg-output-unstable-v1.xml", "xdg_output"),
    ("presentation-time.xml", "presentation"),
    ("ext-idle-notify-v1.xml", "idle_notify"),
    ("idle-inhibit-unstable-v1.xml", "idle_inhibit"),
    ("single-pixel-buffer-v1.xml", "single_pixel"),
    ("content-type-v1.xml", "content_type"),
    ("alpha-modifier-v1.xml", "alpha_modifier"),
    ("xdg-dialog-v1.xml", "xdg_dialog"),
    ("xdg-system-bell-v1.xml", "system_bell"),
    ("xdg-toplevel-tag-v1.xml", "toplevel_tag"),
    ("kde-server-decoration.xml", "kde_decoration"),
    ("relative-pointer-unstable-v1.xml", "relative_pointer"),
    ("pointer-constraints-unstable-v1.xml", "pointer_constraints"),
    ("pointer-gestures-unstable-v1.xml", "pointer_gestures"),
    ("keyboard-shortcuts-inhibit-unstable-v1.xml", "shortcuts_inhibit"),
    ("virtual-keyboard-unstable-v1.xml", "virtual_keyboard"),
    ("wlr-virtual-pointer-unstable-v1.xml", "virtual_pointer"),
    ("ext-foreign-toplevel-list-v1.xml", "foreign_list"),
    ("wlr-gamma-control-unstable-v1.xml", "gamma_control"),
    ("wlr-output-power-management-unstable-v1.xml", "output_power"),
    ("wlr-data-control-unstable-v1.xml", "data_control"),
    ("ext-data-control-v1.xml", "ext_data_control"),
    ("wlr-output-management-unstable-v1.xml", "output_management"),
    ("ext-workspace-v1.xml", "ext_workspace"),
    ("hyprland-global-shortcuts-v1.xml", "global_shortcuts"),
    ("hyprland-focus-grab-v1.xml", "focus_grab"),
    ("hyprland-lock-notify-v1.xml", "lock_notify"),
    ("hyprland-toplevel-mapping-v1.xml", "toplevel_mapping"),
    ("hyprland-surface-v1.xml", "hyprland_surface"),
# `hyprland-ctm-control-v1` is not here. Its `blocked` event has a
# `<description>` with no `summary`, which this `wayland-scanner` refuses --
# so the table could not be checked against libwayland's, and an unchecked
# table is the one thing this generator exists to avoid. The colour work it
# does is `wlr-gamma-control`'s as well, and that one is offered.
    ("hyprland-toplevel-export-v1.xml", "toplevel_export"),
    ("pointer-warp-v1.xml", "pointer_warp"),
    ("ext-background-effect-v1.xml", "background_effect"),
    ("tearing-control-v1.xml", "tearing_control"),
    ("fifo-v1.xml", "fifo"),
    ("commit-timing-v1.xml", "commit_timing"),
    ("security-context-v1.xml", "security_context"),
    ("vicinae-hotkey-v1.xml", "hotkey"),
    ("ext-image-capture-source-v1.xml", "capture_source"),
    ("ext-image-copy-capture-v1.xml", "image_copy"),
    ("linux-dmabuf-v1.xml", "linux_dmabuf"),
]

HEADER = """// @generated by tools/common/gen/gen-wayland-protocol.py from
// src/user/system/linux/compositor/protocol/protocols/{source}. Do not edit: edit the XML or the
// generator and run
//
//     python3 tools/common/gen/gen-wayland-protocol.py
//
// which `cargo xtask check` verifies.
{copyright}
//! The `{protocol}` protocol, as its XML defines it.
//!
//! Every name, version, opcode and enumeration value here is the file's, and
//! the wording of each doc comment is its `summary`.

// The summaries are the protocol's prose, not this tree's. The generator
// backticks the protocol's own identifiers in them; it does not try to groom
// the rest into Rust doc style, so a pixel format written `XRGB8888` or a
// layout written `4:4:4:4` stays as the protocol wrote it.
#![allow(
    clippy::doc_markdown,
    reason = "the doc comments are the protocol XML's summaries, copied"
)]

use compositor_wire::{{ArgType, Interface, Method}};

"""


def rust_name(name):
    """`wl_surface` as a Rust type name: `WlSurface`."""
    return "".join(part.title() for part in name.split("_"))


# Words a Rust module cannot be called. A protocol is free to name an enum
# `type`, and `content-type-v1` does.
KEYWORDS = {
    "as", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
    "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "async", "await", "box", "final", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield",
}


def module_name(name):
    """An enum or message group as a Rust module name, keywords escaped."""
    return f"r#{name}" if name in KEYWORDS else name


def const_name(name):
    """An enum entry or message name as a Rust constant."""
    name = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name)
    upper = name.upper()
    # A leading digit cannot start an identifier: xdg_positioner's anchors and
    # wl_output's transforms have entries like `90`.
    return upper if not upper[:1].isdigit() else "N" + upper


def arg_type(arg):
    """One `<arg>` as a `compositor_wire::ArgType`."""
    kind = arg.get("type")
    nullable = arg.get("allow-null") == "true"
    if kind == "int":
        return "ArgType::Int"
    if kind == "uint":
        return "ArgType::Uint"
    if kind == "fixed":
        return "ArgType::Fixed"
    if kind == "fd":
        return "ArgType::Fd"
    if kind == "array":
        return "ArgType::Array"
    if kind == "string":
        return f"ArgType::Str {{ nullable: {str(nullable).lower()} }}"
    if kind == "object":
        return f"ArgType::Object {{ nullable: {str(nullable).lower()} }}"
    if kind == "new_id":
        # Without an `interface` the client names one on the wire, which is
        # `wl_registry.bind` and nothing else in these protocols.
        return "ArgType::NewId" if arg.get("interface") else "ArgType::AnyNewId"
    raise SystemExit(f"unknown argument type {kind!r}")


def summary(element, fallback):
    """The XML's own words for something, on one line."""
    description = element.find("description")
    if description is not None and description.get("summary"):
        return " ".join(description.get("summary").split())
    if element.get("summary"):
        return " ".join(element.get("summary").split())
    return fallback


# A protocol name in a summary: `wl_surface`, `xdg_toplevel.set_title`,
# `zwlr_layer_shell_v1`. The XML's prose is written in plain text, and a doc
# comment that names code without marking it as code fails clippy's
# `doc_markdown` -- and reads worse.
IDENTIFIER = re.compile(r"(?<![`\w.])([a-z][a-z0-9]*(?:_[a-z0-9]+)+(?:\.[a-z0-9_]+)?)(?![`\w])")


def doc(text, indent=""):
    """A doc comment, wrapped, with the protocol's own names in backticks."""
    text = IDENTIFIER.sub(r"`\1`", text)
    words = text.split()
    lines, line = [], ""
    for word in words:
        if len(line) + len(word) + 1 > 68:
            lines.append(line)
            line = word
        else:
            line = f"{line} {word}".strip()
    if line:
        lines.append(line)
    return "".join(f"{indent}/// {line}\n" for line in lines or [""])


def methods(interface, tag):
    """The `<request>`s or `<event>`s of an interface, in opcode order."""
    out = []
    for message in interface.findall(tag):
        signature = ", ".join(arg_type(arg) for arg in message.findall("arg"))
        out.append(
            {
                "name": message.get("name"),
                "since": message.get("since", "1"),
                "signature": f"&[{signature}]",
                "destructor": message.get("type") == "destructor",
                "summary": summary(message, message.get("name")),
            }
        )
    return out


def render_interface(interface):
    """One `<interface>` as a static table and a module of its numbers."""
    name = interface.get("name")
    version = interface.get("version", "1")
    requests = methods(interface, "request")
    events = methods(interface, "event")

    out = []
    out.append(doc(summary(interface, name)))
    out.append(f"/// `{name}`, version {version}.\n")
    out.append(f"pub static {const_name(name)}: Interface = Interface {{\n")
    out.append(f'    name: "{name}",\n')
    out.append(f"    version: {version},\n")
    for label, table in (("requests", requests), ("events", events)):
        if not table:
            out.append(f"    {label}: &[],\n")
            continue
        out.append(f"    {label}: &[\n")
        for method in table:
            out.append("        Method {\n")
            out.append(f'            name: "{method["name"]}",\n')
            out.append(f"            since: {method['since']},\n")
            out.append(f"            destructor: {str(method['destructor']).lower()},\n")
            out.append(f"            signature: {method['signature']},\n")
            out.append("        },\n")
        out.append("    ],\n")
    out.append("};\n\n")

    # The numbers: opcodes by name, and the protocol's enums.
    out.append(doc(f"The opcodes and enumerations of `{name}`."))
    out.append(f"pub mod {name} {{\n")
    for label, table in (("request", requests), ("event", events)):
        if not table:
            continue
        out.append(doc(f"The {label} opcodes of `{name}`, in the protocol's order.", "    "))
        out.append(f"    pub mod {module_name(label)} {{\n")
        for opcode, method in enumerate(table):
            out.append(doc(method["summary"], "        "))
            out.append(f'        /// `{name}.{method["name"]}`\n')
            out.append(f"        pub const {const_name(method['name'])}: u16 = {opcode};\n")
        out.append("    }\n\n")
    for enumeration in interface.findall("enum"):
        enum_name = enumeration.get("name")
        bitfield = enumeration.get("bitfield") == "true"
        out.append(doc(summary(enumeration, enum_name), "    "))
        kind = "a bitfield" if bitfield else "an enumeration"
        out.append(f"    /// `{name}.{enum_name}`, {kind}.\n")
        out.append(f"    pub mod {module_name(enum_name)} {{\n")
        for entry in enumeration.findall("entry"):
            out.append(doc(summary(entry, entry.get("name")), "        "))
            value = entry.get("value")
            out.append(
                f"        pub const {const_name(entry.get('name'))}: u32 = {value};\n"
            )
        out.append("    }\n\n")
    out.append("}\n\n")
    return "".join(out)


def render(path, module):
    """One XML file as one Rust module."""
    tree = ElementTree.parse(path)
    protocol = tree.getroot()
    copyright_text = protocol.find("copyright")
    lines = []
    if copyright_text is not None and copyright_text.text:
        lines.append("//\n")
        for line in copyright_text.text.strip().splitlines():
            lines.append(f"// {line.strip()}\n".rstrip() + "\n")
        lines.append("//\n")
    out = [
        HEADER.format(
            source=path.name,
            copyright="".join(lines),
            protocol=protocol.get("name"),
        )
    ]
    for interface in protocol.findall("interface"):
        out.append(render_interface(interface))
    return "".join(out)


def formatted(text):
    """`text` as rustfmt would leave it.

    The generated files are part of a workspace whose gate runs `cargo fmt
    --check`, so a generator that emitted anything rustfmt would rewrite would
    make the two checks disagree and one of them would always be failing.
    Formatting here rather than afterwards means the committed file is the
    generator's output, byte for byte, and `--check` can compare them
    directly.
    """
    # Written and closed before rustfmt is handed its name, and closed again
    # before it is deleted. Windows refuses to open a file a second time while
    # the first handle is still there, so reading it back inside the `with`
    # fails on that host with `PermissionError` and the generator cannot be
    # run there at all -- which is where `cargo xtask check`'s wayland step
    # was failing. `gen-xkb-tables.py` had the same bug and this is its fix.
    #
    # In `OUT`, not the system temporary directory, for a reason the failure
    # would not have shown: rustfmt finds `rustfmt.toml` by walking up from
    # the file it is given, and this tree's sets `newline_style = "Unix"`. A
    # file formatted somewhere else would come back with this host's line
    # endings, and the committed output would then differ by every line on
    # Windows and by none on Linux.
    handle = tempfile.NamedTemporaryFile(
        "w", suffix=".rs", dir=OUT, delete=False, encoding="utf-8", newline="\n"
    )
    written = pathlib.Path(handle.name)
    try:
        with handle:
            handle.write(text)
        result = subprocess.run(
            ["rustfmt", "--edition", "2024", "--emit", "files", str(written)],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            raise SystemExit(f"rustfmt refused the generated code:\n{result.stderr}")
        return written.read_text(encoding="utf-8")
    finally:
        # `OUT` is the generated directory itself, and a temporary file left
        # there is a module the crate would try to compile.
        written.unlink(missing_ok=True)
        written.with_suffix(".rs.bk").unlink(missing_ok=True)


def modules():
    """Every module's path and its text."""
    for source, module in FILES:
        path = PROTOCOLS / source
        if not path.is_file():
            raise SystemExit(f"{path} is missing; vendor the protocol XML first")
        yield OUT / f"{module}.rs", formatted(render(path, module))


def main():
    check = "--check" in sys.argv[1:]
    stale = []
    OUT.mkdir(parents=True, exist_ok=True)
    written = []
    for path, text in modules():
        written.append(path.name)
        if check:
            if not path.is_file() or path.read_text(encoding="utf-8") != text:
                stale.append(path)
        else:
            path.write_text(text, encoding="utf-8")

    # A module left behind for a protocol no longer vendored would still
    # compile and would still be wrong.
    left = sorted(
        entry.name
        for entry in OUT.iterdir()
        if entry.suffix == ".rs" and entry.name not in written and entry.name != "mod.rs"
    ) if OUT.is_dir() else []
    if left:
        if check:
            stale.extend(OUT / name for name in left)
        else:
            for name in left:
                (OUT / name).unlink()

    if check:
        if stale:
            for path in stale:
                print(f"{path.relative_to(ROOT)} is stale", file=sys.stderr)
            print(
                "run `python3 tools/common/gen/gen-wayland-protocol.py`",
                file=sys.stderr,
            )
            return 1
        print(f"gen-wayland-protocol: {len(written)} modules current")
        return 0

    print(f"gen-wayland-protocol: wrote {len(written)} modules under {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
