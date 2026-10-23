# Logging: one place for every log

Version 1, 2026-10-23, design only. Nothing here is built.

The customer asked (2026-10-23): "do we need overall better logs? Should
we have an agent design something like dmesg+ which is the central location
for all logs? where we can then design a central log manager to get all logs
from?" The direction agreed with the product owner is a central log
service in ring 3, compatible with systemd's journal model, not a bigger
`dmesg`. The customer then added: "and it shall be fully isolated ring3
program like our device manager", and asked what people have always wanted
from journalctl; that research is
[`docs/logging/USER-WISHES.md`](logging/USER-WISHES.md), and §2 takes its
top ten in. This document is the design: what feeds the log, what a record
is, how floods are held, where records are kept, how they are read and
forwarded, who may read them, how the service is isolated and restarted,
and the order of the work.

The log manager is called **lode** (customer, 2026-10-23: "lode sounds
good"; a vein of ore, where you dig for what happened; recorded in
`docs/backlog/DECISIONS.md`). The program and the service are `lode`, the
crate `ferrix-lode`, its units `lode.service`, `lode-dev.socket` and so
on, its user `lode`. (`logctl` is taken: it is the kernel's log protocol
for the Pixel 7's USB driver, `src/lib/proto/logctl`.)

`lode` is **one native program** with three roles, chosen by subcommand
or by the name it was started under:

| Started as | Role |
|---|---|
| `lode serve` (what `lode.service` runs) | the service: takes records in, stores them |
| `lode [options]`, or `journalctl [options]` through a link | the reader, with journalctl's options; the link exists only where the Linux personality does (`linux_abi=on`) |
| `lode status`, `top`, `level`, `reload`, `delete`, `verify`, `recover`, `config`, `save`, `cat`, `upload`, `receive` | administration; `lode cat` is `systemd-cat` (also linked under that name); `upload` and `receive` run as units of their own (§10) |

The whole is called **the log**.

## 0. Summary

* `lode` is a **native Ferrix program** in a **sealed job of its own**,
  started early by init as `lode.service`, running as a user of its own
  (`lode`), holding only the descriptors and channels init hands it. It
  takes no device, no network and no handle that reaches another service.
  It can crash and be restarted by init without losing records: the
  kernel's ring is replayed by sequence number, init keeps what services
  wrote while it was down, and every gap it cannot close is said in a
  record of its own (§11).
* **Sources**: the kernel log through `/dev/kmsg` (the parallel branch
  `feature/etxtbsy-kmsg-dmesg`), every unit's standard output and error
  (init's pipes, as today), `/dev/log` (syslog), systemd's native protocol
  and stdout streams, a native channel `ferrix.log` for ring-3 drivers and
  native services, the `devmgr-events` feed, the last boot's kernel log
  from pstore, and files of programs that write nowhere else (§5).
* **Records** are journald's `FIELD=value` pairs; the trusted `_` fields
  come from the kernel, init or `devmgr`, never from the sender (§4).
* **One filter language** -- `and`, `or`, `not`, parentheses, globs and
  regular expressions on any field -- for the command line, ingest rules,
  retention, forwarding and alerts (§6).
* **Floods** are held per source and per priority, folded, sampled or
  dropped by rule, and every record not kept is counted where a reader
  sees it (§7).
* **Storage** is an own, documented, append-only segment format with a
  checksum per record, written with batched `pwrite`s and compressed when
  sealed, a budget on bytes written per byte logged, retention classes
  per service and priority, a RAM tier and a disk tier, split by boot.
  Not journald's file format; journald's export and JSON formats byte for
  byte (§8).
* **Reading**: `lode`, with journalctl's options (`lode -b -1 -u hyprix`,
  `lode -f -p err`, `lode -k`), plus the filter
  language, this run of a service, output templates and defaults that
  need no flags; queries that skip whole segments from a summary, so the
  last lines of a unit cost the same on 100 MB and 100 GB. `dmesg` stays
  the live kernel ring (§9).
* **Forwarding** is opt-in, store-and-forward from a cursor, verified
  TLS, journal-upload's export, RFC 5424, JSON lines and OTLP out, syslog
  in, a merged view of many hosts; for box tests, `ssh box lode -f`
  from the first usable step (§10).
* **Users read their own logs without root** (§12).
* **Kernel lines: 0** beyond `/dev/kmsg`, which the other branch adds.
  One optional kernel item (datagrams sent to a name, for libsystemd's
  `sd_journal_send`) is the customer's call and nothing else waits for it
  (§14).
* **Points**: about 66 in ten steps; after the first three (25 points)
  box tests stop polling `dmesg` (§17).

## 1. Today, and what hurts

What the N150 box showed in October 2026, each item seen there:

| Pain | Cause |
|---|---|
| Every boot message gone within minutes | The kernel log is a 128 KiB ring; the Intel GPU driver's `ferrix.intelgpu.freqlog` writes a line a second to standard error, which is the console, which is the ring |
| Logs in five places | Kernel, drivers, init's units, `sessiond`, hyprix in `dmesg`; `/tmp/yserver.log` (`/etc/yserver.sh`); `/home/ferrix/plog/proton.out` and `plog/dxvk/` (`tools/common/n150/endfield/ef-*.sh`); Steam's `logs/console_log.txt`; the game's logs in the Wine prefix |
| No times | `dmesg` printed none (fixed on the kmsg branch) |
| Nothing survives a reboot | The ring is RAM; init's per-unit ring (`docs/INIT.md` §10, 256 lines) is RAM; only panics survive, through pstore (`docs/POSTMORTEM.md`) |
| Box tests lose lines | Tests poll `dmesg` over ssh, and what the ring overwrote between two polls is gone without a trace |
| Who said it? | A driver's line is bytes on the console; a desktop program's line is in `sessiond`'s unit with `sessiond`'s pid |

What exists and is reused:

* **The kernel log as records** (on `develop` since 4c3087f43 and
  815604fa7, branch `feature/etxtbsy-kmsg-dmesg`, by a parallel agent): `/dev/kmsg` in
  Linux's record format (`prio,seq,usec,-;text`), `EPIPE` once for an
  overrun reader, `SEEK_SET`/`SEEK_DATA`/`SEEK_END`, `log_buf_len=` up to
  4 MiB, `** N printk messages dropped **`, times through `syslog(2)`, and
  Ferrix's own `/bin/dmesg`. Programs' console output is `LOG_USER` info
  (14), kernel lines `KERN_INFO` (6). `lode` consumes this and duplicates
  none of it.
* **init's log pipes** (`src/user/system/linux/init/init/src/logs.rs`):
  `StandardOutput=log` is a pipe init reads, a line at a time, attributed
  to the unit and to the spawn's pid. `StandardOutput=journal` and `kmsg`
  already read as `log` (`docs/INIT.md` §16, L1).
* **init's directory** (`docs/INIT.md` §6): `Offers=`/`Uses=` names routed
  as channels, which is how a native service is reached.
* **`.socket` units** (L9): init binds a socket and hands it to the
  service, as systemd does for `systemd-journald.socket`.
* **`devmgr-events`** (`src/user/system/native/devmgr-events`,
  `docs/DESKTOP.md` §4.2): each driver's DIED, RESTARTED and gave-up as one
  line to every reader of the abstract socket `ferrix.devmgr.events`.
* **pstore** (`docs/POSTMORTEM.md` §3): the kernel log across a reset, which
  the next boot's loader copies to `FERRIX/LASTKERNEL.TXT`.
* **The audit record** (`docs/certification/AUDIT.md`): the kernel's FAU
  rings, read by init into `/var/log/audit/<id>.bin`. It stays as it is
  (§15).
* **The kernel's Unix sockets** (`src/kernel/src/fs/socket.rs`): stream,
  datagram, `SCM_RIGHTS`, `SCM_CREDENTIALS` checked against the sender
  (`EPERM` unless root, as `scm_check_creds`), `SO_PASSCRED`,
  `SO_PEERCRED`, a datagram socket `connect`ed to a bound path. **Not
  there**: a datagram sent to a name with `sendto`/`sendmsg` on an
  unconnected socket, which answers `EOPNOTSUPP`
  (`syscall/sockets.rs`, `check_destination`). §5 and §14 say what that
  costs.
* **zstd**: `ruzstd` (pure Rust, `no_std`) is already in the tree for
  btrfs (`src/lib/fs/btrfs/format`).

## 2. What users want, and where this design answers it

`docs/logging/USER-WISHES.md` ranks 26 themes from systemd's issues, Stack
Exchange, Hacker News, LWN and the documentation of journald's
competitors. Its top ten, each with where this design meets it:

| # | The wish (USER-WISHES.md) | Strongest evidence there | Here |
|---|---|---|---|
| 1 | Writes that cost about what the line costs | systemd #40262 (r=71, open): ~50 IOPS for 2 lines/s; HN 2026-08, 264 points | §8.2: append-only, batched `pwrite`, no `mmap`, metadata written once per source, zstd when sealed; §8.3: a bytes-written budget checked in a gate row |
| 2 | Queries that stay fast as the store grows | #2460 (r=42, open since 2016); Ask Ubuntu, 110 votes | §8.4: a summary per segment so queries skip whole segments, field indexes, a sparse time index, reverse reading for "last N"; §9.7: a benchmark row on 1 GB and 100 GB |
| 3 | A real filter language | #2720, "add NOT", r=131, the most-voted journal issue | §6: one language for CLI, ingest, retention, forwarding and alerts; journalctl's flags compile to it |
| 4 | Control of noisy sources at ingest, never silent | #6432, c=164, the most-discussed journal issue | §7.2: drop, sample, cap the level, fold repeats, each rule counted; `lode top` |
| 5 | Retention and quotas per service and priority; delete by match | #9519 (r=58); "How to clear journalctl", 706 votes, 1.18 M views | §8.5: retention classes, each its own segment series, so expiry is deleting files; a RAM tier and a disk tier; `lode delete` |
| 6 | Every entry attributed, a dying process's last lines included; stdout vs stderr | #2913 (r=31, open since 2016); #5019 (r=42) | §4.2: identity taken when the writer is connected, by the party that starts it, not read from `/proc` afterwards; `_STREAM` |
| 7 | Output usable without flags | #3449 (r=41); "prevent truncating", 169 votes; #15899, #5194 | §9.3, §9.4: no truncation, year and zone, the priority shown, a light-terminal palette, `--format` templates, every printed time parses back |
| 8 | A crash costs at most the unsynced tail, and logging never stops | LWN 2014, CoreOS "corrupts its files and completely stops logging" | §8.2: per-record checksum, truncate the torn tail, sealed segments immutable, readers bounded; §11.4: a full or broken store degrades to RAM |
| 9 | Store-and-forward shipping, syslog in and out | #2877, #19426 (upload quits), #4092 (certificates not verified), #19251 (RFC 5424, c=77) | §10 |
| 10 | The right slice by default: this run, previous boots, my own logs | #1942 (r=38); previous boots, 131 votes; #2744 (r=22) | §9.2 (`-u unit@last`, invocation ids), §8.1 (persistent by default), §12 (per-user segments) |

Themes 11 to 26 are taken where they are cheap and named where they are
not: early boot and kernel messages (16) in §11.5; structured stdout as
JSON (17) in §5; never stalling (18) in §11.4; the collector's own
security (19) in §11.2; runtime log levels (21) through `INTEREST` and
`lode level` (§5, §7); alerts (22) in §7.4; redaction (23) as an opt-in
rule (§7.2); multi-line messages (24) in §5; namespaces (25) as an indexed
field (§4.1). A TUI viewer (20) waits until the top five exist, as the
research advises.

## 3. How others do it, and what Ferrix takes

**systemd-journald.** One daemon reads `/dev/kmsg`, `/dev/log`, its
native datagram socket, stdout streams handed to services, and audit. A
record is a set of `FIELD=value` pairs; fields beginning with `_` are
trusted, written by journald from `SCM_CREDENTIALS` and `/proc`. Rate
limiting is per service, per priority, in a window
(`RateLimitIntervalSec=30s`, `RateLimitBurst=10000`), the burst scaled
with the free disk space, and a suppressed run becomes one message.
Storage is its own indexed file format (hash tables of field values, entry
arrays, offsets, optional sealing) written through a shared mapping,
rotated by size and time, with `journalctl` reading the files directly.
Sockets belong to systemd's socket units, so datagrams queue in the
kernel while journald restarts, and journald parks its stdout streams in
systemd's descriptor store so they survive a restart. **Ferrix takes**:
the field model, trusted fields, the protocols, the limiter's shape,
socket ownership by init, the descriptor store, journalctl's command line,
the export and JSON formats, `--flush`/`--relinquish-var`. **Ferrix
leaves**: the file format and its in-place updates through a mapping (the
cause of #40262, §8.6), scaling a limit by free disk (a surprise,
USER-WISHES theme 15), attribution by reading `/proc` after the fact
(#2913), `_CMDLINE` by default (§12.3), and running as root (the 2019
"System Down" exploit was in journald's parsing).

**syslog-ng.** A graph of sources, parsers, filters and destinations,
written in a configuration language; lenient RFC 3164 and strict RFC 5424
parsers; filters with `and`, `or` and `not`; flow control between a source
and a slow destination; a disk buffer that makes a remote destination
reliable across restarts and outages. **Ferrix takes**: the lenient
parsers' rules, a boolean filter language used in more than one place, and
the disk-backed cursor for forwarding: the store itself is the buffer, and
a forwarder keeps only its cursor (§10). **Ferrix leaves**: the
configuration language and the general graph; there is one store and the
forwarders read it.

**Fuchsia's archivist.** Logs are a service the component framework
routes: a component reaches `fuchsia.logger.LogSink` only because its
manifest asks for it, and the archivist learns *who* is writing from the
framework (the component's moniker), not from the writer. Each component
has a budget in the archivist's memory; what exceeds it is "rolled out"
and counted. The archivist tells a component the lowest severity anyone
wants (`interest`), so debug lines are not even formatted until someone
asks for them. The kernel's log reaches it through a read-only kernel
handle; readers (`log_listener`, `ffx log`) subscribe with filters. The
archivist is an ordinary sandboxed component. **Ferrix takes**:
attribution by the party that started the writer -- init for units,
`devmgr` for drivers -- carried in the channel's routing, not the message
(§4.2); a per-source budget with a counted overflow; `INTEREST` on the
native channel (§5); and the archivist's place: an ordinary isolated
service, not part of the kernel (§11).

**From the rest of USER-WISHES.md's comparisons**: Loki's split -- index
the labels, scan compressed text for the rest -- is §8.4's; LogQL's and
macOS `log --predicate`'s operators are §6's; Windows Event Viewer's
"attach a task to this event" is §7.4's alerts; lnav's merged multi-file
view is `lode --merge` (§10.4).

## 4. The record

### 4.1 Fields

A record is an ordered list of fields. A field name is 1 to 64 bytes of
`A-Z`, `0-9` and `_`, not beginning with a digit; a value is any bytes,
up to 64 KiB (`MESSAGE` up to 256 KiB, the rest cut and `_TRUNCATED=1`
added). A record is at most 1 MiB. These are journald's rules, so that
anything journald accepts round-trips.

| Field | Set by | From |
|---|---|---|
| `MESSAGE`, `PRIORITY`, `SYSLOG_FACILITY`, `SYSLOG_IDENTIFIER`, `SYSLOG_PID`, `MESSAGE_ID`, `CODE_FILE`, `CODE_LINE`, `CODE_FUNC`, `ERRNO`, `TRACE_ID`, `SPAN_ID`, any other non-`_` field | the sender | the protocol |
| `_PID`, `_UID`, `_GID` | `lode` | §4.2 |
| `_COMM`, `_EXE` | `lode` | §4.2 |
| `_SYSTEMD_UNIT`, `_SYSTEMD_SLICE`, `_SYSTEMD_CGROUP`, `_SYSTEMD_USER_UNIT`, `_SYSTEMD_INVOCATION_ID` | `lode` | §4.2. Ferrix's init builds systemd's cgroup tree and unit names (`docs/INIT.md` §5.1), so systemd's field names are the honest ones and tools that filter on them work. The invocation id is new in init: 128 random bits per start of a unit, in the service's environment as `INVOCATION_ID`, as systemd's |
| `_STREAM` | `lode` | `stdout` or `stderr`, for lines that came through a unit's pipes (Ferrix's; #5019) |
| `_FERRIX_DRIVER`, `_FERRIX_DEVICE` | `lode` | `devmgr`'s attestation (§5) |
| `_NAMESPACE` | `lode` | the writer's mount and network namespace ids, from `/proc/<pid>/ns/` at attribution; an ordinary indexed field, not a separate store per namespace (USER-WISHES theme 25) |
| `_TRANSPORT` | `lode` | `kernel`, `stdout`, `syslog`, `journal`, `driver`, `driver-fallback`, `pstore`, `devmgr`, `file`, `remote`, `audit` (§15); all but journald's first five are Ferrix's |
| `_BOOT_ID` | `lode` | 128 random bits drawn when `lode` first starts in a boot, kept in `/run/log/boot_id` so a restarted `lode` keeps it. The kernel has no `/proc/sys/kernel/random/boot_id`; if one is added later for its own reasons, `lode` uses it |
| `_MACHINE_ID` | `lode` | `/etc/machine-id`, written once by the installer or at first boot |
| `_HOSTNAME` | `lode` | `uname`'s nodename |
| `_SOURCE_REALTIME_TIMESTAMP`, `_SOURCE_MONOTONIC_TIMESTAMP` | `lode` | the kernel's stamp for kernel records (`usec` in `/dev/kmsg`), init's receive time for unit lines it buffered, the sender's for native-protocol records that carry one |
| `_KERNEL_SEQNUM` | `lode` | `/dev/kmsg`'s sequence number (Ferrix's): how a restarted `lode` knows where it stopped (§11.3) |
| `REPEATED`, `LAST_MESSAGE`, `N_SUPPRESSED`, `N_DROPPED`, `N_BLANK` | `lode` | on the records that say what a rule folded or dropped (§7) |
| `FERRIX_PRIORITY_SOURCE`, `FERRIX_PRIORITY_WORD` | `lode` | where the level came from, and the words a derived one was read from (below) |
| `__REALTIME_TIMESTAMP`, `__MONOTONIC_TIMESTAMP`, `__UTC_OFFSET`, `__SEQNUM`, `__CURSOR` | the store | when `lode` stored it; the UTC offset in force then (USER-WISHES theme 8; journald keeps none, #6589); the store's own sequence; a cursor in journald's text shape |

A sender that writes a field beginning with `_` has it dropped, as
journald drops it. `_CMDLINE` is not recorded by default (§12.3).

**Every record has a level, and `unspecified` is one of its own** (the
customer, 2026-10-23). A record nobody gave a level is never shown as info:

* The store keeps a level on every record, 0 to 7 or `unspecified` (8 in
  the entry's priority byte), and `FERRIX_PRIORITY_SOURCE` says where it
  came from: `stated` (the writer said it: `PRIORITY=`, a `<PRI>` or `<N>`
  prefix, a `tracing` line's `WARN`, lode's own records), `kernel` (a level
  other than info the kernel was handed with the line, a `/dev/kmsg`
  write's `<3>`), `derived` (read from the line's words, `failed` or
  `refused`, with the words in `FERRIX_PRIORITY_WORD`), or `unspecified`.
  A sender's own `FERRIX_PRIORITY_SOURCE` or `FERRIX_LEVEL` is dropped.
* Info that arrives with no word from the writer says nothing and is
  stored as unspecified: the kernel's own lines and programs' console
  output all come at info, as does every unit's stdout stream unless its
  header says another level; `lode cat` without `-p` sends none, and `-p
  info` puts `<6>` on each line. A syslog datagram without `<PRI>` and a
  native datagram without `PRIORITY` are unspecified too.
* **`FERRIX_LEVEL`** is the explicit level every output that shows fields
  carries (`json`, `export`, `verbose`): `0` to `7` or `unspecified`, made
  from the record, never stored, so a viewer never infers one. journald's
  `PRIORITY` is written only for a known level, as journald's own records
  may lack it. `-o human` shows `?` for unspecified, never `I`.
* Filters: `-p err` and every range leave unspecified out; `-p
  err,unspecified` (commas join ranges) or `--where 'level = unspecified'`
  asks for it; in the filter language it matches only `=` and `!=`. A
  `level` rule never holds an unspecified record. `PRIORITY=3` as a
  journalctl match compares the level, and matches no unspecified record,
  as journalctl's does no record without the field.
* `lode levels [-b ...] [filters]` counts per source (`SYSLOG_IDENTIFIER`,
  else `UNIT`, else `_TRANSPORT`) the records, the unspecified and the
  derived, most unspecified first: where writers still need to say levels.

### 4.2 Attribution

journald learns `_SYSTEMD_UNIT` by reading `/proc/<pid>/cgroup` after a
datagram arrives, and loses it when the sender has already exited -- the
last lines of a crashing service (#2913). Ferrix takes identity *when the
writer is connected, from the party that connected it*, wherever it can,
with no kernel change:

| Path | Identity, and from whom | Race? |
|---|---|---|
| A unit's standard output and error | init, which made the pipes for that unit's spawn: unit, invocation, main pid, stream, ids (as built: each line on the root-only `/run/lode/units`, §18) | none: a dying service's last lines are its unit's, whatever the process |
| A native channel (`ferrix.log`) | init, when it routes the OPEN (`CONNECT` names the client unit, `docs/INIT.md` §6); `devmgr` for a driver's channel | none |
| A stdout stream (`lode cat`, hyprix's programs) | `SO_PEERCRED` at `connect`, resolved through `/proc` at `accept`, while the writer is necessarily alive, and kept for the stream's life | none after accept |
| `/dev/log`, the journal socket | `SCM_CREDENTIALS`, stamped by the kernel at send time (pid, uid, gid); unit, comm and exe from `/proc` at receive, with a per-pid cache of the last resolution kept 10 s after the pid's last record | uid and pid: none. Unit, comm, exe: only for a sender that exits before its *first* datagram is read |

The residual row is a one-shot `logger` or `sd_journal_send` from a
process that exits at once; it is stored with the fields that are certain
(`_PID`, `_UID`, `_GID`) and never with guessed ones. Closing it needs the
kernel to stamp the cgroup on a datagram (Linux has no such message
either); this design does not ask for it (§14).

## 5. Sources

**The kernel log**, `/dev/kmsg`, read one record a read, from `SEEK_DATA`
on the first start of a boot and from the last stored `_KERNEL_SEQNUM` on a
restart (§11.3). `EPIPE` becomes a record: "kernel log: N records lost
before seq S" at warning, `N` from the sequence gap. A kernel record's
priority and facility are `/dev/kmsg`'s; a line with facility `user` (a
program's console output) gets `_TRANSPORT=driver-fallback` when it
matches a driver's name prefix (`intelgpu: ...`), with `SYSLOG_IDENTIFIER`
from the prefix, and `kernel` otherwise. `lode` reads the ring as fast as
it fills, so once it runs nothing the ring overwrites is lost: the
`freqlog` problem ends there, even before drivers move to their channel.
`log_buf_len=` (the kmsg branch) sizes the ring for the part of the boot
before `lode` starts (§11.5).

**Units' standard output and error.** init keeps reading the pipes it
makes today (`StandardOutput=log`, the default) and sends each line to
`lode` over its `ferrix.log` channel as one record with the unit, the
invocation, the spawn's pid and the stream. A line beginning with `<N>`
sets its priority, as journald's `SyslogLevelPrefix=`; otherwise stdout is
info (6) and stderr warning (4) unless the unit says `SyslogLevel=`. A
unit with `LogFormat=json` (Ferrix's key; USER-WISHES theme 17, systemd
#19683) has each line that parses as one JSON object turned into fields
(`message`/`msg` to `MESSAGE`, `level` to `PRIORITY`, other keys upper-cased
with non-name bytes as `_`). A unit with `LogMultiline=indent` or
`LogMultiline=/regex/` joins continuation lines into one record within
100 ms (theme 24). `LogExtraFields=` (systemd's) adds static fields. init
does not wait on `lode`; §11.4 says what it does when the channel is full
or `lode` is down. Keeping init as the reader, rather than handing each
service a stream to `lode` as journald does, means unit output needs no
socket of `lode`'s, survives `lode`'s restart in init's buffer, and is
attributed by the one party that knows the unit for certain.

**Session programs.** `sessiond`'s unit's pipe catches hyprix, `yserver`,
waybar and Steam today, all under `sessiond`'s pid. Two changes make each
its own source: `sessiond` gives the compositor a stream of its own
(identifier `hyprix`), and hyprix's `exec` gives each program it starts a
stream to `lode`'s stdout socket in journald's stdout-stream protocol
(`/run/systemd/journal/stdout`: a header naming the identifier, the unit,
the priority and whether `<N>` prefixes are read, then lines) -- what
`systemd-cat` and `sd_journal_stream_fd()` do, and what GNOME's launcher
does under systemd. The identifier is the program's name, the unit its
`app-<name>-<n>.scope` (`docs/INIT.md` §5.6). Scripts use `lode cat`
(Ferrix's `systemd-cat`, also linked as `systemd-cat`): `lode cat -t yserver
/data/yserver/bin/yserver ...`.

**`/dev/log`**, a datagram socket owned by init's `lode-dev.socket` with
`SO_PASSCRED` set before any sender sends, so every datagram carries the
kernel's stamp. musl's and glibc's `syslog()` `connect` and `send`, which
work today; busybox `logger` uses the same calls. Parsing is journald's:
RFC 5424 when the message begins `<PRI>1 ` (systemd #19251, open, c=77),
otherwise RFC 3164 read leniently (`<PRI>`, an optional `Mmm dd hh:mm:ss`
timestamp, an optional hostname, `ident[pid]:`). RFC 5424's structured
data becomes fields.

**systemd's native protocol**, a datagram socket at
`/run/systemd/journal/socket` (init's `lode-journal.socket`, `SO_PASSCRED`):
newline-separated `FIELD=value`, a binary-safe value written as
`FIELD\n<le64 length><bytes>\n`, and a record too large for a datagram
passed as a sealed memfd with `SCM_RIGHTS` (memfd and its seals exist).
**libsystemd sends with `sendmsg` to the socket's name on an unconnected
socket, which the kernel refuses today** (§1). So until §14's optional
item, a program linked against libsystemd gets `EOPNOTSUPP` from
`sd_journal_*` and those lines are lost unless it also writes to standard
error, which most do. Ferrix's own programs use the native channel or a
connected socket (`lode cat --journal` connects first), which works.

**The native channel `ferrix.log`.** `lode`'s unit says
`Offers=ferrix.log`; a native service, or a Linux one that makes native
calls, opens it through init's directory (`Uses=ferrix.log`). Messages, in
`src/lib/proto/logproto` (`no_std`, no allocation, host-tested, in
`logctl`'s shape):

```
RECORD    client -> lode   priority u8; flags; fields in journald's binary form
INTEREST  lode -> client   the lowest priority anyone stores or follows for this source
DROPPED   client -> lode   how many records the client dropped since its last DROPPED
```

A client never waits: it writes if the channel has room and counts the
record dropped otherwise; the count goes out in the next `DROPPED`.
`INTEREST` is Fuchsia's: a driver at `INTEREST` info does not format its
debug lines at all, and `lode level intel-xelp debug --for 10m` (theme
21) raises it for ten minutes without a restart.

**Drivers.** Drivers are `devmgr`'s children, started with
`process_create` in sealed jobs; their standard error is the console. In
images that boot init (`ferrix.devmgr=init`, every image but the certified
reference), `devmgr.service` says `Uses=ferrix.log`; `devmgr` opens one
`ferrix.log` channel per driver it starts, tells `lode` on its own channel
which driver and device that channel is (`_FERRIX_DRIVER=intel-xelp`,
`_FERRIX_DEVICE=0000:00:02.0`), and hands the client end over the
driver's bootstrap. The driver library's log macro writes to the channel
when it has one and to standard error otherwise. A driver whose channel
closes (`lode` restarted) falls back to standard error -- the kernel ring,
which `lode` reads -- until `devmgr` hands it a new end, which it does
when it sees `lode` come back. Nothing a driver logs is lost to a `lode`
restart; it loses only its attribution for the moment. Under
`ferrix.devmgr=kernel` (the certified reference, no init) drivers keep
writing to standard error, as today.

**`devmgr`'s events.** `lode` is one more reader of
`\0ferrix.devmgr.events`; each event becomes a record
(`_TRANSPORT=devmgr`, `MESSAGE_ID` per kind, `_FERRIX_DRIVER`, the exit
cause), so "which driver died, when, and how" is `lode -t devmgr`.

**The last boot's kernel log**, from pstore. When the loader has written
`FERRIX/LASTKERNEL.TXT` for a boot that ended without `lode` storing its
last lines, `lode` imports the lines it does not have -- matched by the
boot's audit id and the kernel sequence -- as records of *that* boot
(`_TRANSPORT=pstore`), as `systemd-pstore` does. `lode -b -1` then
ends with what the machine said before it hung or panicked.

**Programs that write only files** (Steam's `console_log.txt`, a game's
logs in its Wine prefix): a file source watched with inotify, configured
per path in `/etc/ferrix/lode.d/*.conf`, each line a record with the
configured identifier and the file's owner as `_UID` (`_TRANSPORT=file`;
syslog-ng's `file()`, Vector's file source). journald has no such source;
Ferrix's desktop needs one.

**Syslog over the network** (theme 9): `lode-receive.service`, opt-in, its
own user and job, listens for RFC 3164/5424 on TCP, TLS and, if asked,
UDP 514, and writes into the store's remote area (§10.4).

## 6. One filter language

USER-WISHES' third theme and its most-voted issue (#2720, `NOT`), and the
same need under themes 4, 5, 10 and 22: one expression language, in
`src/lib/log/filter`, used by `lode`, ingest rules, retention
classes, forwarding filters and alerts, so a rule written once means the
same in all of them.

```
expr    := expr "or" expr | expr "and" expr | "not" expr | "(" expr ")" | test
test    := field op value | "priority" cmp prio | "since" time | "until" time
field   := MESSAGE | _SYSTEMD_UNIT | unit | id | driver | ...   (unit, id, driver: short names)
op      := "=" | "!=" | "~" (regex) | "!~" | "in" "[" value, ... "]"
value   := "string" (globs: * ? [..]) | /regex/
prio    := emerg | alert | crit | err | warning | notice | info | debug | 0..7
```

```
unit = "nginx*" and not priority > warning
id = "proton" and MESSAGE ~ /trace:fps/
driver = "intel-xelp" or (unit = "hyprix.service" and priority <= err)
```

Regular expressions are Rust's `regex` (linear time, no backtracking), so
a rule cannot hang `lode`. A filter compiles to a plan that uses the field
indexes and segment summaries (§8.4) for its indexed tests and scans only
what they leave. journalctl's own arguments are sugar that compiles to it:
`-u 'nginx*'`, `-u '!noisy.service'` (exclusion), `-t`, `-p`,
`FIELD=VALUE ... + ...`, `-g`. `--where 'EXPR'` takes the language
directly, and `lode save NAME 'EXPR'` keeps a named query for
`--where @NAME`. A SQL view, as lnav has, is not planned before the
language is in use.

## 7. Floods and ingest rules

### 7.1 The limiter

Per source -- a unit, a scope, a driver, or a uid for what has none of
those -- and per priority class, a fixed window and a burst, as journald's
`journal-ratelimit.c` (fixed window, as init's restart policy,
`docs/INIT.md` §5.4):

| Class | Priorities | Default burst per 30 s |
|---|---|---|
| errors | 0-3 (emerg to err) | 10,000 |
| warnings | 4-5 (warning, notice) | 2,000 |
| chatter | 6-7 (info, debug) | 1,000 |

A flood of chatter never suppresses the same source's errors. The limits
are never scaled by free disk space or anything else unrelated (theme 15).
When a window ends with records suppressed, one record at warning says so:
`Suppressed 6,124 messages from intel-xelp (driver) in 30 s, 7 of them
warnings`, with `MESSAGE_ID` for suppression and `N_SUPPRESSED=` per
class. Units set `LogRateLimitIntervalSec=`, `LogRateLimitBurst=`,
`LogLevelMax=` and `LogFilterPatterns=` (systemd's keys); every effective
value is shown by `lode status UNIT`. The kernel's own records are not
limited by `lode`: the kernel decides what it says.

### 7.2 Ingest rules

`/etc/ferrix/lode.d/*.rules`, one rule a line, read at start and on
`lode reload` (theme 21), applied in order after the limiter:

```
drop     where unit = "kio*" and MESSAGE ~ /thumbnail/
sample 1/100 where driver = "intel-xelp" and priority = debug
level  notice where unit = "snmpd.service"
fold   30s where id = "chrome"              # identical MESSAGE from one source -> one record, REPEATED=N
redact /\b\d{1,3}(\.\d{1,3}){3}\b/ "ip" where unit = "nginx.service"   # opt-in, never default (theme 23)
```

Every rule has a counter of what it dropped, sampled away or folded,
shown by `lode status` and written as one summary record per rule per
30 s while it is acting, so filtering is never silent (theme 4). A fold
writes the first record at once and one `REPEATED=N` record when the run
ends or the window closes. Redaction is opt-in and per source; macOS's
default write-time redaction is what USER-WISHES warns against.

### 7.3 Memory

Every queue in `lode` is bounded: a per-source queue of 512 records and a
global one of 16 MiB, both counted when full. A full source queue drops
that source's newest chatter first and its errors last, reported like a
suppression. `lode`'s `MemoryMax=` (64 MiB) bounds it all, and an OOM
kill of `lode` is a restart like any other (§11).

### 7.4 Top talkers and alerts

`lode top [--since 1h]` lists lines and bytes per source from the
segment summaries (§8.4), without reading records. An alert is a rule
with an action (theme 22; Windows' "attach a task to this event"):

```
alert rate > 100/s where id = "chrome"   notify "chrome is flooding the log"
alert  where priority <= crit            start crit-notify@%n.service
```

`notify` sends a desktop notification through the session's notification
daemon; `start` asks init to start a unit (the record's cursor in its
environment). Each alert has a rate limit and a 10-minute dedupe window.
Alerts run in `lode` as rules; the action is a message to init or to the
session bus relay, never something `lode` does itself (§11.2).

## 8. Storage

### 8.1 Where, and the tiers

| Tier | Path | Holds |
|---|---|---|
| RAM | `lode`'s memory, a ring of 8 MiB, mirrored to `/run/log/journal/<machine-id>/` | every record, debug included, until the ring wraps; the whole store until `/var` is writable; a `--tmpfs-root` VM's whole store |
| Disk | `/var/log/journal/<machine-id>/` | what the retention classes say (§8.5); persistent by default (theme 11) |

`lode-flush.service` (after the root switch and `local-fs.target`) moves
the RAM tier's records to disk, as `journalctl --flush`;
`lode-relinquish.service` closes the disk segments before init remounts
`/` read-only (`docs/INIT.md` §8.2 step 2) and sends anything after to
RAM, so the store never pins a filesystem at shutdown (theme 16, systemd
#867). On the N150 and every installed machine `/var/log` is on the root
btrfs volume (`docs/N150.md` §3.1, `docs/AB-BOOT.md` §6.2). A disk that is
full or read-only is said once and the store degrades to RAM, counting
(§11.4).

### 8.2 The format

A directory holds segments, each one file, named by class, owner and boot:

```
<class>@<boot-id>-<first-seqnum>.fxj               system records of a retention class
user-<uid>/<class>@<boot-id>-<first-seqnum>.fxj    records whose _UID is a user's (>= 1000)
summary.fxs                                         one summary per segment (§8.4)
```

A segment is a 128-byte header (magic `FXJOURNL`, version, machine id,
boot id, class, first sequence number, state `online` or `sealed`), then
two kinds of entry, appended and never rewritten:

```
SOURCE  u32 length  u32 crc32c  u32 source-id   fields that do not change per record:
        _PID _UID _GID _COMM _EXE _SYSTEMD_* _STREAM _TRANSPORT _HOSTNAME _NAMESPACE ...
RECORD  u32 length  u32 crc32c  u64 seqnum  u64 realtime_us  u64 monotonic_us
        i32 utc_offset_s  u32 source-id  u8 priority  fields (journald's binary form)  u32 length
```

Trusted metadata is written once per source in a segment, not repeated per
record (#5285: journald's files are the size of their own JSON export).
The trailing length lets a reader walk an online segment backwards
without an index, which is how "last N" starts at the end (theme 2).

**Writes** (theme 1). `lode` collects what arrives in one wakeup into a
buffer and writes it with one `pwrite` at the segment's end, at most every
`WriteIntervalMs=` (50 ms) or at 64 KiB, and at once for priority crit (2)
or more urgent. Never a shared mapping (#40262's cause: journald dirties
pages of its mapping on every line, and `SyncIntervalSec` delays only the
`fsync`). `fdatasync` every `SyncIntervalSec=` (5 s; journald's is 5 min)
when anything was written since, at once after crit or more urgent, and
before `lode-relinquish`. The 5 s bound is for the N150's hangs, where the
last seconds were what was wanted.

**Sealing.** A segment is sealed at `SegmentMaxSize=` (16 MiB of records),
`SegmentMaxSec=` (1 day) or a new boot: `lode` writes a sealed copy --
records zstd-compressed in 64 KiB frames (`ruzstd`, already in the tree;
theme 1, systemd #31358), the field indexes and the sparse time index
(§8.4) after them, a footer with their offsets and checksum -- then
renames it over the online file. The online file is never modified; a
crash during sealing leaves either the old online file or the new sealed
one. Sealed segments are immutable. (Built so far: the compression,
§18 "Compression, as built"; the indexes are still to come.)

**Crash safety** (theme 8). A reader stops at the first entry whose length
runs past the file, whose checksum does not match, or whose offsets are
not strictly increasing -- it cannot loop. When `lode` opens an `online`
segment left by a crash, it truncates it to the last good entry, seals it,
and records "segment X: N bytes of a torn record dropped": one torn record
lost, never the file, where journald renames a whole file it finds
uncleanly closed aside. `lode verify` reports per segment; `lode
recover FILE` copies out every readable record of a damaged one.

### 8.3 The write budget

**Bytes written to the disk per byte logged** is a number with a budget,
checked in a gate row (§17.1 part E), so it cannot drift back (USER-WISHES
top item 1):

| Workload | Budget, bytes written to the device per byte of `MESSAGE` |
|---|---|
| 2 lines a second of 100 bytes, one hour, `SyncIntervalSec=5s` | at most 40 (journald on ext4: about 490, #40262's 49 KB a line) |
| 2,000 lines a second of 100 bytes, one minute | at most 3 |

Measured by QEMU's block statistics for the disk (`query-blockstats`
before and after, through the run's QMP socket), with nothing else writing;
the first figure is dominated by btrfs's commit of each `fdatasync`, which
is why the sync interval is a setting. (Built: §18 "The write budget, as
built" -- the fast load is within its budget, the slow one over it by
btrfs's commit.)

### 8.4 Finding records fast

**Summaries.** `summary.fxs` holds, per segment: its time range, its boots,
its priority range, its record count, and per source its id, unit,
identifier, driver, uid, invocation ids, record count and bytes. It is
small (a few hundred bytes a segment) and rewritten by rename at each
seal. `--list-boots`, `lode top`, `--disk-usage` per source and unit-name
completion read it alone; a query opens only the segments whose summary
can match (theme 2; completion that scans the journal is systemd #6238).

**Indexes in a sealed segment**: for `_SYSTEMD_UNIT`,
`_SYSTEMD_INVOCATION_ID`, `SYSLOG_IDENTIFIER`, `PRIORITY`, `_TRANSPORT`,
`_UID`, `_PID`, `_COMM`, `_FERRIX_DRIVER`, `MESSAGE_ID`, `TRACE_ID`: a
sorted table of values with the 64 KiB frames their records are in. A
sparse time index: the first timestamp of each frame. An online segment's
indexes are kept in memory by `lode` and rebuilt by a scan in a reader.

**Text** (theme 13). `-g` and `MESSAGE ~` scan the frames the indexed
tests leave, decompressed in parallel, at disk speed; `--grep` finds
exactly what `| grep` finds (#20445). An n-gram index per sealed segment
is an option to measure, not a promise.

**The benchmark** (§17.1 part F): `lode -u X -n 50` and `-b -1 -n 50`
on a store of 1 GB and one of 100 GB (generated), cold and warm cache; the
target is the same order of time on both, under 100 ms warm.

### 8.5 Retention classes

A retention class is a filter with an age and a size (theme 5). Records
are assigned a class when stored, by the first class whose filter matches,
and each class is its own series of segments, so expiry is deleting whole
files, never rewriting:

```
# /etc/ferrix/lode.d/retention.conf
class errors  where priority <= err                 keep 1y   max 1G
class auth    where unit in ["auth.service"]        keep 90d  max 64M
class desktop where unit = "app-*.scope"            keep 7d   max 512M
class default                                       keep 30d  max 4G
class debug   where priority = debug                ram-only
```

`ram-only` records live in the RAM tier alone (#14588: only errors on
fragile flash). The global limits stay journald's (`SystemMaxUse=`, 10 %
of the filesystem, at most 4 GiB; `SystemKeepFree=`, 15 %), in bytes
everywhere (#18353), and vacuum removes the oldest sealed segment of the
class most over its share first. A source above `SourceMaxShare=` (25 %
of its class by default) has its own oldest records dropped from that
class at the next seal, never another source's: a driver logging a line a
second for a month crowds out its own history, not the boot.
`SystemKeepFree=` is checked before every batch, not at rotation only: a
full root volume is everyone's `ENOSPC` on Ferrix.

**Deleting by match** (Unix & Linux's most-viewed journalctl question):
`lode delete --where 'unit = "nginx.service"' [--since --until]`,
root's, rewrites each affected sealed segment without the matching records
(a copy, then a rename) and writes a record saying what was deleted, by
whom and how many.

### 8.6 Why not journald's file format

| | journald's format | Ferrix's segments |
|---|---|---|
| Readers | real `journalctl`, libsystemd's `sd_journal_*`, Python's `systemd.journal`, Vector, promtail | `lode`; anything else through `-o export` or `-o json` |
| Licence | documented in systemd's `docs/JOURNAL_FILE_FORMAT.md`; a clean-room MIT writer is allowed; linking libsystemd (LGPL-2.1+) is not needed for either choice | MIT, ours, documented in `src/lib/log/store/FORMAT.md` (step 1) so `zgrep`-style tools can be written against it |
| Effort | journald's `journal-file.c` and `sd-journal` are about 8,000 lines of C: hash tables, entry arrays, the tail's offsets, compact mode, compression, sealing, online/offline states. A compatible writer is about 25 points, then every corner real readers check | part of §17's step 1 |
| Crash behaviour | a file not closed cleanly is renamed aside and a new one started | the torn entry alone is dropped |
| Write cost | in-place updates through a mapping per record (#40262) | one append per batch |
| Getting Ferrix's logs into systemd's tools | copy the files | `journalctl -o export \| systemd-journal-remote -o x.journal -` on a Linux machine makes a real journal file |

Nothing on Ferrix needs to *read* journal files with libsystemd; programs
that *write* through libsystemd need only the socket (§5). The store sits
behind a trait (`Store`), so a journald-format backend could be added
without touching sources, rules or queries; nothing asks for it.

### 8.7 Configuration

`/etc/ferrix/lode.conf` with journald's keys and meanings where they exist
(`Storage=`, `SystemMaxUse=`, `SystemKeepFree=`, `RuntimeMaxUse=`,
`MaxRetentionSec=`, `SyncIntervalSec=`, `RateLimitIntervalSec=`,
`RateLimitBurst=`, `MaxLevelStore=`, `MaxLevelConsole=`,
`ForwardToConsole=`), Ferrix's where they do not (`WriteIntervalMs=`,
`SegmentMaxSize=`, `SourceMaxShare=`); rules, classes, alerts and file
sources in `/etc/ferrix/lode.d/`. Read with init's INI reader
(`src/lib/init/svc`'s `ini`), so the syntax corners are systemd's.
`lode config` prints the effective configuration (systemd #34830).

## 9. Reading

### 9.1 The reader

The reader is `lode` started without `serve` (or as `journalctl`), and
reads the segment files directly, as systemd's `journalctl` does: no
daemon on the read path, so no reader can slow the service down or hold
it. It runs as the person who started it, with that person's rights to the
files (§12), never with the service's.

| Option | Meaning |
|---|---|
| `-b [N\|ID]`, `--list-boots` | a boot, by offset or id; boots from the summaries |
| `-k`, `--dmesg` | kernel records (`_TRANSPORT=kernel`), of any boot kept |
| `-u UNIT[@INV]`, `--invocation`, `--user-unit`, `-t ID`, `-T ID`, `--driver NAME` | sources, with globs and `!` (§6) |
| `-p PRIO[..PRIO]` | priority or range |
| `-g REGEX`, `--case-sensitive` | `MESSAGE` matches |
| `--where EXPR`, `--where @NAME` | the filter language (§6) |
| `--since`, `--until` | journald's forms, ISO 8601 and RFC 3339, and every timestamp `journalctl` prints (#15899, #5194) |
| `-f`, `-n N`, `-r`, `-e`, `--until-exit`, `--until-match REGEX` | follow; last N; newest first; the end; follow until the invocation ends (#43185) or a match appears (#36754), the exit status saying which |
| `-o FORMAT`, `--format TEMPLATE`, `--output-fields` | §9.3 |
| `--cursor`, `--after-cursor`, `--cursor-file`, `--show-cursor` | resuming a read; the base of forwarding and box tests |
| `FIELD=VALUE ...`, `+` | journald's matches |
| `--merge`, `-D DIR`, `--file`, `-H HOST` | several stores interleaved by time; another directory; another host through ssh (#4793) |
| `--disk-usage`, `--vacuum-size`, `--vacuum-time`, `--rotate`, `--flush`, `--sync`, `--verify` | journald's maintenance options; the ones that change the store ask `lode` through `lode` |
| `--utc`, `--tz ZONE`, `--delta` | display zone; a column of time since the previous record (#24641, `dmesg -d`) |

### 9.2 This run, and the right slice

`_SYSTEMD_INVOCATION_ID` is indexed from the start (theme 10, #1942).
`-u nginx.service@last` is the current or last run, `@-1` the one before;
`svc status UNIT` shows the last 10 lines of the current invocation, not
of the unit's whole history; a failed start prints the failed
invocation's own last lines rather than a hint to run another command.
`-u` resolves unit aliases (#11846). Previous boots are kept by default
(§8.1), and `--list-boots` reads only the summaries, so it is instant.

### 9.3 Output

* **Byte-compatible with journald** where journald defines it, because
  scripts parse it: `-o short`, `short-iso`, `short-iso-precise`,
  `short-precise`, `short-monotonic`, `with-unit`, `cat`, `json`,
  `json-pretty`, `json-seq`, `export`, `verbose` (journald's: a time line
  with the cursor, then each field indented; built in the field-quality
  landing). `-o json` is one object a
  line, every field a string, binary values as arrays of numbers; `-o
  export` is the Journal Export Format.
* **Templates** (#3449): `--format '{time:iso} {unit} {priority:letter}
  {message}'` over any field, with `time:iso|rfc3339|mono|delta`,
  `priority:letter|name|num` and width and padding; saved as a named
  format in `/etc/ferrix/lode.d/formats.conf` and in
  `~/.config/ferrix/lode/formats.conf`.
* **Ferrix's default, `-o human`**, when no `-o` is given:
  `2026-10-23 14:01:02.123 +02:00 E hyprix[812]: message` -- the year and
  the zone (theme 8), the priority as a letter (`E W N I D`, `?` for a
  record with no level (§4.1), and colour on a terminal; #39281), the
  identifier and pid, the message whole.

### 9.4 Defaults

* No truncation, ever: a long line wraps in the pager, and nothing is cut
  when the output is not a terminal (169-vote question, #2342).
* The pager opens at the end (`-e` behaviour) for a plain `lode`
  or `-u`; `-r` gives newest first.
* Colours from a palette checked for contrast on light and dark
  backgrounds (#13561, #14495), overridable by `JOURNALCTL_COLORS`,
  off with `NO_COLOR`.
* `--since` and `--until` accept every form `journalctl` prints.
* A filter that names a unit, identifier or field value that does not
  exist says so on standard error, rather than printing `-- No entries --`
  and leaving a typo to look like an empty log.

### 9.5 And `dmesg`

`dmesg` (the kmsg branch's) stays what it is: the live kernel ring, no
daemon, working when `lode` does not run -- the rescue shell, the
initramfs, a gate image without init. `lode -k` is the kernel's
records as stored, across boots and past what the ring forgot. Neither
replaces the other.

### 9.6 `svc log`, and administration

`svc log UNIT` becomes `lode -u UNIT@last -n 50`, falling back to
init's buffer when the service is not running (§11.4), so it answers in
the rescue target too. The administration subcommands -- `lode status
[SOURCE]` (effective limits, rule counters, queue fill, store use), `top`,
`level`, `reload`, `delete`, `verify`, `recover`, `config`, `save` -- talk
to the service over a stream socket, `/run/lode/control`, root's and
`adm`'s by mode; `status` of one's own sources is open to everyone.

### 9.7 The benchmark

§8.4's benchmark is a row of its own (§17.1 part F) and its figures go in
this section once measured, as the IPC figures go in
`docs/HOTPATHS.md`.

## 10. Forwarding and many hosts

### 10.1 For box tests, from step 4

`ssh box lode -f -o json --after-cursor="$(cat cursor)"` from
nazuna. The reader keeps the cursor of the last record it printed; a
dropped connection or a reboot of the box resumes there, and every record
the box stored since is printed exactly once. ssh already authenticates,
and the logs never leave the encrypted channel. `cargo xtask box-log` does
the reconnecting, writes the stream to the run's log directory, and turns
"wait for line X" into `--until-match` with a deadline, replacing the
`dmesg` polling of the N150 scripts.

### 10.2 Continuous forwarding

(Built in part: see §18, "Store-and-forward, as built" and "TLS and
lode-receive, as built". RFC 5424 and OTLP are still to come.)

`lode-upload.service`, its own unit, user and sealed job, off unless
`/etc/ferrix/lode-upload.conf` names a destination (theme 9):

* **One cursor per destination**, in `/var/lib/ferrix/lode-upload/<dest>`,
  advanced only after the destination acknowledged. The store is the disk
  buffer (syslog-ng's lesson): an outage of the destination delays
  forwarding with exponential back-off up to 5 min and loses nothing that
  vacuum has not removed; the forwarder never gives up and exits (#2877,
  #19426). A forwarder that falls behind past retention says how many
  records it never sent.
* **TLS that verifies**: the destination's certificate against a
  configured CA or a pinned key, and a client certificate presented and
  required on the receiving side (#4092); plaintext only to `localhost`
  or over ssh.
* **Outputs**: journal-upload's `POST /upload` with the export format (a
  real `systemd-journal-remote` receives it); RFC 5424 over TCP or TLS
  (and UDP if asked) for syslog servers (#32852, #7170); JSON lines over
  HTTP(S) for Vector, Loki and Fluent Bit; OTLP logs over HTTP for
  OpenTelemetry collectors, `TRACE_ID`/`SPAN_ID` carried (theme 26).
* **What leaves**: a filter in §6's language; records of the auth facility
  and `auth.service` never unless named.

### 10.3 Receiving

(Built first as `lode-receive` on a Linux host, for the viewer on nazuna:
§18, "TLS and lode-receive, as built". The Ferrix-side service below is
still to come.)

`lode-receive.service` (opt-in) takes journal-upload's export, RFC
3164/5424 and JSON lines, and writes each host into
`/var/log/journal/remote/<machine-id>/` with `_TRANSPORT=remote` and the
host's own boot ids; the receiving machine's retention classes apply to
it (#5242: journal-remote ignores rotation).

### 10.4 A merged view

`lode --merge -D /var/log/journal/remote/*` (or `-H a -H b` over ssh)
interleaves hosts by wall-clock time with the host shown (#34169). Within
one boot, order is by monotonic time, which survives a wall-clock jump
(#5345); across boots and hosts, by wall clock.

## 11. Isolation and restart

### 11.1 What `lode` is

A **native** program (`no_std` + `alloc`, `ferrix_rt`, in
`src/user/system/native/lode`, crate `ferrix-lode`; all three roles of the summary's table), the shape of `devmgr` and
`devmgr-events`. Its logic is in `src/lib/log/` (`record`, `filter`,
`limit`, `rules`, `store`, `query`, `parse` for RFC 3164/5424, the native
protocol, stdout streams and JSON lines, `format`), `no_std` + `alloc`,
`forbid(unsafe_code)`, host-tested, under Miri, and fuzzed at every
parser (theme 19: the parser is the attack surface). It talks to the world
through two fronts:

* **the native front**, always: its bootstrap channel to init, the
  `ferrix.log` channels it serves, the `devmgr-events` socket, and storage;
* **the Linux front**, only where the kernel has the Linux personality
  (`linux_abi=on`, every image today): `/dev/kmsg`, `/dev/log`, the journal
  sockets, `lode`'s socket, and storage as files. It is a module behind a
  cargo feature, `linux-front`; without it `lode` builds and serves native
  clients alone.

Under `linux_abi=off` (`docs/LINUX-ABI-SEPARATION.md` §11) there is no
`/dev/kmsg` and no file system in the kernel (`fs=none`). The kernel log
then reaches `lode` through the native log call that MICROKERNEL's rows 8
to 12 owe every native process ("the native exit, thread, clock, log and
unmap calls"), or through `logctl`'s channel if its reader is widened from
a device binding to a capability init hands out; and storage through
whatever native file server those rows bring, behind the same `Store`
trait. Neither is this design's to build, and neither changes
`src/lib/log/`. The reader role is the same native program over the same
`query` crate, so it exists without the Linux personality too; only the
`journalctl` link does not.

### 11.2 Its box

| | What `lode` gets |
|---|---|
| Job | a **sealed** job (`docs/CGROUPS.md` §3.3, landed 2026-10-23) at `system.slice/lode.service`: init makes it with `job_create(JOB_SEALED)` and starts `lode` in it by `process_create`, so no process moves into or out of it, and `lode` starts processes nowhere but under it (it starts none) |
| Ids | user and group `lode`, through init's native helper for a `User=` native service (`docs/INIT.md` §5.2, P0b). No supplementary groups |
| Limits | `MemoryMax=64M`, `TasksMax=8`, `CPUWeight=50`, `IOWeight=50`, `OOMPolicy=stop` (then restarted) |
| Descriptors, opened by init as root and handed over (§11.2.1) | `/dev/kmsg` read-only; the sockets of `lode-dev.socket`, `lode-journal.socket`, `lode-stdout.socket` and `lode-control.socket`; directory descriptors for `/run/log/journal` and `/var/log/journal` (the latter from `lode-flush` on); its stored stdout streams (§11.3) |
| Channels | its bootstrap to init (READY, OFFER `ferrix.log`, WATCHDOG, START for alerts); the `ferrix.log` channels routed to it |
| Files it may write | `/run/log/` and `/var/log/journal/`, which `lode` owns, and nothing else `lode` owns; under `ProtectSystem=strict` once init applies L13's mount keys to native services |
| System calls | an allow-list (`SystemCallFilter=`, L13c) once init installs filters for native starts; until then, the uid and the job are the box |
| Network | none: it never makes an inet socket; forwarding and receiving are other units (§10) |
| Devices, handles to other services, the audit handle | none |

What it cannot do, then: reach a device, a network, another service's
channel or job, any file but its own directories, or start a process --
an alert's `start` is a request to init, which checks it against
`lode.service`'s `Uses=` like any other. What a compromised `lode` can do:
forge, drop or read logs -- which is what a log service holds -- and use
its memory and CPU budget.

#### 11.2.1 Handing descriptors to a native service

A Linux service gets its sockets at `execve`, in `LISTEN_FDS`. A native
service is made by `process_create` and has only standard input, output
and error. So init gives a native unit its descriptors over its control
socket: the service connects to `/run/ferrix/control`, asks `FDS`, and
init -- after checking with `SO_PEERCRED` that the caller's pid is in that
unit's job -- answers with the unit's `.socket` descriptors and its
`OpenFile=` paths (systemd 253's key: init opens the path as root, so the
service needs no right to open it) by `SCM_RIGHTS`, named as
`LISTEN_FDNAMES` names them. This is init code over calls that exist; it
is not specific to `lode`, and `audit.service` (AUTH phase 2) is the next
native unit that wants it.

### 11.3 Restart without loss

**Who restarts it: init**, by `Restart=always` and its restart policy
(`src/lib/init/restart`: `RestartSec=100ms`, doubling, at most five starts
in ten seconds, then `failed`). Not `devmgr`: `devmgr` holds every device's
authority, and a log service needs none of it; putting `lode` under
`devmgr` would put a non-driver in `drivers.slice` and give `devmgr` a
second job. Not the kernel's starter: that is kernel lines, and only
`devmgr`'s place before the root volume needs it. A hung `lode` is a dead
one: `WatchdogSec=10s` (systemd's key, new in init) has `lode` send
WATCHDOG on its bootstrap every 5 s from its main loop, and init kills the
job (`cgroup.kill`, allowed on a sealed job) when one is missed.

What carries each source across the gap:

| Source | Held where while `lode` is down | On restart |
|---|---|---|
| Kernel | the kernel ring (`log_buf_len=`) | read from `SEEK_DATA`, records at or below the last stored `_KERNEL_SEQNUM` skipped; a gap in the numbers is a "N kernel records lost" record |
| Units' output | init's per-unit buffer (§11.4) | replayed in order with init's receive times as `_SOURCE_REALTIME_TIMESTAMP`, then "init dropped N lines of UNIT" if it overflowed |
| `/dev/log`, journal socket | the sockets' kernel queues: init owns the sockets, so they exist across the gap | read on |
| stdout streams | init's descriptor store: `lode` sends each accepted stream to init (`FDSTORE=1`, systemd's `FileDescriptorStoreMax=`), and gets them back on start (#6620) | read on, each with its header and attribution kept beside it in `/run/log/streams/` |
| Drivers and native clients | the client's own counter; drivers fall back to standard error, which is the kernel ring | `DROPPED` from each client on reconnect; `devmgr` hands drivers new ends |
| The RAM tier | `/run/log/journal/`, its mirror | reloaded |
| What `lode` held unwritten | -- | lost: at most one batch (50 ms or 64 KiB) |

On every start after the first in a boot, `lode`'s first record is `log
service restarted (previous instance: killed by SIGKILL at 14:03:12,
watchdog); last stored seqnum 81234; kernel records missing: 0; unit lines
dropped by init: 0; socket records: unknown` at warning, with `MESSAGE_ID`
for restarts, so a gap is never silent. "Unknown" is said where the loss
cannot be counted -- a datagram a sender gave up on.

### 11.4 It must not take down anything else

* **No one waits for `lode`.** init writes to its `ferrix.log` channel
  without waiting; when the channel is full or `lode` is down, init keeps
  the lines in the unit's buffer -- today's ring of 256 lines a unit,
  raised to a byte budget of 64 KiB a unit and 1 MiB in all -- counts what
  falls off, and replays when `lode` returns. Drivers and native clients
  never wait (§5).
* **The one place a sender can wait** is a full socket queue: a program
  writing to `/dev/log` or the journal socket with a blocking `send` waits
  while the queue is full, on Linux as here. While `lode` runs, it empties
  the queues into its own bounded queues and drops from those (§7.3), so a
  kernel queue is full for at most one wakeup. While `lode` is down, the
  wait is bounded by the restart: the watchdog (10 s) plus `RestartSec=`.
  A stdout pipe's writer is read by init, not by `lode`, so a service's
  `printf` never waits on `lode` (theme 18, #16456).
* **Readers cannot hold it.** `lode`, the forwarders and box tests
  read files; `lode` requests have a deadline and a queue of their own.
* **Its budget is its own.** Its job's limits cap its memory, tasks, CPU
  and I/O share; an OOM kill or a crash is a restart.
* **When it gives up** (five crashes in ten seconds): the unit is `failed`,
  init says so on the console, keeps unit output in its buffers, writes
  units' lines to the console as before `lode` existed, and the kernel
  ring goes on. The system runs; it has lost its long memory until `svc
  restart lode`.
* **A full or read-only disk** is said once (not once a line, #25265,
  #2339), the store degrades to the RAM tier, and what could not be kept
  is counted.

### 11.5 When it starts

Kernel, then `devmgr` (kernel-started, or by init's starter), then init.
`lode.service` is init's first unit, `DefaultDependencies=no`, before
`sysinit.target` and everything else, its sockets bound before any other
unit starts, as `systemd-journald` is. Everything before it -- the kernel's
boot, `devmgr`'s start, the drivers' first lines, init's first lines -- is
in the kernel ring, which `lode` reads from its oldest record when it
first starts; kernel and service records share one time base, the kernel's
monotonic clock (theme 16). `log_buf_len=1M` on images that boot init
keeps all of that (an N150 boot to the desktop is about 300 KiB of log
with `ferrix.intelgpu.trace` off).

## 12. Who may read what

### 12.1 Policy

| Reader | Reads |
|---|---|
| root | everything |
| group `adm` | every system and user segment (Linux's `adm`; journald's `systemd-journal` group is made an alias of it) |
| a user | their own segments: every record whose `_UID` is theirs, their session's programs included, without root (theme 10, #2744) |
| `lode-upload` | as `adm`, through its supplementary group |

The directory is `lode:lode 2755`; system segments `lode:adm 0640`; a user's
directory `user-<uid>/` is `lode:<uid> 0750` and its segments
`lode:<uid's primary group> 0640`, which with Ferrix's user private groups
(`ferrix:x:1000:`) is that user alone. Only `lode` writes. (As built, a
user's directory and segments are `lode:adm`, like the system's, and the
user is handed theirs by lode: one group per file cannot be both the
user's and `adm`'s, §18 "Users' own records and units' runs".) USER-WISHES
(theme 14) warns against "file permissions over a shared file"; here no
file is shared between readers of different rights: a record is in
exactly one owner's segments. `wheel` is not a reader by membership; a
person who wants it is added to `adm` with `authctl`. The image's
`/etc/group` gains `adm:x:4:` and `lode:x:91:` (91 if free; `auth` is 90).

### 12.2 Sessions and authd

Records from a session are `_UID=1000`, so they go to that user's
segments. `authd`'s lines are `_UID` of `auth`, in system segments
readable by `adm`. Its audit lines keep going to
`/var/log/ferrix/auth.log` as `docs/AUTH.md` §3.6 says.

### 12.3 Privacy

* No secret in a record: the rule for every writer, as `authd`'s audit
  lines already keep it ("never holds the secret, its length or its
  hash", `docs/AUTH.md` §3.6). Code review checks it; redaction rules
  (§7.2) are a help, not a promise.
* `_CMDLINE` and the environment are not recorded: a command line is where
  tokens end up. `_COMM` and `_EXE` say who; a unit may opt in with
  `LogCommandLine=yes`.
* Forwarding is opt-in, off on every image, with the auth facility
  excluded.
* A user's records are not readable by other users (§12.1).
* `lode delete` and `--vacuum-*` are root's, and each deletion is itself
  recorded (theme 23's deletion requests).

## 13. Moving what exists onto it

### 13.1 Ad-hoc logs

| Today | After |
|---|---|
| `freqlog`, a line a second to standard error | the driver's `ferrix.log` channel at debug (step 7), so `ram-only` by default (§8.5) and `INTEREST`-gated: not formatted unless someone follows it, `lode --driver intel-xelp -p debug -f` |
| `/tmp/yserver.log` from `/etc/yserver.sh` | `lode cat -t yserver` in the script; the file goes |
| `/home/ferrix/plog/proton.out` | `lode cat -t proton` around Proton; the fps channel is `lode -t proton -g trace:fps` |
| DXVK's `plog/dxvk/*.log` | `DXVK_LOG_PATH=none`, so DXVK writes to standard error, which `lode cat` holds |
| Steam's `console_log.txt`, a game's logs in its prefix | the file source (§5) |
| hyprix, waybar, `sessiond`, Steam's standard output | per-program streams (step 6) |
| `svc log` | `lode -u UNIT@last` (§9.6) |
| `dmesg` polled over ssh by box tests | `cargo xtask box-log` (§10.1) |
| `LASTKERNEL.TXT` read by hand | `lode -b -1` |

The N150 scripts under `tools/common/n150/` change in step 6's landing;
they keep writing their reports to `plog/` (results, not logs).

### 13.2 The console

Today init writes every unit line to the console, and the console is the
kernel ring, so every service's chatter evicts the kernel's own lines --
half of the N150's problem. With `lode` running, init writes a unit's lines
to the console only at or above `MaxLevelConsole=` (default `notice`;
`info` on gate images, whose expectations read unit lines from the serial
console; `docs/INIT.md` §8.1's boot-log lines are init's own and always
go). Before `lode` is up, and after it has failed, init writes everything
as now. Gate images set it with `ferrix.log.console=info` on the kernel
command line, read by init, so no gate's transcript changes.

## 14. The kernel

**Kernel lines: 0.** Everything above uses calls the kernel has, plus
`/dev/kmsg`, which `feature/etxtbsy-kmsg-dmesg` adds for its own reasons
(Linux's ABI, `dmesg`'s times, the dropped-lines marker) and which is
reviewed with that branch. This design adds no device node, no system call,
no field to a kernel record and no capability (customer, 2026-10-22: keep
the kernel slim, policy and knowledge in ring 3). Attribution without a
race (§4.2) comes from init and `devmgr`, not from the kernel.

**One optional item, the customer's to decide**: `sendto`/`sendmsg` with a
destination on an unconnected `AF_UNIX` datagram socket. Today it answers
`EOPNOTSUPP` (`check_destination`), and it is what libsystemd's
`sd_journal_send()` and `sd_notify()` do. Without it, programs linked
against libsystemd lose their `sd_journal_*` lines (§5), and
`docs/INIT.md` §5.3's `NOTIFY_SOCKET` stays unbuilt. It is a generic Linux
ABI gap in the load ring (`fs/socket.rs`, `syscall/sockets.rs`), about 100
to 150 lines: look the name up as `connect` does (`sockname::socket_at`),
check the type, queue the record as `send_record` does, with the sender's
address for `recvfrom`. It adds no `unsafe`. Nothing here waits for it.

## 15. Certification

* **The item.** `lode` (all its roles), the `lode-upload` and `lode-receive`
  units and the `src/lib/log/` crates are ring-3 programs and
  libraries outside the item (`tools/common/data/certification-item.json`);
  `docs/CONVENTIONS.md` says ring-3 programs need no review. The kmsg
  branch's kernel changes (console core ring, `fs/kmsg.rs` load ring) are
  that branch's review, marked PENDING there.
* **What the consultant should still look at**, as a design review before
  step 7:
  1. **`devmgr`'s drivers gain a `ferrix.log` end.** `devmgr` is outside
     the item and the reference configuration is `ferrix.devmgr=kernel`,
     where nothing changes; but `devmgr-proto` is the protocol the item's
     kernel half speaks to `devmgr`. The design puts the new messages in
     `logproto` and the hand-over in the driver bootstrap `devmgr` alone
     writes, not in `devmgr-proto`, to keep them out; the consultant
     confirms that.
  2. **The audit record stays where it is.** init reads the FAU rings into
     `/var/log/audit/<id>.bin` (`docs/certification/AUDIT.md` §4), and
     that file, not the journal, is FAU_STG's storage. `lode` does not take
     the audit handle (it is not transferable; the reader's move to
     `audit.service` is AUTH phase 2's). If the customer later wants audit
     records *shown* in `lode`, init would forward decoded copies
     with `_TRANSPORT=audit`; the copy would claim nothing, but AUDIT.md's
     FAU_SAR text would have to say so, a `docs/certification/` change.
  3. **Tamper evidence** (journald's forward-secure sealing, USER-WISHES
     theme 19) is not proposed. If the Security Target ever claims
     integrity for operational logs, a hash chain per segment and a signed
     seal are the place; nothing claims it today.
* **The kernel stays as slim as it is** (§14).

## 16. Open questions for the customer

1. §14's optional kernel item (datagram to a name): build it, so
   libsystemd's programs log and `NOTIFY_SOCKET` works, or not.
2. `adm` as the readers' group, or a Ferrix name.
3. `SyncIntervalSec=5s` (more writes, shorter loss) or journald's 5 min;
   §8.3's budget is written for 5 s.
4. Debug records `ram-only` by default (§8.5), or on disk like journald.
5. Ferrix's `-o human` as the default output (§9.3), or journald's `short`.
6. Forwarding's first output to build (§10.2): journal-upload's export, or
   JSON lines.

## 17. Plan

**Where it stands (2026-10-23).** Landed: the design and steps 1, 2,
2a, 2b, 3, 4 and 5, and the field-quality fixes from the N150's first
records, and store-and-forward to a collector (§10.2's first part);
compression of sealed segments, and the write-budget row (§8.3) as
below, and the customer's order after them: TLS forwarding and
`lode-receive` on a Linux host (§10.2, §10.3), for the viewer on nazuna;
then the first of the three steps the customer approved after §20's audit:
units' own streams with their runs, and users' own records (§18 "Users'
own records and units' runs"), then the second, the journalctl
compatibility pack (§18 "journalctl's options"), then the third, `lode
top` and alerts (§18 "Top talkers and alerts").
The viewer is a separate agent's, a Rust desktop app built from the
customer's design. Follow-up rows: the uploader as a native service in a
sealed job (today a plain Linux service, §18); a machine id written at
first boot, so a machine is not filed under its host name; RFC 5424 and
OTLP outputs.



| Step | What | Points | Rows (tests) |
|---|---|---:|---|
| 1 | `src/lib/log/`: record, segment format with `FORMAT.md`, torn-tail recovery, sealing with zstd and indexes, summaries, retention classes, vacuum; `logproto` | 9 | host tests; Miri; fuzz `log_segment`; `check` |
| 2 | `src/lib/log/`: the filter language and its planner, limiter, ingest rules, parsers (RFC 3164/5424, native, stdout stream, JSON lines), formats (journald's byte for byte against goldens from systemd's test data; `human`; templates) | 7 | host tests; fuzz `log_filter`, `log_syslog`, `log_native` |
| 3 | `lode`: kmsg import with sequence tracking, the sockets, RAM and disk tiers, flush and relinquish, boot and machine id, restart record, `lode`'s socket | 8 | its loop over a fake front, host-tested; `test-log` parts A and C |
| 4 | init: `lode.service` and its sockets, `Type=native` + sealed job, `OpenFile=` and `FDS` for native units, the descriptor store, `WatchdogSec=`, `INVOCATION_ID`, unit output to `ferrix.log` with buffer and replay, `MaxLevelConsole=`, `LogFormat=`, `LogMultiline=` | 9 | `svc` host tests; `test-init` unchanged; `test-log` part B |
| 5 | the reader and administration roles, `lode cat`, the `journalctl` link; `svc log` and `svc status` through them; `cargo xtask box-log` | 7 | host tests over fixture stores; `test-log` part D; the benchmark, part F |
| 6 | sessions: `sessiond` and hyprix give each program a stream; yserver, Proton and DXVK scripts; alerts' `notify` | 3 | `test-desktop`: waybar's and foot's lines under their own identifiers |
| 7 | drivers: `logproto` client in the driver library with `INTEREST`, `devmgr` hands ends and re-hands after a restart, `freqlog` moved, `devmgr-events` import | 6 | `test-devmgr`: a driver's line with `_FERRIX_DRIVER`; a `lode` kill during driver logging loses no line |
| 8 | `lode-upload` (journal-upload export, RFC 5424, JSON lines, OTLP) and `lode-receive`, cursors, back-off, verified TLS, `--merge` | 8 | `test-net`: upload to the host's `systemd-journal-remote`, read back by the host's own `journalctl`; the destination down for a minute loses nothing |
| 9 | file sources; pstore import | 4 | `test-log` part G: a forced panic, then `lode -b -1` ends with it |
| 10 | the N150 box run (§17.2) | 3 | box |
| 11 | the write budget (§8.3) | 2 | `test-log` part E |
| 2a | session ids (§19.1): the piece that owns seat0 issues a session id and a random token per session, tells lode on the root-only `/run/lode/seat`, hands the compositor the token beside the card; lode stamps records with `_SESSION_ID`, `lode -s ID` | 5 | host tests; `test-compositor`: a session's records carry its id, a second session a new one |
| 2b | crash screenshots (§19): the provider protocol crate, lode's side, sessiond's seat token, hyprix as the first provider; off by default | 8 | host tests of the protocol; `test-compositor`: a forced crash with capture on stores an image linked to its record, with capture off none, with no provider a `CAPTURE=none` record, a hung provider times out |
| | **Total** | **79** | |
| (opt) | §14's datagram-to-name, if the customer wants it | 3 | stage 7's socket checks; a `sd_journal_send` program from a libsystemd build |

Order: 1 and 2 (one crate family, one agent), then 3 and 4 together (they
meet at `lode.service`; init is a second agent), then 5. After step 5's
`lode -f` and `box-log` (step 1, 3, 4 and part of 5: about 25
points with a minimal `lode`), box tests stop polling `dmesg`. 6 to
9 are independent of each other; 11 lands with 3 if it can.

### 17.1 `test-log`, the image row

Boots an init image with `log_buf_len=1M` and runs, in order:

* **A.** a line from a service's stdout and one from its stderr, one from
  busybox `logger` (`/dev/log`), one through the native protocol over a
  connected socket, one from `/dev/kmsg` -- each read back with
  `lode -o json`, with the trusted fields right, `_STREAM` right, and
  a sender-written `_PID` dropped; a service that writes a line and
  `exit(1)`s at once has that line under its unit (§4.2).
* **B.** a flood of 100,000 info lines from one unit: its errors in the
  same window all kept, another unit's lines all kept, one suppression
  record with the right count; a `drop` and a `fold` rule each with its
  summary record; `lode`'s memory under its limit.
* **C.** `kill -9` of `lode` during a flood: the restart record, kernel
  records gap-free by `_KERNEL_SEQNUM`, init's replay, a stdout stream
  still connected after the restart, and the flooding unit never stalled
  longer than the restart.
* **D.** a reboot: `--list-boots` shows two, `-b -1` reads the first;
  QEMU killed while `lode` writes: the next boot drops at most the torn
  record and says so; a user reads their own records and not the
  system's, `adm` reads both; `-u X@last` shows one invocation;
  `--where 'not unit = "flood.service"'` excludes it.
* **E.** the write budget (§8.3), from QEMU's block statistics.
* **F.** the benchmark (§8.4) on generated stores of 1 GB (in the row) and
  100 GB (a manual run, its figures recorded in §9.7).
* **G.** a forced panic, then the pstore import.

Negative controls through `gate.sh control`: limiter off (B's other unit
loses lines), sequence check off (C's gap not said), checksum check off (D
reads garbage), init attestation off (A's dying line loses its unit).

### 17.2 What the box run proves

On the N150, with the PO's OK and the box holder's turn, an image of the
landing hash: an hour on the desktop with `freqlog` on, then `lode
-b -k` still shows the boot's first line; `cargo xtask box-log` on nazuna
follows the whole hour and a `reboot` without a lost or repeated record
(cursor resume); a power cycle by the plug during writes leaves at most
the last 5 s and the torn record missing, said in the next boot's first
records; Proton's and yserver's lines are under their identifiers; the
write budget's first row holds on the real NVMe and btrfs. What QEMU
cannot show and the box does: real btrfs latency under `fdatasync`, a real
power cut, the real driver's rate.

## 19. Crash screenshots (the customer, 2026-10-23; planned as step 2b, after 2a)

When something crashes -- a process killed by a fault, a driver restarted,
a game's error popup that is gone before anyone looks (Endfield's
"illegal instruction" on the N150) -- lode keeps a picture of the screen at
that moment, linked to the crash's record, so `lode --around` shows it
beside the lines. **Off by default**: `/etc/ferrix/lode.d/capture.conf`
with `Capture=yes` turns it on.

**An open interface, not a hyprix hookup.** lode never reads `card0` or
any display device: it stays in its box (§11.2). Instead any desktop --
hyprix, a future shell, a kiosk, a headless framebuffer grabber -- can
register with lode as a *screenshot provider* and be asked for a picture.
X sessions under yserver are hyprix's windows, so hyprix's capture covers
them.

**The protocol** (`src/lib/proto/lode-capture`, `no_std`, host-tested,
typed, length-prefixed messages over the stream socket `/run/lode/capture`,
mode 0666; anyone may connect, only a proven seat holder is accepted):

```
REGISTER    provider -> lode   name; seat; outputs; formats it can send (QOI, PNG); seat token
ACCEPTED    lode -> provider   the formats lode takes
REFUSED     lode -> provider   why (no token, a wrong token, the seat held by another)
CAPTURE     lode -> provider   request id; the record's sequence number; one line of what crashed
IMAGE       provider -> lode   request id; format; width; height; the encoded bytes
DECLINED    provider -> lode   request id; why (locked screen, nothing shown, privacy mode)
UNREGISTER  provider -> lode   (or the connection closes: the provider is gone)
```

### 19.1 Session ids first (step 2a, the customer's order)

The seat token the providers prove themselves with is built first, on its
own, as step 2a, because a session id on every record is useful before any
picture: which login's programs said what. Before writing it, step 2a
reads what exists and reuses it rather than adding a second notion of who
holds the seat: `sessiond` (seat0's owner, `docs/AUTH.md` §6.2), card0's
DRM-master work (0e3e343a3), `authd`'s console revoke, and the backlog
rows for sessiond and session switching. Kernel lines: 0 unless a generic
typed mechanism is truly needed, and then counted and asked for first.
The order is 2, then 2a, then 2b.

**Proving it owns the display.** `sessiond` already owns seat0 and hands
the compositor its card (`docs/AUTH.md` §6.2, `compositor_seat`). When it
starts a session it also draws a random seat token, tells lode on a socket
only root can reach (`/run/lode/seat`, root's 0600: "seat0 is held by pid
P of uid U with token T"), and gives T to the compositor beside the card.
lode accepts a REGISTER only from that pid (`SO_PEERCRED`) with that token;
a new session's token replaces the old, and the old provider is dropped.
An untrusted client cannot post a picture: it has neither the pid nor the
token, and lode only ever stores an IMAGE that answers a CAPTURE it sent.

**Several providers.** One per seat: the one holding the seat's current
token. A headless grabber may register for a seat no compositor holds
(`seat: headless`). lode never merges or picks between pictures.

**No provider.** The crash record is stored as always, with
`CAPTURE=none` and the reason (off, no provider, declined, timed out).
Logging never waits for a picture.

**Timeouts.** The crash record is written first. A CAPTURE is answered
within 2 s or recorded `CAPTURE=timed-out`; three time-outs in a row drop
the provider until it registers again. The image arrives on its own
connection, read without blocking into a buffer capped at 16 MiB, so a
hung or slow compositor costs lode a socket and a buffer, never a stalled
read of the kernel log.

**What triggers it.** Crash records (§18, step 2), `devmgr`'s driver
deaths and restarts, and a rule action, `capture where EXPR`, for
anything else (`capture where message ~ "illegal instruction"`). At most
one picture each 10 s.

**Storage and privacy.** The screen can show secrets, so capture is off
unless turned on, and a picture is kept like the journal: in
`/var/log/journal/captures/<boot>-<seqnum>.<qoi|png>`, `lode:adm 0640`,
readable by exactly those who read the journal (§12). A record
`CAPTURE_OF=<seqnum>` with `CAPTURE_FILE=` links it to its crash. Kept 7
days and at most 256 MiB, the oldest removed first, and removed with
`lode delete` when its crash record is. A provider may decline while the
screen is locked.

**Certification.** lode, sessiond and hyprix are ring-3 programs outside
the item; no kernel lines.

## 18. Where it stands (2026-10-23)

The design landed as dbc6f9bc4. The customer said go (2026-10-23, "ok
then go with it"): the steps are built in order, each gated and landed
before the next, the smallest useful first. No requirement ids are needed:
nothing here is in the item.

### Step 1, as built

`src/lib/log/lode` (`ferrix-lode-core`, `no_std`, no `unsafe`, 31 host
tests) and `src/user/system/native/lode` (the native program, `/sbin/lode`
in every initramfs, `/bin/lode` and `/bin/journalctl` linking to it in
every image that boots init). Kernel lines: 0.

* **The service** (`lode.service`, `Type=native`): started by init with a
  bootstrap channel, it opens `/dev/kmsg` and makes `/var/log/journal`
  (or `/run/log/journal` where `/var` cannot be written) as root, gives
  the store to uid 91 and group `adm` (4), becomes uid 91 for good, cuts a
  torn tail off the newest segment, finds this boot's last stored kernel
  sequence number, writes its started or restarted record, says READY,
  and stores every kernel record as it reads it: one `write` a wakeup,
  `fdatasync` every 5 s and at once after crit or worse, segments of 16
  MiB, the oldest removed past 1 GiB. A restart reads the ring from its
  oldest record, skips what was stored, and says in a record how many
  were lost where the sequence jumps. The boot id is drawn once a boot
  and kept in `/run/lode/boot_id` for a restarted instance.
* **Its box**: `Slice=drivers.slice`, the slice the kernel seals
  (`docs/CGROUPS.md` §3.3), so lode's job is sealed with no new init or
  kernel mechanism; init's `Restart=always`. `test-log` checks uid 91,
  the cgroup `/drivers.slice/lode.service` and a refused move out of it.
  Not yet: init handing it its descriptors (§11.2.1), so it opens them as
  root before dropping to its user; `WatchdogSec=`; the sandboxing keys,
  which init applies to Linux services only.
* **Sources**: the kernel log alone. Units' output arrives through it,
  since init writes every unit line to the console as `  init
  NAME[PID]: TEXT`; lode reads that into `UNIT`, `SYSLOG_IDENTIFIER`
  and `SYSLOG_PID` -- not the trusted `_SYSTEMD_UNIT`, since anything can
  write to the console. init's `ferrix.log` channel (step 4 of §17)
  brings the trusted fields. Kernel subsystems' `  name     text` lines
  get `FERRIX_SUBSYSTEM`, a driver's `name: text` the identifier.
* **The reader**: `-b [N|ID]`, `--list-boots`, `-k`, `-u`, `-t`, `-T`,
  `-p P[..Q]`, `-g` (globs, any case for a lower-case pattern), `-S`/`-U`
  in every form it prints, `-n`, `-r`, `-e`, `-f`, `-o human|short|
  short-iso|short-iso-precise|short-precise|short-monotonic|with-unit|
  cat|json|export`, `--after-cursor`, `--show-cursor`, `-D`, `--utc`,
  `FIELD=VALUE` matches with `+`, `--where` (the filter language's core:
  `and`, `or`, `not`, parentheses, globs, `~` as "contains", `priority`
  by name), and for the N150's needs `lode mark TEXT`, `--since-mark`/
  `--until-mark`, `--around TEXT DURATION`, and local time from `TZ`,
  `/etc/TZ` or `/etc/localtime`'s POSIX footer with no zone database.
  `lode verify [--kernel]` checks the segments and a boot's kernel
  sequence. No indexes yet (step 5): a query reads the boot's segments
  whole.
* **Box tests stream** with `ssh box lode -f -o json --after-cursor=C`
  and a `--where` filter; the cursor resumes across a dropped connection
  or a reboot.
* **The desktop's image** enables it (`basic.target.wants`), so the N150
  keeps its boots once installed from this landing.

### Step 2, as built

Floods held, retention per class, deletion, crashes (§7, §8.5; the
N150's needs 3a and 5):

* **Rules** in `/etc/ferrix/lode.d/*.rules` (`drop`, `sample 1/N`, `level
  P`, `fold DURATION`, `limit BURST/DURATION`, each `where` a filter), read
  at start; a file that does not read is said in an error record and not
  used. A rule that held records back says how many every 10 s (`RULE`,
  `N_DROPPED`).
* **Folding**: a source's message repeating its last, within 30 s (or a
  `fold` rule's window), is counted; `message repeated N time(s): ...`
  with `REPEATED=N` follows when another message comes or the window ends.
* **The limiter**: per source (`UNIT`, else the identifier, else the
  subsystem, else the transport) and per class -- errors 10,000, warnings
  2,000, chatter 1,000 a 30 s window, or a `limit` rule's -- never scaled
  by anything else; `Suppressed N message(s) from SOURCE` with
  `N_SUPPRESSED`. The kernel's own records and lode's are never held.
* Every record that says what was held carries `HELD_FIRST_SEQNUM` and
  `HELD_LAST_SEQNUM`, so `lode verify --kernel` counts those gaps as said.
  A restart that finds a gap no record accounts for -- a window or fold
  still open when the earlier instance was killed -- says it in a record
  of its own, so a crash never leaves a gap unsaid.
* **Retention classes** (`retention.conf`, §8.5; without it, `errors`
  (priority err or worse) a year and 256 MiB, `system` 90 days and 1 GiB):
  one segment series per class, `<class>@<boot>-<seq>.fxj`; `ram-only`
  classes in `/run/log/journal`; a segment is sealed at 16 MiB or a day,
  and vacuum removes whole sealed segments by age and size per class, then
  the oldest past 1 GiB (64 MiB in `/run`).
* **`lode rotate`** asks the service, through the kernel log, to seal every
  segment and begin new ones; **`lode delete --where EXPR`** (root's)
  rotates, then rewrites every sealed segment holding a match without it
  (a new file synced, then renamed over the old), and writes a
  `lode-delete` record saying how many it removed and by whom.
* **Crash records**: the kernel's `signal` lines that end a process
  become priority err with `MESSAGE_ID` for crashes, `CRASH_PID`,
  `CRASH_SIGNAL` (by name), `CRASH_ADDRESS`, `CRASH_PC`, `CRASH_TRAP`,
  and `CRASH_COMM`/`CRASH_PROGRAM` when the process is still there to read
  them from; `lode --where 'crash = "*"'` lists them.
* `test-log` adds: 50 repeats become one line and one `REPEATED=49`; 300
  lines over `limit 100/2s` become 100 and one record of 200 held; a
  dropped source is absent and its drop said; an error lands in the
  `errors` class; `lode delete` removes two records and says so.

### Step 2a, as built

Session ids (§19.1). What already existed is reused, not doubled:
`sessiond` owns seat0 and numbers its sessions from the count `login`
keeps (`/run/ferrix/login/next`, `session-<n>.scope`), so the session id is
that `n`, the one its scope already has.

* `sessiond` draws a 16-byte token per session from `/dev/urandom`, gives
  it to the compositor in `FERRIX_SESSION_TOKEN` beside the card's channel,
  and once the scope exists writes `SESSION <n> seat=seat0 uid=U pid=P
  scope=session-<n>.scope unit=<its own unit> token=<hex>` to lode's
  `/run/lode/seat`; `END <n>` when the session is gone. A lode that is not
  there costs one line on `sessiond`'s output; the session goes on.
* lode makes `/run/lode/seat` while it is still root, leaves it root's
  0600, and refuses a peer whose uid is not 0. It keeps the open sessions
  in `/run/lode/sessions`, so a restarted lode takes them up; says each
  begin and end in a record (`SESSION_ID`, `SESSION_SEAT`, `SESSION_UID`,
  `SESSION_SCOPE`, `SESSION_UNIT`; never the token); and stamps
  `SESSION_ID` on every record of the session's unit while it lasts.
* `lode -s ID` (`--where 'session = ID'`) is a session's lines; `lode
  session begin|end` is the same line for a seat owner other than
  `sessiond` (a kiosk's starter, the row).
* `test-log`: a session begun before the kill keeps stamping the unit's
  lines after lode's restart, and stops at its end.
* Kernel lines: 0; ring-3 programs only (`sessiond`, lode), none in the
  item.

### Step 2b, as built

Crash screenshots (§19), on the session token of step 2a.

* `src/lib/proto/lode-capture` (`ferrix-lode-capture`, `no_std`, no
  `unsafe`): the provider protocol's seven messages as frames, decoded
  strictly, and a QOI encoder and decoder -- the one picture format every
  provider can write without a library. Host-tested.
* lode (`capture.rs`): off unless `/etc/ferrix/lode.d/capture.conf` says
  `Capture=yes`; then `/run/lode/capture` (0666) and
  `<journal>/captures/` (`lode:adm 2750`). A REGISTER is accepted only from
  the pid `sessiond` named for the seat with that session's token
  (`SO_PEERCRED`); anything else is REFUSED and dropped. Triggers: crash
  records, `devmgr` lines saying a driver died, and `capture where EXPR`
  lines of the file. The crash record is stored first; then one CAPTURE to
  the seat's provider, at most one each 10 s; the answer within 2 s or a
  `timed-out` record, three in a row dropping the provider. Every outcome
  is a record, `CAPTURE_OF=<seqnum>` and `CAPTURE=stored|none|declined|
  timed-out|skipped`; a stored picture is
  `captures/<boot>-<seqnum>-<time>.qoi`, 0640, kept 7 days and 256 MiB.
* hyprix (`lodeshot.rs`): takes `FERRIX_SESSION_TOKEN` out of its
  environment first thing in `main`, registers for seat0, and answers each
  CAPTURE with its first screen as last shown (the GPU's or a client's
  buffer, or the software canvas), or declines while the session is
  locked. A host test runs the compositor headless against a socket
  standing in for lode and decodes the 1024x768 picture it sends.
* `lode provide-stub TOKEN` is the protocol's smallest peer (a 2x2
  picture): what `test-log` proves lode's side with -- `none` with no
  provider, one stored QOI file from the proven one, a refusal for one
  with the wrong token.
* Not yet: `lode delete` removing a deleted crash's picture; PNG.
* Kernel lines: 0; ring-3 only (lode, sessiond, hyprix), none in the item.

### Step 3, as built

The Linux front (§5; `front.rs`), made while lode is still root:

* `/run/systemd/journal/dev-log`, syslog datagrams, and `/dev/log`, a link
  to it in devfs's fixed table beside `fd`, `stdin`, `stdout` and `stderr`
  -- the one kernel change of the log so far, two lines in
  `src/kernel/src/fs/devfs.rs` (load ring; `fs` exists only under
  `linux_abi=on`) and the devfs boot check's expected names. Without lode
  the link dangles and `syslog()` is refused at once, as on Linux without
  journald. RFC 5424 when a datagram says `1 `, RFC 3164 read leniently.
* `/run/systemd/journal/` is made by lode as root, root's 0755, so while
  lode is stopped nobody else can bind the syslog socket there and take
  other programs' records (the consultant's C2, ledger line 3404).
* `/run/systemd/journal/socket`, systemd's native protocol, a large record
  in a passed memfd. libsystemd's `sd_journal_send` sends to the name on an
  unconnected socket, which the kernel refuses (§14's optional item, not
  built): a program that connects first gets through (`lode cat
  --journal`).
* `/run/systemd/journal/stdout`, stdout streams: the seven-line header,
  then lines, `<N>` prefixes read when the header says so.
* Each datagram's pid, uid and gid are the kernel's (`SO_PASSCRED`), a
  stream's are `SO_PEERCRED` at accept; `_COMM`, `_SYSTEMD_CGROUP`,
  `_SYSTEMD_UNIT` and `_SYSTEMD_SLICE` come from `/proc` at once and are
  kept 10 s for a sender that has exited since. A session's scope stamps
  its programs' records with its id. A one-shot sender that has exited
  before lode reads its datagram keeps the kernel's `_PID`, `_UID` and
  `_GID` but has no `_COMM` or unit (§4.2's residual; seen on AArch64 with
  busybox `logger`), never a wrong one.
* `COMMAND | lode cat [-t ID] [-p PRIO] [--journal]`: standard input into
  the log, as `systemd-cat` sends it (no program to start: pipe into it).
* Not yet: streams parked in init's descriptor store across a restart of
  lode (a stream open while lode restarts is lost and its writer sees
  `EPIPE`); the sockets owned by init rather than lode.
* `test-log`: busybox `logger`, a stream and a native datagram each arrive
  once with the trusted fields; `/dev/log` is the link; with lode stopped,
  `logger` returns at once.

### Step 4, as built

Box tests stream to nazuna (§10.1; the N150's needs 2 and 10).

* `cargo xtask box-log --host <DEST> [-- <lode options>] [--until-match
  TEXT] [--out FILE]` runs `lode -f -o json` on the machine over ssh,
  writes every record to `~/.local/share/ferrix/logs/box/<UTC>-<host>.jsonl`
  (its first line naming the host, this tree's commit and the time, so the
  evidence can be cited) and a line of each to the terminal, and keeps the
  last record's cursor. A dropped connection -- the machine rebooting --
  is made again every 2 s and goes on `--after-cursor` the last record
  kept, across the reboot; the store's sequence numbers make a repeat
  impossible. `--until-match` ends it at the first record whose message
  holds the text; xtask's `--timeout` bounds the whole follow.
* Not yet: lode's own store-and-forward to a collector (§10.2's
  journal-upload, RFC 5424 and JSON outputs) and receiving (§10.3); ssh is
  the transport and its authentication the access control.

### Step 5, as built

Queries that stay fast as the store grows (USER-WISHES top item 2, §8.4).

* At every seal the service writes `<segment>.sum` beside the segment
  (`ferrix-lode-core` `summary`): its sequence and time range, record
  count, priority range, and every value of `SYSLOG_IDENTIFIER`, `UNIT`,
  `_SYSTEMD_UNIT`, `_TRANSPORT`, `SESSION_ID` and `_FERRIX_DRIVER` it
  holds, with counts, under a CRC; a new file renamed over the old. At
  start it summarises up to 64 sealed segments that have none (an older
  lode's); vacuum removes a summary with its segment; `lode delete`
  rewrites the summary of each segment it rewrites.
* A reader skips a segment whose summary proves the query cannot match
  (a time range missed, a cursor already past it, a priority range or a
  summarised field's value absent under an `and`), and reads a segment
  with no summary -- the one being written -- always.
* `-n N` with no marks or `--around` reads the candidate segments newest
  first and stops once no segment left can hold a newer match than the
  Nth found; `--list-boots` takes the last time from summaries.
* `lode bench RECORDS [DIR]` writes a store of that many records over 200
  units and times `-u <rare unit> -n 50` both ways. On 200,000 records (3
  segments): x86-64 25 ms through summaries against 409 ms reading all,
  AArch64 133 ms against 1,970 ms, the same 50 records. test-log requires
  the same answer and no slower, and `-u lode-test -n 3` on its own store.
* Not yet: the field indexes inside a sealed segment and its sparse time
  index (a segment that may match is read whole); compression.

### Field quality, as built (the N150's first records, 2026-10-23)

What the first 1,907 records from a real machine showed lode reading
wrong, fixed in one landing before step 6:

* **Names.** A name with a number's tail glued before it (`0usbhid`,
  `00usbhid`, `0xusbhid`: another writer left digits on the console with
  no newline) is read without them. ANSI colour is taken out before a
  line is read. A Rust `tracing` line (`2026-...Z  WARN russh::server:
  text`) gives its crate as the identifier and its own level. A program's
  padded name without the kernel's two-space indent (`devmgr   intel-gt
  started`) is read as the kernel's are. `error:` and `WARNING:` name a
  level, never a program. The 49 records with no `SYSLOG_IDENTIFIER` were
  these.
* **Levels** (1,906 of 1,907 were info): §4.1. A level is stated, the
  kernel's, derived from the words -- `failed`, `could not`, `cannot`,
  `unable`, `error:` an error; `refused`, `denied`, `lost`, `dropped`,
  `timed out`, `warning` a warning; none of them after `0` or `no` one or
  two words before (`0 rows refused`); for the kernel's own lines only the
  error words and an upper-case `WARNING:`, since its boot checks report
  what they refused on purpose -- or unspecified.
* **`FERRIX_SUBSYSTEM`** (on 155 of 1,907): the subsystem a line names,
  else the program that wrote it, else `kernel` for the kernel's own lines.
  Every line that says who wrote it now has one.
* **The empty `MESSAGE`** (record 2) was a blank console line: blank
  kernel and console lines are no longer stored, and a record says how many
  there were (`N_BLANK`, with their kernel sequence range, so `verify
  --kernel` still accounts for every one).
* **Floods alike but for their numbers** fold: two messages are the same
  when they differ only in runs of digits (hyprix's `not scanning out
  directly ... the top one is 839x497 at 181,99` every frame). The record
  that says so reads `message repeated N time(s), numbers varying: <the
  first>` and carries `LAST_MESSAGE`, the last of them. A line the
  limiter held breaks a run, and a mark is never folded. Each kind of rule is now read on its own,
  the first match deciding: the first matching `fold` sets the fold
  window, the first `limit` the limit, and the first `drop`, `sample` or
  `level` whether the line is held -- so a fold rule and a limit rule both
  apply to one line. `fold 0s where ...` turns folding off for lines
  that must each be kept (test-log's flood).
* **`-o verbose`**, journald's.
* test-log checks each on all three architectures: a similar flood folded
  with its last line, a glued name read, and a line with no level shown as
  unspecified in json, human and verbose, left out by `-p err`, brought in
  by `-p err,unspecified`, and counted by `lode levels`.

### Store-and-forward, as built (§10.2's first part)

`lode upload`, `lode-upload.service`, sends the log on to collectors.

* **The store is the buffer.** Each destination keeps a cursor in
  `/var/lib/ferrix/lode-upload/<name>`: the store sequence number of the
  last record it acknowledged. The cursor is written through a new file
  renamed over the old one, and only after the destination answered
  `2xx`. A destination that is down is tried again after 1 s, 2, 4, ...,
  up to 5 min, for ever; the uploader never gives up and exits. Killed,
  init starts it again and it goes on from the cursor: nothing is sent
  twice that was acknowledged, nothing lost that the store still holds.
* **What was gone before it was sent is said.** Records missing from the
  store's sequence after a cursor (vacuum, `lode delete`, a RAM-only class
  at a reboot) are counted in a record of their own: `MESSAGE_ID`
  e2a6c4b8d0f24e1a9c3b5d7f9e1a3c5b, `N_UNSENT`, `UPLOAD_DESTINATION`, the
  range.
* **Configuration**: `/etc/ferrix/lode-upload.conf`, one destination a
  line, `NAME URL FORMAT [auth] [where EXPR]`. No file, or no line: it
  says so and ends. `export` posts the Journal Export Format as
  `systemd-journal-upload` posts it (`POST /upload`, `Content-Type:
  application/vnd.fdo.journal`); `json` posts journald's JSON a line
  (`application/x-ndjson`) for Vector, Loki and Fluent Bit. At most 1,000
  records or 1 MiB a request. The filter is §6's language; records of the
  auth and authpriv facilities and of `auth.service` go only to a line that
  says `auth`.
* **Plain HTTP only to this machine** (`localhost`, `127.0.0.0/8`): a
  configuration naming another host is refused and says to use an ssh
  tunnel. TLS is not built yet; it comes with a TLS stack for native
  programs.
* **Its box**: started as root to make its state directory, then uid 92 in
  `adm`, which reads the log and nothing else. An ordinary Linux service
  in `system.slice`, not the sealed `drivers.slice`: that slice takes
  native services only (init's start of a Linux process there fails
  `EPERM`), and the uploader holds nothing the log depends on. Off unless
  an image enables the unit.
* **Reading**: it reads only what follows its cursor -- sealed segments
  that end before it are skipped by their summaries, the one being written
  is read from where the last read stopped -- at most 5,000 records ahead,
  and an idle pass, which sees no segment change size, reads nothing.
* `test-log` runs `lode upload-stub`, a collector on 127.0.0.1 that keeps
  what it is sent and answers 503 while told to. The uploader starts
  before it; 5 records reach it; while it refuses, nothing is counted as
  kept and the failures are said; once it is back, 5 more follow. Then the
  uploader is killed, and after init starts it again, 5 more. All 15
  arrive exactly once, in the export format.
* Not yet: TLS and client certificates; RFC 5424 over TCP; OTLP;
  `lode-receive` and `--merge` (§10.3, §10.4); a check against a real
  `systemd-journal-remote` (nazuna has none installed; the body is the
  same `-o export` that the format tests cover); a cursor past a store that was
  wiped and begun again, which today waits for the new store to pass it.

### Compression, as built (§8.2)

* When a segment is sealed (16 MiB, a day, `lode rotate`), the service
  compresses it: its entries in zstd frames of about 64 KiB of whole
  entries (`ruzstd`'s fastest level, the crate the btrfs code already
  uses), each frame with its unpacked and packed length and a CRC-32C, and
  the segment's header kept whole in front (`ferrix-lode-core`
  `compress`, magic `FXJZSTD1`). Written to a new file, synced, renamed
  over the sealed one: a crash leaves one or the other.
* **Unpacked, it is the segment it was made from, byte for byte** (cut at
  its last good entry): the same header, the same entries at the same
  offsets. So every reader -- `lode`, `-f` holding an offset, the
  uploader, `verify`, `delete` -- reads it through one function
  (`store::bytes`) and nothing else changed. A frame whose checksum or
  length is wrong ends the unpacking there, as a torn entry ends a scan.
* **No stall**: the service packs four frames (256 KiB) a turn of its
  loop and reads the kernel log in between, so sealing a full segment
  never holds the log back; it does not sleep while a segment waits.
  Segments sealed but not compressed -- by an older lode, or one stopped
  midway -- are queued at start. A segment that changed meanwhile
  (`lode delete` rewrote it) or did not get smaller is left as it is;
  `lode delete` compresses again what it rewrites of a compressed one. A
  compressed segment is never cut as torn nor reopened for writing.
* The N150's first 1,907 records pack 5.6 to 1 (682 KB to 123 KB), at
  about 110 MB/s on nazuna. `test-log` requires a compressed segment
  after `lode rotate`, and then reads all 3,000 flood lines and `verify
  --kernel` through it on all three architectures; host tests cover the
  round trip, a torn tail, a flipped bit and frame-at-a-time packing.
* Not yet: the field indexes and the sparse time index after the frames
  (§8.4); a sealed segment that may match is unpacked whole.

### The viewer, as built (2026-10-23)

The customer's design of a log viewer (`~/.local/share/ferrix/design/lode-app`
on nazuna) is the app `lode-viewer`, in every image a
person runs and "Logs" in the launcher. It reads the store as `lode`
does, with `ferrix-lode-core` (segments, compressed or not, summaries, the
filter language, the JSON output), as the person running it, and follows it for Live as `lode -f`
does. It also opens a JSONL export (`lode -o json`, `cargo xtask box-log`'s
files) on Ferrix or a Linux host. Its window is the desktop's own
`compositor-toolkit`, drawn on the CPU; egui and iced were set aside because
both reach Wayland through winit, which loads libwayland and libxkbcommon at
run time, which a static Ferrix program cannot.

* **Overview** (a boot's track, lanes, tiles, activity per 10 s, rate
  against the limiter, top sources, the window's warnings and worse first),
  **Logs** (the filter language, saved queries, chips, facets with
  unspecified among the levels, held and folded lines, lode's marks, a
  record's every field) and lode's state at the sidebar's foot.
* A record from before the field-quality landing above, stored as info, is
  shown at the level its words say by `ingest::guess_level`, with a note
  saying so; the filter language sees that level.
* `test-compositor --boot lode-viewer` boots it on the desktop, types a
  query into it and requires the 13 records of 801 it must find (x86-64).
* On a Linux desktop too (nazuna's GNOME, any Wayland session; its
  README's `install-linux.sh`), with a machine picker over the stores
  `lode-receive` keeps, `~/.local/share/ferrix/lode-receive/<machine-id>/`
  (or `host-<hostname>/`), each followed Live; `--import FILE.jsonl --into
  DIR` makes such a store from an export.
* Not yet: the design's Timeline, Sources, Crashes, Boots and Rules pages,
  and dragging the brush's edges.

### The write budget, as built (§8.3)

`cargo xtask test-log-budget` (x86-64 by default, KVM): init with lode's
store on a btrfs volume at `/data`, `/var/log/journal` a link there; a
unit writes two loads to the kernel log with nothing else writing; xtask
reads the volume's `wr_bytes` from QEMU (`query-blockstats` on the boot's
QMP socket) around each and divides by the `MESSAGE` bytes. It also
requires every line stored and none lost (`verify --kernel`).

| Load | Measured (2026-10-23) | Budget | The row requires |
|---|---:|---:|---:|
| 2 lines a second of 100 bytes, a minute (the hour's ratio: each 5 s sync is the same) | 45.4 | 40 | 50 |
| 20,000 lines of 100 bytes, about 2,000 a second | 2.61 | 3 | 3 |

* **Field names as indexes.** An entry wrote every field's name in full
  -- `_SOURCE_MONOTONIC_TIMESTAMP`, `FERRIX_PRIORITY_SOURCE` and five more
  on every kernel record, 128 bytes of a 380-byte record for a 100-byte
  message. The 40 names records carry most (`segment::NAMES`, append only)
  are now written as a two-byte index (a name length with its top bit set,
  which no real name has); everything else, order included, is as before,
  and older segments read as they did. The fast load went from 3.96 to
  2.61, within its budget.
* **The slow load is btrfs's.** A minute with lode running and no load
  writes 60 KiB to the volume; with the load, 532 KiB for 12 KiB of
  messages: about 40 KiB for each of the twelve five-second `fdatasync`s,
  whatever lode wrote (2.6 KiB). That is the commit Ferrix's btrfs makes
  for an `fsync`; lode can only sync less often (`SyncIntervalSec=`). The
  row holds the slow load to 50, so it cannot grow, and says it is over
  the design's 40. Meeting 40 is the file system's: an `fsync` through a
  log tree, as Linux's btrfs does, rather than a whole commit.

### TLS and lode-receive, as built (§10.2, §10.3)

The customer's order: a Ferrix machine's log over TLS to nazuna, kept
there for the viewer. One workspace, `src/user/system/linux/lode-net`, a
std program against musl as dhcp is, with two programs:

* **`lode-upload`** (`/bin/lode-upload`, `lode-upload.service`), in the
  desktop's image and `test-log`'s, replaces the native `lode upload`:
  the same cursor, back-off and reading of the store (std's file system,
  `ferrix-lode-core`'s formats), and now `https://` to any host. A line is
  `NAME https://HOST:PORT/upload export ca=FILE [cert=FILE key=FILE]
  [where EXPR]`; plain `http://` stays limited to this machine. The keys
  are read as root before it becomes uid 92. Each upload says
  `X-Lode-Machine-Id` (from `/etc/machine-id`) and `X-Lode-Hostname`.
  Records gone before they were sent are said in a record of their own
  (`MESSAGE_ID` e2a6c4b8..., `N_UNSENT`).
* **`lode-receive`**, on a Linux host as an ordinary user:
  `pair --machine NAME --server-name HOST [--port P] [--root DIR] [--json]`
  makes (once) a CA, the server's certificate for HOST, and NAME's client
  certificate, and prints the files and the line the machine needs;
  `run [--listen ADDR:PORT] [--allow-unpaired]` takes uploads; `check
  [--json]` says whether it runs, where, its certificate's fingerprint and
  the machines it keeps; `install-service` writes and starts a systemd user
  unit; `show --machine DIR` prints a machine's records as JSON lines. Exit
  status 0 done, 1 error, 2 usage, 3 not running (`check`), 4 certificates.
* **The stores**: `~/.local/share/ferrix/lode-receive/<machine-id>/` (or
  `host-<hostname>/` for a machine with no machine id), each a lode store
  as lode lays out its own -- `system@<boot>-<seq>.fxj`, `.sum` beside, the
  sealed ones compressed -- with a `machine` file (`hostname=`,
  `last_seen=`, `paired=`); `.pki/` and `.receive.lock` beside them. The
  viewer opens them with `ferrix-lode-core`. A machine directory is bound to
  the certificate it was first paired with: another is refused.
* **Once, and only after it is safe.** The receiver answers 200 only after
  the records are synced; a record whose sequence number it already holds
  in a boot it holds is dropped, so an upload sent again after a lost
  answer stores nothing twice; a lower number in a boot never seen (a
  machine whose store began again) is new.
* **The TLS** is rustls with a provider of ours (`src/provider.rs`): TLS
  1.3 only, one cipher suite (ChaCha20-Poly1305 with SHA-256), one key
  exchange (X25519), one signature scheme (Ed25519), on the RustCrypto
  crates. Why so few: we make both ends -- `pair` issues Ed25519
  certificates and the two programs speak only to each other -- so every
  algorithm more is code that never runs. Why ours: ring and aws-lc need a
  C compiler for the musl Arm targets, which the tree does not have; the
  RustCrypto organisation's `rustls-rustcrypto` is an alpha that pins an
  older `rustls-webpki` and brings RSA (RUSTSEC-2023-0071), so its glue for
  our three algorithms is copied, with attribution, not the crate.
  Randomness is the kernel's alone, `getrandom(2)` with no flags (on Ferrix
  the kernel's ChaCha20 generator seeded from firmware and the processor,
  `src/kernel/src/random.rs`); no seeding in user space, no fallback, and a
  failed read fails the handshake or the key. The client must present a
  certificate from the receiver's CA unless `run --allow-unpaired`.
* **Keys** are made from the kernel's random numbers, written 0600 through
  a new file renamed into place (`.pki/` itself 0700), and never printed:
  `pair` prints paths and fingerprints.
* **Tests**: the provider against RFC vectors (SHA-256, HMAC, rustls's HKDF
  on our HMAC per RFC 5869, X25519 per RFC 7748 with a low-order point
  refused, Ed25519 per RFC 8032, a record sealed and opened, tampering
  refused); on the host, a paired machine's uploads kept once across a
  resend, a wrong CA, a foreign client certificate, an expired one and none
  at all refused, keys 0600 and absent from `pair`'s output, and our
  provider against rustls's own ring provider both ways (`ring` a
  test-only dependency). `cargo xtask test-log-remote` boots a guest that
  sends forty lines over TLS to `lode-receive` on the host through xtask's
  gateway (10.0.2.2), takes the receiver away after twenty, writes twenty
  more while the uploader fails and backs off, brings it back, and
  requires all forty kept once, in order, under the guest's host name.
* Not yet: a machine id at first boot (today a guest is
  `host-ferrix/`); RFC 5424 and OTLP; certificate revocation (a paired
  machine is unpaired by removing its `.pki/clients/NAME.pem`).

### Users' own records and units' runs, as built (2026-10-23)

The first of the three steps the customer approved after §20's audit
(§20.1 gaps 6 and 10, themes 12 and 14).

* **Units' streams, attributed by init** (§4.2). init makes a pipe for a
  unit's standard output and another for its standard error (an error that
  inherits a log gets its own), and hands lode every line on
  `/run/lode/units` -- made by lode as root, 0600, any peer but root
  refused -- with the unit, its run, the process, its uid and gid and the
  stream (`ferrix-lode-core` `units`). lode stores it with
  `_SYSTEMD_UNIT`, `_SYSTEMD_INVOCATION_ID`, `_STREAM`, `_PID`, `_UID` and
  `_GID`, `_TRANSPORT=stdout`, and the identifier and level its words say,
  as before. Nothing waits for lode: init's connection is non-blocking,
  what lode is slow to take is not sent, and a lode that is gone is looked
  for again once a second.
* **The console copy is paired, not stored twice.** init still writes every
  line to the console, after the socket: gates read units' lines there, and
  it is how a line reaches the log while lode is not running. lode pairs a
  copy it reads in the kernel log with the trusted line it was handed (same
  unit, pid and message): the trusted record takes the copy's
  `_KERNEL_SEQNUM`, so `verify --kernel` still finds no gap, and the copy is
  not stored. A copy with no trusted line -- lode was down, or init's
  socket full -- is stored as before, with `UNIT` read from its text. A
  trusted line waits for its copy until the kernel log has been read to its
  end and 2 s have passed. Standard error is not raised to warning (§5
  said it would be): most of Ferrix's services write their ordinary lines
  there, so a stream says where a line went, not how bad it is.
* **Runs** (§9.2). init draws 32 hex digits each time a unit's cgroup is
  made -- each start -- gives them to its processes as `INVOCATION_ID`,
  and says them in `svc status` (`Invocation:`). Summaries keep
  `_SYSTEMD_INVOCATION_ID`'s values, so a run's records skip every other
  segment; a reader that knows fewer summary fields ignores the new one.
  `lode -u UNIT@last` is the current or last run, `@-1` the one before,
  `@N` the Nth kept, `@ID` one run; `--invocation` and `-I` as journalctl's.
  `svc status UNIT` ends with the run's last ten lines and `svc log UNIT` is
  the run (`lode -u UNIT@last -n 50`), each falling back to init's ring
  when lode keeps nothing the caller may read.
* **A user's own records** (§12.1, `access`). A record whose trusted
  `_UID` is 1000 or more (never `nobody`) goes to `user-<uid>/` beside the
  system's segments, a series per retention class, vacuumed against a
  quarter of the store's limit. Every file stays `lode:adm 0640`, so root,
  `adm` and the uploader read them as before (`lode-upload` reads the users'
  directories too). A user who cannot open the store is handed theirs:
  `lode` connects to `/run/lode/read` (`SOCK_SEQPACKET`, 0666), and lode,
  having read the caller's uid with `SO_PEERCRED`, passes each of that
  user's segments and summaries open for reading (`SCM_RIGHTS`), then
  `END`. The reader reads them itself; lode opens and never reads for
  anyone. Unix permissions name one group a file, and a user's segments
  must be `adm`'s too: that is why the design's `lode:<uid>` modes were not
  used.
* `test-log` on all three architectures: a unit's standard error stored
  once, under its unit, `_STREAM=stderr`, with its console copy's kernel
  sequence number; a oneshot started twice told apart by `-u twice@last`,
  `@-1`, its invocation id, `svc log` and `svc status`; a unit running as
  uid 1000 reads its two lines with their stream and none of lode's own
  records, which are the system's; root finds `user-1000/` and reads them.
* **The viewer**: `lode-viewer` already looks in `user-<uid>/` as root or
  `adm`; a person who is neither needs it to ask `/run/lode/read` as `lode`
  does (`ferrix_lode_core::access` has the packets) -- a change in the
  viewer, told to its agent. The store's format did not change.
* Not yet: `_EXE`; init writing units' lines to the console only at or
  above `MaxLevelConsole=` once lode takes them (§13.2), which waits for the
  gates that read units' lines from the console to read them from lode.
* Kernel lines: 0. init, `svc`, lode and `lode-upload` are ring-3
  programs outside the item.

### journalctl's options, as built (2026-10-23)

The second of the three steps the customer approved after §20's audit.

* **`-F FIELD` and `-N`**: each value of a field, and each field name,
  once, over the records the other options choose (`-b`, `-u`, ...), as
  journalctl prints them, `PRIORITY`, `_BOOT_ID` and `_HOSTNAME` included.
* **`-g` is a regular expression** (`ferrix-lode-core` `regex`, the tree's
  own, no crate): PCRE2's syntax a person writes at a shell -- classes,
  `\d \w \s \b`, `[:alpha:]`, `* + ? {n,m}` and their lazy forms,
  groups, `|`, anchors, `(?i)` -- matched by a Pike VM, so no pattern can
  make a read slow; back-references, look-around and possessive
  quantifiers are refused with where and why (exit 2), never matched
  wrongly. journalctl's smart case: a pattern with no upper-case letter
  ignores case, `--case-sensitive[=yes|no]` says otherwise. `--around` and
  the marks still take text.
* **Output**: `json-pretty` as systemd's JSON printer lays it out (a tab,
  `" : "`, a byte array's items two tabs in), `json-seq`, `json-sse`,
  `short-full` (`Fri 2026-10-23 14:01:02 CEST`: the zone's name from the
  POSIX rule, `UTC`, or the offset), `short-unix`. JSON strings are now
  escaped as systemd escapes them: `\b` and `\f`, and DEL left as it is.
* **`--user-unit UNIT`**: `_SYSTEMD_USER_UNIT`, `USER_UNIT` and the
  coredump and object forms, `.service` when the name has no type.
* **`--disk-usage`**: `Archived and active journals take up 8.0M in the
  file system.`, over every segment and summary the caller may read, sizes
  as systemd's `FORMAT_BYTES` writes them.
* **`--vacuum-size`, `--vacuum-time`, `--vacuum-files`** (root's): the
  oldest sealed segments first, never one being written, each said as
  `Deleted archived journal PATH (SIZE).` and each store directory closed
  with `Vacuuming done, freed SIZE of archived journals from DIR.`, on
  standard error as journalctl's log lines; sizes and spans read as
  systemd's `parse_size` and `parse_sec` read them. A vacuum is recorded in
  the log (`lode-vacuum`), as a deletion is.
* **`--sync`, `--flush`, `--rotate`** ask the service on
  `/run/lode/control` (root's 0600, any peer but root refused) and return
  once it is done; `lode rotate` and `lode delete` use it too, and the
  kernel-log request is kept for an older lode. `--flush` makes
  `/var/log/journal` for lode first when it is missing; lode then takes the
  directory, seals what it was writing in `/run`, moves every sealed segment
  of a class that is not RAM-only over -- compressed on the way, synced,
  then removed from `/run` -- and writes there from then on.
* `test-log` on all three architectures: `-F _STREAM`, `-N`, a regular
  expression with smart case, refused and case-sensitive, `json-pretty`
  and `short-full`; `--sync`, `--flush`, `--rotate` and `--disk-usage`
  answering; `--vacuum-time` removing sealed segments with journalctl's
  lines and its record; and a lode restarted with `/var/log/journal`
  unreachable keeping the log in `/run` until `--flush` moves it. Host
  tests: the regular expressions (a nested quantifier on 5,000 characters
  included), the formats against goldens, sizes and spans.
* `test-log-budget` still restarts lode to move its store: a store moved by
  `--flush` just before the slow load measured 53.6 to 59.1 bytes a byte against
  45.7 for the restart, a cost of btrfs's commits after the move that the
  row does not set out to measure.
* Not yet: `-o short-delta`, `--relinquish-var`, `--user`, `-M`, the
  journald.conf keys. Kernel lines: 0.

### Top talkers and alerts, as built (2026-10-23)

The third of the three steps the customer approved after §20's audit
(§7.4, theme 22): a way for box runs to flag floods.

* **Summaries count sources.** Each sealed segment's summary now also
  holds every source -- the limiter's: unit, else identifier, else
  subsystem, else transport -- with its records and the bytes of their
  messages, in a section after the values that a reader written before it
  ignores (the CRC covers it). The viewer reads them as before.
* **`lode top [-b ...] [-n ROWS] [filters]`**: per source, records, bytes
  and share, most first, for this boot unless `-b` says; a sealed segment
  wholly in the window with no filter is counted from its summary alone,
  the rest from their records. **`lode top --live [SECONDS]`** prints every
  few seconds who wrote what since the last look, per second.
* **Alerts** (`ferrix-lode-core` `alert`): `alert [NAME] [rate > N/DURATION]
  [where EXPR] notify "TEXT"` or `... start UNIT` in any `*.rules` file.
  Records are counted as they arrive, before the limiter, so a flood it
  holds back is still seen; per source, more than N in a window fires, and
  an alert fires at most once a source in 10 minutes. Each firing is a
  record (`MESSAGE_ID` a1e47f1e..., `ALERT`, `ALERT_SOURCE`,
  `ALERT_ACTION`, `N_RECORDS`). `%n`, `%s` and `%c` are the name, the
  source and the count.
* **`notify`** is a line, `alert NAME SOURCE TEXT`, to every reader of
  `/run/lode/alerts` (made by lode as root, 0666: the text is root's).
  notifyd reads it beside devmgr's events and shows a banner keyed by
  alert and source; `lode alerts` prints the lines, for a box run over ssh.
* **`start`** asks init on its control socket as lode's own user; init
  starts a unit for a user other than root only when its file says
  `StartableBy=` that user, by name or number (a new `[Unit]` key in
  `src/lib/init/svc`). init's answer is read without waiting and said in a
  record, a refusal included. Images that run lode need `lode` (uid 91) in
  `/etc/passwd` for `StartableBy=lode`; `StartableBy=91` needs nothing.
* `test-log` on all three architectures: a flood over `rate > 50/s` said
  once to `lode alerts` and in the log; a crit line starting a unit that
  allows it; a unit that does not refused and said; `lode top` listing the
  sources; `--live` showing a source writing now. Host tests: the alert
  rules, rates, the ten-minute quiet and the `%` forms; summaries' sources
  and a summary from before them; notifyd's alert lines.
* Found on the way: `lode delete` leaves the deleted records' kernel
  sequence numbers unsaid, so `verify --kernel` reports them as a gap
  unless a restart's record covers them, as it did in every `test-log` run
  until now. The deletion record should carry the numbers it removed.
* Not yet: `capture` as an alert action; alerts from summaries rather than
  arriving records. Kernel lines: 0.

### Crash records name their process, as built (2026-10-23)

The first of the customer's gap items: the N150's crash records said only
a pid and a pc (`signal pid 27915 ended by signal 5 at 0x0, pc
0x7ffff57601a1: Breakpoint`), because lode read `/proc/<pid>` after the
process was gone and its pid could be reused.

* **The kernel says who it was, while it still is.** When a fault ends a
  process, the Linux personality (`syscall/deliver.rs`, load ring) writes
  a line just before the core's `ended by signal`: `  signal   pid N is
  NAME exe PATH at FILE+OFFSET cmdline ARGS` -- the name, the path it was
  started from as `/proc/<pid>/exe` gives it (` (deleted)` kept), the file
  mapping the program counter is in with the pc's offset into that file
  (`libcef.so+0x1a01a1`, `?` for anonymous memory), and the arguments
  joined by spaces, at most 512 bytes copied (`procfs::crash_identity`).
  Every field has its control bytes as spaces, so a name or path cannot
  split the line. The core passes the faulting pc to the personality's
  `fault_signal` (`trap.rs`, its one call). Kernel lines: 66 added and 7
  changed in the first form (3 of them in the core), plus a bounded
  argument copy (`Process::args_prefix`) after the consultant's review,
  ledger line 3460.
* **lode joins the two lines**: it keeps the identity line per pid and
  gives the crash record that follows `CRASH_COMM`, `CRASH_EXE`,
  `CRASH_MODULE`, `CRASH_CMDLINE` and `CRASH_PROGRAM`. `/proc` stays the
  fallback for a kernel without the line.
* **Privacy.** A command line can carry a secret (a token, a password on
  the command line). It now sits in the kernel log, which only privileged
  readers may read (`dmesg_restrict`), on the serial console (physical
  access), and in lode's system segments (`lode:adm 0640`), as
  systemd-coredump keeps `COREDUMP_CMDLINE` for the same readers. §12.3's
  rule that `_CMDLINE` is not recorded is the customer's to change for
  ordinary records (the name-and-path item); for crashes it is changed
  here, at the customer's order of 2026-10-23.
* `test-log`: `lode fault-stub`, a read of address 0, is recorded with
  `CRASH_EXE=/sbin/lode`, `CRASH_MODULE=/sbin/lode+0x...` and its command
  line ("crash" row). Boots on every architecture show the line for the
  kernel's own fault checks (`pid 258 is mprotect exe /mprotect at ?
  cmdline /mprotect`, AArch64).

### Session programs, names and event kinds, as built (2026-10-23)

The customer's gap items after tonight's Endfield start failures: what a
desktop program, the X server, Steam and Proton said was in files on
tmpfs or nowhere, records named no program, and an error dialog left no
trace.

* **`lode cat --tee FILE --echo`.** `--tee` also writes everything to FILE,
  made afresh as `>` makes it, so a script's log file stays where its
  readers look; `--echo` also to standard output. Without a lode, or when
  it goes away, the lines go to standard output instead and the command's
  output keeps being read: nothing waits, nothing is lost.
* **Scripts**: the desktop's `/etc/yserver.sh` (`yserver`, and
  `/tmp/yserver.log` as before), Steam's `desktop.sh` (`steam`,
  `/tmp/steam.log`), the N150's `ef-player-anv.sh` (`proton`,
  `plog/proton.out`, Proton's exit status kept) pipe into `lode cat`.
* **hyprix's programs** (`exec`, `exec-once`, keybinds, fuzzel's) write to
  `lode cat -t NAME --echo`, NAME the program's file name or a shell
  script's (`sh /etc/yserver.sh` is `yserver`); the echo keeps the console,
  and every gate that reads it, as it was. The echo's copy comes back as
  hyprix's unit's line, and lode keeps the two as one: an echoed stream's
  line (`FERRIX_ECHOED=1`, the stream header's seventh line) waits up to
  2 s for its copy and takes the copy's kernel sequence number
  (`units::Echoes`). `session-streams off` in `events.conf` turns it off.
* **Names on every record with a pid** (`process-names`): `_COMM` and
  `_CMDLINE` from `/proc` when the pid is first seen, while it writes, kept
  10 s after its last record; `_EXE` from `/proc/<pid>/exe` where lode may
  read it, and for a unit's processes from init, which says once a spawn
  which program it runs (`P <pid> <path>` on `/run/lode/units`), since
  lode's user may not read another user's `exe`. `_CMDLINE` is recorded by
  default now (the customer's order; §12.3 said it was not): the same
  privacy note as for crash records applies.
* **App errors and windows** (hyprix, `lode_events.rs`): when a window
  first maps, a window whose class is Windows' dialog class `#32770` or
  whose title or class says error, failed, failure, fatal, exception or
  crash is an `app-error` record (native protocol: `FERRIX_EVENT=app-error`,
  `WINDOW_TITLE`, `WINDOW_CLASS`, `WINDOW_PID`, `WINDOW_COMM`, `WINDOW_EXE`,
  `WINDOW_DIALOG` for a modal or child window, at warning); any other
  window a `window-opened` record when that kind is on (off by default).
  lode takes a screenshot of an app error as of a crash (§19), and desktop
  images ship `Capture=yes`.
* **`events.conf`** (`/etc/ferrix/lode.d/`, a person's
  `~/.config/ferrix/lode/events.conf` over it for their session's kinds):
  `KIND on|off [LEVEL]` for `window-opened` (off), `app-error`,
  `session-streams`, `crash-names`, `process-names`, `capture` (on).
  `lode events` lists them with where each was set; which records of a
  kind are kept is the rules' (`drop where event = window-opened and id =
  waybar`; `event` is `FERRIX_EVENT` in the filter language). hyprix reads
  the files each time it would make an event; **`lode reload`** (root's,
  on the control socket) has lode read its rules, alerts and events again.
* `test-log` ("streams", "names" rows): `--tee` keeps the file and the
  record, an echoed line is one record with its kernel sequence number,
  `lode events` and `lode reload` answer, a stream's record has its
  writer's `_CMDLINE` and a unit's line its program's `_EXE`. Host tests:
  `events.conf`, echoes, hyprix's dialog test, switches and identifiers.
* Not yet: a hang snapshot (`lode snapshot PID`); "not responding" as a
  capture trigger; file sources for the game's `Player.log` and CrashSight;
  the image and command line on lode's boot record; the window's process
  through yserver for X clients (`WINDOW_PID` is yserver's).

### Known issues

* **None open.** The kernel finding step 1 landed with -- `/dev/kmsg`
  handing out a record cut short on Arm SMP, and a write merged into a
  half-written console line -- was fixed in the core by 722824a8d; step
  2's `test-log` then stored 3,000 of 3,000 flood lines on all three
  architectures, and the row is strict again.

## 20. Feature status (2026-10-23)

The customer asked what lode still lacks against everything promised. Each
row: **built**, **partly** or **missing**, the evidence, and what is left
in the plan's points (§17; a point is about a day's work with its gate).
"test-log" is `cargo xtask test-log`, which runs on all three
architectures.

### 20.1 The top ten wishes (§2)

| # | Wish | Status | Evidence | Left |
|---|---|---|---|---:|
| 1 | Writes that cost about what the line costs | partly | append-only, no `mmap`, compressed when sealed (`compress.rs`), field names as indexes; `test-log-budget`: fast load 2.67 (budget 3), slow load 45.7 against 40 | 0 in lode (the slow load is btrfs's `fsync`, a kernel item); 2 for per-source metadata written once (§8.2's SOURCE entries) |
| 2 | Queries fast as the store grows | partly | segment summaries skip whole segments, newest-first `-n` (`lode bench`, `test-log` part F) | 4: field indexes and a sparse time index inside sealed segments (§8.4); the 1 GB / 100 GB benchmark |
| 3 | A real filter language | built | `--where` with and/or/not, globs, `~`; rules, retention, capture and upload use it (`filter.rs`, host tests) | 1: `~` is a substring, not a regular expression |
| 4 | Noise controlled at ingest, never silent | built | limiter, `drop`/`sample`/`level`/`fold`/`limit` rules each said; folding of lines alike but for numbers; `lode top` and `--live`, flood alerts (`test-log` held/folded, "top", "live" and "alerts" rows) | 0 |
| 5 | Retention per service and priority; delete by match | built | retention classes (`retention.conf`), RAM-only classes, vacuum; `lode delete --where` (`test-log` delete row); journalctl's `--vacuum-size/-time/-files` and `--disk-usage` (`test-log` "vacuum" row) | 0 |
| 6 | Every entry attributed; stdout vs stderr | built | sockets: `_PID/_UID/_GID` from `SO_PEERCRED`, `_COMM` and cgroup read from `/proc` when the sender connects; units' lines handed over by init with `_SYSTEMD_UNIT`, `_SYSTEMD_INVOCATION_ID`, `_STREAM`, `_PID`, `_UID`, `_GID` (§18 "Users' own records and units' runs"; `test-log` "stream" row) | 0.5: `_EXE` |
| 7 | Output usable without flags | partly | `-o human` with year, zone and level letter (`?` for none), never truncated; 11 journald formats | 2: `--format` templates; colour on a terminal; a pager |
| 8 | A crash costs at most the unsynced tail | built | per-entry CRC and torn-tail cut, restart without loss (`test-log`: SIGKILL mid-flood, `verify --kernel` "unsaid gaps: none"), sealed segments immutable | 1: a full or failing disk degrading to RAM (§11.4) is untested |
| 9 | Store-and-forward, syslog in and out | partly | `lode-upload` with cursor, back-off, TLS; `lode-receive` (`test-log-remote`); `/dev/log` in | 3: RFC 5424 out over TCP/TLS, OTLP out; 2: `lode-receive` on Ferrix and a merged view (§10.3, §10.4) |
| 10 | The right slice by default | built | `-b`, `--list-boots`, persistent by default, `-u` globs, `-u UNIT@last`/`@-1`, `-s` sessions, marks, users' own segments (`test-log` "runs" and "user" rows) | 0 |

### 20.2 The themes after the top ten (USER-WISHES.md)

| Theme | Status | Evidence | Left |
|---|---|---|---:|
| 11 Previous boots, persistent by default | built | `/var/log/journal` when writable, `-b -1`, `--list-boots` | 0 |
| 12 This run of a service | built | `INVOCATION_ID` from init, `-u UNIT@last`, `svc status`/`svc log` show the run (`test-log` "runs" row) | 0 |
| 13 Full-text search | built | `-g` a regular expression in linear time with journalctl's smart case and `--case-sensitive` (`regex.rs`; `test-log` "jctl" row), `--around`, marks | 0 |
| 14 Unprivileged access | built | users' segments in `user-<uid>/`, handed to their user by lode (`/run/lode/read`; `test-log` "user" row) | 0; the viewer asking for them is the viewer agent's |
| 15 Rate limiting that surprises | built | every hold is said (`N_SUPPRESSED`, `N_DROPPED`, `REPEATED`); the kernel's lines never held | 0 |
| 16 Early boot and kernel messages | built | `/dev/kmsg` read from its first record, `_KERNEL_SEQNUM`, `verify --kernel` | 0 |
| 17 Structured logging from programs | partly | native protocol, `lode cat --journal`; tracing lines read for crate and level | 1: JSON lines on stdout turned into fields (§5) |
| 18 Logging never stalls the system | partly | `logger` returns at once with no lode (`test-log` "no lode"); compression a frame at a time | 1: a test that a stalled disk never blocks a writer |
| 19 The collector's own security | built | lode in a sealed job, its own user (`test-log`: uid 91, cgroup, move refused); uploader uid 92 | 1: the uploader as a native service in a sealed job |
| 20 A viewer | partly | `lode-viewer` (the viewer agent's), on Ferrix and on nazuna, over `lode-receive`'s stores | the viewer agent's |
| 21 Runtime log levels per service | missing | no `INTEREST` or `lode level` | 3 (with drivers, plan step 7) |
| 22 Alerts and actions | built | `alert` rules with a rate, `notify` to `/run/lode/alerts` (notifyd's banners, `lode alerts`), `start` through init with `StartableBy=` (`test-log` "alerts" row) | 0 |
| 23 Privacy and redaction | missing | no `redact` rule; `_CMDLINE` is not recorded | 1 |
| 24 Multi-line messages | partly | a value with a newline is stored whole (native protocol, export) | 1: `LogMultiline=` for units' stdout (§5) |
| 25 Containers and namespaces | missing | no `_NAMESPACE` | 1 |
| 26 Trace and request correlation | partly | `TRACE_ID`/`SPAN_ID` kept as sent | 1: filters and the viewer joining on them |

### 20.3 The design's sections

| § | Promise | Status | Evidence | Left |
|---|---|---|---|---:|
| 5 | Sources: kmsg, `/dev/log`, native socket, stdout streams | built | `front.rs`; `test-log` "front: 1 1 1 1 0" | 0 |
| 5 | Drivers' `logproto` with `INTEREST`; `devmgr` hands ends | missing | plan step 7 | 6 |
| 5 | pstore: the last boot's panic | missing | plan step 9 | 3 |
| 5 | File sources (ad-hoc logs moved over, §13.1) | missing | plan step 9 | 2 |
| 6 | One filter language everywhere | built | §20.1 #3 | 0 |
| 7.1-7.2 | Limiter and ingest rules | built | `limit.rs`, `test-log` | 0 |
| 7.3 | Bounded memory | partly | per-source state is bounded by sources, not capped | 1 |
| 7.4 | `lode top`, alerts | built | §18 "Top talkers and alerts" | 0 |
| 8.1 | RAM and disk tiers, `/var` taken when it comes | partly | both tiers; `lode --flush` moves the RAM tier to `/var/log/journal` and writes there from then on (`test-log` "flush" row) | 0.5: `--relinquish-var`, and a `lode-flush.service` that asks at boot |
| 8.2 | Format, CRC, torn tail, sealing, compression | built | `segment.rs`, `compress.rs`, `test-log` | 2: SOURCE entries (§20.1 #1) |
| 8.3 | Write budget row | built | `test-log-budget` | 0 (btrfs's part is the kernel's) |
| 8.4 | Summaries, field indexes, time index | partly | summaries | 4 |
| 8.5 | Retention classes, vacuum, delete | built | `retention.rs`, `test-log` | 0 |
| 8.7 | Configuration: `WriteIntervalMs=`, `SyncIntervalSec=`, `SegmentMaxSize=` | missing | fixed in `serve.rs` | 1 |
| 9.1-9.4 | The reader and its defaults | partly | §20.4 | see §20.4 |
| 9.5 | `dmesg` | built | `dmesg` reads `/dev/kmsg` (`src/user/system/linux/dmesg`) | 0 |
| 9.6 | `svc log`, `svc status` through lode | built | `svc log` is `lode -u UNIT@last`, `svc status` ends with the run's last lines, init's ring when lode cannot answer (`test-log` "runs" row) | 0 |
| 9.7 | Benchmark | partly | `lode bench` at 200,000 records | 1 |
| 10.1 | `box-log` | built | `cargo xtask box-log` | 0 |
| 10.2 | Continuous forwarding | partly | export and JSON over HTTP(S), TLS 1.3, client certificates (`test-log-remote`) | 3: RFC 5424, OTLP |
| 10.3 | Receiving | partly | `lode-receive` on Linux | 2: on Ferrix, RFC 3164/5424 in |
| 10.4 | A merged view | missing | `-m` accepted and ignored | 2 |
| 11 | Isolation, restart without loss, start order | built | `test-log` | 0 |
| 12.1 | Who may read what | built | root and `adm` everything, a user their own (theme 14) | 0 |
| 12.2 | Sessions and authd | built | `_SESSION_ID`, `-s`, `/run/lode/seat` (`test-log`, `test-compositor`) | 0 |
| 12.3 | Privacy | partly | `_CMDLINE` off | 1 (theme 23) |
| 13.1 | Ad-hoc logs moved onto lode | missing | `/var/log/audit`, files of programs | 2 |
| 13.2 | The console | partly | lode reads what the console carried; `MaxLevelConsole=` in init missing | 1 |
| 14 | The kernel: zero lines beyond `/dev/kmsg` and the devfs link | built | every landing: 0 kernel lines | 0 |
| 19 | Crash screenshots | built | provider protocol, hyprix provider, `test-log` capture row | 0 |

### 20.4 What journalctl and journald users rely on

| Feature | Status | Evidence or note | Left |
|---|---|---|---:|
| `-o short, short-iso, short-iso-precise, short-precise, short-monotonic, with-unit, cat, json, export, verbose` | built | `format.rs` | 0 |
| `-o json-pretty, json-seq, json-sse, short-full, short-unix` | built | `format.rs`, host goldens; `test-log` "jctl" row | 0 |
| `-o short-delta` | missing | needs the record before | 0.25 |
| `--output-fields` | missing | | 0.5 |
| `-u` (globs, `name` is `name.service`), `-t`, `-T`, `-k`, `-p` (ranges, `unspecified`), `-g`, `-S`/`-U`, `-n`, `-r`, `-f`, `-e`, `-b`, `--list-boots`, `FIELD=VALUE` with `+`, `--after-cursor`, `--show-cursor`, `-D`, `--utc` | built | `cli.rs`; `test-log` | 0 |
| `--user-unit` | built | `_SYSTEMD_USER_UNIT`, `USER_UNIT` and their coredump and object forms, as journalctl's | 0 |
| `--user`, `-M/--machine`, `--file`, `--root`, `--image` | missing | | 1 |
| `--cursor-file`, `--since` from a cursor's time | missing | | 0.5 |
| `-F/--field`, `-N/--fields` (a field's values) | built | over the records the other options choose (`test-log` "jctl" row) | 0 |
| `-x`, `--catalog`, `--list-catalog` | missing | no message catalog | 2 |
| `-g` as a PCRE2 regular expression, `--case-sensitive` | built | PCRE2's syntax a person writes at a shell; back-references and look-around refused with the reason | 0 |
| `--facility` | missing | `--where 'SYSLOG_FACILITY = 4'` works | 0.5 |
| `--disk-usage`, `--vacuum-size/-time/-files` | built | journalctl's sentences and sizes (`FORMAT_BYTES`); a vacuum is recorded in the log | 0 |
| `--rotate`, `--verify` | built | `lode rotate`, `lode verify` | 0 |
| `--flush`, `--sync`, `--rotate` | built | the control socket `/run/lode/control` (`test-log` "jctl admin" and "flush" rows) | 0 |
| `--relinquish-var` | missing | | 0.25 |
| `--setup-keys`, `Seal=` (forward-secure sealing) | missing | the CRC catches damage, not tampering | 3 |
| `--namespace`, `LogNamespace=`, `--list-namespaces` | missing | | 2 |
| `--header`, `--new-id128` | missing | | 0.5 |
| `--no-pager`, `-a`, `-l`, `-q`, `-m` | accepted, no effect | there is no pager, nothing is truncated; `-m` merges nothing yet | (in 10.4) |
| journald.conf `Storage=`, `SystemMaxUse=`, `MaxRetentionSec=`, `Compress=` | partly | `retention.conf` classes say the same differently; compression always on | 0.5: read journald.conf's keys |
| `RateLimitIntervalSec=`, `RateLimitBurst=` | partly | the limiter's bursts and rules (`limit`), not these keys | 0.5 |
| `MaxLevelStore=`, `MaxLevelConsole=`, `ForwardToSyslog/KMsg/Console/Wall=` | missing | `level` rules do the first | 1 |
| `SplitMode=uid` | built, always on | users' own segments (theme 14) | 0 |
| `sd_journal_print`, `/dev/log`, stdout streams, `systemd-cat` | built | `test-log` front row; `lode cat` | 0 |
| `sd-journal` / libsystemd reading the files | missing, by design | §8.6: the format is lode's; `-o export` is the bridge | 0 |
| `systemd-journal-upload` / `-remote` interoperability | partly | `lode-upload` posts the same export; a real journal-remote is not tested (none on nazuna) | 1 |

### 20.5 The customer's asks today

| Ask | Status | Evidence |
|---|---|---|
| Every record has a level; unspecified is one of its own | built | §4.1; `test-log` "levels" row; `lode levels` |
| Crash screenshots | built | §19; `test-log` capture row; hyprix provider |
| Sessions | built | §19.1; `test-log` session row |
| The viewer | built (the viewer agent's) | `lode-viewer`, on nazuna over `lode-receive` |
| TLS forwarding to nazuna | built | `lode-upload`, `lode-receive`, `test-log-remote` on all three architectures |
| The box's field quality | built | §18 "Field quality" |
| Box run of TLS forwarding | waiting | the PO's box run |
