# Ferrix

An operating system written in Rust for x86-64 and AArch64, whose acceptance
test is that it compiles Rust.

Not "has a shell", not "draws a window": it hosts `rustc`. That is the hardest
thing a general-purpose OS is routinely asked to do, and the only goal that
forces every subsystem to be real — threads and futexes, demand paging over
gigabytes, `fork`/`execve`, a hundred and fifty syscalls, and a filesystem that
survives a crash.

```
$ cargo xtask test-boot --arch both
  x86_64: booting under QEMU (timeout 120s)
    | Ferrix 0.1.0 on x86_64
    |   memory   507 MiB total, 501 MiB usable, 113 regions
    |   kernel   0x1e00a000 -> 0xffffffff80000000, 152 KiB
    |   physmap  0xffff800000000000 covering 512 MiB
    |   tables   root 0x1de0a000
    |   display  1280x800, stride 1280
    |   acpi     rsdp at 0x1fb7e014
    |   stage 1  loader hand-off verified
    |   traps    vectors installed
    |   frames   498 MiB managed, 498 MiB free, 127580 entries at 0x1780000 (2048 KiB)
    |   stage 2  frame allocator, heap and vmap arena verified
    |   clock    HPET at 100.000 MHz
    |   irqs     APIC, local APIC timer at 62.684 MHz
    |   stage 3  2 breakpoints, 4 page faults, 1001 ticks at 990 Hz
    |   w^x      318 mappings swept, 21 executable, none writable
    |   reclaim  3 MiB from the loader and ACPI, 501 free; arena 3 live, 36 KiB
    | FERRIX-BOOT-OK stages 1-3
  x86_64: boot ok
  aarch64: boot ok
```

## What exists today

Stages 1, 2 and 3 of `docs/ROADMAP.md`. Both architectures boot from firmware
to a Rust kernel which verifies the hand-off, brings up a buddy allocator over
every usable frame, starts a kernel heap — so `Box`, `Vec` and `BTreeMap` work
— installs its own trap vectors, services a page fault by mapping the faulting
address and letting the instruction retry, brings up an interrupt controller,
and runs a clock.

Virtual memory is finished rather than sketched: a `vmap` arena hands out
guard-paged ranges and kernel stacks over them, the loader's identity map is
dropped — which is what turns "higher-half" from a linker script's claim into a
demonstrated fact — the loader's own memory and the ACPI-reclaim regions go back
to the buddy allocator, empty slab pages are returned to it rather than hoarded,
and a sweep of the live page tables asserts that no mapping is both writable and
executable.

Time works and interrupts arrive. The local APIC and the GICv2 are programmed,
the local APIC timer is calibrated against the HPET, AArch64 uses the
architected virtual timer, and both are reached through one facade —
`irq::register`, `timer::after`, `trap::Frame`. What stage 3 still owes is
hardware Ferrix cannot currently be booted on to test: GICv3, and the local
APIC's TSC-deadline mode. The scheduler, the syscall layer and everything above
them are ahead.

The kernel proves it on every boot rather than asserting it: the memory map is
checked to be sorted and to describe the loader's own allocations, the direct
map is checked to alias physical memory by reading the kernel's first bytes
through both mappings, and the allocators are hammered with four thousand
blocks and required to give every frame back.

## No assembly at boot

Both architectures boot via UEFI, so firmware calls a Rust `efi_main` with the
CPU already in 64-bit mode. There is no bootstrap assembly on either machine,
which is unusual and is a direct consequence of choosing UEFI over Multiboot.

The assembly that does exist — 107 lines, **99.56% Rust** — is confined to
constructs the machine defines before a Rust function could run: installing a
translation regime and jumping to an address that did not exist a moment
earlier, and the CPU primitives with no Rust spelling. `docs/ASSEMBLY.md` is the
argument for each one; `scripts/check-asm-budget.py` fails the build on any site
that is not on the list, on a file over its budget, and on an entry that has
gone stale.

That number is a trend, not a gate. Assembly here is a fixed cost that does not
grow with the system, so the percentage rises as the OS is written.

## Getting started

```
cargo xtask build     --arch both --release   # bootable images in build/
cargo xtask run       --arch x86_64           # boot it, serial on your terminal
cargo xtask test-boot --arch both             # boot it and assert it came up
cargo xtask check                             # every gate CI runs
```

You need QEMU and UEFI firmware. Debian and Ubuntu: `qemu-system-x86`,
`qemu-system-arm`, `ovmf`, `qemu-efi-aarch64`. Windows:
`winget install SoftwareFreedomConservancy.QEMU`, which ships the firmware too.
Nothing else — the FAT32 image is written by `xtask`, so there is no `mtools` or
`dosfstools` to install and the image is byte-for-byte reproducible.

## Layout

| | |
|---|---|
| `libs/` | Architecture-neutral logic: the hand-off ABI, the ELF reader, page table construction, the buddy allocator, the kernel heap. Host-testable **by design** — it is the only code `cargo test`, Miri and the fuzzers can reach. |
| `boot/` | The UEFI loader. Reads the kernel, builds the address space, leaves firmware. |
| `kernel/` | The kernel. |
| `xtask/` | Host build driver: cross-compiles both halves, writes the FAT32 image, drives QEMU. |
| `scripts/` | The quality gates. |
| `docs/` | [Architecture](docs/ARCHITECTURE.md) · [Roadmap](docs/ROADMAP.md) · [Assembly](docs/ASSEMBLY.md) · [Reliability](docs/RELIABILITY.md) · [Boot log](docs/BOOT-LOG.md) |

## Quality gates

Ported from the [Starling](https://github.com/Fancy-Mumble/starling) workspace:
`cargo fmt --check`, clippy at `-D warnings` on all four targets, `cargo-deny`,
Miri, fuzzing, and a lint table that denies `unwrap`, `expect`, `panic!`,
`unreachable!` and unchecked indexing in production code — with every exemption
argued at the site and checked by `scripts/check-panic-audit.py`.

Three gates are this project's own:

* **The assembly allow-list**, above.
* **The unsafe audit.** Starling sets `unsafe_code = "deny"` and means it; a
  kernel cannot, because writing a page table entry *is* the program. So unsafe
  is not forbidden here, it is made expensive: a `SAFETY:` comment on every
  block, one unsafe operation per block, a `# Safety` section on every unsafe
  function, and `scripts/check-unsafe-audit.py` in CI so that a clippy release
  which softens a nursery lint cannot quietly retire the rule.
* **The boot test.** Everything else checks the source. This one boots it, on
  both architectures. An OS that compiles and does not boot is not a passing
  build.

## Licence

MIT. See [LICENSE](LICENSE).
