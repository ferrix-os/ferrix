# Settings, and game mode

Written on 2026-10-11 at the customer's asking: *"add a basic settings app,
which for now will have the game mode option where you have a list of all
apps and then you can select which ones shall be in 'game mode'."*

Sections 1 to 6 are what is built. Section 7 is a **proposal, not built and
not decided**: how the one effect that would trade a protection for speed
could attach to the switch. Section 8 lists what the customer still decides.

## 1. What it is

`/bin/settings`, the `settings` app in ferrix-os/apps (`docs/APPS.md`), in
every image a person runs, with a launcher entry and an icon of its own. A
window with the pages down its left side and the chosen page beside it.

There is one page, **Game mode**: every application a launcher lists, each
with its icon, its name, a line saying what it is, and a switch. Typing
narrows the list; Up, Down, Page Up, Page Down, Home and End move the
selection; Enter turns the selected switch (so does Space while nothing is
typed); Escape empties the filter; the pointer selects a row, turns a switch
and scrolls with the wheel; `Ctrl+Q` closes the window. The foot of the
window says what the last switch came to, and when the file could not be
written it says why and the switch stays where it was.

It draws with the desktop's own libraries, as the launcher, the bar and the
lock screen do (`compositor-toolkit` for the window, `compositor-text` for
the words, `compositor-image` for the icons), in the brand's colours and
Inter (`docs/brand/BRAND.md`). No toolkit came with it.

`settings --list` prints the same list with no window, a line an
application: `on` or `off`, the desktop-file ID, the name.

A second page is a variant of `Page` in the app's `model.rs`, a line in
`Page::ALL`, and its drawing; the sidebar, `--page` and the hit-testing take
it from there.

## 2. "All applications" is the launcher's list

Ferrix has no registry of applications. A launcher lists `.desktop` entries
from the XDG data directories: `~/.local/share/applications`, then
`/usr/share/applications` (or what `XDG_DATA_HOME` and `XDG_DATA_DIRS` say).
The system's own entries are installed there by every desktop
(`tools/common/xtask/src/fuzzel.rs`: the terminal, the test pattern, top,
Chrome and vkgears where the image has them, Steam under `--everything`); an
app's package installs its own (`docs/APPS.md` §10); and a program that
writes one for itself under the user's directory, as Steam and Wine do for
what they install, is found the same way.

So that Settings can never offer a different list from the one the launcher
starts from, the code that finds and reads the entries is one crate both
use: `compositor-desktop` (`src/user/system/linux/compositor/desktop/`). Its
`entry` and `icon` modules were the fuzzel app's `desktop` and `icon` until
this landing, moved unchanged. Settings lists what
`entry::Search::listing` finds: every entry a launcher shows, once, by its
desktop-file ID. An entry a launcher hides (`NoDisplay`, `Hidden`, a
`TryExec` that is not there) is not offered, since nobody can start it from
one.

What that leaves out, on purpose: a program with no entry (anything started
only from a terminal), and a single game inside Steam's library unless Steam
wrote an entry for it. Steam itself is one application; switched on, every
game it starts inherits its mark (§4).

## 3. The file

`$XDG_CONFIG_HOME/ferrix/game-mode.conf`, which is
`~/.config/ferrix/game-mode.conf` where that is not set: beside the other
per-user configuration, under a directory of Ferrix's own. Plain text, one
desktop-file ID a line:

```text
# Ferrix game mode: the applications started in game mode, one desktop-file
# ID a line. Written by Settings (/bin/settings); edit it freely. A launcher
# reads it each time it starts an application (docs/SETTINGS.md).
pattern.desktop
steam.desktop
```

* A line starting with `#`, and an empty line, is skipped. A line that
  cannot be an ID (white space or a `/` in it) is skipped and complained of
  by whoever reads it.
* No file is no application in game mode.
* An ID nothing installed has is kept when Settings writes: the application
  may come back.
* Settings turns a switch by reading the file again, changing the one line
  and writing the whole set, sorted, to a new file in the same directory
  that is synchronised and then renamed over the old one. A reader sees the
  old set or the new one, never part of either, and a hand edit to another
  line made a moment before is kept.
* The file is `0644` and the user's own. Any program of the user's can read
  it, and any can write it. That is acceptable for what the mark does today
  (§4) and is exactly why §7 does not let this file grant anything that
  costs a protection.

The format, the path, the write and the mark are
`compositor_desktop::game_mode`; nothing else in the tree spells them.

## 4. What game mode does today

**One thing.** An application switched on is started by the launcher with
two variables in its environment:

```text
FERRIX_GAME_MODE=1
FERRIX_GAME_MODE_APP=<its desktop-file ID>
```

Everything it starts inherits them, which is how a game Steam starts has
Steam's mark. An application that is *not* switched on is started with both
taken out, so a launcher that was itself started from a game does not hand
the mark on.

**Nothing is faster for it, and nothing is given up.** No scheduling
weight, no processor, no pacing and no protection changes. The page says so
in these words, above the list. No speed-up is claimed because none was
made, and none could have been measured on the night this was written (the
gate host ran at a load above 30).

**Where the mark is given.** In one place: the launcher's start
(`exec::start` in the fuzzel app, through
`game_mode::mark_for_launch` and `Mark::apply`). fuzzel is otherwise a port
that follows upstream; this is the one thing in it that is Ferrix's.

**What is not covered.**

* A program started from a terminal, or by a key bound to `exec` in
  `hyprland.conf`: hyprix starts those itself and knows no desktop-file ID
  for them. A terminal that is itself switched on hands its mark to every
  command typed in it.
* Another launcher. One that wants to take part calls the same two
  functions.
* A program can set the variables for itself. Today that gains it nothing.

**Why no real effect was attached.** The candidates were looked at, and
each either does not reach the customer's desktop without a new privileged
path or is not this landing's to decide:

| Candidate | What stands in the way |
|---|---|
| A larger `cpu.weight` for the game's cgroup (`docs/GAME-PATH.md` §5.3, G0) | The `--everything` desktop runs as the user `ferrix`. A user may make a scope only under their own `user-<uid>.slice` and may set no property on it (`init`'s control socket: only root changes state), so the launcher cannot raise a weight; and a scope beside the session's own would outlive the session (`docs/AUTH.md` §6.4 leaves that to the customer). It needs init, or the session service of `docs/GAME-PATH.md` §5.4, to grant it. G0 is stage 24's, with a design review and a measurement owed first |
| A lower nice value | `setpriority` below the caller's own needs privilege, as on Linux; the launcher has none |
| Affinity | `sched_setaffinity` is accepted and not applied (`docs/GAME-PATH.md` §5.1) |
| Turning vertical sync off (`vblank_mode=0`, `__GL_SYNC_TO_VBLANK=0`) | It works through the environment, but it trades tearing for latency, which is the customer's choice per game (`docs/GAME-PATH.md` §4.4), not a default to slip in |
| Holding the screen awake while a game runs | hyprix would have to know which window is the marked program's; for an X11 game the Wayland client is yserver. Possible, and not built |

## 5. What will hang off the mark

Nothing here is promised; each is its own landing with its own review.

* **The game session** (`docs/GAME-PATH.md` §5): §5.4 lets hyprix ask for a
  session for a fullscreen, focused window when "a window rule names it, the
  user turned the session on for it, or it declared itself a game". *The
  user turned it on* is this switch. The session service will read the set
  by the function the launcher reads it with.
* **G0's weights** (§5.3 there), once something privileged can set them.
* **The scope's name.** When the launcher's programs get scopes of their
  own inside the session, a marked one's is where a weight or a floor is
  written.
* **The speculation barrier towards the graphics driver** (§7 below): the
  one-way barrier does not need the mark at all; the opt-in that shares the
  driver's domain must not be granted by it alone.

## 6. How it is tested

* **Host tests of the crate** (`compositor-desktop`, run by `cargo xtask
  check` with the compositor's workspace): the file's format read and
  written, the comment and bad-line rules, the atomic save (the file
  replaced whole, mode `0644`, nothing left beside it, and a save that
  cannot be made leaving the file as it was), the mark set for an
  application in the set and taken out for any other, the mark reaching a
  started program's real environment, and the listing against the tree's own
  entries with a Steam-like and a Wine-like entry under the user's
  directory. fuzzel's own tests of the entry and icon readers moved with the
  code.
* **Host tests of the app** (`cargo xtask check`, the apps row): the model
  (filter, selection, scroll, a switch that shows only what was written),
  the store (a switch keeps a hand edit to another line; a refused write is
  said), the layout and hit-testing, the command line, and the frame drawn
  with the tree's fonts and the system's icons and read back: each row's
  switch is the accent with its knob at the right when on and grey with its
  knob at the left when off, at buffer scales 1 and 2.
* **The launcher's host test** (the fuzzel app): an application in the
  user's file is started with the mark in its environment and another one
  without it.
* **`cargo xtask test-apps`**: the app's two smoke lines, `settings
  --version` and `settings --list`, on the image.
* **`cargo xtask test-compositor --boot settings`**
  (`tools/common/xtask/src/compositor/settings.rs`): the desktop comes up
  with Settings open and the app installed as its package installs it. Keys
  typed by `/bin/vkbd` filter the list to a probe application and turn its
  switch on; Settings says it wrote the file; `settings --list` in a second
  process reads the switch back as on; fuzzel starts the probe, a zinc
  script, which prints `FERRIX_GAME_MODE=1` and its own ID and reads its ID
  out of the file with the shell's `read`; the switch is turned off; the
  probe started again prints no mark and finds the file without its ID. The
  screen with the switch on is kept as `build/<arch>/settings.ppm`.
* **The negative control**: with the launcher's one line that gives the
  mark turned into a comment (`launch.game.apply(&mut command)` in the
  fuzzel app's `exec.rs`), the same boot fails with *"with its switch on,
  Game Probe was not started in game mode by the launcher"*. Run by hand on
  2026-10-11 on x86-64 under KVM; the boot's own second half (switch off, no
  mark) is the control in the other direction on every run, and the judge's
  host tests hold each refusal's text.

## 7. Proposal: sharing the graphics driver's speculation domain

**Status: a proposal for review. Nothing in this section is built, and no
kernel code was written for it.** It was asked for with the app so that the
switch is not designed into a corner. The certification consultant's
verdict on it is in §7.9.

### 7.1 What is being asked for

A request from a game to the ring-3 NVIDIA driver `nvrm` crosses two
address-space switches, and at each the kernel issues the predictor barrier
(`IBPB` on x86-64; `docs/certification/SPECULATION.md` §3). Measured on
2026-10-10 under KVM on the bench (`docs/NVIDIA.md` §4.7 on branch
`nv-trip`): a version query's round trip was 15.5 µs with the barrier and
4.7 µs with it left out, so about 11 µs of a request is the two barriers. A
game makes one request a frame through NVIDIA's GLX: 0.7 ms a second at 60
frames a second, and 27 ms a second (2.7% of one processor) at the 2,442
requests a second counted on the card at an uncapped 2,200 frames a second.
Nothing a player sees at a capped rate; a measurable share of an uncapped
benchmark.

Two ways to stop paying it were put to the customer:

1. **A one-way barrier, by default**: kept on the switch *into* the driver,
   left out on the switch *back* to the client. A kernel change, not built,
   another session's (`nv-trip`).
2. **The game in the driver's speculation domain, opt-in per application**:
   no barrier either way. A departure from the certified configuration, off
   unless turned on.

### 7.2 What the item has today

From `docs/OPAQUE-KERNEL.md` §9.2 and requirements `L.object.114` to `116`:

* A domain is a job *made* marked (`JOB_SPECULATION_DOMAIN`, needing MANAGE
  on the parent); nothing sets the mark later.
* A process is a member only if it was *born* in the marked job, by
  `process_create` or by a member's fork in that job. A process moved in
  does not join. `CLONE_INTO_CGROUP` is a move.
* A member leaves for good when it moves between jobs or stops being
  dumpable (a set-id `execve`, a change of credentials, `PR_SET_DUMPABLE`).
* The switch skips the barrier exactly when both spaces are in the same
  non-zero domain. A domain is therefore an equivalence class: there is no
  pairwise or one-directional relation in the item.
* AoU-14: one domain holds only programs that may read each other's memory;
  MANAGE on a marked job is the authority to place programs there;
  partitions of different criticality shall not share one.

### 7.3 The mark must not be the grant

The mark of §4 is an environment variable set from a file the user's own
programs can write. Any program running as the user can put itself in the
file, or set the variable. That is harmless while the mark changes nothing
and stays tolerable for a scheduling weight inside the user's own slice.

It is not tolerable for this effect. A program that shares `nvrm`'s domain
can train the predictor `nvrm` then runs on, and read `nvrm`'s memory
speculatively (V-07): the memory of the process that holds every client's
GPU state, the compositor's and the lock screen's among them. If the user's
file granted that, any program the user ran once could grant it to itself.

So the proposal separates two lists:

| | Game mode (built) | Driver domain (proposed) |
|---|---|---|
| Where | `~/.config/ferrix/game-mode.conf` | `/etc/ferrix/game-domain.conf`, root's, `0644` |
| Who writes it | the user, or any program of theirs | root only: Settings asks `authd` for the user's password and a `wheel` membership, as `su` does (`docs/AUTH.md`), and a small set-up helper writes the line |
| What it can cost | nothing, later a share of the user's own processor time | the separation between that application and the driver |
| Read by | the launcher | the session service only |

An application gets the effect only if it is in **both**: the user wants it
treated as a game, and an administrator accepted the trade for it. The page
shows the second switch under the first, disabled with the reason when the
user is not in `wheel`, with the trade in plain words beside it.

### 7.4 Who grants it, and how the kernel learns

The grantor is the **session service** of `docs/GAME-PATH.md` §5.4: one
root program init starts, holding the handle the grant needs, as `devmgr`
holds devices. Neither hyprix nor the launcher holds it. The launcher's
part is unchanged: it marks. When a marked application starts, the service
(told by the launcher over its socket, or by hyprix when the window maps)
looks the ID up in root's list and acts.

How the kernel is told has two candidates, and this is the centre of the
review:

**A. By birth, with what exists.** `devmgr` makes `nvrm`'s job marked and
hands MANAGE on it to the session service, which AoU-14 already foresees
("hand that right only to a program trusted to place others there"). The
service starts the game *in that job*.

* No new requirement for membership. `L.object.114` to `116` stand.
* It does not fit a Linux program as things are. A game is forked by the
  launcher in the session's cgroup and `execve`d; entering the driver's job
  by `CLONE_INTO_CGROUP` is a move, which does not join. The game would
  have to be forked by a member already in the job: a starter process born
  there, running as the user from its first instruction, since a change of
  credentials leaves the domain. Whether `process_create` can make a
  process that then runs a Linux program as a given user was **not
  established**.
* It welds two things together that §5 keeps apart: the game would be in
  the *driver's* cgroup, charged to it, killed with it, and outside the
  user's session scope, so the session's end would not end it; and moving
  it to where G0's weight is written would take it out of the domain.

**B. By admission, a new rule.** A native call, `domain_admit(job, process)`
or the like, by which a holder of MANAGE on a marked job admits an existing
process, which keeps its own job and cgroup.

* It contradicts the sentence the domain's evidence rests on: "nothing joins
  a domain except by being started in it". `L.object.115` changes, and with
  it its check: new requirement ids reserved first, a check and a negative
  control for each new rule (only under MANAGE on the marked job; only a
  process the caller could otherwise control; refused for a process that is
  not dumpable, that has ever changed credentials, or that shares its space
  with a non-member; recorded as an audit event; the admitted process's
  space carries the domain only from the next switch, with the barrier
  issued once on every processor that last ran it, as a leave does in
  reverse).
* Leaving is unchanged and already one-way (`leave_speculation_domain`,
  with F-60's grace period), so revoking is built: the service makes the
  process leave, and it cannot be admitted again.
* The scheduler's and the cgroup's view of the game are untouched.

**Recommended: B, if this is built at all** (§7.7 says when). A is cheaper
in evidence and wrong in structure; its starter process would be a second,
privileged way of starting applications beside the launcher.

### 7.5 What a domain is cannot be bent: who else is in it

Because a domain is an equivalence class, and because predictor state
carries over every switch that leaves the barrier out, admitting a game to
`nvrm`'s domain has consequences beyond the pair:

* **Two admitted games share a domain with each other.** Each can read the
  other speculatively. The service should admit one at a time, with the
  game session's "one session at a time" (§5.4 there).
* **It collides with question 4 of `docs/GAME-PATH.md` §10.** That question
  recommends yserver, hyprix and `nvrm` in one domain among themselves, and
  the game not. A process is in one domain. If `nvrm` is in the system
  path's domain and a game is admitted to it, the game shares a domain with
  hyprix, which sees every window and every key. So one of three must be
  chosen: `nvrm` shares with the system path and no game is ever admitted;
  or `nvrm`'s domain is for admitted games and the system path keeps its
  barriers towards `nvrm`; or the customer accepts that an admitted game can
  read the compositor. The third should not be offered as a switch.
* **`nvrm` is not a neutral party.** V-10, V-11 and V-13 already record that
  `nvrm` is NVIDIA's closed code, accepted by design, serving every client.
  A game in its domain can aim at whatever `nvrm` holds of *other* clients.
  On a single-user desktop with one game in front that is the compositor's
  frames and the other windows' textures.

### 7.6 How the one-way default relates

The one-way barrier is a property of the driver's side of a request, not of
the application: it would apply to every client of a driver that asks for
it, with no list and no mark. It is not expressible as a domain (it is not
symmetric), so it is a new rule in the item's core ring of its own, with its
own requirement, check and control, and it is `nv-trip`'s to design.

What this proposal needs said about it, since the two share a page:

* **Kept, into the driver:** a client's predictor training never reaches
  `nvrm`. This is the direction V-07's attack on a shared service runs in,
  and it stays closed for every client, marked or not.
* **Dropped, back to the client:** the client runs on predictor state
  `nvrm` left. Two flows open. `nvrm` could steer the client's speculation;
  a client already hands `nvrm` everything it draws, so that adds little.
  And the client can probe what `nvrm`'s branches left behind, which depend
  on what `nvrm` last did for *whoever it served before*: a history channel
  from other clients, through the driver, to this one. It is narrower than
  a speculative read and it is not nothing; the barrier into the driver
  does not close it. If `nvrm` also shares a domain with hyprix (question
  4), hyprix's history is in that state too.
* **At most half.** One of the two barriers a round trip pays is left out:
  on the bench's figures about 5 of the 11 µs, to be measured.
* **Whether it is for every client or only for marked ones** is a decision
  (§8). For every client it is a change to the reference configuration's
  behaviour that the mark plays no part in. For marked ones only, the kernel
  must be told which clients, which brings back §7.3: the user's file must
  not be what tells it, so it would go through the service and root's list
  like the full opt-in, and the two lists of §7.3 become three states of one
  root-owned line (*barrier both ways*, *one way*, *none*).

So the relation is: the one-way barrier is the default the mark does not
gate; the full opt-in is the same line taken one step further, for one
application, by an administrator, through the service.

### 7.7 Whether to build it

The opt-in buys what the one-way barrier leaves: on the bench's figures
about 5 µs a request, 12 ms a second at 2,442 requests a second and 0.3 ms
a second at 60. It costs a change to the rule the domain's evidence rests on,
a privileged write path from Settings, a service that does not exist yet,
and a standing exception to explain in three certification documents.

**Recommended: do not build the opt-in until the one-way barrier is built
and measured on the rig** (`docs/GAME-PATH.md` §8.1), and then only if the
remaining barrier shows in M1's tail or M8. Until then game mode's second
switch is not shown at all: a disabled switch that names a trade is an
invitation to ask for it.

### 7.8 What the safety documents would have to say

If B is built, in the same landing as the code:

* **`certification-item.json`, the reference configuration.** A named
  setting beside `mitigations` and `devmgr`: the certified setting is *no
  process admitted to a domain it was not born in*; the mechanism is in the
  item and verified (its default, its refusals, its audit record), and a
  system on which an admission was made is outside the reference
  configuration, as one built with `--mitigations off` is. That is what
  "a documented departure, off by default" means in the evidence.
* **`SAFETY-MANUAL.md`, AoU-14.** Today: "The integrator shall place in one
  domain only programs that may read each other's memory", with membership
  by birth. Added: an admission is the integrator's act through MANAGE on
  the marked job and nothing else's; the holder of that right shall admit
  only on an administrator's authenticated decision, never on a file or a
  variable the admitted program's own user can write; at most one admitted
  process per domain at a time; a domain that holds a device driver serving
  several clients shall admit none whose user may not read the other
  clients' data; partitions of different criticality still never share. And
  the sentence "so a privileged program is never in one by accident" gains
  its counterpart: no program is in one by its own asking.
* **`SECURITY-TARGET.md`.** FDP_IFF.1's speculative exception names
  admission beside birth. **FMT_MSA.3**: the restrictive default is
  unchanged in substance and must say so in the new words ("a process is in
  a domain only if born in its marked job *or admitted to it by a holder of
  MANAGE on that job*"). **FMT_MSA.1**: the attribute now has a management
  operation after creation, restricted to that right; today's note that
  FMT_SMR.1 and FMT_SMF.1 are not claimed gets harder to keep, since "an
  administrator decides" is a role in everything but name. The O.ISOLATE
  row of the rationale changes with them.
* **`SPECULATION.md`.** §3's row "Spectre v2, program → program" and the
  paragraph "Inside a speculation domain the invalidation is left out" name
  admission; §1's statement of the reference configuration names the
  setting; the measured cost goes in §8 with its conditions.
* **`VULNERABILITY-ANALYSIS.md`.** V-07 is extended to an admitted member,
  and a new entry records the driver-mediated reach of §7.5 (an admitted
  client and what `nvrm` holds of others), accepted by design only outside
  the reference configuration. V-10, V-11 and V-13 are read again against
  it.
* **`AUDIT.md`.** A new event beside `DOMAIN`: who admitted which process
  to which domain; and the leave that revokes it, if leaves are not
  recorded today.
* **`TRACEABILITY.md`** and the model: the changed `L.object.115`, the new
  ids, their checks in `object/domain_check.rs` and a negative control each.

The one-way barrier needs its own rows in the same documents; they are
`nv-trip`'s to write, and §7.6's two flows are what its vulnerability entry
has to state.

### 7.9 The review

The certification consultant reviewed this section as a design on
2026-10-11. The verdict and its conditions are recorded here once given
(ledger `~/.local/share/ferrix/cert-consultant/reviews.md` on the gate
host).

## 8. Decisions for the customer

None is taken. Each with a recommendation.

1. **Is a mark with no effect acceptable as the first version**, with the
   page saying so? *Recommended: yes; the alternative was a weight only a
   root desktop could set, which is not the customer's desktop.*
2. **What should game mode do first** once something privileged can act on
   the mark: G0's weights (`docs/GAME-PATH.md` §5.3), holding the screen
   awake, or nothing until the game session? *Recommended: G0, with its
   review and its measurement on the rig, since it is already in stage 24's
   68 points.* This is question 8 of `docs/GAME-PATH.md` §10 seen from the
   switch: with this page, "by a rule the user writes" has a place to be
   written.
3. **May a user's scope for a game sit beside the session's scope**, so
   that a weight has somewhere to be written, although it then outlives the
   session unless `sessiond` ends it too (`docs/AUTH.md` §6.4)?
   *Recommended: no; give the launcher's programs scopes inside the
   session's instead, which needs init to nest a scope.*
4. **The one-way barrier: for every client of `nvrm`, or only for
   applications in game mode?** *Recommended: every client, decided on
   `nv-trip`'s own design and review, so that the user's file never gates a
   protection.*
5. **The full opt-in (§7): build it at all?** *Recommended: not before the
   one-way barrier is measured (§7.7).*
6. **If it is built: admission (B) or birth (A)?** *Recommended: B.*
7. **Question 4 of `docs/GAME-PATH.md` §10 becomes a three-way choice**
   (§7.5): `nvrm` with the system path, `nvrm` with admitted games, or one
   domain for all. *Recommended: `nvrm` with the system path and no game
   admitted, unless the rig shows the remaining barrier matters.*
8. **Who may turn the second switch**: any member of `wheel` with their
   password, or root alone? *Recommended: `wheel` with the password, as
   `su`.*
9. **Should hyprix's own `exec` binds and the terminal mark what they
   start** (§4, not covered)? *Recommended: no; a key bound to a game can
   name the launcher's entry instead.*

## 9. Where it stands

2026-10-11: built in one session. The `settings` app, the
`compositor-desktop` crate with fuzzel moved onto it, the mark at the
launcher's start, the host tests, `test-compositor --boot settings` passing
on x86-64 under KVM and its negative control fired. Not run: the boot on
AArch64 and ARMv7-A; the page on the `--everything` desktop as the user
`ferrix` (the boot's desktop is root's, its home `/`); a screen with a
buffer scale above one other than in the host test. Not built: any effect
(§4), a second page, a pointer cursor other than the arrow, drag on the
scroll bar, a list that follows the file or the applications changing while
the window is open (it reads both when it starts and the file again at each
switch).
