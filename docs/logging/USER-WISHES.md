# What people have wanted from journald and journalctl

Input for the central log manager's design (`docs/LOGGING.md`, written on its
own branch). The customer asked on 2026-10-23 to look up "what people always
wanted from journalctl as we write a fully fledged central log manager". This
is that look-up: what users have asked for, complained about, or praised in a
competitor, with the evidence, how common it is, and what we should do about
it.

Collected 2026-10-23, read-only (nothing was posted anywhere).

## How the evidence was gathered, and its limits

- **systemd's GitHub issues**: the search API over `repo:systemd/systemd`,
  sorted by reactions and by comments, for the `journal` and `journal-remote`
  labels, for `journalctl` and `journald` in titles, and for one query per
  theme. "r=" is the total reaction count (nearly all +1), "c=" the comment
  count, both as of 2026-10-23. The top issues were read, with their last
  comments, to learn whether and how each was resolved.
- **Stack Exchange**: the API, top-voted questions matching `journalctl` on
  Unix & Linux, Server Fault, Ask Ubuntu, Super User and Stack Overflow.
  Votes and views say what people *need to do* and cannot find out how —
  a different signal from issues, which say what they want changed.
- **Hacker News**: the Algolia API, top stories on journald and their
  top-level comments.
- **LWN, distribution forums and mailing lists, vendor knowledge bases, and
  the documentation of lnav, Loki, macOS unified logging and Windows Event
  Viewer**, through web search.
- **Reddit could not be read.** Its API and `old.reddit.com` refuse this
  host's requests, and web search returned no Reddit pages. No claim below
  rests on Reddit; Hacker News and Stack Exchange stand in for it. The Lemmy and
  forum threads that web search did return say the same things (binary format,
  speed, cannot exclude noisy units, truncation and paging).
- A reaction count is a lower bound for demand: most people who hit a
  problem search, find a workaround and leave. Stack Exchange view counts
  are the better gauge of how many people hit something.

---

## Top ten: what the new log manager must have

1. **Writes that cost about what the log line costs.** journald's
   write amplification is the loudest current complaint: about 50 IOPS for
   2 lines a second, tens of kilobytes written per line (#40262, r=71, open;
   HN front page 2026-08, 264 points, 223 comments). Append-only, batched,
   compressed writes; measure bytes written per line in the gate.
2. **Queries that stay fast as the store grows.** `systemctl status` taking
   over 10 s on a 4 GB journal has been open since 2016 (#2460, r=42, c=70);
   "Systemd logs are too large and slow" is a top Ask Ubuntu question (110
   votes). The last-N lines of one unit must cost the same on a 100 MB and a
   100 GB store.
3. **A real filter language: NOT, OR, grouping, globs and regexes on any
   field.** The single most up-voted journal issue is "add NOT operator to
   journal matching" (#2720, r=131, open since 2016).
4. **Control of noisy sources at ingest — never silently.** "Filter mechanism
   for logs in journald" is the most-discussed journal issue (#6432, c=164,
   r=89; answered only in 2023 by `LogFilterPatterns=`). Drop, sample, or
   fold repeats ("repeated N times") per source, and always count what was
   dropped where a reader will see it.
5. **Retention and quotas per service and per priority, and deleting by
   match.** Per-service retention (#9519, r=58, open since 2018); "How to
   clear journalctl" (Unix & Linux, 706 votes, 1.18 M views), "clear entries
   for a specific unit only" (48 + 88 votes). Keep errors for a year and
   debug for a day; keep a quiet critical service's few lines from being
   rotated out by a chatty one.
6. **Every entry attributed correctly, including a dying process's last
   words.** journald loses the unit of messages from a process that has
   already exited (#2913, r=31, c=38, open since 2016) — the messages that
   explain a crash. Also tell stdout from stderr (#5019, r=42, open). Ferrix
   can attach the sender's identity in the kernel at send time, so this is
   cheap for us and impossible for journald.
7. **Output people can use without flags**: no truncation, a year and a
   time zone, the priority visible, colours readable on a light terminal,
   user-defined formats. Custom output formats (#3449, r=41, open); "how to
   prevent text from truncating" (169 votes); "parse its own timestamps"
   (#15899, r=28) and ISO 8601 `--since` (#5194, r=24).
8. **Survives a crash or power loss without losing more than the unsynced
   tail, and never takes logging down with it.** Corruption after unclean
   shutdown is the standing objection to the binary format (LWN 2014
   threads, Arch and Fedora lists, CoreOS: "journald corrupts its files and
   completely stops logging"). A torn tail is truncated, not a file renamed
   away; readers never loop on bad pointers.
9. **Simple, robust shipping to a central store, and syslog in and out.**
   journal-upload quits when the server is away (#2877, #19426), the remote
   ignores rotation (#5242), client certificates are not verified (#4092);
   RFC 5424 on `/dev/log` (#19251, c=77) and forwarding (#32852, #15200) are
   still open. Store-and-forward with resume from a cursor, TLS done right,
   RFC 5424 and JSON, and a merged, time-ordered view of many hosts.
10. **The right slice by default: this run of the service, previous boots,
    and your own logs without root.** "Logs since the unit restarted"
    (#1942, r=38; Server Fault, 55 votes) arrived only in systemd 257 as
    `--invocation`; previous boots (Unix & Linux, 131 votes) need persistence
    that distributions long left off; users reading their own logs (#2744,
    r=22; Server Fault, 60 votes) is still awkward.

---

## The themes, ranked

Ranked by the weight of the evidence: reactions and comments on issues,
votes and views on questions, and how often the same complaint recurs in
independent places. "How common" is the judgement that ranking rests on.

### 1. Write cost: I/O amplification and on-disk size

**The wish.** Logging two lines a second should not cost dozens of IOPS, and
a log store should not be as large as its own JSON export.

**Evidence.**
- [#40262](https://github.com/systemd/systemd/issues/40262) "Excessive IO
  caused by systemd-journald" — 2026-01-03, open, r=71, c=25. A VM writing
  2 lines a second does about 50 IOPS. A 2026-08-03 analysis (+66 on the
  comment) shows journald writes every line through its shared mapping at
  once; `SyncIntervalSec` delays only the fsync, not the write. Kernel
  developers in the thread (large-folio dirtying) and a database author
  ("the design was wrong. It should not have used mmapped writes. pwrite
  would have been far better", +35) agree on the cause.
- [Hacker News, 2026-08-13](https://news.ycombinator.com/item?id=49290215)
  "Single log line is 49KB+ (ext4) / 110KB+ (btrfs) of systemd-journald disk
  writes" — 264 points, 223 comments. Recurring replies: "set
  `Storage=volatile`", "install a proper syslog daemon", "visibly slower than
  grepping gzipped text logs".
- [#15292](https://github.com/systemd/systemd/issues/15292) "excessive and
  hugely abnormal disk IO" — 2020, closed as measured wrongly, r=27, c=30;
  re-reported as #40262.
- [#5285](https://github.com/systemd/systemd/issues/5285) "request for
  optimizing journal file size" — r=36: the journal is about the size of
  its own JSON export, with every field name and boot ID repeated per entry.
- [#31358](https://github.com/systemd/systemd/issues/31358) "compress
  archived journald logs" — 2024, open, r=18.
- [#12579](https://github.com/systemd/systemd/issues/12579) zstd dictionary
  compression — open, r=3.

**How common.** Very. The highest-reacted open journal issue that is a bug
rather than a feature, a front-page HN thread this year, and the usual
workaround (volatile storage) gives up persistence altogether.

**What we should do.** Write append-only with `pwrite`-style sequential
writes into a buffer, flushed on a size or time bound and on every entry at
or above a configured priority (so a crash keeps the important lines).
Store per-stream metadata once (boot ID, unit, executable, cgroup, uid) and
refer to it from each entry instead of repeating it. Compress sealed
segments with zstd (and a per-stream dictionary when it pays). Make "bytes
written per logged byte" a measured number with a budget, checked in a
gate, so the figure cannot drift back.

### 2. Query speed on a large store

**The wish.** `systemctl status foo`, `journalctl -u foo -n 50` and
`journalctl -b -1` should answer at once however much history is kept.

**Evidence.**
- [#2460](https://github.com/systemd/systemd/issues/2460) "Showing status of
  service via systemctl is slow (>10s) if disk journal is used" — 2016,
  open, r=42, c=70.
- [Ask Ubuntu 1012912](https://askubuntu.com/questions/1012912) "Systemd
  logs (`journalctl`) are too large and slow" — 110 votes, 133 k views.
- [sosreport #246](https://github.com/sosreport/sos/issues/246) — 2014,
  `journalctl --verify` five minutes on a 4.1 GB journal; a maintainer:
  "'journalctl is slow' seems to be a rather common meme".
- [#6238](https://github.com/systemd/systemd/issues/6238) "journalctl --unit
  searches logs on tab completion" — open, r=11, c=25: completion reads the
  whole journal.
- [#20445](https://github.com/systemd/systemd/issues/20445) "`journalctl
  --grep=` missing entries found by `journalctl | grep`" — c=22.
- HN 2026-08 thread: "The indexing system it uses is slow … you will get
  better performance out of a modern grep like ag or rg."
- Older: [freedesktop #78337](https://bugs.freedesktop.org/show_bug.cgi?id=78337)
  (start-up time grows with the number of boots),
  [Red Hat #1036720](https://bugzilla.redhat.com/show_bug.cgi?id=1036720),
  btrfs fragmentation of journal files.

**How common.** Very; it is the complaint people mean when they say "binary
logs".

**What we should do.** Index what queries filter on (time, boot,
unit/invocation, priority, identifier, pid, uid, transport) with a sparse
time index per segment and a per-segment summary (which units, priority
range, time range) so a query skips whole segments without opening them.
Read newest-first for "last N" and status views. Keep a list of known
values per field (units, identifiers) so completion never scans entries.
`--grep` must find exactly what `| grep` finds (theme 13 below is how to make
it fast). Publish a benchmark: last 50 lines of one unit, on stores of
1 GB and 100 GB, cold and warm cache.

### 3. The filter language: NOT, OR, grouping, patterns

**The wish.** Exclude a noisy unit; match several units by pattern; combine
conditions freely.

**Evidence.**
- [#2720](https://github.com/systemd/systemd/issues/2720) "RFE: add NOT
  operator to journal matching" — 2016, open, **r=131** (the most-reacted
  journal issue), c=22. "having AND, OR, NOT and grouping … would be
  awesome."
- [#20457](https://github.com/systemd/systemd/issues/20457) patterns for
  `journalctl -t` — open, r=8.
- [Super User 1198464](https://superuser.com/questions/1198464) "How to
  exclude journalctl entries using a filter?" — 26 votes, 39 k views.
- [Server Fault 754953](https://serverfault.com/questions/754953) "how to
  filter by message?" — 24 votes, 84 k views;
  [Server Fault 721963](https://serverfault.com/questions/721963) "How do you
  use journalctl patterns" — 60 votes, 183 k views.
- [HN 2021](https://news.ycombinator.com/item?id=27258798) "Exclude lines in
  less (or journalctl)"; blog and Lemmy comments: "fall back to `grep -v`".
- Praise elsewhere: LogQL's label selector then line-filter pipeline
  (`|=`, `!=`, `|~`, `!~`) in Grafana Loki; macOS `log show --predicate`
  (NSPredicate with `AND`/`OR`/`NOT`, `CONTAINS`, `BEGINSWITH`); Windows Event
  Viewer's saved Custom Views across logs; lnav's SQL over logs.

**How common.** Very; and the one open request with the most votes.

**What we should do.** One small expression language used everywhere (CLI,
API, ingest filters, retention rules, alert rules): field comparisons,
`and`/`or`/`not`, parentheses, globs and regexes on any field, time ranges,
priority ranges. Keep journalctl's `FIELD=value` arguments and `-u`, `-t`,
`-p` as sugar that compiles to it, with globs allowed (`-u 'nginx*'`,
`-u '!noisy.service'`). Allow saving a query under a name. Optionally a
read-only SQL view for analysis, as lnav does — after the core language,
not instead of it.

### 4. Noise at the source: ingest-time filtering, sampling, repeat folding

**The wish.** A daemon that spams the log should not push everything else
out or fill the disk, and the user should be able to stop storing its known
noise without losing its real errors.

**Evidence.**
- [#6432](https://github.com/systemd/systemd/issues/6432) "Filter mechanism
  for logs in journald" — 2017–2023, **c=164** (the most-commented journal
  issue), r=89. Closed by `LogFilterPatterns=` (systemd 253). Requests in
  the thread for classic "last message repeated N times" folding.
- Follow-ups: [#31113](https://github.com/systemd/systemd/issues/31113)
  `LogFilterPatterns=` not for user services (open, r=5);
  [#27790](https://github.com/systemd/systemd/issues/27790) a filter for
  kernel messages; [#7557](https://github.com/systemd/systemd/issues/7557)
  process logs externally before storing (open, r=8).
- [#34753](https://github.com/systemd/systemd/issues/34753) "Send
  notification if an app is spamming the journal" — 2024, open, c=15: Chrome
  wrote 120 lines a second; over a million entries before anyone noticed.
- HN 2026-08: "you practically cannot filter journald … a bug in amdgpu
  after resume … took a while to get that filtered"; "Open a file picker
  and kio will … spam tens or hundreds of thousands of entries".
- [#9328](https://github.com/systemd/systemd/issues/9328) suppress
  start/stop messages of timers — c=22;
  [#4919](https://github.com/systemd/systemd/issues/4919) frequent timer
  status lines — r=9.

**How common.** Very, and growing on desktops, where every application's
stderr lands in the journal.

**What we should do.** Per-source ingest rules in the filter language
(drop, sample 1-in-N, keep only priority ≤ X, fold identical repeats within
a window into one entry with a count). Every rule keeps a counter of what it
dropped, visible in status and as a periodic summary entry, so filtering is
never silent. Report the top talkers (lines and bytes per source per hour)
as a built-in query, and optionally notify when a source crosses a rate.

### 5. Retention, quotas and deletion per service and per priority

**The wish.** Different lifetimes for different logs; delete one unit's
logs; keep important and rare lines from being rotated out by chatty
sources; keep debug in RAM and errors on disk.

**Evidence.**
- [#9519](https://github.com/systemd/systemd/issues/9519) per-service
  retention when vacuuming — 2018, open, r=58 (on systemd's TODO for years:
  "allow per-priority and per-service retention times").
- [#3045](https://github.com/systemd/systemd/issues/3045) per-unit journal
  limits — r=12; [#4751](https://github.com/systemd/systemd/issues/4751),
  [#10230](https://github.com/systemd/systemd/issues/10230) (per-unit
  settings).
- [#14588](https://github.com/systemd/systemd/issues/14588) volatile *and*
  persistent with different levels — open, r=16: a fragile eMMC wants only
  errors on flash.
- [Unix & Linux 139513](https://unix.stackexchange.com/questions/139513)
  "How to clear journalctl" — **706 votes, 1.18 M views**, the top journalctl
  question anywhere on Stack Exchange.
- [Unix & Linux 272662](https://unix.stackexchange.com/questions/272662)
  "clear journalctl entries for a specific unit only" — 48 votes;
  [Stack Overflow 36718019](https://stackoverflow.com/questions/36718019)
  "remove logs of a specific unit" — 88 votes.
- [#18353](https://github.com/systemd/systemd/issues/18353) `MaxUse` and
  `MaxFileSize` use different metrics; [#27401](https://github.com/systemd/systemd/issues/27401)
  `--vacuum-size=0` does nothing (c=9).
- [#20673](https://github.com/systemd/systemd/issues/20673) "journal, GDPR,
  PII and data deletion requests" — open.
- Namespaces (`LogNamespace=`, systemd 245, per
  [LWN](https://lwn.net/Articles/814068/)) give per-namespace limits, at the
  price of global ordering — the workaround people are pointed to.

**How common.** Very. The view counts dwarf every other theme: managing
disk use is what most people come to journalctl for.

**What we should do.** Retention rules in the filter language
(`unit=nginx.service and priority>=info: 14d, 2GiB`; `priority<=err: 1y`),
evaluated per segment at seal time — segments are already per stream or
per class, so expiring them is a file deletion, not a rewrite. Tiers:
RAM ring for everything, disk for what a rule says. Deleting by match
(one unit, one user, a time range) as a supported, logged operation.
One unit (bytes) across all size settings. `logctl disk-usage` broken
down by source.

### 6. Attribution: the right unit, the last words, stdout vs stderr

**The wish.** Every line names the unit, process and stream that wrote it,
even when the process has just died.

**Evidence.**
- [#2913](https://github.com/systemd/systemd/issues/2913) "journald is
  unable to attribute messages incoming from processes that exited to their
  cgroup, due to /proc vs SCM_CREDS race" — 2016, open, r=31, c=38, still
  active in 2025–2026. On HN in 2022 a commenter called it the "journald
  drops the most important error messages" issue
  ([HN](https://news.ycombinator.com/item?id=31797073)): the last lines of a
  crashing service lose `_SYSTEMD_UNIT`, so `-u` does not show them.
- [#5019](https://github.com/systemd/systemd/issues/5019) "journald should
  distinguish service log msgs written to stdout and stderr" — 2017, open,
  r=42.
- [#30886](https://github.com/systemd/systemd/issues/30886) incorrect cgroup
  metadata from systemd-executor — c=23;
  [#1347](https://github.com/systemd/systemd/issues/1347) `_SYSTEMD_UNIT`
  sometimes missing.

**How common.** Common in effect, rarely recognised: people see "no
entries" for a crashed unit and do not know why.

**What we should do.** Take identity at send time, not by reading `/proc`
afterwards: the kernel attaches pid, uid, cgroup/unit and an executable
identity to each message (socket credentials extended with the cgroup, or
the IPC channel's sender identity), so a message from a process that has
since exited is attributed as well as any other. Capture stdout and stderr
on separate streams and record which. Trusted fields are set by the
manager; a client cannot forge them.

### 7. Output: truncation, pager, formats, colours, the priority

**The wish.** Readable output by default; custom formats without `jq`.

**Evidence.**
- [#3449](https://github.com/systemd/systemd/issues/3449) "journalctl: add
  custom output formats" — 2016, open, r=41.
- [Unix & Linux 229188](https://unix.stackexchange.com/questions/229188)
  "how to prevent text from truncating in terminal" — 169 votes, 216 k
  views; Fedora devel list complaint about truncation and auto-paging.
- [Unix & Linux 422213](https://unix.stackexchange.com/questions/422213)
  "see the latest x lines from systemctl service log" — 141 votes, 462 k
  views; [654561](https://unix.stackexchange.com/questions/654561) "go to
  the end of the file in journalctl" — 55 votes, 143 k views (`-e` is not
  the default).
- [Server Fault 805775](https://serverfault.com/questions/805775) "how to
  display colors in its output?" — 40 votes;
  [#13561](https://github.com/systemd/systemd/issues/13561) yellow
  unreadable on a light terminal (c=19);
  [#14495](https://github.com/systemd/systemd/issues/14495) audit lines hard
  to see in VT colours (open, c=39).
- [#39281](https://github.com/systemd/systemd/issues/39281) put the priority
  in short output; [#2342](https://github.com/systemd/systemd/issues/2342)
  make the hostname optional (r=7, c=12).
- [#43718](https://github.com/systemd/systemd/issues/43718) less indentation
  for multi-line messages.
- `journalctl -xe` as the stock hint after a failed start, which mostly
  repeats what `systemctl` printed; upstream changed it to `-xeu <unit>`
  ([commit](https://git.alternativebit.fr/picnoir/Systemd/commit/8c62ad19290ff9239f5c399173eacb676169922b)).

**How common.** Very common in questions (hundreds of thousands of views),
modest in issues — people learn the flags rather than file a bug.

**What we should do.** Stay byte-compatible for journalctl's named formats
(scripts parse them) but add `--format '{time:iso} {unit} {priority} {message}'`
templates over any field. Defaults: wrap rather than truncate in the pager
and never truncate when piped; open at the end; show the priority (letter
or colour) in short output; colours from a palette tested on light and dark
backgrounds and overridable. A failed-unit hint that prints the failed
invocation's own lines instead of naming a command.

### 8. Time: parsing, display, ordering

**The wish.** Accept the timestamps it prints; show a year and a zone; put
entries from several boots or hosts in the right order.

**Evidence.**
- [#15899](https://github.com/systemd/systemd/issues/15899) "journalctl
  should be able to parse its own timestamps" — r=28.
- [#5194](https://github.com/systemd/systemd/issues/5194) "`--since` cannot
  parse ISO8601" — r=24, c=20.
- [#6589](https://github.com/systemd/systemd/issues/6589) store time zones in
  journal files — open, r=4.
- lnav's README: the default journalctl output omits the year, so logs that
  span years need `-o short-iso`.
- Ordering: [#662](https://github.com/systemd/systemd/issues/662) order lines
  from boots without a useful wall clock (c=58);
  [#27422](https://github.com/systemd/systemd/issues/27422) `--boot -1 -3 -5`
  show the same messages (c=24); [#34169](https://github.com/systemd/systemd/issues/34169)
  `--merge` does not interleave machines (c=29);
  [#8979](https://github.com/systemd/systemd/issues/8979) (c=15);
  [#10020](https://github.com/systemd/systemd/issues/10020) reorder by
  `_SOURCE_REALTIME_TIMESTAMP`; [#5345](https://github.com/systemd/systemd/issues/5345),
  [#37272](https://github.com/systemd/systemd/issues/37272) lost or
  misordered entries on time jumps; [#28746](https://github.com/systemd/systemd/issues/28746)
  `--since/--until` misses lines (c=13).
- [#24641](https://github.com/systemd/systemd/issues/24641) `dmesg -d`
  (time deltas) for journalctl — c=19.

**How common.** Common; ordering bugs are a long tail of separate reports.

**What we should do.** Every entry carries a monotonic time and boot ID,
the wall-clock time and the UTC offset in force when it was written.
Every printed timestamp form parses back as `--since`; ISO 8601 and RFC 3339
throughout. Ordering is by (boot, monotonic) within a boot and by wall
clock across boots and hosts, with a `--merge` that interleaves. Show the
year by default; `--utc` and a display zone option; a delta column.

### 9. Crash and corruption robustness

**The wish.** An unclean shutdown must not cost more than the last moment,
and must not stop logging.

**Evidence.**
- LWN's 2014 systemd debate threads
  ([621301](https://lwn.net/Articles/621301),
  [621334](https://lwn.net/Articles/621334)): journald rotates a corrupt file
  away and readers skip damage; commenters report losing the lines just
  before a crash and one an endless loop on backward pointers.
- [CoreOS users, "Journald corrupts its files and completely stops logging
  messages"](https://groups.google.com/g/coreos-user/c/BhcHjYcXqho) — fixed
  by `SIGUSR2`.
- [#24150](https://github.com/systemd/systemd/issues/24150) corrupted user
  journal files (c=33); [#24116](https://github.com/systemd/systemd/issues/24116)
  `--verify` FAIL (Bad message);
  [#23651](https://github.com/systemd/systemd/issues/23651)
  remote journal "corrupted or uncleanly shut down";
  [Red Hat solution 2649461](https://access.redhat.com/solutions/2649461)
  (`--verify` reports Bad message); [Red Hat #1508984](https://bugzilla.redhat.com/show_bug.cgi?id=1508984);
  Arch and Fedora list reports 2013–2014.
- [HN "Ask HN: Why does systemd insist on using binary logs?"](https://news.ycombinator.com/item?id=34733549)
  and the 2026 HN thread ("prone to corruption on unclean reboots").

**How common.** Commonly cited, less commonly hit today than in 2014; it
is the root of the "binary logs" distrust and so matters for adoption out
of proportion to its rate.

**What we should do.** A record format with per-record length and checksum
so a reader stops cleanly at a torn tail and recovery truncates to the last
good record; never rewrite earlier bytes of an active segment (no
in-place linked lists across the file). Sealed segments are immutable.
A `verify` that reports per segment and a `recover` that copies out what
is readable. A full or broken store never blocks writers (theme 18) and
says so loudly once.

### 10. Remote collection and forwarding

**The wish.** Shipping logs to a central host should be one setting that
survives outages, and syslog should work in both directions.

**Evidence.**
- journal-remote / upload: [#5242](https://github.com/systemd/systemd/issues/5242)
  remote ignores rotation rules (r=12, c=21);
  [#1387](https://github.com/systemd/systemd/issues/1387) several VMs
  uploading to one remote (open, r=8);
  [#2877](https://github.com/systemd/systemd/issues/2877) upload quits when the
  remote is down (open); [#19426](https://github.com/systemd/systemd/issues/19426)
  same (open); [#4092](https://github.com/systemd/systemd/issues/4092) remote
  neither requires nor verifies client certificates (open, c=13);
  [#5744](https://github.com/systemd/systemd/issues/5744) upload errors (c=18);
  [#39166](https://github.com/systemd/systemd/issues/39166) "Buffer space is
  too small" (2025, open); [#5748](https://github.com/systemd/systemd/issues/5748)
  one connection at a time; [#13445](https://github.com/systemd/systemd/issues/13445)
  JSON for upload; [#2910](https://github.com/systemd/systemd/issues/2910)
  SSH transport; [Fedora discussion](https://discussion.fedoraproject.org/t/can-i-use-freeipa-certificates-for-systemd-journal-remote-and-systemd-journal-upload/126775)
  on certificates.
- [#4793](https://github.com/systemd/systemd/issues/4793) `journalctl -H`
  for a remote host — open, r=12.
- Syslog: [#19251](https://github.com/systemd/systemd/issues/19251) RFC 5424
  on `/dev/log` — open, c=77; [#15200](https://github.com/systemd/systemd/issues/15200)
  listen on UDP 514 — open, r=11; [#32852](https://github.com/systemd/systemd/issues/32852)
  RFC 5424 forwarding — open; [#7170](https://github.com/systemd/systemd/issues/7170)
  forward to a remote syslog server.
- HN 2026: "For 99% of installs the basic assumption that local logging
  (with local reading) is the primary mode is just wrong." In practice
  people pair journald with rsyslog, Vector, Fluent Bit or Promtail rather
  than use journal-remote.

**How common.** Common among people running more than a few machines; the
low reaction counts reflect that most of them gave up on journal-remote
rather than voted.

**What we should do.** Forwarding as a first-class output: a durable
cursor per destination, store-and-forward with back-off and resume, bounded
by its own quota; TLS with mutual authentication that actually verifies;
outputs for our native protocol, RFC 5424 (TCP/TLS, and UDP if asked),
JSON lines over HTTP, and OTLP logs. Inputs for RFC 3164/5424 on
`/dev/log`, TCP and UDP. The central store is the same program in
"receiver" role, splitting by host and applying the same retention rules.
`logctl -H host` reads a remote store over the same API.

### 11. Previous boots and persistence by default

**The wish.** After a crash or reboot, yesterday's logs are there.

**Evidence.**
- [Unix & Linux 159221](https://unix.stackexchange.com/questions/159221)
  "How do I display log messages from previous boots under CentOS 7?" —
  131 votes, 258 k views; [Ask Ubuntu 765315](https://askubuntu.com/questions/765315)
  — 43 votes, 114 k views; [Server Fault 809093](https://serverfault.com/questions/809093)
  "view older journalctl logs (after a rotation maybe?)" — 63 votes.
- Distributions shipped volatile journals plus rsyslog for years; Debian 12
  dropped rsyslog from the default install, so `/var/log/syslog` vanished
  for many users ([Thomas-Krenn wiki](https://www.thomas-krenn.com/en/wiki/Replacement_of_/var/log/syslog_by_journalctl_in_Debian_12),
  [HowtoForge](https://forum.howtoforge.com/threads/limited-logs-in-bookworm.92961/)).
- The opposite wish, keep only a little on fragile flash: #14588 (theme 5).

**How common.** Very common as a question; settled once persistence is on.

**What we should do.** Persistent by default with sane quotas (theme 5),
the RAM tier for early boot until the store is mounted (theme 16), and a
`--list-boots` that is instant (segment summaries, theme 2).

### 12. "This run of the service" and other natural slices

**The wish.** Show the logs since the last restart; follow one run until it
ends.

**Evidence.**
- [#1942](https://github.com/systemd/systemd/issues/1942) "add switch to
  show logs since unit restart" — 2015–2024, r=38. Fixed in systemd 257
  (`--invocation`, via [#5317](https://github.com/systemd/systemd/issues/5317)).
  A 2023 comment: "This should be the default for `journalctl -u foobar`".
- [Server Fault 768901](https://serverfault.com/questions/768901) "logs from
  the last time foo.service ran" — 55 votes.
- [#43185](https://github.com/systemd/systemd/issues/43185) follow until the
  invocation ends (2026, open); [#36754](https://github.com/systemd/systemd/issues/36754)
  exit `--follow` on a `--grep` match (open).
- [#11846](https://github.com/systemd/systemd/issues/11846) `-u` with unit
  aliases; [#7772](https://github.com/systemd/systemd/issues/7772) and
  [#1621](https://github.com/systemd/systemd/issues/1621) `--user --unit`
  confusion.

**How common.** Common.

**What we should do.** Invocation ID as an indexed field from the start.
`status` shows the current or last invocation; `-u` takes `@last`,
`@-2` and a `--since-restart` spelling. Follow modes that end with the
invocation or on a match (exit status says which). Resolve unit aliases.

### 13. Full-text search and grep ergonomics

**The wish.** Search message text fast and exactly; use the usual text
tools when wanted.

**Evidence.**
- [#20445](https://github.com/systemd/systemd/issues/20445) `--grep` misses
  entries `| grep` finds (c=22).
- HN 2026: "the indexing … you will get better performance out of … rg";
  "visibly slower than grepping gzipped text logs"; the perennial "`cd
  /var/log && zgrep errorstring *` was too convenient"
  ([HN](https://news.ycombinator.com/item?id=34733549)).
- [Unix & Linux 199988](https://unix.stackexchange.com/questions/199988)
  "How to inspect systemd journal files directly?" — 66 votes, 97 k views.
- Loki's model (index labels only, scan compressed chunks for text fast) is
  praised as cheap and fast enough.

**How common.** Common; part of the speed and "binary logs" complaints.

**What we should do.** Make the scan fast before building a full-text
index: compressed column-like segments of message text that a parallel
scan reads at disk speed, after metadata and time pruning. A token or
n-gram index per sealed segment is an option to measure, not a promise.
A documented, stable, simple segment format and a `cat`-like exporter, so
that `zgrep`-style workflows stay possible.

### 14. Unprivileged access: my own logs, my group's logs

**The wish.** A user reads their own services' and sessions' logs without
root or `sudo`.

**Evidence.**
- [#2744](https://github.com/systemd/systemd/issues/2744) "Allow users to
  read their own volatile journals" — 2016, open, r=22.
- [Server Fault 806469](https://serverfault.com/questions/806469) "allow a
  user to use journalctl to see user-specific systemd service logs" — 60
  votes; [681632](https://serverfault.com/questions/681632) without root on
  CentOS 7 — 21 votes; [Unix & Linux 109490](https://unix.stackexchange.com/questions/109490)
  "no log for normal user systemd services" — 48 votes.
- [#1719](https://github.com/systemd/systemd/issues/1719) a bus API for
  unprivileged clients via polkit; [#4786](https://github.com/systemd/systemd/issues/4786)
  sudo-group access (c=12); [#40027](https://github.com/systemd/systemd/issues/40027)
  per-user configuration.

**How common.** Common.

**What we should do.** Readers go through the manager's API, which
filters by the caller's identity: a user always sees entries whose sender
uid is theirs; group- or capability-based grants for more. Never rely on
file permissions over a shared file.

### 15. Rate limiting that surprises

**The wish.** Do not drop a service's logs without the operator knowing
why and where.

**Evidence.**
- [HashiCorp support: "journal: Suppressed xxxxx messages from
  vault.service"](https://support.hashicorp.com/hc/en-us/articles/5314240426259-Why-am-i-seeing-journal-Suppressed-xxxxx-messages-from-system-slice-vault-service-in-my-logs);
  InfluxDB community thread; RHEL guides on raising `RateLimitBurst`.
- [Launchpad 1732803](https://bugs.launchpad.net/bugs/1732803)
  `RateLimitBurst` silently divided by 4 when free disk was low (fixed in
  v232); the burst is scaled by free disk space, which nobody expects.
- #34753 (theme 4) asks for the opposite: a lower, notifying limit.

**How common.** Moderate; it bites server operators of chatty daemons.

**What we should do.** Rate limits per source with the defaults and the
effective values visible in status; a dropped-message counter per source
that is queryable and emitted as one summary entry per window; never scale
a limit by an unrelated quantity. Prefer back-pressure for local trusted
sources up to a bound, then drop with a count (theme 18).

### 16. Early boot and kernel messages

**The wish.** See everything from the first kernel line, including the
initrd, in one place.

**Evidence.**
- [#26586](https://github.com/systemd/systemd/issues/26586) "Improve user
  experience on early system boot failures" — c=76.
- [#11104](https://github.com/systemd/systemd/issues/11104) journald misses
  initial kernel messages on arm64 (c=15).
- [#867](https://github.com/systemd/systemd/issues/867) `/var` busy at
  shutdown due to journald (c=99) — the shutdown end of the same problem.
- People want kernel and service messages in one time-aligned view: the
  `dmesg -d` request ([#24641](https://github.com/systemd/systemd/issues/24641), c=19).

**How common.** Moderate; painful when it happens (the logs you need are
the ones you cannot see).

**What we should do.** Start the RAM tier before any service, hand it over
to disk when the store mounts, keep the kernel ring buffer's timestamps
aligned with ours, and flush to disk (or hand back to RAM) cleanly around
shutdown so the store never pins a filesystem. Ferrix's kernel log can
feed the manager directly with its boot-relative times.

### 17. Structured logging from applications

**The wish.** Attach key/value fields from any language without linking
libsystemd.

**Evidence.**
- [#19683](https://github.com/systemd/systemd/issues/19683) "Structured log
  data from `stderr` using JSON?" — open, c=10.
- [#3041](https://github.com/systemd/systemd/issues/3041) custom journal
  fields from the unit file — r=13.
- [#19251](https://github.com/systemd/systemd/issues/19251) RFC 5424
  structured data on `/dev/log` (theme 10).
- [#8691](https://github.com/systemd/systemd/issues/8691) `_HOSTNAME` cannot
  be set by API (r=7).

**How common.** Moderate and rising (JSON logging is the norm in services).

**What we should do.** Accept journald's native protocol unchanged; also
accept one JSON object per line on stdout/stderr when a unit opts in, and
RFC 5424 structured data; let a unit add static fields. Keep trusted
(`_`-prefixed) fields un-forgeable.

### 18. Logging must never stall the system

**The wish.** A slow or broken log store must not hang the services that
log.

**Evidence.**
- [#16456](https://github.com/systemd/systemd/issues/16456) "Multiple
  daemons hang waiting to log messages to journald" (c=12).
- [#6620](https://github.com/systemd/systemd/issues/6620) journald restarts
  and SIGPIPE on stdout streams (c=23).
- [#25265](https://github.com/systemd/systemd/issues/25265) journald spams
  the console on disk full; [#2339](https://github.com/systemd/systemd/issues/2339)
  log spam after `/` went read-only.
- [HN, "Eliminating Delays from systemd-journald"](https://news.ycombinator.com/item?id=11289406)
  (59 points); the CoreOS-era watchdog kills mentioned in the 2026 HN thread.

**How common.** Moderate; severe when it happens.

**What we should do.** Bounded queues per source; a writer blocks only up
to a short bound, then entries are dropped with a count. Restarting the
manager keeps stdout/stderr streams open (they are held by the service
manager or passed back, not owned by the log process). Disk full or
read-only: say so once, switch to RAM, keep counting.

### 19. Security of the collector itself

**The wish.** (Implied rather than asked.) The process that parses every
untrusted line on the system must not be a root exploit.

**Evidence.** [HN "System Down: A systemd-journald exploit"](https://news.ycombinator.com/item?id=18873530)
— 2019, 385 points, 344 comments (Qualys: local root through journald's
parsing). Forward-secure sealing is cited as a reason for the binary
format ([HN](https://news.ycombinator.com/item?id=34733549),
[LWN 512895](https://lwn.net/Articles/512895/)); [#15425](https://github.com/systemd/systemd/issues/15425)
asks `--verify-key` to report unsealed files.

**How common.** One very loud event; low as a recurring request.

**What we should do.** Written in safe Rust with no `unsafe` in parsing,
fuzzed on every input protocol, running unprivileged with only the
capabilities it needs. Optional tamper-evidence (hash chain per segment,
signed seals) as an addition, not in the default write path.

### 20. A viewer: interactive TUI or GUI

**The wish.** Browse, filter and follow interactively instead of
re-running commands.

**Evidence.** [#11760](https://github.com/systemd/systemd/issues/11760)
toggle between follow and scroll/search (open); [Unix & Linux 314985](https://unix.stackexchange.com/questions/314985)
"Combining tail && journalctl" (112 votes, 195 k views);
[129416](https://unix.stackexchange.com/questions/129416) listening for
changes (74 votes); lnav's README recommends piping journalctl into lnav
(merged views, SQL, format detection); Windows Event Viewer's Custom Views;
third-party TUIs and HN "Show HN" journalctl wrappers.

**How common.** Moderate. People cope with `less` and lnav; few file it.

**What we should do.** A TUI over the same API: live follow that pauses
when scrolling and resumes, interactive include/exclude by clicking a
field value, saved views, a histogram of rate over time. Later a desktop
viewer for hyprix. Not before themes 1–5.

### 21. Per-service log levels at runtime

**The wish.** Raise or lower one service's verbosity without a restart.

**Evidence.** [#31101](https://github.com/systemd/systemd/issues/31101)
runtime change of `LogLevelMax` (open); [#4089](https://github.com/systemd/systemd/issues/4089)
disable logging or set level per service; [#2236](https://github.com/systemd/systemd/issues/2236)
reload configuration (r=10); [#34830](https://github.com/systemd/systemd/issues/34830)
query loaded configuration.

**How common.** Low to moderate.

**What we should do.** Ingest rules (theme 4) are live-reloadable; a
`logctl level <unit> debug --for 10m` that expires; the effective
configuration is queryable.

### 22. Alerting and actions on patterns

**The wish.** Run something, or notify, when a pattern appears.

**Evidence.** Windows Event Viewer's "Attach Task To This Event" is a
long-praised feature ([Microsoft TechNet](https://technet.microsoft.com/en-us/library/cc748900(WS.10).aspx));
#34753 (notify on spam); #36754 (exit follow on a match);
[Journalwatch on HN](https://news.ycombinator.com/item?id=12417859) and
"journal-guardian" show people building it themselves.

**How common.** Low as a journald request (people use monitoring stacks),
but a clear differentiator.

**What we should do.** Subscriptions in the filter language that start a
unit, send a desktop notification or post to a hook, with a rate limit and
a dedupe window. Cheap once the filter language and the follow API exist.

### 23. Privacy and redaction

**The wish.** Control what personal data (IPs, user names) is stored and
for how long; honour deletion requests.

**Evidence.** [#2447](https://github.com/systemd/systemd/issues/2447) "journal
ip anonymization" — closed the same day, r=10, c=16, still argued in 2018;
[#20673](https://github.com/systemd/systemd/issues/20673) GDPR deletion
requests (open). Counter-example: macOS redacts runtime values as
`<private>` *at write time*, and developers complain that the data is gone
for good and that turning it off was made deliberately hard
([steipete.me, 2025](https://steipete.me/posts/2025/logging-privacy-shenanigans),
[Eclectic Light](https://eclecticlight.co/2021/09/27/explainer-logs/)).

**How common.** Low but legally important in the EU.

**What we should do.** Redaction as an opt-in ingest rule (regex or field
→ hash or mask), never on by default; shorter retention for sources
marked personal; delete-by-match (theme 5). Learn from macOS: do not
destroy data silently by default.

### 24. Multi-line messages

**The wish.** A stack trace is one entry, not forty.

**Evidence.** [#5188](https://github.com/systemd/systemd/issues/5188)
"Document or implement multi-line logging via stdout"; #43718 indentation
of multi-line messages; #19251 asks RFC 5424 line-break handling; log
shippers (Datadog, OpenTelemetry filelog, Fluent Bit) all carry multiline
rules because stdout collectors split per line.

**How common.** Moderate in the wider logging world, weak as a journald
request (the native protocol already carries newlines).

**What we should do.** Native protocol and JSON carry multi-line messages
whole. For plain stdout, an opt-in per-unit continuation rule (lines
starting with whitespace, or a start-of-entry regex) with a time bound.

### 25. Containers and namespaces

**Evidence.** Thin. `LogNamespace=` (systemd 245) exists and is mainly
recommended as the retention workaround (theme 5); #2913 and #30886 matter
for containers too. Docker and Podman offer a journald log driver; no
large body of complaints about it was found.

**What we should do.** Make the namespace (container, cgroup, Ferrix
namespace) an ordinary indexed field and a scope for access rules and
quotas, rather than a separate daemon per namespace — which keeps global
ordering, the cost LWN notes for systemd's design.

### 26. Trace and request correlation

**Evidence.** No journald-specific demand was found. The OpenTelemetry
world correlates logs and traces through `trace_id`/`span_id` fields that
logging libraries inject (SigNoz, Datadog documentation).

**What we should do.** Nothing special beyond themes 3 and 17: accept
`TRACE_ID`/`SPAN_ID` (and OTLP's names) as ordinary indexed fields, and
emit them on OTLP output. Do not build tracing.

---

## Dropped or demoted themes

- **Binary format as such.** The objection is to its consequences (speed,
  corruption, opacity), covered by themes 1, 2, 9 and 13. Commenters who use
  it well defend it ("the feature I want most",
  [HN](https://news.ycombinator.com/item?id=31797073)). Keeping a binary,
  indexed store is fine if those four are fixed and the format is documented.
- **Disk usage control** is folded into theme 5, **log levels per service**
  is theme 21, **boot IDs** into themes 8, 11 and 12.
- **Time zones** stand only as part of theme 8; no separate demand.
