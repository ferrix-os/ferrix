# i915's own files, vendored

These files are copied unchanged from Linux v7.0 (tag `v7.0`, commit
`028ef9c96e96197026887c0f092424679298aae8`). They are MIT-licensed: see each
file's header, and `NOTICE` here. Nothing builds them.
`tools/common/gen/gen-intel-xelp.py` reads their tables and writes
`../src/generated/`, and `--check` keeps the two in step
(`docs/INTEL-GPU.md` §3.6).

| File here | Linux path | Tables taken |
|---|---|---|
| `intel_uncore.c` | `drivers/gpu/drm/i915/intel_uncore.c` | `__gen12_fw_ranges`, `gen12_shadowed_regs` |
| `gt/intel_lrc.c` | `drivers/gpu/drm/i915/gt/intel_lrc.c` | `gen12_xcs_offsets`, `gen12_rcs_offsets` |
| `gt/intel_mocs.c` | `drivers/gpu/drm/i915/gt/intel_mocs.c` | `gen12_mocs_table`, `GEN11_MOCS_ENTRIES` |
| `gt/intel_gpu_commands.h` | `drivers/gpu/drm/i915/gt/intel_gpu_commands.h` | every integer `#define` |
| `include/pciids.h` | `include/drm/intel/pciids.h` | `INTEL_ADLN_IDS` |

To move to a later Linux, replace the files from that tag, run the generator,
and read the diff of `src/generated/` against i915's own changelog.
