#!/usr/bin/env bash
# Assert the layering this OS rests on. See docs/ARCHITECTURE.md.
#
#   1. The loader and the kernel are two programs that meet at one struct.
#      `boot` must never link `ferrix-kernel` and the kernel must never link
#      the loader; everything they agree on lives in `ferrix-bootinfo`.
#   2. `src/lib/*` is host-testable, architecture-neutral logic. That is the whole
#      reason it exists -- it is the code `cargo test`, Miri and the fuzzers can
#      reach -- so it must depend on neither the kernel nor the loader, and must
#      contain no `#[cfg(target_arch)]`.
#   2a. `src/user/system/native/*` is ring-3 programs: the native runtime, devmgr, drivers. A
#      program may use `src/lib/*` and never the kernel or the loader, and neither
#      of those may link a program. Its `target_arch` conditionals live under
#      its own `src/arch/`.
#   3. Architecture code is reached through the `crate::arch` facade and never
#      by name. Generic kernel code that says `arch::x86_64::` compiles on one
#      machine and breaks the other, and the break is discovered by whoever
#      next builds for AArch64 rather than by whoever wrote it.
#   4. `#[cfg(target_arch)]` appears only under `src/kernel/src/arch/` and
#      `src/boot/common/uefi/src/arch/`. This is rule 3's teeth: without it, "the facade" is a
#      naming convention that erodes one conditional at a time.
#   5. Every workspace member inherits the workspace lint table.
#
# A principle in a design document does not survive contact with a bring-up
# session at 2am. A build failure does.
#
# Passes trivially while a layer is still empty, so it can be wired into CI
# before the code it guards exists.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.."

status=0

have() { cargo metadata --no-deps --format-version 1 2>/dev/null | grep -q "\"name\":\"$1\""; }

# Resolve a crate's shipping dependency graph, or fail loudly.
#
# A `cargo tree` that errors must NOT be treated as "no violations found": that
# turns a broken manifest into a green layering check, which is worse than no
# check at all.
#
# Only edges that ship (`--edges normal`). A `[dev-dependencies]` entry is not
# part of the artifact: `src/lib/platform/elf` may legitimately pull in a test-only crate,
# and that says nothing about what the kernel links.
resolve() {
    local crate="$1"
    if ! cargo tree -p "$crate" --edges normal --prefix none --no-dedupe 2>/tmp/cargo-tree-err.$$; then
        echo "ERROR: could not resolve the dependency graph for $crate." >&2
        echo "  The layering rule was NOT checked. Fix the manifest first:" >&2
        sed 's/^/    /' /tmp/cargo-tree-err.$$ >&2
        rm -f /tmp/cargo-tree-err.$$
        return 1
    fi
    rm -f /tmp/cargo-tree-err.$$
}

# Assert that $crate (described as $role) depends on none of $forbidden, an
# extended-regex alternation of package names. A crate that does not exist yet
# is skipped, not failed, so a layer can be guarded before it is filled.
forbid() {
    local crate="$1" role="$2" forbidden="$3"
    have "$crate" || return 0

    local deps
    if ! deps=$(resolve "$crate"); then
        status=1
        return 0
    fi

    # Anchored on the whole name, and it matters: `ferrix-boot` is a prefix of
    # `ferrix-bootinfo` and `ferrix-devmgr` of `ferrix-devmgr-proto`, so a
    # looser match reports a crate that depends on the ABI as depending on the
    # program. `cargo tree --prefix none` starts every line with the package
    # name and follows it with a space, so that is the anchor; a word boundary
    # would land between `devmgr` and `-proto`, since `-` is not a word
    # character to grep.
    local offenders
    offenders=$(echo "$deps" | grep -oE "^(${forbidden}) " | sed 's/ $//' | sort -u || true)
    if [[ -n "$offenders" ]]; then
        echo "LAYERING VIOLATION: $crate ($role) links a crate it must not:" >&2
        echo "$offenders" | sed 's/^/    /' >&2
        status=1
    else
        echo "ok:   $crate ($role) links nothing it must not"
    fi
}

# ---------------------------------------------------------------------------
# 1. Loader and kernel are two programs.
# ---------------------------------------------------------------------------
# The kernel's packages: the binary at src/kernel and the library crates
# beneath it (docs/KERNEL-CRATES.md). Every rule that keeps something away
# from "the kernel" names all of them: an anchored `ferrix-kernel` does not
# match `ferrix-kernel-core`.
kernel_crates=$(find src/kernel -maxdepth 2 -name Cargo.toml -not -path '*/target/*' 2>/dev/null | sort \
    | xargs -r sed -n '/^\[package\]/,/^\[/s/^name = "\(.*\)"$/\1/p' \
    | paste -sd '|' - || true)
if [[ "|$kernel_crates|" != *"|ferrix-kernel|"* ]]; then
    echo "ERROR: no ferrix-kernel package found under src/kernel; the kernel's rules were NOT checked." >&2
    exit 1
fi
K="$kernel_crates"

forbid ferrix-boot   "UEFI loader" "$K"
forbid ferrix-kernel "kernel"      "ferrix-boot"

# The kernel's own crates, in ring order (docs/KERNEL-CRATES.md, the
# certification consultant's ledger line 1477 (5)): the core links none of
# the others, the item neither the load nor the binary, the load not the
# binary. Which of them exist is certification-item.json's kernel_crates: a
# crate listed there and not in the workspace fails here rather than being
# passed over (ledger 1565 (2)(c)); one not listed yet (before the split)
# has no rule to keep.
listed=$(python3 -c 'import json,sys; print(" ".join(k for k in json.load(open(sys.argv[1]))["kernel_crates"] if not k.startswith("//")))' tools/common/data/certification-item.json)
for crate in $listed; do
    if ! have "$crate"; then
        echo "LAYERING VIOLATION: $crate is in kernel_crates and not in the workspace; its rules were NOT checked" >&2
        status=1
    fi
done
kernel_rule() {
    [[ " $listed " == *" $1 "* ]] && forbid "$@"
    return 0
}
kernel_rule ferrix-kernel-core "the kernel's core ring" "ferrix-kernel|ferrix-kernel-item|ferrix-kernel-load|ferrix-boot"
kernel_rule ferrix-kernel-item "the kernel's item ring" "ferrix-kernel|ferrix-kernel-load|ferrix-boot"
kernel_rule ferrix-kernel-load "the kernel's load ring" "ferrix-kernel|ferrix-boot"

# ---------------------------------------------------------------------------
# 2. The host-testable libraries sit below both.
# ---------------------------------------------------------------------------
forbid ferrix-bootinfo "handoff ABI"  "$K|ferrix-boot|ferrix-elf|ferrix-frame"
forbid ferrix-elf      "ELF64 reader" "$K|ferrix-boot"
forbid ferrix-frame    "frame allocator" "$K|ferrix-boot"
forbid ferrix-paging   "page tables"  "$K|ferrix-boot"
forbid ferrix-sched    "scheduler logic" "$K|ferrix-boot"
forbid ferrix-pci      "PCI configuration space" "$K|ferrix-boot"
forbid ferrix-block    "block core" "$K|ferrix-boot"
forbid ferrix-btrfs    "btrfs read path" "$K|ferrix-boot"
forbid ferrix-btrfs-write "btrfs write path" "$K|ferrix-boot"
forbid ferrix-netwire  "network wire formats" "$K|ferrix-boot"
forbid ferrix-nettcp   "TCP state machine" "$K|ferrix-boot"
forbid ferrix-net      "net core" "$K|ferrix-boot"
forbid ferrix-netring  "net ring protocol" "$K|ferrix-boot"
forbid ferrix-netserve "net driver serve loop" "$K|ferrix-boot"
forbid ferrix-netlink  "netlink messages" "$K|ferrix-boot"

# ---------------------------------------------------------------------------
# 2a. Ring-3 programs sit beside the kernel, never in it.
#
# A crate under `src/user/system/native/` -- the native runtime, devmgr, a driver -- runs in user
# mode. It may use any `src/lib/` crate, `src/lib/proto/native-abi` above all, and nothing
# of the kernel or the loader, whose code cannot run there. Neither of those may
# link a `src/user/system/native/` crate either: that would be ring-3 code in ring 0. The names
# come from each manifest's `[package]`, so a new program is covered without an
# edit here.
# ---------------------------------------------------------------------------
user_crates=$(find src/user/system/native -name Cargo.toml -not -path '*/target/*' 2>/dev/null | sort \
    | xargs -r sed -n '/^\[package\]/,/^\[/s/^name = "\(.*\)"$/\1/p' \
    | paste -sd '|' - || true)
if [[ -n "$user_crates" ]]; then
    for crate in ${user_crates//|/ }; do
        forbid "$crate" "ring-3 program" "$K|ferrix-boot"
    done
    for kernel_crate in ${K//|/ }; do
        forbid "$kernel_crate" "kernel" "$user_crates"
    done
    forbid ferrix-boot   "UEFI loader" "$user_crates"
fi
forbid ferrix-btrfs-vfs "btrfs mount" "$K|ferrix-boot"
forbid ferrix-blkring  "block ring protocol" "$K|ferrix-boot"
forbid ferrix-virtio-blk "virtio-blk driver logic" "$K|ferrix-boot"
forbid ferrix-virtio-net "virtio-net driver logic" "$K|ferrix-boot"
forbid ferrix-dwmac "DesignWare QoS core logic" "$K|ferrix-boot"
forbid ferrix-yt6801 "YT6801 glue logic" "$K|ferrix-boot"
forbid ferrix-blkserve "block driver serve loop" "$K|ferrix-boot"

# ---------------------------------------------------------------------------
# 3. Generic kernel code reaches architecture code through the facade.
#
# `src/kernel/src/arch/mod.rs` is the facade and is where the architecture names
# are supposed to appear -- the three architectures', and the drivers the Arm
# pair share; everything else under src/kernel/src must go through it.
# ---------------------------------------------------------------------------
if [[ -d src/kernel/src ]]; then
    offenders=$(grep -rnE '(crate::)?arch::(x86_64|aarch64|armv7a|arm_common)::' src/kernel/src \
        --include='*.rs' \
        | grep -v '^src/kernel/src/arch/' || true)
    if [[ -n "$offenders" ]]; then
        echo "LAYERING VIOLATION: generic kernel code names an architecture module directly:" >&2
        echo "$offenders" | sed 's/^/    /' >&2
        echo "    Add the operation to the crate::arch facade instead." >&2
        status=1
    else
        echo "ok:   generic kernel code reaches the CPU only through crate::arch"
    fi
fi

# ---------------------------------------------------------------------------
# 4. Conditional compilation on the architecture lives in the arch directories.
#
# `target_pointer_width` and `target_endian` are not architecture selection --
# they are properties both of our targets share, and asserting one is fine
# anywhere. `target_os` likewise: xtask is a host program.
#
# A native program under `src/user/system/native/` follows the same rule: its crate's
# `src/arch/` is the facade, as `src/kernel/src/arch/` is the kernel's.
# ---------------------------------------------------------------------------
offenders=$(grep -rnE 'cfg[^)]*target_arch' src/kernel/src src/boot/common/uefi/src src/lib src/user/system/native 2>/dev/null \
    --include='*.rs' \
    | grep -vE '^(src/kernel/src/arch/|src/boot/common/uefi/src/arch/|src/user/system/native/(drivers/[^/]+/)?[^/]+/src/arch/)' || true)
if [[ -n "$offenders" ]]; then
    echo "LAYERING VIOLATION: target_arch conditional outside an arch directory:" >&2
    echo "$offenders" | sed 's/^/    /' >&2
    echo "    Architecture differences belong in src/kernel/src/arch/<arch>/ behind" >&2
    echo "    the facade, not as a conditional in generic code." >&2
    status=1
else
    echo "ok:   every target_arch conditional is inside an arch directory"
fi

# ---------------------------------------------------------------------------
# 5. Every workspace member inherits the workspace lint table.
#
# `[lints] workspace = true` and a per-crate override cannot coexist: cargo
# refuses the manifest. That is what makes this check a straight yes or no. A
# crate that genuinely needs an exception states it in its own source with a
# reason, where the code that needs it is, rather than opting out wholesale.
# ---------------------------------------------------------------------------
while IFS= read -r manifest; do
    grep -q '^\[package\]' "$manifest" || continue
    # A manifest declaring its own `[workspace]` (the fuzz crate does, so
    # `cargo fuzz` can pick its own profile) inherits from itself.
    grep -q '^\[workspace\]' "$manifest" && continue
    if ! sed -n '/^\[lints\]/,/^\[/p' "$manifest" | grep -qE '^\s*workspace\s*=\s*true'; then
        echo >&2 "$manifest does not inherit the workspace lints"
        echo >&2 "  add:  [lints]"
        echo >&2 "        workspace = true"
        status=1
    fi
done < <(find src/boot src/kernel src/lib src/user/system/native tools/common/xtask -name Cargo.toml -not -path '*/target/*' 2>/dev/null | sort)

if [[ $status -ne 0 ]]; then
    echo >&2
    echo "See docs/ARCHITECTURE.md. The loader and kernel meet at one struct, the" >&2
    echo "host-testable libraries sit below both, and the CPU is reached through" >&2
    echo "one facade. The build is what makes those structural." >&2
fi

exit $status
