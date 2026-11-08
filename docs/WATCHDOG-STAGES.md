# One hung program is that program's problem

**The customer's order (2026-11-08):** "if a single program hang triggers
the global system watchdog that's bad; instead it should trigger a
program's watchdog and kill that single program, or restart the program,
but not kill the whole system." And, the same day: "look up how other OS
handle a hang of a single process and pick the best option that is most
robust, while not having the issues like Windows, where a program is
marked as unresponsive but is actually usable, or the opposite case where
it hangs but the watchdog is never triggered."

This document designs `docs/ROBUSTNESS.md`'s class D items: R10's stage 0
(per-service watchdogs in init) and R4 (the staged system watchdog), and
the change neither names but both need: init's feed of `/dev/watchdog`
must not depend on any one program. §1 says what goes wrong today, §2
surveys what other systems do, §3 is the decision, §4 to §7 the design
by stage, §8 the tests, §9 the order of landings, and §10 where it stands.

Linux v7.0 is read from `~/.local/share/ferrix/robustness-ref/linux-v7.0/`
and systemd v258 from `~/.local/share/ferrix/systemd-ref/v258/` (fetched
2026-11-08 from the v258 tag). Both are cited by file and function, and
were read, not copied. Other systems are cited by their published
documentation; where a figure comes only from a secondary source it says
so.

## 1. What goes wrong today

`/dev/watchdog` is fed by init from its one event loop
(`src/user/system/linux/init/init/src/watchdog.rs`, `Watchdog::feed_if_due`
after `epoll_wait` in `Init::run`). When no feed comes within the timeout
the kernel's task reports FX-9008 and halts, or resets under
`ferrix.panic=reset` (`src/kernel/src/watchdog.rs`, `expired`). So the
system watchdog measures one thing: **whether init's loop turns.**
Anything that holds that loop holds the machine's life.

**The audit of init's loop** (develop 6e9d229ea). Every call the loop makes
that can wait on something outside init:

| Call | Where | Waits on |
|---|---|---|
| `mount(2)`, `umount2(2)` for `.mount` units | `main.rs` `mount`, `unmount` | the filesystem: a btrfs mount reads the disk; `fs=server` waits on btrfsd |
| `units::read` (boot, `svc daemon-reload`, `enable`) | `units.rs` `read`, `add` | the disk; and `open(2)` of a FIFO in a unit directory blocks until a writer comes |
| the audit record's file | `audit.rs` `Reader::read` | writes to a file on `/`: the disk |
| `spawn_native`'s ELF read | `main.rs` `spawn_native` | the disk |
| `user_uid`, `owner`, `make_homes` | `main.rs` | `/etc/passwd` and `/home` on the disk |
| `forked_main` | `readiness.rs` | `/proc` and the PID file: the disk |
| `say` | `main.rs` | the console: a stopped serial line (XOFF) or a full console |
| lode's `connect` | `lode.rs` `Lode::connect` | lode's listener, if its backlog is full |
| `std::thread::sleep(100 ms)` after a failed `epoll_wait` | `Init::run` | nothing; bounded |

Nothing in the loop waits on another process's IPC answer: the control
socket's clients, the notify pipes, the log pipes and the directory's
channels are all non-blocking. But the disk and the console are reached
from the loop in seven places, and "the root filesystem answers slowly"
is exactly the failure that started 2026-11-07's hangs (`ROBUSTNESS.md`
§5.1, catch1 `hang3`). One slow read is a reset.

**And a hung service is not seen at all.** `ferrix-svc` knows when a
service's processes exit, not when they stop working. A lode, hyprix or
auth that deadlocks keeps its processes and its unit stays `running`.
Whatever needs it then waits, and if that is init (it is not today, by
the audit) or every user program, the only answer left is FX-9008.

## 2. How other systems handle one hung program

For each: what it measures, where it is wrong in each direction, and what
it does.

### 2.1 Windows

* **Window "Not Responding" and ghosting.** `IsHungAppWindow` says a
  window is hung when its thread "is not waiting for input, is not in
  startup processing, and has not called PeekMessage within the internal
  timeout period of 5 seconds" ([Microsoft Learn, IsHungAppWindow][win-hung]).
  The window manager then shows a *ghost* window, a copy of the last
  image that can be moved, minimised and closed, with "(Not Responding)"
  in its title.
  * Measures: whether the GUI thread pumped its message queue.
  * False positive: a program doing a long step on its GUI thread (a
    load, a save, a modal loop that does not pump, a synchronous call) is
    marked hung although it will come back by itself. This is the case
    the customer names: "marked as unresponsive but is actually usable".
  * False negative: a program that pumps messages but is logically stuck
    (its worker thread deadlocked, the UI drawing a spinner for ever) is
    never marked. A program with no window is never looked at.
  * On a hang: ghosting, and a dialog offering *Close the program* or
    *Wait*; a close sends a hang report through Windows Error Reporting
    and terminates the process. Nothing is killed without the person.
* **Service recovery.** The Service Control Manager counts each service's
  failures since boot and takes the Nth action of its failure-actions
  list on the Nth failure: none, restart the service, restart the
  computer, or run a command, with a reset period after which the count
  goes back to zero ([Microsoft Learn, MsiServiceConfigFailureActions][win-sc];
  [golang.org/x/sys `windows/svc/mgr/recovery.go`][win-go]). By default
  only a service that ends without reporting `SERVICE_STOPPED` counts.
  * Measures: the service's process ending. There is no service
    heartbeat: a hung service that keeps running is not a failure.
  * False negative: every deadlock.

### 2.2 macOS

* **The spinning wait cursor.** The window server shows it over an app's
  windows when the app has not taken events from its queue for about two
  seconds ([The Eclectic Light Company][mac-ball]; the two seconds are
  cited from WWDC 2012 session 709, which this document could not check).
  * Measures: the main run loop serving events.
  * False positive: Windows' exactly, and Eclectic Light notes it
    "can occur repeatedly with excellently-written apps which are not in
    any form of distress".
  * False negative: as Windows'.
  * On a hang: the cursor only. Force Quit is the person's.
* **launchd.** `KeepAlive` restarts a job that exits, on conditions;
  `ThrottleInterval` (10 s by default) bounds how often a job is spawned;
  `ExitTimeOut` (20 s) is the wait between `SIGTERM` and `SIGKILL` at a
  stop ([`launchd.plist(5)`][mac-launchd]). There is no per-job heartbeat.
  * False negative: a hung daemon that keeps running.
* **The userspace watchdog (`watchdogd`).** It checks that critical
  daemons check in, and panics the machine when one does not. Apple has
  not documented it beyond its man page, so this document does not lean
  on it.

### 2.3 iOS

* **0x8badf00d.** "The operating system's watchdog monitors launch times
  and app responsiveness"; an app whose main thread does not answer in
  time at launch, resume or suspend is killed, the crash report's
  termination reason saying `0x8badf00d` ([Apple, Addressing watchdog
  terminations][ios-watchdog]).
  * Measures: the main thread finishing a lifecycle step in time.
  * False positive: a slow device, a debugger, a large first-launch step.
    Apple disables it under the debugger for that reason.
  * False negative: a hang outside those steps is not killed; the person
    swipes the app away.
  * On a hang: the kill, no question asked. Acceptable on a phone where
    the app is suspended and relaunched; on a desktop it loses work.

### 2.4 Android

* **ANR.** An app that has not answered an input event within 5 s gets
  an Application Not Responding dialog ([Android Developers, ANRs][and-anr];
  [Diagnose and fix ANRs][and-fix]). A broadcast receiver that does not
  finish in time (10 s in the foreground on Android 13 and lower) and a
  service whose lifecycle callback does not finish (secondary sources
  quote AOSP's `SERVICE_TIMEOUT`, 20 s) are ANRs too, without a dialog
  in the background.
  * Measures: input *delivered but not consumed*, and lifecycle
    callbacks finished, both by the system.
  * False positive: smaller than Windows', since an idle app with no
    input pending is never hung, but a long step on the main thread
    while the person taps is.
  * False negative: a logically stuck app that still consumes input.
  * On a hang: the dialog, *Close app* or *Wait*; traces of every thread
    are written for the report. Background ANRs are killed silently.
* **system_server's `Watchdog.java`.** A thread inside `system_server`
  posts to each critical handler thread and takes each registered
  monitor's lock (`HandlerChecker`, `Watchdog.Monitor`, `addMonitor`);
  when a check has not come back for the timeout (30 s twice, version
  dependent, [AOSP Watchdog internals][and-wd]), it dumps every thread's
  stack and kills its own process, and Android's init restarts the
  framework. This is the closest analogue to Ferrix's init: the
  supervisor watches *named critical parts* by an explicit round trip,
  never the whole of user space.

### 2.5 Linux desktops and servers

* **The compositor's ping.** `xdg_wm_base.ping` asks a client to answer
  with `pong`; "it's unspecified what will happen if the client doesn't
  respond ... or in what timeframe" (`xdg-shell.xml`, `ping`). X11's
  `_NET_WM_PING` is the same round trip. mutter's `check-alive-timeout`
  is 5000 ms by default and 0 turns it off ([GNOME commits, mutter
  check-alive-timeout][gnome-alive]); it then dims the window and offers
  *Force Quit* or *Wait*. KWin does the same.
  * Measures: whether the client's event loop dispatches.
  * False positive: users report the dialog for programs that are fine
    under load ([Fedora Discussion][gnome-fp]); it is Windows' case.
  * False negative: a stuck program whose loop still answers pings.
* **systemd's service watchdog.** `WatchdogSec=` arms a timer when the
  service is up (`src/core/service.c`, `service_enter_start_post` calls
  `service_reset_watchdog`); each `WATCHDOG=1` on the notify socket moves
  it (`service_notify_message`'s `WATCHDOG=` branch); `WATCHDOG=trigger`
  fires it at once (`service_force_watchdog`); `WATCHDOG_USEC=` changes it.
  The timer stops outside start-post, running and reload
  (`service_set_state`). When it fires, `service_dispatch_watchdog` enters
  `SERVICE_STOP_WATCHDOG` with result `watchdog`: the kill operation is
  `KILL_WATCHDOG` (`state_to_kill_operation`), which sends `WatchdogSignal=`
  (`SIGABRT` by default, so the program leaves a core) and arms
  `TimeoutAbortSec=` before `SIGKILL` (`service_enter_signal`). `Restart=`
  then decides, `on-watchdog` among its values, under `StartLimitBurst=`.
  The service is told its interval in `WATCHDOG_USEC` at exec.
  * Measures: an explicit heartbeat the program itself sends from the
    place that does its work. Only programs that opt in are watched.
  * False positive: only if the program pings from the wrong place, or
    its interval is shorter than its longest honest step; both are the
    program author's, and both are visible in the unit file.
  * False negative: a program whose pinging thread is alive while its
    work is stuck (ping from a timer thread). The rule is to ping from
    the loop that does the work.
  * On a hang: abort (core), kill, restart with backoff, give up past the
    start limit. The machine is untouched.
* **systemd as pid 1 and the hardware watchdog.** systemd pings
  `/dev/watchdog` from its own main loop (`src/core/manager.c`,
  `manager_loop` calls `watchdog_ping` each turn and sleeps at most
  `watchdog_runtime_wait`, `src/shared/watchdog.c`). It can set a
  pre-timeout and a governor (`watchdog_setup_pretimeout`,
  `watchdog_setup_pretimeout_governor`). This is today's Ferrix design,
  and it works for systemd only because systemd's loop never blocks: its
  mounts, unit loads and process starts all happen in forked helpers or
  asynchronously. Ferrix's loop does block (§1), so copying the place of
  the feed without systemd's discipline is the bug.
* **The kernel's detectors.** `kernel/hung_task.c`:
  `check_hung_uninterruptible_tasks` walks every thread every
  `hung_task_timeout_secs` (120 s), and `check_hung_task` reports one in
  `TASK_UNINTERRUPTIBLE` whose switch count has not moved. `kernel/watchdog.c`'s
  soft lockup (`watchdog_timer_fn`, `is_softlockup`: a CPU that has not
  scheduled for `2 * watchdog_thresh`) and hard lockup
  (`watchdog_hardlockup_check`, `kernel/watchdog_buddy.c`) report CPUs,
  not programs. All three report; they panic only when asked
  (`hung_task_panic`, `softlockup_panic`).
  * Measures: progress *in the kernel*: a task that does not leave an
    uninterruptible wait, a CPU that does not schedule.
  * False positive: a legitimately long uninterruptible wait (a dead NFS
    server), which is why it reports rather than kills.
  * False negative: none for what it measures; it says nothing of user
    space.
* **The pre-timeout.** `drivers/watchdog/watchdog_dev.c` takes
  `WDIOC_SETPRETIMEOUT`; `watchdog_pretimeout.c`'s
  `watchdog_notify_pretimeout` hands it to a governor; `pretimeout_panic.c`
  panics, so the report precedes the reset. No governor kills a culprit.
* **earlyoom and systemd-oomd.** Ring-3 daemons that kill by policy
  (memory pressure, swap) before the kernel's last resort. The analogy
  for stage 2: policy is decided in ring 3 ahead of time, and the kernel
  keeps only the mechanism of last resort.

### 2.6 QNX, Fuchsia and seL4

* **QNX HAM.** The High Availability Manager watches *entities*: their
  death, and, for those that register one, a missed heartbeat, with two
  severities (`CONDHBEATMISSEDLOW`, `CONDHBEATMISSEDHIGH`). Each
  condition runs a list of actions in order: notify by pulse or signal,
  wait, restart the entity (`ham_action_restart`), and after a recovery
  `ham_action_heartbeat_healthy` re-arms the check; `HREARMAFTERRESTART`
  re-arms it across a restart ([QNX 8.0 HAM, actions][qnx-actions];
  [HAM overview][qnx-over]). HAM guards itself with a second instance,
  the Guardian.
  * Measures: an explicit heartbeat, as systemd's.
  * On a hang: escalation in steps, a softer action first, then a
    restart. This is the shape §3 takes for services.
* **Fuchsia.** A component that terminates is restarted or not by its
  parent's policy; one marked `on_terminate: reboot`, allowed only by a
  security policy allowlist, reboots the system through power_manager,
  and component_manager panics if that call fails ([RFC-0110][fx-rfc];
  [termination policies][fx-term]). Fuchsia documents no per-component
  hang detection: like Windows' service manager, it acts on termination.
  Its lesson is the allowlist: only a named few may take the machine
  down.
* **seL4** leaves all of this to the system built on it; it documents no
  watchdog policy.

### 2.7 What the survey shows

| System | Measures | Busy but fine marked hung | Stuck but not marked | On a hang |
|---|---|---|---|---|
| Windows window | queue not pumped, 5 s | yes, often | yes (loop alive) | ghost; person chooses |
| Windows SCM | process exit | n/a | every deadlock | restart by count |
| macOS cursor | run loop, ~2 s | yes, often | yes | cursor only |
| launchd | process exit | n/a | every deadlock | restart, throttled |
| iOS | lifecycle step deadline | slow devices | outside the steps | kill |
| Android ANR | input delivered not consumed, 5 s | less: idle is never hung | yes (loop alive) | dialog; person chooses |
| Android Watchdog | critical threads' round trip, monitors' locks | rare | no, for what it watches | dump, kill framework, restart |
| Wayland/X11 ping | loop dispatches, 5 s | yes, under load | yes | dim; person chooses |
| systemd WatchdogSec | explicit heartbeat | only by author error | ping from wrong thread | abort, kill, restart, limit |
| Linux hung_task, lockups | kernel progress | long D waits | none | report (panic if asked) |
| QNX HAM | explicit heartbeat | only by author error | as systemd | staged actions, restart |
| Fuchsia | termination | n/a | every deadlock | restart, or reboot if allowlisted |

Three conclusions:

1. **A single signal of liveness is wrong in both directions.** Every
   one-signal GUI detector (queue not pumped, ping not answered) marks a
   long honest step as hung and misses a program whose loop runs while
   its work is stuck. Android's input rule is better than Windows' only
   because it adds a second fact: something was asked and not answered.
2. **An explicit heartbeat from where the work is done is the only
   detector without false positives**, and it needs the program to opt
   in. That suits services, which Ferrix writes, and not arbitrary
   programs and games, which it does not.
3. **Only the supervisor and the kernel may stop the machine**, and the
   systems that get this right (Android's Watchdog, Fuchsia's allowlist,
   Linux's report-not-panic) bound the set of things whose failure is
   fatal to a short, named list.

## 3. The decision

The coordinator's starting view, checked against §2, holds, with two
changes marked **(changed)**.

1. **Services: an explicit heartbeat, then abort, kill and restart (stage
   0, R10).** systemd's `WatchdogSec=` with its keys and semantics, and
   QNX's escalation: the miss is logged with the program's threads and
   their calls, `WatchdogSignal=` (`SIGABRT`) gives a core, `TimeoutAbortSec=`
   later `SIGKILL`, then `Restart=` with its doubling `RestartSec=` and the
   start limit. No false positive but the author's; no service is
   watched that did not opt in.
2. **Init's feed does not depend on init's loop (changed).** The starting
   view says init's feed must not block; the survey says why the fix is
   structural: systemd may feed from its loop only because that loop
   never blocks, and Ferrix's does (§1). So init feeds from **a thread of
   its own that does nothing else**: no allocation, no lock, no file but
   `/dev/watchdog` and `write(2)` to the console. The system watchdog then
   measures what the customer asked it to: whether ring 3 gets to run at
   all. A main loop that does not turn is *reported* by that thread (how
   long, and in which call), never a reset. Making the loop itself
   non-blocking (systemd's discipline) is still right, and is §5.3's
   follow-up list, but the machine's life no longer waits on it.
3. **GUI programs: "not responding" only when several signals agree
   (changed: the kernel's part is a report, not a flag).** hyprix's job,
   not init's: a window is not responding when, for longer than 5 s,
   *all* of these hold: a ping (`xdg_wm_base.ping`) unanswered; input
   delivered to it and not consumed (Android's rule); no frame committed.
   A program that keeps committing frames, or that has no input pending
   and answers nothing because nothing was asked, is never marked; a long
   load that renders a progress bar is not hung. Its process's CPU and
   I/O progress is a fourth signal hyprix may add from `/proc`; it is not
   required, because a program spinning in a stuck loop burns CPU and
   still is stuck, so CPU time cannot clear a window, only make the
   window's "Wait" more likely to help. The kernel's hung-task scan
   (stage 1's, §6) names a task stuck in the kernel in the log and the
   event log; it is a report for whoever reads it, and hyprix may show it
   as the reason, but the kernel flags nobody.
4. **GUI programs are never killed by a detector.** hyprix dims the
   window and offers *Wait* and *Close*, as Windows, GNOME and Android
   do; only the person closes. A game that hangs is the person's to
   close. Nothing in init imposes a watchdog on a program that did not
   ask for one.
5. **The machine resets only for a kernel that makes no progress (stages
   1 to 3, R4).** With init's feed on its own thread, a missed feed means
   that no thread of pid 1 got to run for the whole timeout. Before the
   reset: a pre-timeout dump of the tasks that are stuck (stage 1), then
   the kill of a set init registered ahead of time (stage 2), then the
   reset if the feed still does not come (stage 3).

GUI detection (3, 4) is hyprix's and lands with its owner; this document
sets the rule and owns 1, 2 and 5.

## 4. Init's feeder thread (step 2: lands first)

**What changes.** `Watchdog::from_command_line` opens the device as today
and then starts a thread, `watchdog`, that feeds every third of the
timeout. The main loop no longer feeds and no longer bounds its
`epoll_wait` by the feed.

**What the thread may do, and nothing else:**

* `write(2)` of one byte (`1`, never `V`) to the open `/dev/watchdog`;
* `clock_nanosleep(2)` for the interval;
* read two atomics the main loop writes (below);
* `write(2)` a line it formats into a fixed array on its own stack to
  standard error, which is the console.

It allocates nothing, takes no lock (not standard output's: the main
loop may hold it while the console is stopped), opens nothing after it
starts, reads no file and asks no process. The main loop's `clone3`
children run no allocator between `clone3` and `execve` (`spawn.rs`'s
rule), so a second thread in pid 1 cannot leave a child holding its
allocator's lock. The thread is started before the first unit and the
main loop waits on an atomic for its first feed, so its own start-up
(standard library thread set-up, which allocates) is over before any
`clone3`.

**The main loop's liveness, reported.** Before each turn's work the loop
stores the monotonic time in `TURNED` and a *phase* code in `PHASE`: one
of a short fixed list naming each of §1's blocking sites (`waiting` for
`epoll_wait`, `mount`, `units`, `audit`, `spawn`, `say`, `lode`, ...). On
each feed the thread looks: if `PHASE` is not `waiting` and `TURNED` is
more than the stall bound old (the watchdog's timeout, so one full
system-watchdog period), it writes

```
  init     the loop has not turned for 31 s, in `units`; the watchdog is still fed
```

once, then again at each doubling (62 s, 124 s, ...), and

```
  init     the loop turns again after 47 s
```

when it does. These are console lines, so lode takes them from
`/dev/kmsg` and they are in the event log's tail at a later report.
`waiting` is excluded because an idle init waits in `epoll_wait` with no
timeout: idleness is not a stall.

**What it no longer catches**, said in `watchdog.rs`'s module comment and
`POSTMORTEM.md` §6.11: a deadlock inside init's main loop is no longer a
reset. It is a report repeated at doublings. The machine stays reachable
(sshd and the session do not need init's loop), and a person or a
management agent can restart it. This is the customer's order: one stuck
program, init's loop included, does not end the machine. `kill -STOP 1`
stops every thread of pid 1 and is still FX-9008, as §6.11's case (1) and
the box's 2026-11-08 02:32 run showed.

**The kernel's view.** Nothing in the kernel changes for step 2.
FX-9008's text, "user space stopped feeding /dev/watchdog", stays true.

## 5. Per-service watchdogs (step 1: R10's stage 0)

### 5.1 The unit keys

| Key | Meaning | Default |
|---|---|---|
| `WatchdogSec=` | the interval; 0 or `infinity` is off | off |
| `WatchdogSignal=` | the first signal at a miss | `SIGABRT` |
| `TimeoutAbortSec=` | from that signal to `SIGKILL` | `TimeoutStopSec=` |

They parse as systemd's (`load-fragment-gperf.gperf.in`: `Service.WatchdogSec`,
`Service.TimeoutAbortSec`, `Service.WatchdogSignal` under the kill
context). Only `Type=notify` and `Type=native` services can send a ping
(Ferrix has no `NOTIFY_SOCKET`; a notify service writes to `NotifyFd=`,
a native one on its bootstrap channel). `WatchdogSec=` on another type is
a load warning and the watchdog stays off, rather than a service killed
for never pinging.

### 5.2 The pings

* A notify service writes `WATCHDOG=1` to reset the timer, `WATCHDOG=trigger`
  to fire it now, `WATCHDOG_USEC=<µs>` to change the interval for this
  run (`ferrix-svc-proto`'s `notify::parse`: `Notice::Watchdog`,
  `WatchdogTrigger`, `WatchdogUsec`). It is told its interval in
  `WATCHDOG_USEC` in its environment, as systemd's `EXEC_SET_WATCHDOG`
  does, so `sd_watchdog_enabled(3)` works unchanged. `WATCHDOG_PID` is not
  set: Ferrix's notify descriptor is the service's own, so there is no
  sender to check, and `sd_watchdog_enabled` accepts its absence.
* A native service sends WATCHDOG on its bootstrap channel: a new
  directory kind, number 7, no handle, empty name, or `trigger` as its
  name (`ferrix-native-abi`'s `directory::Kind::Watchdog`).
* Each ping is an event to the manager (`Event::Watchdog { unit, ping }`).

### 5.3 The state machine (`ferrix-svc`, manager/service.rs)

* **Armed** when the service is up: on entering start-post, as
  `service_enter_start_post` does, so a service that is slow to say READY
  is under `TimeoutStartSec=`, not the watchdog. **Disarmed** on any
  state but start-post, running and reload. A slot keeps the watchdog's
  deadline beside its timer's, and `Manager::deadline` is the earlier of
  all.
* **A miss** in start-post, running or reload: the manager logs
  `<unit>: watchdog timeout (limit 2s)`, records `Ended::Watchdog`, and
  enters a new sub-state `stop-watchdog`: `WatchdogSignal=` to whom
  `KillMode=` names (the main process for `process` and `mixed`, the
  cgroup for `control-group`), and the timer to `TimeoutAbortSec=`. It
  emits `Action::WatchdogMissed { unit, main }` first, for the backend's
  report. `ExecStop=` is not run: the program is not answering, so asking
  it to stop politely is pointless (systemd skips it too).
* **After the abort timeout**: `stop-sigkill` as today (`cgroup.kill`),
  then `ExecStopPost=`, then `stopped`, where `Restart=` decides with
  `Ended::Watchdog`: `on-watchdog`, `on-abnormal`, `on-failure` and
  `always` restart, after `RestartSec=` doubled per restart up to 32
  times it, until `StartLimitBurst=` in `StartLimitIntervalSec=` is spent
  (`ferrix-restart`'s `Policy`, unchanged).
* **A process that exits** during `stop-watchdog` goes on as in
  `stop-sigterm` (`check_stopped`).

### 5.4 The report (init's backend)

On `Action::WatchdogMissed` init reads, for each thread of the main
process, `/proc/<pid>/task/<tid>/comm` and `/proc/<pid>/task/<tid>/wchan`
(Ferrix's `wchan` names the call a thread waits in and its descriptor's
target, `fs/procfs/debug.rs` `wchan_text`) and says, as one console line
and one line to lode with the unit's name:

```
  init     lode.service: watchdog timeout (limit 2s): pid 412 threads lode/412 futex, lode-io/413 read:/dev/vda
```

At most 16 threads are named, then `and N more`. These reads are of
procfs files the kernel renders from its own task records; they take no
lock the hung program holds.

### 5.5 Who gets one

hyprix, lode, auth and the GPU driver are the services whose hang every
program feels (`ROBUSTNESS.md` §5.3's stage 0). Each needs its owner to
ping from its work loop, so each is its own landing by its owner:

* lode (native): a WATCHDOG on its channel from its main loop;
* hyprix: `WATCHDOG=1` from the frame loop, which also turns when idle;
* auth: from its request loop;
* the GPU driver is devmgr's (T0 driver restart, `docs/BACKLOG.md`), so its
  watchdog is devmgr's heartbeat, not init's, and is out of this
  document.

This landing gives the mechanism and a test service; it sets no
`WatchdogSec=` in a shipped unit.

## 6. Stage 1: the pre-timeout hang dump (R4 stage 1, kernel, core)

Its own design verdict and code review (`ROBUSTNESS.md` §9, RD.0).

* **The pre-timeout.** `WDIOC_SETPRETIMEOUT` and `WDIOC_GETPRETIMEOUT`
  (`linux/watchdog.h`; Linux's `watchdog_dev.c` `watchdog_ioctl`), the
  seconds before the timeout at which stage 1 runs; 0 turns it off; by
  default half the timeout, which `WDIOF_PRETIMEOUT` in `GETSUPPORT`
  announces. A pre-timeout is at most the timeout less one second.
* **The dump**, run by the watchdog's own task on the tick that passes the
  pre-timeout, once per expiry (a feed re-arms it):
  * every task that is runnable or running, and every task blocked whose
    switch count has not moved since the last look (`sched::inspect`'s
    sample, Linux's `check_hung_task`), with its pid, tid, names, state,
    processor, its call (`inspect::call_of`) and its kernel chain at
    link addresses (`inspect::chain_of`, `backtrace::slide`);
  * the process table read with a bounded `try_lock` (fault-relanding's
    `listed_try`, 42838ca11) and, if it stays held, one line saying so;
  * at most 128 lines, then `and N more`; formatted into fixed storage,
    allocating nothing;
  * to the kernel log, so to the console, pstore and, at a reset, the
    event log's tail; at a recovery, lode reads it from `/dev/kmsg`.
  This is fault-relanding's dump without its diagnostic parts: no list of
  "key" process names, no install from `open`, no function pointer
  through an atomic.
* **The reset does not depend on the dump** (RD.3): the dump runs in the
  task before it looks at the deadline again, it is bounded in lines and
  in spins, and stage 3's check is the task's next look. A dump that
  cannot take a lock says so and ends.

## 7. Stage 2: the sacrifice set (R4 stage 2, kernel, core)

Its own design verdict and code review.

* **The policy is init's.** After it arms the watchdog init registers the
  cgroups whose processes may be killed when ring 3 does not run: by
  default `user.slice` (people's sessions, their programs and games).
  Never init, the compositor, lode, the drivers or `drivers.slice`.
* **The mechanism is one ioctl and one list.** `FERRIX_WDIOC_SACRIFICE`, on
  the open `/dev/watchdog`, takes a descriptor of a cgroup2 directory.
  The kernel refuses the root cgroup, the caller's own cgroup and any of
  its ancestors (RD.4's root-job and own-job rule in cgroup terms), and a
  descriptor that is not a cgroup2 directory; it holds at most 8, as
  references to the cgroups' jobs. A release of the watchdog file empties
  the list.
* **When.** At pre-timeout plus half the time left (three quarters of the
  timeout by default), if no feed came, the task writes `cgroup.kill`'s
  kill to each registered cgroup and says `watchdog killed <path>: no feed
  for N s` for each. If a feed then comes, the next look says `watchdog
  recovered after killing <paths>`.
* **What it cannot help**: a stall in the kernel itself, a lock held by the
  culprit's own teardown (class C), which will not let the kill finish.
  That is stage 3's.

**Stage 3** is today's FX-9008 and `ferrix.panic`'s reset, unchanged.

## 8. Tests

`cargo xtask test-watchdog` gains these cases, each on x86-64 under KVM,
and `test-boot` runs on x86-64 KVM, AArch64 and ARMv7-A for the init
change:

| # | Case | Passes when | Control (fires on today's code) |
|---|---|---|---|
| W1 | `wd-test.service` (`Type=notify`, `WatchdogSec=2`, `Restart=on-watchdog`, a busybox script that pings five times and then sleeps) under `ferrix.init.watchdog=5` | init says the watchdog timeout with the script's thread and `nanosleep`, `SIGABRT` ends it, it is restarted at least twice, the system watchdog fed throughout and no FX-9008 in 40 s | without the manager's watchdog (control patch) no timeout line appears: the hung service runs on |
| W2 | a user program spinning (`while :; do :; done`) and one blocked for ever (`cat` on a FIFO) under `ferrix.init.watchdog=5` | no FX-9008 in 20 s, both still running | n/a: today passes too; the row guards it |
| W3 | init's loop blocked: a FIFO named `wedge.service` in `/run/ferrix/units` and `svc daemon-reload` | init says the loop has not turned, in `units`; no FX-9008 for 3 timeouts; a write to the FIFO and init says it turns again | today's feed: FX-9008 within the timeout (control patch feeds from the loop again) |
| W4 | every thread of pid 1 stopped (`kill -STOP 1`) | FX-9008 within the window, as §6.11 case (1) | n/a: the row proves a true stop still resets |
| W5 (stage 1) | as W4 with a pre-timeout | the dump lines precede FX-9008, pid 1's threads among them | control patch with the dump disabled: no dump line |

§6.11's eight cases stay.

## 9. Order

1. This design, to the certification consultant.
2. Step 2 (init's feeder thread, §4) and step 1 (per-service watchdogs,
   §5), ring 3 and `ferrix-svc`, one landing, W1 to W4.
3. Stage 1 (§6), kernel core, its own code review, W5.
4. Stage 2 (§7), kernel core, its own code review.
5. Each service's ping, by its owner (§5.5); hyprix's "not responding" (§3
   items 3 and 4), by hyprix's owner.

## 10. Where it stands

2026-11-08, watchdog-stages: this design written; consultant's verdict
below once given.

[win-hung]: https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-ishungappwindow
[win-sc]: https://learn.microsoft.com/en-us/windows/win32/msi/msiserviceconfigfailureactions-table
[win-go]: https://go.googlesource.com/sys/+/master/windows/svc/mgr/recovery.go
[mac-ball]: https://eclecticlight.co/2017/10/28/why-the-spinning-beachball/
[mac-launchd]: https://www.unix.com/man-page/osx/5/launchd.plist/
[ios-watchdog]: https://developer.apple.com/documentation/xcode/addressing-watchdog-terminations
[and-anr]: https://developer.android.com/topic/performance/vitals/anr
[and-fix]: https://developer.android.com/topic/performance/anrs/diagnose-and-fix-anrs
[and-wd]: https://xckevin.github.io/en/blog/android-watchdog-systemserver/
[gnome-alive]: https://lists.gnome.org/archives/commits-list/2020-February/msg12485.html
[gnome-fp]: https://discussion.fedoraproject.org/t/how-to-disable-app-is-not-responding-popup-on-gnome/74001
[qnx-actions]: https://qnx.com/developers/docs/8.0/com.qnx.doc.ham/topic/hamover_ACTIONS.html
[qnx-over]: https://www.qnx.com/developers/docs/6.3.2/ham_en/ham/hamover.html
[fx-rfc]: https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0110_reboot_for_critical_components
[fx-term]: https://fuchsia.googlesource.com/fuchsia/+/e4bafa03b206/docs/concepts/components/v2/termination_policies.md
