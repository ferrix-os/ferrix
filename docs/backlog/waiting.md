# Backlog detail: Waiting on the customer

Detail kept out of [`docs/BACKLOG.md`](../BACKLOG.md), which holds a short line per item linking here. Each item's text is as it stood in the backlog when it was split out (2026-10-16); add new findings here, under the item's id, and keep the backlog line short.

### wc-001

* Settings' game mode (`docs/SETTINGS.md` §8, 2026-10-11; questions 1, 2
  and 5 are decided, under *Decisions*): whether a game's scope may sit
  beside the session's; whether the one-way barrier towards `nvrm` is for
  every program that follows the driver or only towards named jobs; whether
  a game enters the driver's speculation domain by admission or by birth;
  whether the administrator who grants it is a member of `wheel` with their
  password or root alone; whether the certified item takes such a mechanism
  at all, and how a certified configuration relates to one that can grant
  game mode per application at run time; and with it the two-way form of
  the game path's question 4.

### wc-002

* The game path, stage 24 (`docs/GAME-PATH.md` §10, 2026-10-10; that it is
  a roadmap stage is decided): which Linux its exit is judged against; whether the game
  domain may open stage 14 early, in the item's core ring; speculation
  domains for yserver, hyprix and `nvrm`; whether the display on the card
  offers VRR; hardware to check the software latency measure against light
  (a photodiode device, or the Pixel 7's 240 frames-a-second camera); a
  native render process in place of `virgl_test_server`; the session's
  defaults; and a bare-metal run before any figure leaves the tree.

### wc-003

* Developing Ferrix on Ferrix (2026-10-11): the six decisions of
  `docs/DEV-ON-FERRIX.md` §9 -- the editor (helix with rust-analyzer
  recommended, VS Code assessed later), how a kernel is tried from inside (a
  second slot with a way back recommended, and a two-point measurement of
  QEMU on Ferrix), where secrets live before the PC, a second root for the
  second slot, live driver updates on a developer image, and when the shared
  toolchain volume is fetched again.

### wc-004

* Stage 23, Arknights: Endfield (2026-10-10), before its step 0 can run: a
  game account the customer can afford to lose, since nobody can rule out a
  ban for playing under Wine on a kernel no other player has; about 110 GB
  of disk for the Linux guest (nazuna's root volume had 88 GB free that
  evening); and the card for the run. Before step 4: whether the game's
  third-party EULA permits Wine and a third-party Proton build, which nobody
  has read. In step 5: whether Ferrix may answer `Linux` to this one program
  where `uname -s` says `Ferrix`. And whether the stage's ≈ 205 points enter
  the burndown's scope.

### wc-005

* User namespaces for Steam (I5b of `docs/I386.md`): scout's requirements
  check refuses a kernel without them ("Steam now requires user namespaces
  to be enabled", exit 71, fatal in `steam.sh`), and the client's UI,
  `steamwebhelper`, runs in a pressure-vessel (bubblewrap) container.
  Ferrix answered every namespace flag `EINVAL` until N3 (`syscall/namespace.rs`);
  stage 13 parks the init's L13 on namespaces too. `test-steam-bootstrap`
  sets the check aside meanwhile, and says so.
  **Answered 2026-09-28:** the customer decided to build user and mount
  namespaces, enough for Steam; designed in `docs/NAMESPACES.md` (landings
  N1 to N7 and NP, 39 points to N6), reviewed by the certification
  consultant, branch `steam-userns`. N1, per-mount flags and `MS_REMOUNT`,
  landed 2026-09-28; N2, binds and `MNT_DETACH`, 2026-09-30; N3, a mount
  namespace per process and bubblewrap as root, 2026-09-30 (1384e6e6); N4,
  user namespaces, 2026-10-01 (d171ffe5); NP, procfs by
  `ptrace_may_access`, 2026-10-04 (7e9a2806f). N5 to N7 (unprivileged
  mounts, bwrap and the bootstrap as uid 1000, pressure-vessel) are not
  built (`docs/NAMESPACES.md` §9).
