# Stage 14 — Real-time domains  ·  *month*

The `SoftRt` and `HardRt` modes: FIFO/RR classes, threaded interrupts,
priority-inheritance mutexes, EDF with constant-bandwidth admission control,
and the runtime domain-mode switch with its quiescence protocol.

**Exit:** a cyclictest-shaped boot test measuring wake-up latency on a `HardRt`
domain while a `Throughput` domain on other cores is saturated, reporting a
maximum inside the stated bound; plus an admission test that refuses an
unschedulable set instead of missing deadlines.

**Research (2026-10-26):** [docs/REALTIME.md](../REALTIME.md) surveys what
Linux, seL4 MCS, QNX, Zircon and the ARINC 653 systems do, what Ferrix has
today, and proposes the stage's design and an order of landings. Nothing is
built yet.

---

