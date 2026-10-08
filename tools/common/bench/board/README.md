# Board benches: Linux, seL4 and Ferrix on the DK1

What `docs/BOARD-BENCH.md` runs on the STM32MP157D-DK1 for the two kernels that
are not Ferrix, and the timing contract all three follow.

| Directory | What |
|---|---|
| `common/` | `board-bench.h`: the cycle and instruction counters, the sample loop, the percentiles and the output lines. Ferrix's `ipc-bench` follows the same contract in Rust. `selftest.c` checks the header. |
| `linux/` | Linux 7.2.9 for the DK1: the build script, the PMU user-access module, and `lbench`, the Linux round trips (B1). |
| `sel4/` | seL4's STM32MP1 platform as patches against the pinned seL4, its build script, and the matched root task (B2). The patches carry seL4's licences. |
| `posix/` | lmbench 3.0-a9 and SQLite 3.53.4's `speedtest1`, static on musl 1.2.5: the fetch-and-build script with its pins, the run scripts, and what Linux and Ferrix images run (B5). |

Built on nazuna with `arm-linux-gnueabihf-gcc -marm -mcpu=cortex-a7`: the
`-marm` is never left out, because that gcc's default is Thumb-2, and
Ferrix's own armv7a programs are A32. Outputs stay in
`~/.local/share/ferrix/board-bench/` on nazuna.
