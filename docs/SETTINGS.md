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
switch is not designed into a corner. The certification consultant read it
as a design on 2026-10-11; the verdict, and what it made this section say,
is in §7.9.

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
  partitions of different criticality shall not share one. It already names
  "a driver and the client it serves". What would be new here is a client
  chosen at run time by a user, and a driver that also serves clients
  outside the domain.

Four precisions, from the consultant's read of the code:

* `CLONE_INTO_CGROUP` is called a move in the design; the code treats the
  child as born in the target job and puts it out when its domain differs
  from its parent's (`syscall/process.rs`). The result is the same today.
  But the fork of an *admitted* process, born in the game's own unmarked
  job, would not be a member; an admission rule must decide that.
* There is a fourth way to stop being dumpable, a parent that was not; a
  space a non-member comes to share is out for good; a marked job inside a
  marked job is a domain of its own; and every marking is a `DOMAIN` audit
  record.
* The equivalence class is forced, not chosen: predictor state survives any
  chain of switches that leave the barrier out, so no pairwise relation
  would be sound.
* **No program an image ships is in a domain today.** The only caller of
  the marking outside the kernel's own checks is the IPC benchmark. Making
  `nvrm`'s job marked is by itself a change to what the reference
  configuration contains.

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
| What it can cost | nothing, later a share of the user's own processor time | the separation between the driver and, in effect, every program of that user while the admitted one runs |
| Read by | the launcher | the session service only |

An application gets the effect only if it is in **both**: the user wants it
treated as a game, and an administrator accepted the trade for it.

**What root's list really grants is wider than one application**, and the
page would have to say so. The list names a desktop-file ID, and what that
ID runs is the user's: an entry under the user's own directory shadows the
system's, and the `Exec` line, the game's files and its environment are the
user's to write. `FERRIX_GAME_MODE_APP` can be set by any program, and a
window's app id is chosen by the client. Neither the launcher nor hyprix is
a trusted source of which process is which application. So while an
admitted process runs, the grant is in effect to *every program of that
user*, not to "that application". Either the table's cost row and the
page's words say that, or the list names a root-owned executable that the
service itself starts, which no Steam game is.

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
  the *driver's* cgroup, charged to its limits and its pin accounting
  (V-13), killed with it, and outside the user's session scope, so the
  session's end would not end it; and moving it to where G0's weight is
  written would take it out of the domain.
* It is not free of item change either. A native process is made with its
  creator's ids (`AUDIT.md` §1), so a starter born in `nvrm`'s job would
  have the service's uid and leave the domain on becoming the user. And the
  service would hold MANAGE on the driver's job, which is also the right to
  kill the driver and to start anything there.

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
* Leaving is built and one-way (`leave_speculation_domain`, with F-60's
  grace period). *Revoking is not*: no call lets one process make another
  leave, except a move between jobs, which this option says it leaves
  alone. Revocation by a third party would be a designed call of its own,
  and an admission should be tied to something whose loss ends it (the
  admitter's death, the handle's close).
* The scheduler's and the cgroup's view of the game are untouched.

**Recommended: B, if this is built at all** (§7.7 says when), and not as
drafted above: the consultant's conditions for B's own design round are in
§7.9. A looks cheaper in evidence, is wrong in structure, and needs an item
change too; its starter process would be a second, privileged way of
starting applications beside the launcher.

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
  hyprix, which sees every window and every key. So one of two must be
  chosen: `nvrm` shares with the system path and no game is ever admitted;
  or `nvrm`'s domain is for admitted games and the system path keeps its
  barriers towards `nvrm`. A third, one domain for all, in which an
  admitted game can read the compositor, is withdrawn here and not offered.
* **It collides with the game session as written.** `docs/GAME-PATH.md`
  §5.4 has the service "move the client's job" into the game domain. If
  that is a move of the process between jobs, an admitted game leaves its
  speculation domain for good at the moment it gains its session. One of
  the two designs has to change before both can exist.
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
* **Less than half.** One of the two barriers a round trip pays is left
  out. From the stage columns of the same table the way back falls from 5.4
  to 1.6 µs and the way in with the handler from 9.7 to 3.0 µs: the one-way
  saving is nearer 3.8 than 5 of the 10.8 µs, plus the client's cold run
  afterwards, which no column holds.
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

**What this account leaves open, handed to `nv-trip`'s own review** (the
consultant's W1 to W7; ledger line 690 is that review's ground):

1. The ledger already refused a rule told per request by the load ring, so
   the rule is an attribute of a job. The skip is then taken by *whatever
   space a processor enters after the driver's*, client or not, unless the
   entering side is named by a core-ring fact too. The design must say
   which, and what an intervening kernel thread or the idle loop does.
2. "The driver steering the client adds little" holds only for a client
   blocked in its own call (V-10). For any other program that follows
   `nvrm` on a processor it is new reach, and it composes with V-11.
3. The history channel has an injection side the account above lacks:
   without compromising `nvrm`, one client chooses which of `nvrm`'s
   branches run, and so what the next program consumes.
4. Probing gives a client `nvrm`'s branch targets and so its layout, which
   makes V-11 easier.
5. Whether the return-stack refill (or the ERAPS clear) stays on the way
   back, said for each architecture, or x86-64 stated alone.
6. A one-way source that is in a domain with others hands their training on
   to the client: excluded when the job is made, or carried in the
   vulnerability row.
7. The saving measured per leg, on bare metal.

### 7.7 Whether to build it

The opt-in buys what the one-way barrier leaves: on the bench's figures
about 7 µs a request on the bench's figures (§7.6), 17 ms a second at 2,442
requests a second and 0.4 ms a second at 60. It costs a change to the rule the domain's evidence rests on,
a privileged write path from Settings, a service that does not exist yet,
and a standing exception to explain in three certification documents.

**Recommended: do not build the opt-in until the one-way barrier is built
and a bare-metal figure exists**, and then only if the remaining barrier
shows in M1's tail or M8. The consultant's ledger line 690 rules the same
way, and the distinction matters: the 11 µs is a KVM bench figure on a
loaded host, and the rig of `docs/GAME-PATH.md` §8.1 is two guests, so a
figure "measured on the rig" is not the one owed. Until then game mode's second
switch is not shown at all: a disabled switch that names a trade is an
invitation to ask for it.

### 7.8 What the safety documents would have to say

If B is built, in the same landing as the code:

* **`certification-item.json`, the reference configuration.** A *boot
  setting*, read once before any program runs, default off, recorded in the
  manifest and in the `SYSTEM` audit record exactly as `devmgr` and
  `devmgr_settings` are. With it off the admission call refuses
  unconditionally, which a check proves on every boot and a control shows
  firing. The mechanism is in the item, with requirements, checks, controls
  and coverage for both states; no claim is made of "on" beyond that off is
  off. This is the shape the consultant says the four targets can carry.
  The first draft's "a system on which an admission was made is outside the
  reference configuration" cannot be: the reference configuration is a
  build and boot configuration, and a run-time history is something nothing
  attests; and guidance alone is not enough, since a call reachable in the
  evaluated build is a TSF interface whether or not guidance forbids it,
  and for DO-178C is active code or deactivated code with a shown means
  against activation. Whether the item takes such a mechanism at all is the
  customer's decision in `CLAIM.md` (§8).
* **`SAFETY-MANUAL.md`, a new assumption of use**, written beside AoU-14
  rather than widening it: an admission is the integrator's act through a
  right of its own on the marked job; its holder shall admit only on an
  administrator's authenticated decision, never on a file or a variable the
  admitted program's own user can write; a domain that holds a device
  driver serving several clients shall admit none whose user may not read
  the other clients' data; partitions of different criticality still never
  share; no program is in a domain by its own asking. "At most one admitted
  process per domain" is not an assumption: the kernel enforces and counts
  it.
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
  setting; §4, §5, §7 and §9 are read again; the measured cost goes in §8
  with its conditions.
* **`VULNERABILITY-ANALYSIS.md`.** V-07 is extended to an admitted member,
  and a new entry records the driver-mediated reach of §7.5 (an admitted
  client and what `nvrm` holds of clients outside the domain), with the
  T.MEMORY row. V-10 and V-13 are read again against it, and V-11 against
  "a client learns `nvrm`'s layout".
* **`AUDIT.md`** and the Security Target's FAU_GEN.1 event list: an
  admission as `GRANTED` (admitter, admitted, domain), a refusal as
  `REFUSED`, the leave that revokes, and the setting in `SYSTEM`.
* **`TRACEABILITY.md`** and the model: `L.object.115` is *not* stretched;
  it stays the rule for domains that do not admit, and each admission rule
  gets an id of its own, reserved first in `requirement-reservations.json`,
  with one check in `object/domain_check.rs` and one control. `H.TRAP.17`
  and `H.OBJ.8` are read again.
* **`ITEM.md` and the native ABI**: a new call number is in the item ring.
* **`MEMORY-AND-TIMING.md`**: the admit path's locks and their bounds, and
  that a ring-3 service could now cause the leave's grace period on every
  processor at will.
* **`CLAIM.md`**'s open decisions; **`docs/AUTH.md`** for the helper that
  writes root's list; and coverage carried for both settings.

The one-way barrier needs its own rows in the same documents; they are
`nv-trip`'s to write, and §7.6's two flows are what its vulnerability entry
has to state.

### 7.9 The review

The certification consultant reviewed this section as a design on
2026-10-11, at commit 21b54b3d7. It is an internal review, not independent
verification (F-27, F-29). Ledger line 698 of
`~/.local/share/ferrix/cert-consultant/reviews.md` on the gate host:

> OK IF (G1)-(G9) for §7/§8 to land as a proposal; opt-in code (A or B) NOT
> YET — a design round of its own on (B1)-(B13), and not before the one-way
> barrier is built and a bare-metal figure exists (ledger 690); the one-way
> barrier stays nv-trip's design review under 690, which must also answer
> (W1)-(W7).

The built game mode changes no defence and got no verdict: the branch
touches no file of the item.

**G1 to G9** are conditions on this document, and it carries them: the
four precisions in §7.2; what root's list really grants in §7.3; option
A's item-side problems and "revoking is not built" in §7.4; the third
choice withdrawn and the conflict with the game session's move in §7.5; W1
to W7 and the 3.8 µs reading in §7.6; ledger 690 and the bare-metal figure
in §7.7; the boot setting, the new assumption of use and the longer list in
§7.8; the decisions reworded in §8; and this record.

**B1 to B13** are what option B's own design owes before any code:

1. The boot setting of §7.8, with its off-state check and control.
2. A right of its own, as SET_LIMIT was split from MANAGE for F-40: given
   only by `job_create` to the maker of a marked job, never through
   `job_for_cgroup` or a cgroup file; and on the other side a named right
   on a handle to the process, never a comparison of uids.
3. Admissibility is a property of the domain fixed at its creation (a
   second `job_create` option, in the `DOMAIN` record); a domain made
   without it never admits.
4. "At most one admitted per domain" enforced and counted by the kernel,
   with a check and a control.
5. What an admission covers, stated and checked: threads, forks (out
   today) and `execve` (kept); and which of a Steam or Proton game's
   processes makes the requests, measured first.
6. The admission tied to something whose loss ends it; revocation by a
   third party a designed call, not a cgroup move.
7. Every new per-process state enumerated for fork, clone, `execve` and
   `launch::load_native`, failing closed when absent.
8. "The barrier issued once on every processor that last ran it" on
   admission argued or dropped; what needs stating is the order of the
   space's domain store against a concurrent switch and a concurrent
   `CLONE_VM`.
9. `L.object.115` not stretched; an id, a check and a control per rule.
10. The audit records of §7.8.
11. Lock bounds in `MEMORY-AND-TIMING.md`.
12. All three architectures checked; the cost stated as x86-64 only.
13. The vulnerability row for what `nvrm` holds of clients outside the
    domain (ledger 690) and the new assumption of use, in the design.

## 8. Decisions for the customer

Each with a recommendation. The customer answered three things on
2026-10-11, which decide questions 1, 2 and 5 and the direction of 8; the
rest are open.

1. **Is a mark with no effect acceptable as the first version**, with the
   page saying so? *Recommended: yes.* **Decided by the customer on
   2026-10-11: "yes".**
2. **What should game mode do first** once something privileged can act on
   the mark? *Recommended here: G0's weights.* **Decided by the customer on
   2026-10-11, and not as recommended:** *"disable speculative protections
   and basically everything that brings down latency and brings up fps that
   might not be acceptable in normal high assurance apps, but this path
   should require admin privileges on the system, as otherwise normal apps
   could disable some protections"*. So the first privileged effect is the
   speculation protections and the other trades of latency and frame rate
   that a high-assurance program would not accept, for the application
   switched on and for no other; G0's weights are not first. It is to be
   designed and built, behind administrator privilege. §7 is the first
   piece of that design and stays a proposal until its design round
   (§7.9, B1 to B13) is reviewed; the full list of protections and trades,
   and which of them can be per application at all, is that round's to
   write. Nothing of it is built, and the page still says game mode is a
   mark only.
3. **May a user's scope for a game sit beside the session's scope**, so
   that a weight has somewhere to be written, although it then outlives the
   session unless `sessiond` ends it too (`docs/AUTH.md` §6.4)?
   *Recommended: no; give the launcher's programs scopes inside the
   session's instead, which needs init to nest a scope.*
4. **The one-way barrier: for every program that follows the driver on a
   processor, or only towards jobs the integrator names?** (§7.6, point 1:
   as an attribute of the driver's job it is the first unless the entering
   side is named too.) *Recommended: decided on `nv-trip`'s own design and
   review; either way the user's file never gates a protection.*
5. **The full opt-in (§7): build it at all?** *Recommended here: not
   before the one-way barrier is measured (§7.7).* **Decided by the
   customer on 2026-10-11 with question 2: build it.** The order the
   consultant set stands and is not what was decided: the one-way barrier
   and a bare-metal figure first, then the design round, then code (ledger
   lines 690 and 698).
6. **If it is built: admission (B) or birth (A)?** Both need a change to
   the item. *Recommended: B, after its own design round (§7.9, B1 to
   B13).*
7. **Question 4 of `docs/GAME-PATH.md` §10 becomes a choice of two**
   (§7.5): `nvrm` with the system path, or `nvrm` with admitted games.
   *Recommended: `nvrm` with the system path and no game admitted, unless a
   bare-metal figure shows the remaining barrier matters.*
8. **Who may turn the second switch**: any member of `wheel` with their
   password, or root alone? *Recommended: `wheel` with the password, as
   `su`.* **The direction is decided (customer, 2026-10-11): administrator
   privilege is required**, "as otherwise normal apps could disable some
   protections". Which of the two forms that takes is still open.
9. **Should hyprix's own `exec` binds and the terminal mark what they
   start** (§4, not covered)? *Recommended: no; a key bound to a game can
   name the launcher's entry instead.*
10. **Does the certified item take an admission mechanism at all**, in the
    shape of §7.8: in the item, behind a boot setting that is off in the
    reference configuration? It is the customer's decision in
    `docs/certification/CLAIM.md`. It is **still open**, and the answer to
    question 2 makes it the hard one: the customer wants the trade per
    application at run time, and the consultant's shape puts it behind a
    setting fixed at boot. How a certified configuration and one that can
    grant game mode relate is the design round's first question.

## 9. Where it stands

2026-10-11: built in one session. The `settings` app, the
`compositor-desktop` crate with fuzzel moved onto it, the mark at the
launcher's start, the host tests, `test-compositor --boot settings` passing
on x86-64 under KVM and its negative control fired. Not run: the boot on
AArch64 and ARMv7-A; the page on the `--everything` desktop as the user
`ferrix` (the boot's desktop is root's, its home `/`); a screen with a
buffer scale above one other than in the host test. §7 was reviewed as a
design by the certification consultant (OK IF G1 to G9, which this
document carries; code NOT YET; ledger line 698, §7.9). The same day the
customer decided that the mark with no effect is acceptable as the first
version, and that the first privileged effect is to be the speculation
protections and the other latency and frame-rate trades, behind
administrator privilege (§8, questions 1, 2, 5 and 8); that design round is
on branch `game-mode-design`. Not built: any effect
(§4), a second page, a pointer cursor other than the arrow, drag on the
scroll bar, a list that follows the file or the applications changing while
the window is open (it reads both when it starts and the file again at each
switch).
