# Stage 24 — The game path  ·  *≈ 68 points before the game domain, about 190 with it and the later rows*

A fullscreen game is faster on Ferrix than on a stock Linux desktop on the
same hardware, where a system can be: in the time from an input to the
picture that shows it, and in how even the frames are while the machine does
something else. [`docs/GAME-PATH.md`](../GAME-PATH.md) is the design and the
build plan, written on 2026-10-10 at the customer's request; the customer
decided the same day that it is a roadmap stage (its §10, question 1), and
this page is that plan as a stage. The rows and points below are the
document's §8, taken as they are and not estimated again.

**Nothing of it is built, and nothing of it is measured.** Every figure the
design quotes was measured by another session for another purpose, or is an
estimate and says so. Nobody has measured, on either system, the time from
an input to the frame that shows it, a distribution of frame times, or
anything under a load that is defined and repeatable. So the stage's first
step is the rig that measures them.

**Exit.** By the stage's own rig, on the RTX 3060 at 1920x1080 and 60 Hz,
with the same binaries on both systems, in two guests of the same shape
with their processors pinned (`docs/GAME-PATH.md` §2.3 to §2.5):

* **M1, input to scan-out with vsync on:** median at most half a refresh
  plus the game's own frame time plus 2 ms, 99th percentile at most one
  refresh plus the frame time plus 2 ms, for any game and with no support
  from it; and at least one refresh under stock Linux.
* **M4, frame times under the background load:** at most one missed refresh
  a minute and no frame longer than two refreshes with the load running;
  and fewer misses than stock Linux, and than Linux with GameMode.

Stock Linux is the design's baseline guest, Debian 13 with sway and Xwayland
on the same NVIDIA driver; which Linux the exit is judged against is
question 2 below and is not decided. The other metrics (M2, M3, M5 to M9)
are reported with the exit and are not it; M8, frames a second with vsync
off, has a tie as its ceiling, because the same closed userspace draws the
frames on both systems. Like stage 23's, the exit is a recorded run on the
card, not a boot that stays in CI; what stays in CI is each row's gate that
needs no card, which the design gives for R1 to R3, P2, I1, P5 and G0 to
G5.

## Where it stands in the order

After [stage 21](stage-21-bare-metal-gpu-ferrix.md)'s zero-copy landings,
which are not landed. The design names what it stands on:

* **displayctl v9**, flips of a driver's own dmabuf, which stands on
  `nv-vram-dmabuf`. P1 and P2 need it.
* **yserver handing an X client's pixmap on as a dmabuf** (`nv-present`,
  yserver's fork). The lane for an X game needs it.
* **`nv-trip`**, the request from NVIDIA's libraries into `nvrm`, being
  cut.
* **A hardware cursor through NVKMS**, second in the customer's order for
  stage 21, without which a visible pointer takes a game off the lane.

All four are other sessions' work, counted in stage 21 or nowhere, and give
this stage no points. Everything on the card but R1 to R3 waits on them.
The stage does not wait for stage 23: a native game measures it, and §7 of
the design says what Windows games through Wine add.

**The game domain is stage 14's ground.** G1 to G5 below, 52 points, make
processors that nothing but the game and its present path may run on. That
is [stage 14](stage-14-real-time-domains.md)'s partition and fixed-priority
class, in the certified item's core ring. They are listed here because the
exit's M4 and the bound M5 lean on them, and they are not in this stage's
68: whether they may open stage 14 early is question 3, and it is open.
G0, a session built from what the scheduler already has, is in the 68 and
is the measurement that question waits for. If G0 alone does not reach M4's
target, the exit waits on question 3 (this page's reading of the plan; the
design says only that G0 goes "as far as a heuristic goes").

**The switch a person turns exists, with nothing behind it yet.** Since
2026-10-11 Settings has a game-mode page (`docs/SETTINGS.md`): a list of
the installed applications with a switch each, kept in the user's
`~/.config/ferrix/game-mode.conf`, and the launcher starts a switched-on
application with `FERRIX_GAME_MODE=1`. That is the design's "the user
turned the session on for it" (§5.4) written down, and what G0 and G5 will
read. It changes no weight, processor or defence, carries no points of this
stage, and `test-compositor --boot settings` holds it.

## The steps

Points are first guesses in the roadmap's sizes, made before any of it ran.
"Consultant" is a review by the certification consultant before the row
lands; "design" is a design review before code. What each row needs, and
the section that designs it, is in `docs/GAME-PATH.md` §8. The
certification consultant has seen the design and this page as a docs
landing only: the design review of the session and the game domain (the
design's §5, G0 to G5) and of P1 is owed before any code.

### Step 1: the rig  ·  19 to 21 points

| # | Landing | Points | Proved by |
|---|---|---|---|
| R1 | The frame recorder: per-swap times and Present completions in the preload library, and the summariser, brought into the tree under `tools/common/` with the baseline's scripts | 5 | the same binary's record on both guests; a 50 ms sleep injected every ten seconds is counted exactly. No card |
| R2 | The latency probe and the injector's log: input to scan-out on both systems | 5 | the probe on both; a 16.7 ms delay added in the probe moves the median by one refresh; a gate without the card. No card |
| R3 | The load script, the launch timer, the sleep and wake-up program, and both guests defined with pinned processors and an invariant TSC | 3 | the load holds every processor at 100% on both; the clock line says TSC |
| R4 | The baseline: M1 to M9 on both guests as they are, the system-call breakdown, and the record written into the design's §12 | 3 | the table, with its logs. Needs the card, the Linux guest, and `nv-present` or its successor on the card |
| R5 | The software measure checked against light, once | 3, or 5 with a built device | the two agree within the device's error on both systems. Needs question 6 |

R1 to R3 need nobody's decision and no card, and are the next step.

### Step 2: the first cut that can be compared with Linux  ·  47 points

In the design's order, by gain for effort.

| # | Landing | Points | Gain expected | Review |
|---|---|---|---|---|
| — | displayctl v9, flips of a driver's own dmabuf | another session's; none here | 9 to 15 ms and three copies off hyprix's own frame | its own |
| P1 | The flip's time: `FLIPPED` carries it, the DRM event reports it, hyprix's pace follows it | 8 | M1: up to a refresh of phase error; the base for everything timed | design, consultant (the display core's protocol and checks) |
| P2 | The game lane in hyprix: a fullscreen dmabuf surface flipped as it is, with the fallback | 8 | M1: hyprix's frame out of the path; M7: about 1.2 processors | — |
| — | yserver hands an X client's pixmap on as a dmabuf | another session's; none here | the read-back's 2 ms and yserver's thread | its own |
| — | A hardware cursor through NVKMS | stage 21's; none here | the lane survives a visible pointer | its own |
| I1 | Input: time carried end to end, hyprix's input thread, no coalescing under a pointer lock | 5 | M1's tail; the mouse's lag becomes a number per hop | — |
| P6 | Tearing: the hint from Wayland and from Present to an NVKMS tearing flip | 5 | M2 exists | consultant for the protocol flag |
| P7 | Just in time: the hold, in yserver and in hyprix, with its controller and its rule | 8 | M1: about one refresh, for any game | — |
| G0 | The session with what exists: affinity applied, weights for the game, the path and the rest | 8 | M4, as far as a heuristic goes | design, consultant, ids reserved first (the scheduler applies affinity: core ring) |
| P5 | hyprix draws backwards from the deadline | 5 | M1 for composited clients: up to a refresh | — |

P1, P2, I1, P6, P7, G0 and P5 are 47. The first card run of P2 decides
whether NVKMS scans out a client's image; if it copies, the lane is still
far better than today and no longer level with Linux (the design's first
risk).

### The game domain, which is stage 14's ground  ·  52 points, not in this stage's 68

| # | Landing | Points | Proved by | Review |
|---|---|---|---|---|
| G1 | A domain made and dissolved at run time through a handle; the partition's rules | 21 | boot checks: a task outside never runs inside, the domain dissolves with its handle, a machine too small is refused; a control for each | question 3; design, consultant, ids reserved first |
| G2 | The fixed-priority class in that domain with budgets, and the wake-up test | 13 | M5 held by a boot with the load on the other processors; a looping thread stopped by its budget | design, consultant |
| G3 | Interrupts placed with the domain | 5 | the card's and the mouse's lines counted per processor | — |
| G4 | Memory under the session: a reclaim floor for a job, `mlock` | 8 | a game job's pages survive the load's pressure | design, consultant, ids reserved first (`mm/**` is core ring) |
| G5 | The session service, hyprix's request and the rule | 5 | a `test-compositor` boot: fullscreen makes the domain, losing focus dissolves it; M4 on the card | — |

G4's `mlock` is also something stage 23's E1.3 names as missing for Wine;
whichever reaches it first writes it once.

### Later, each built only on a measurement  ·  73 points if all of it is built

| # | Landing | Points | Built if |
|---|---|---|---|
| P8 | VRR through NVKMS | 8 | the display offers it (question 5) |
| P10 | `wp_linux_drm_syncobj_v1` and dmabuf feedback version 4 in hyprix | 13 | a Wayland-native game or Wine's Wayland driver is wanted |
| I2 | The pointer tap | 8 | hyprix's hop is above 0.3 ms at the 99th percentile after I1; design, consultant |
| P9 | An overlay on a second NVKMS layer | 8 | games are seen leaving the lane for overlays |
| S2 | A native render process in place of `virgl_test_server` | 34 | composited games matter after the lane (question 7); a design of its own |
| W1 | `/dev/ntsync` | none here: [stage 23](stage-23-arknights-endfield.md)'s E3.5, which has 8 for "fsync or ntsync"; the design puts ntsync alone nearer 13 | stage 23 reaches it |
| W2 | Reflex through dxvk-nvapi, tried | 2 | stage 23's step 3 has a game with it |

### In sum

| Part | Points |
|---|---|
| Step 1, the rig | 19 to 21 |
| Step 2, the first cut | 47 |
| **This stage before the game domain** | **≈ 68** |
| The game domain, stage 14's ground (question 3) | 52 |
| Later, each on a measurement | 73 if all of it is built |
| All of it | about 190 |

The sums are the design's own (§8). The points are not in the sized scope
the [burndown](status.md) is drawn on.

## Open decisions

The design's §10, each with its recommendation there. Question 1 is
decided: this is a stage. **Questions 2 to 10 are open, and none is taken
here.**

2. Which Linux is the opponent in the exit: stock, or tuned as well.
3. Whether the game domain may open stage 14 early, in the item's core
   ring, without EDF or priority inheritance.
4. Whether yserver, hyprix and `nvrm` may share a speculation domain, and
   whether a game may join it.
5. VRR: whether the display on the card offers it, and whether it is
   wanted.
6. Hardware for the check against light (R5): a photodiode device, the
   Pixel 7's 240 frames-a-second camera, or neither.
7. A native render process in place of `virgl_test_server` (S2, 34
   points).
8. The session's defaults: on for every fullscreen program that declares
   itself a game or only by a rule, and how many processors it may take.
9. A bare-metal run before any figure leaves the tree.
10. Who settles the design's §4.2 and §6.2 lists with the present and
    `nv-trip` sessions.

The game-mode switch raises ten more, in `docs/SETTINGS.md` §8. The
customer decided three on 2026-10-11: a mark first; then the speculation
protections and the other latency trades per application, to be built;
administrator privilege required. Open: where a game's scope may sit,
whether the one-way barrier is for every client of `nvrm`, how and by which
administrator a game is let into the driver's speculation domain, whether
the certified item carries that at all, and question 4 narrowed to a choice
of two (§7 there, a proposal, reviewed as a design: ledger line 698).

## What it cannot win, and what can go wrong

The design's §9, shortened; it is the page to read before quoting a figure.

* **Not winnable:** frames bound by the GPU and anything inside NVIDIA's
  libraries (a tie is the ceiling, 97% the target); the display's own
  delay; the price of a system call; one more switch each way for a driver
  in ring 3; launch time, where parity is the target; and breadth across
  cards, monitors and games.
* **A guest hides and invents effects.** M4 and M5 in a guest say which
  system is ahead on this host, not what either does on a machine; in
  absolute terms they wait for bare metal, which is stage 21's exit.
* **The margin may not shrink** without G1 and G2: the hold still removes
  the queueing refresh, with Linux-sized margins.
* **A core-ring change for a game.** The domain touches the scheduler the
  certification rests on; the reviews are in the plan.
* **Three sessions on one path.** This design, `nv-trip` and the present
  work each change what a frame passes through, and nothing is agreed
  between them yet (question 10).
