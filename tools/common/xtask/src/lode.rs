//! lode, the log manager (`docs/LOGGING.md`): what an image carries for it,
//! and `cargo xtask test-log`.
//!
//! The program is a native one, `/sbin/lode`, in every initramfs
//! (`native::PROGRAMS`); an image that boots init carries `/bin/lode` and
//! `/bin/journalctl` as links to it and `lode.service` among the shipped
//! units, and an image that wants the log kept enables the unit
//! ([`enabled`]): the desktop's, and `test-log`'s.
//!
//! `test-log` boots init with lode enabled and a unit that floods the
//! kernel log with 3,000 numbered lines, kills lode with `SIGKILL` a third
//! of the way through, and lets init start it again. It requires that every
//! kernel sequence number between the first and the last stored is either
//! stored or covered by a record that says it was lost (`lode verify
//! --kernel`), that the restart was recorded, that the flood's last line is
//! stored, and that the unit's own lines are stored under its unit's name.

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, init, initramfs, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-log",
    help_at: 401,
    help: &[
        "test-log      Boot init with lode: fold, limit, drop and delete records, flood the kernel log,",
        "              kill lode mid-flood, and require that no record is lost but the ones a record says were",
    ],
    run: |_, args| test_log(args),
}];

/// The service's unit, shipped in `/lib/ferrix/units` with init's.
pub(crate) const UNIT: &str = "lode.service";

/// The links an image that boots init carries to the program.
pub(crate) fn links() -> Vec<File> {
    ["bin/lode", "bin/journalctl"]
        .into_iter()
        .map(|path| File {
            path: path.to_owned(),
            mode: 0o777,
            content: Content::Link("/sbin/lode".to_owned()),
        })
        .collect()
}

/// The link that starts `lode.service` with `basic.target`.
pub(crate) fn enabled() -> File {
    File {
        path: format!("etc/ferrix/units/basic.target.wants/{UNIT}"),
        mode: 0o777,
        content: Content::Link(format!("/lib/ferrix/units/{UNIT}")),
    }
}

/// What a desktop image says to lode: crash and app-error screenshots on,
/// within lode's own spacing and retention (§19), and `events.conf` with
/// every kind at its default, so a person sees what can be switched.
pub(crate) fn desktop_settings() -> Vec<File> {
    [
        (
            "etc/ferrix/lode.d/capture.conf",
            "# Crash and app-error screenshots (docs/LOGGING.md §19): at most one\n\
             # each 10 s, kept 7 days and 256 MiB, readable as the journal is.\n\
             Capture=yes\n",
        ),
        ("etc/ferrix/lode.d/events.conf", EVENTS_CONF),
    ]
    .into_iter()
    .map(|(path, text)| File {
        path: path.to_owned(),
        mode: 0o644,
        content: Content::Bytes(text.as_bytes().to_vec()),
    })
    .collect()
}

/// `events.conf` as a desktop image ships it: the defaults, said.
const EVENTS_CONF: &str = "\
# Which kinds of event the log takes (docs/LOGGING.md §18, `lode events`).
# KIND on|off [LEVEL]; ~/.config/ferrix/lode/events.conf overrides the
# kinds made in a person's session. Which records of a kind are kept is the
# rules' (*.rules): `drop where event = window-opened and id = waybar`.
window-opened   off
app-error       on   warning
session-streams off
crash-names     on
process-names   on
capture         on
";

/// The uploader's unit, shipped beside [`UNIT`].
pub(crate) const UPLOAD_UNIT: &str = "lode-upload.service";

/// test-log's collector: the `upload-stub` the flood's unit starts.
const UPLOAD_CONF: &str = "\
# test-log's collector (tools/common/xtask/src/lode.rs).
stub http://127.0.0.1:19532/upload export where id = \"uploadme\"
";

/// The flood's unit: after lode, its output through init's log pipe.
const TEST_UNIT: &str = "\
# test-log's flood (tools/common/xtask/src/lode.rs).
[Unit]
Description=Flood the kernel log and kill lode in the middle
After=lode.service

[Service]
Type=oneshot
# Its rows take most of two minutes on armv7a in a busy gate pool: well
# past the 90 s a start may take by default.
TimeoutStartSec=600
ExecStart=/bin/busybox sh /lib/lode-test.sh
";

/// The rules the flood's image carries: `noisy` is limited over two
/// seconds and `dropme` dropped; `noisy`'s lines and the flood's differ
/// only in their numbers, so folding is off for them (`fold 0s`); every other console line, the flood's
/// included, is not limited, so the row counts the flood whole -- even a
/// line the kernel cut short, which no rule naming its text would match.
const TEST_RULES: &str = "\
# test-log's rules (tools/common/xtask/src/lode.rs).
limit 100/2s where id = noisy
fold 0s where id = noisy
fold 0s where id = uploadme
fold 0s where message ~ \"lode flood\"
drop where id = dropme
limit 100000/30s where transport = console
alert flood rate > 50/s where id = noisy notify \"%s is flooding the log\"
alert crit where id = alerty and priority <= crit start alert-handler.service
alert nostart where id = alertn start twice.service
";

/// A unit that runs as the user `ferrix` (uid 1000): its lines go to that
/// user's segments, and as that user it reads them through lode's
/// `/run/lode/read` -- and nothing of the system's.
const USER_UNIT: &str = "\
# test-log's user (tools/common/xtask/src/lode.rs).
[Unit]
Description=Write as a user, and read one's own records back

[Service]
Type=oneshot
User=ferrix
ExecStart=/bin/busybox sh /lib/lode-user.sh
";

/// What [`USER_UNIT`] runs.
const USER_SCRIPT: &str = r#"bb=/bin/busybox
echo "user line on stdout"
echo "user line on stderr" >&2
$bb sleep 2
own=$(/bin/lode -b -o json -g "user line on" | $bb grep -c '"_UID":"1000"')
system=$(/bin/lode -b -t lode -o cat | $bb wc -l)
err=$(/bin/lode -b -o json -g "user line on stderr" | $bb grep -c '"_STREAM":"stderr"')
echo "lode-user: own $own system $system stderr $err"
"#;

/// A oneshot started twice: two runs of one unit, each with its own
/// `INVOCATION_ID`, which `-u UNIT@last`, `@-1` and `svc status` tell apart.
const TWICE_UNIT: &str = "\
# test-log's two runs (tools/common/xtask/src/lode.rs).
[Unit]
Description=Say which run this is

[Service]
Type=oneshot
ExecStart=/bin/busybox sh -c \"echo run $INVOCATION_ID\"
";

/// The unit an alert starts: its file lets lode start it.
const HANDLER_UNIT: &str = "\
# test-log's alert handler (tools/common/xtask/src/lode.rs).
[Unit]
Description=Say that an alert started this
StartableBy=lode

[Service]
Type=oneshot
ExecStart=/bin/busybox echo handled
";

/// The marker the flood's unit ends with.
const DONE: &str = "lode-test: done";

/// The flood. Every line it says goes through init's log pipe, so the
/// unit's lines reach lode as `lode-test.service`'s.
const TEST_SCRIPT: &str = r#"bb=/bin/busybox
say() { echo "lode-test: $*"; }
i=0
until [ -n "$($bb pidof lode)" ]; do
  i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode never ran"; say done; exit 1; }
  $bb usleep 100000
done
# Alerts (step 3): follow what lode says from the start.
( exec /bin/lode alerts ) > /tmp/alerts 2>&1 &
# Store-and-forward (step 6): a collector that comes up after the
# uploader, so its first tries fail and back off.
up=/tmp/uploaded
# The service: the lode that is neither the collector nor `lode alerts`.
lode_pid() { for p in $($bb pidof lode); do $bb grep -q 'upload-stub\|alerts' /proc/$p/cmdline 2>/dev/null || { echo $p; return; }; done; }
( exec /bin/lode upload-stub 19532 $up ) > /tmp/upload-stub 2>&1 &
upload_count() { n=$($bb grep -c '^MESSAGE=upload ' $up 2>/dev/null); echo ${n:-0}; }
upload_wait() { i=0; until [ "$(upload_count)" -ge "$1" ]; do i=$((i+1)); [ $i -gt 400 ] && break; $bb usleep 100000; done; }
i=0; while [ $i -lt 5 ]; do echo "<14>uploadme: upload $i" > /dev/kmsg; i=$((i+1)); done
# Floods held (step 2): a repeat folded, a source over its rule's limit,
# a dropped source, an error in its own class, a deletion.
i=0; while [ $i -lt 50 ]; do echo "<14>folder: the same line" > /dev/kmsg; i=$((i+1)); done
echo "<14>folder: another line" > /dev/kmsg
i=0; while [ $i -lt 300 ]; do echo "<14>noisy: line $i" > /dev/kmsg; i=$((i+1)); done
i=0; while [ $i -lt 20 ]; do echo "<14>dropme: line $i" > /dev/kmsg; i=$((i+1)); done
echo "<11>erring: an error" > /dev/kmsg
echo "<14>deleteme: one" > /dev/kmsg
echo "<14>deleteme: two" > /dev/kmsg
# Field quality: lines alike but for their numbers folded, a name with a
# number's tail glued on, a line with no level, one with a level read from
# its words.
i=0; while [ $i -lt 20 ]; do echo "<14>similar: frame $i at $((i*3)),99" > /dev/kmsg; i=$((i+1)); done
echo "<14>similar: done" > /dev/kmsg
echo "0usbhid: glued" > /dev/kmsg
echo "<14>levelless: just words" > /dev/kmsg
echo "<14>guessy: port 2 failed to start" > /dev/kmsg
$bb sleep 11
say "folded: $(/bin/lode -b -t folder -o cat | $bb grep -c '^the same line$') $(/bin/lode -b -t folder -o json | $bb grep -c '"REPEATED":"49"')"
say "limited: $(/bin/lode -b -t noisy -o cat | $bb grep -c '^line ') $(/bin/lode -b -t lode -o json | $bb grep '"SUPPRESSED_SOURCE":"noisy"' | $bb sed 's/.*"N_SUPPRESSED":"\([0-9]*\)".*/\1/' | $bb head -n 1)"
say "dropped: $(/bin/lode -b -t dropme -o cat | $bb wc -l) $(/bin/lode -b --where 'rule = "drop*"' -o json | $bb grep -c '"N_DROPPED":"20"')"
upload_wait 5
say "uploaded: $(upload_count)"
# The collector down: nothing kept, nothing lost, sent once it is back.
$bb touch $up.refuse
i=5; while [ $i -lt 10 ]; do echo "<14>uploadme: upload $i" > /dev/kmsg; i=$((i+1)); done
$bb sleep 3
say "while down: $(upload_count)"
$bb rm $up.refuse
upload_wait 10
# The uploader killed: started again, it goes on from its cursor.
for p in $($bb pidof lode-upload); do $bb kill -9 $p && say "killed the uploader"; done
i=10; while [ $i -lt 15 ]; do echo "<14>uploadme: upload $i" > /dev/kmsg; i=$((i+1)); done
upload_wait 15
$bb sleep 1
say "upload: $(upload_count) $($bb grep '^MESSAGE=upload ' $up | $bb sort -u | $bb wc -l) $($bb grep -c '^SYSLOG_IDENTIFIER=uploadme$' $up) $($bb grep -vc '^MESSAGE=upload \|^$\|=' $up) $(/bin/lode -b -t lode-upload -o cat | $bb grep -c 'trying again in')"
say "fields: $(/bin/lode -b -t similar -o json | $bb grep -c '"REPEATED":"19"') $(/bin/lode -b -t similar -o json | $bb grep -c '"LAST_MESSAGE":"frame 19 at 57,99"') $(/bin/lode -b -t usbhid -o cat | $bb grep -c '^glued$')"
say "levels: $(/bin/lode -b -t levelless -o json | $bb grep -c '"FERRIX_LEVEL":"unspecified"') $(/bin/lode -b -t levelless -o json | $bb grep -c '"PRIORITY"') $(/bin/lode -b -t levelless -p err -o cat | $bb wc -l) $(/bin/lode -b -t levelless -p err,unspecified -o cat | $bb wc -l) $(/bin/lode -b -t levelless | $bb grep -c ' ? levelless: just words') $(/bin/lode -b -t levelless -o verbose | $bb grep -c '^    FERRIX_LEVEL=unspecified$') $(/bin/lode -b -t guessy -p err -o json | $bb grep -c '"FERRIX_PRIORITY_SOURCE":"derived"') $(/bin/lode levels -b | $bb grep -c '^levelless ')"
say "classes: $($bb ls /var/log/journal | $bb grep -c '^errors@') $(/bin/lode -b -t erring -o cat | $bb wc -l)"
say "delete: $(/bin/lode delete --where 'id = deleteme')"
say "deleted: $(/bin/lode -b -t deleteme -o cat | $bb wc -l) $(/bin/lode -b -t lode-delete -o cat | $bb grep -c 'removed 2 record')"
# The Linux front (step 3): busybox logger through /dev/log, a stdout stream
# and a native-protocol datagram, each with the kernel's word on the sender.
$bb logger -t sysloggy "from logger" || say "FAIL logger"
echo "from a stream" | /bin/lode cat -t streamy -p warning || say "FAIL lode cat"
echo "from the journal socket" | /bin/lode cat --journal -t nativey -p err || say "FAIL lode cat --journal"
$bb sleep 1
say "devlog: $($bb readlink /dev/log)"
# Units' own streams (step 1 of 2026-10-23's order): init hands lode each
# line with its unit, run, stream and ids; the console's copy is paired, not
# stored twice.
echo "to stderr once" >&2
$bb sleep 1
say "stream: $(/bin/lode -b -u lode-test -g 'to stderr once' -o json | $bb grep '"_STREAM":"stderr"' | $bb grep -c '"_SYSTEMD_UNIT":"lode-test.service"') $(/bin/lode -b -g 'to stderr once' -o cat | $bb wc -l) $(/bin/lode -b -g 'to stderr once' -o json | $bb grep -c '"_KERNEL_SEQNUM"')"
# Runs: a oneshot twice, its current run and the one before.
/bin/svc start twice.service > /dev/null 2>&1
/bin/svc start twice.service > /dev/null 2>&1
$bb sleep 1
inv=$(/bin/svc status twice.service | $bb sed -n 's/^ *Invocation: //p')
say "runs: $(/bin/lode -b -u twice -o cat | $bb grep -c '^run ') $(/bin/lode -u twice@last -o cat | $bb grep -c "^run $inv\$") $(/bin/lode -u twice@-1 -o cat | $bb grep '^run ' | $bb grep -vc "$inv") $(/bin/lode -u twice@last -o json | $bb grep -c "\"_SYSTEMD_INVOCATION_ID\":\"$inv\"") $(/bin/svc log twice.service | $bb grep -c '^.*run ') $(/bin/svc status twice.service | $bb grep -c "run $inv")"
# journalctl's options (step 2): a field's values and the field names, -g
# as a regular expression with smart case, json-pretty and short-full, and
# what root asks the service.
tab=$(printf '\t')
say "jctl: $(/bin/lode -b -F _STREAM | $bb grep -c '^stderr$') $(/bin/lode -b -N | $bb grep -c '^_SYSTEMD_INVOCATION_ID$') $(/bin/lode -b -g '^run [0-9a-f]{32}$' -o cat | $bb wc -l) $(/bin/lode -b -g 'RUN [0-9A-F]{32}' -o cat | $bb wc -l) $(/bin/lode -b -g 'RUN [0-9a-f]{32}' --case-sensitive=no -o cat | $bb wc -l) $(/bin/lode -b -g '(?=x)' -o cat > /dev/null 2>&1; echo $?) $(/bin/lode -u twice@last -o json-pretty | $bb grep -c "^$tab\"_SYSTEMD_UNIT\" : \"twice.service\",\$") $(/bin/lode -u twice@last -o short-full | $bb grep -c '^[A-Z][a-z][a-z] 20[0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9] UTC ')"
say "jctl admin: $(/bin/lode --sync && echo synced) $(/bin/lode --flush && echo flushed) $(/bin/lode --rotate && echo rotated) $(/bin/lode --disk-usage | $bb grep -c '^Archived and active journals take up [0-9.]*[BKMG] in the file system\.$')"
# A user's own records (§12.1): written to user-1000/, read by that user
# through lode, and by root beside everything else.
/bin/svc start lode-user.service > /dev/null 2>&1
$bb sleep 1
say "user: $(/bin/lode -b -u lode-user -g 'lode-user: own' -o cat | $bb sed 's/^lode-user: //')"
say "users: $($bb ls /var/log/journal | $bb grep -c '^user-1000$') $(/bin/lode -b -o json -g 'user line on' | $bb grep -c '"_UID":"1000"')"
say "front: $(/bin/lode -b -t sysloggy -o json | $bb grep -c '"_TRANSPORT":"syslog"') $(/bin/lode -b -t streamy -p warning -o json | $bb grep -c '"_TRANSPORT":"stdout"') $(/bin/lode -b -t nativey -p err -o json | $bb grep -c '"_TRANSPORT":"journal"') $(/bin/lode -b -t sysloggy -o json | $bb grep -c '"_PID":"[0-9]') $(/bin/lode -b -t sysloggy -o json | $bb grep '"_SYSTEMD_UNIT"' | $bb grep -vc '"_SYSTEMD_UNIT":"lode-test.service"')"
# Sessions (step 2a): what a seat's owner tells lode, kept across lode's restart.
/bin/lode session begin 7 seat=seat0 uid=0 pid=$$ scope=session-7.scope unit=lode-test.service token=00112233445566778899aabbccddeeff || say "FAIL lode session begin"
say "in session"
first=$(lode_pid)
/bin/lode mark "flood begins" || say "FAIL lode mark"
n=0
while [ $n -lt 3000 ]; do
  echo "<14>lode flood $n" > /dev/kmsg
  n=$((n+1))
  if [ $n -eq 1000 ]; then $bb kill -9 $first && say "killed lode $first"; fi
done
/bin/lode mark "flood ends"
i=0
until /bin/lode -b -g "lode flood 2999" -o cat | $bb grep -q "lode flood 2999"; do
  i=$((i+1)); [ $i -gt 300 ] && { say "FAIL the flood's last line was never stored"; break; }
  $bb usleep 100000
done
say "restarted as $(lode_pid)"
say "still in session"
$bb sleep 1
/bin/lode session end 7
$bb usleep 500000
say "out of session"
$bb sleep 1
say "session: $(/bin/lode -s 7 -o cat | $bb grep -c 'in session$') $(/bin/lode -s 7 -o cat | $bb grep -c 'out of session') $(/bin/lode -b -t lode -o cat | $bb grep -c '^session 7 began on seat0')"
# Crash screenshots (step 2b): none without a provider; a picture from the
# one that is the session's pid with its token; a refusal for one that is not.
/bin/lode mark "capture me 1"
$bb usleep 500000
token=0123456789abcdef0123456789abcdef
( $bb sleep 1; exec /bin/lode provide-stub $token ) > /tmp/stub 2>&1 &
stub=$!
/bin/lode session begin 8 seat=seat0 uid=0 pid=$stub scope=session-8.scope unit=lode-test.service token=$token
i=0; until $bb grep -q accepted /tmp/stub; do i=$((i+1)); [ $i -gt 100 ] && break; $bb usleep 100000; done
/bin/lode provide-stub 00000000000000000000000000000000 > /tmp/wrong 2>&1
/bin/lode mark "capture me 2"
i=0; until $bb grep -q "sent record" /tmp/stub; do i=$((i+1)); [ $i -gt 100 ] && break; $bb usleep 100000; done
$bb usleep 500000
seq_of() { /bin/lode -b --where "mark = \"$1\"" -o json | $bb sed 's/.*"__SEQNUM":"\([0-9]*\)".*/\1/' | $bb head -n 1; }
s1=$(seq_of "capture me 1"); s2=$(seq_of "capture me 2")
say "capture: $(/bin/lode -b -t lode -o json | $bb grep -c "\"CAPTURE_OF\":\"$s1\",\"CAPTURE\":\"none\"") $(/bin/lode -b -t lode -o json | $bb grep -c "\"CAPTURE_OF\":\"$s2\",\"CAPTURE\":\"stored\"") $($bb ls /var/log/journal/captures | $bb wc -l) $($bb head -c 4 /var/log/journal/captures/* 2>/dev/null) $($bb cat /tmp/wrong)"
$bb kill $stub
/bin/lode session end 8
/bin/lode rotate
# Sealed segments compressed (§8.2), a few frames a turn of lode's loop.
packed() { n=0; for f in /var/log/journal/*.fxj /run/log/journal/*.fxj; do [ "$($bb head -c 8 $f 2>/dev/null)" = FXJZSTD1 ] && n=$((n+1)); done; echo $n; }
i=0; until [ "$(packed)" -ge 1 ]; do i=$((i+1)); [ $i -gt 300 ] && break; $bb usleep 100000; done
say "compressed: $(packed)"
say "verify: $(/bin/lode verify --kernel -b | $bb tail -n 1)"
/bin/lode -b --since-mark "flood begins" --until-mark "flood ends" -o cat > /tmp/window
say "stored: $($bb grep -c "^lode flood " /tmp/window) of 3000"
/bin/lode -b -g "lode flood " -o cat | $bb awk '/^lode flood /{seen[$3]=1} END{for(i=0;i<3000;i++) if(!(i in seen)) printf "%d ", i}' > /tmp/missing
[ -s /tmp/missing ] && say "missing: $($bb cat /tmp/missing)"
for m in $($bb cat /tmp/missing); do /bin/lode -b -g "flood $m" -o json | while read -r l; do say "record: $l"; done; done
/bin/lode -b -g "lode flood " -o cat | $bb awk '/^lode flood /{c[$3]++} END{for(i in c) if(c[i]>1) printf "%s ", i}' > /tmp/twice
[ -s /tmp/twice ] && say "twice: $($bb cat /tmp/twice)"
if [ -s /tmp/missing ]; then m=$($bb cat /tmp/missing | $bb awk '{print $1}'); /bin/lode -b -o json | $bb grep -n "lode flood $((m-1))\"" -A3 | while read -r l; do say "near: $l"; done; fi
say "restart records: $(/bin/lode -b -t lode -g "lode restarted" -o cat | $bb grep -c "lode restarted")"
say "unit: $(/bin/lode -b -u lode-test -g "killed lode" -o json | $bb grep -c '"UNIT":"lode-test.service"')"
pid=$(lode_pid)
say "uid: $($bb grep '^Uid:' /proc/$pid/status | $bb awk '{print $2}')"
say "cgroup: $($bb cat /proc/$pid/cgroup)"
if echo $pid > /sys/fs/cgroup/cgroup.procs 2>/dev/null; then say "sealed: moved"; else say "sealed: refused"; fi
say "boots: $(/bin/lode --list-boots | $bb tail -n +2 | $bb wc -l)"
say "kernel records: $(/bin/lode -b -k -o cat | $bb wc -l)"
# Fast reads (step 5): a unit's last lines through the segments' summaries,
# on a generated store and on this one.
say "bench: $(/bin/lode bench 200000 /tmp/lode-bench)"
say "newest: $(/bin/lode -b -u lode-test -n 3 -o cat | $bb wc -l)"
# Session programs (the customer's gap items): `lode cat --tee` keeps a
# script's log file and gives the log its lines under a name; `--echo`
# also writes them on, and the copy that comes back through init is kept
# once, as the stream's. `lode events` lists the kinds; `lode reload`.
( echo "teed line"; $bb sleep 1 ) | /bin/lode cat -t teey --tee /tmp/teey.log
echo "echoed line" | /bin/lode cat -t echoey --echo
$bb sleep 2
say "streams: $($bb cat /tmp/teey.log) $(/bin/lode -b -t teey -o cat | $bb wc -l) $(/bin/lode -b -g '^echoed line$' -o cat | $bb wc -l) $(/bin/lode -b -t echoey -o json | $bb grep -c '"_KERNEL_SEQNUM"') $(/bin/lode events | $bb grep -c '^app-error  *on ') $(/bin/lode reload && echo reloaded)"
say "names: $(/bin/lode -b -t teey -o json | $bb grep -c '"_CMDLINE":"/bin/lode cat -t teey --tee /tmp/teey.log"') $(/bin/lode -b -u lode-test -g 'to stderr once' -o json | $bb grep -c '"_EXE":"/bin/busybox"')"
# A crash names its process (the kernel's identity line): its name, the
# path it was started from, where its pc was, its command line.
/bin/lode fault-stub > /dev/null 2>&1
$bb sleep 1
say "crash: $(/bin/lode -b --where 'crash = "*"' -o json | $bb grep '"CRASH_CMDLINE":"/bin/lode fault-stub"' | $bb grep '"CRASH_EXE":"/sbin/lode"' | $bb grep -c '"CRASH_MODULE":"/sbin/lode+0x')"
# Alerts and top (step 3): the noisy flood above fired `flood` once, to
# `lode alerts`; a crit line started a unit that lets lode start it; a unit
# that does not was refused, and said so.
echo "<10>alerty: on fire" > /dev/kmsg
echo "<14>alertn: please" > /dev/kmsg
$bb sleep 2
say "alerts: $($bb grep -c '^alert flood noisy noisy is flooding the log$' /tmp/alerts) $(/bin/lode -b -t lode -o json | $bb grep '"ALERT":"flood"' | $bb grep -c '"ALERT_SOURCE":"noisy"') $(/bin/lode -b -u alert-handler -g handled -o cat | $bb wc -l) $(/bin/lode -b -t lode -o cat | $bb grep -c 'init refused to start twice.service')"
say "top: $(/bin/lode top -n 100 | $bb head -n 1 | $bb awk '{print $1, $2, $3, $4}') $(/bin/lode top -n 100 | $bb grep -c '^lode-test.service ')"
( $bb timeout 5 /bin/lode top --live 1 > /tmp/live 2>&1 ) &
$bb sleep 2
i=0; while [ $i -lt 100 ]; do echo "<14>livey: line $i" > /dev/kmsg; i=$((i+1)); done
$bb sleep 4
say "live: $($bb grep -c '^livey ' /tmp/live)"
# Vacuuming (step 2), last: it removes what the rows above read.
vac=$(/bin/lode --vacuum-time=1s 2>&1)
$bb sleep 1
say "vacuum: $(echo "$vac" | $bb grep -c '^Deleted archived journal /') $(echo "$vac" | $bb grep -c '^Vacuuming done, freed [0-9.]*[BKMG] of archived journals from /') $(/bin/lode -b -t lode-vacuum -o cat | $bb grep -c '^removed ')"
# --flush (step 2): a lode that could not reach /var/log/journal when it
# started keeps the log in /run; --flush gives it the directory, and it
# moves what it kept there and writes there from then on.
/bin/svc stop lode.service > /dev/null 2>&1
$bb mv /var/log/journal /var/log/journal.kept
$bb ln -s /var/log/later /var/log/journal
/bin/svc start lode.service > /dev/null 2>&1
$bb sleep 1
echo "<14>flushme: before" > /dev/kmsg
$bb sleep 1
before=$($bb ls /run/log/journal | $bb grep -c 'fxj$')
$bb mkdir /var/log/later
/bin/lode --flush || say "FAIL lode --flush"
echo "<14>flushme: after" > /dev/kmsg
$bb sleep 1
say "flush: $([ $before -gt 0 ] && echo kept) $($bb ls /run/log/journal | $bb grep -c 'fxj$') $([ $($bb ls /var/log/later | $bb grep -c 'fxj$') -gt 0 ] && echo moved) $(/bin/lode -b -t flushme -o cat | $bb tr '\n' ' ')"
# Without lode, syslog() is refused at once, not left waiting.
/bin/svc stop lode.service > /dev/null 2>&1
$bb timeout 5 $bb logger -t gone "with no lode" ; said=$?
say "no lode: $([ $said -eq 124 ] && echo hung || echo returned)"
/bin/svc start lode.service > /dev/null 2>&1
say done
"#;

/// The units, scripts and accounts the flood's unit uses beside itself.
fn test_units() -> Vec<File> {
    let mut files = Vec::new();
    for (path, text) in [
        ("etc/ferrix/units/lode-user.service", USER_UNIT),
        ("etc/ferrix/units/twice.service", TWICE_UNIT),
        ("etc/ferrix/units/alert-handler.service", HANDLER_UNIT),
        ("lib/lode-user.sh", USER_SCRIPT),
        (
            "etc/passwd",
            "root:x:0:0:root:/:/bin/sh\nlode:x:91:4:lode:/:/sbin/nologin\nferrix:x:1000:1000:ferrix:/:/bin/sh\n",
        ),
        ("etc/group", "root:x:0:\nferrix:x:1000:\n"),
    ] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o644,
            content: Content::Bytes(text.as_bytes().to_vec()),
        });
    }
    files
}

/// `cargo xtask test-log`.
fn test_log(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        test_arch(arch, args)?;
    }
    Ok(())
}

/// One architecture's boot.
fn test_arch(arch: Arch, args: &Args) -> Result<()> {
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = std::fs::read(crate::busybox::program(arch)?)
        .map_err(|error| Error::new(format!("reading busybox: {error}")))?;
    files.push(File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: Content::Bytes(busybox),
    });
    files.push(enabled());
    files.extend(crate::lode_net::carried(arch)?);
    files.push(File {
        path: format!("etc/ferrix/units/multi-user.target.wants/{UPLOAD_UNIT}"),
        mode: 0o777,
        content: Content::Link(format!("/lib/ferrix/units/{UPLOAD_UNIT}")),
    });
    files.push(File {
        path: "etc/ferrix/lode-upload.conf".to_owned(),
        mode: 0o644,
        content: Content::Bytes(UPLOAD_CONF.as_bytes().to_vec()),
    });
    // No shell for a console getty to start: masked, so its retries do not
    // write half lines into the log the flood is counted in.
    files.push(File {
        path: "etc/ferrix/units/getty@.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/dev/null".to_owned()),
    });
    files.push(File {
        path: "etc/ferrix/lode.d/capture.conf".to_owned(),
        mode: 0o644,
        content: Content::Bytes(b"Capture=yes\ncapture where mark = \"capture me*\"\n".to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/lode.d/test.rules".to_owned(),
        mode: 0o644,
        content: Content::Bytes(TEST_RULES.as_bytes().to_vec()),
    });
    files.push(File {
        path: "lib/lode-test.sh".to_owned(),
        mode: 0o755,
        content: Content::Bytes(TEST_SCRIPT.as_bytes().to_vec()),
    });
    files.extend(test_units());
    files.push(File {
        path: "etc/ferrix/units/lode-test.service".to_owned(),
        mode: 0o644,
        content: Content::Bytes(TEST_UNIT.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/units/multi-user.target.wants/lode-test.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/etc/ferrix/units/lode-test.service".to_owned()),
    });
    println!("  {arch}: building an image whose init starts lode");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let options = format!("{} log_buf_len=1M\n", init::command_line().trim_end());
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    println!("  {arch}: flooding the kernel log and killing lode in the middle");
    let lines = qemu::watch_lines(arch, &image, &kernel, args, DONE)?;
    judge(&lines).map_err(|why| {
        Error::new(format!(
            "{arch}: lode lost records or did not say so: {why}\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    println!(
        "  {arch}: lode kept every kernel record across a SIGKILL mid-flood, or said which it lost"
    );
    Ok(())
}

/// The value after `what` on the line that has it.
fn said<'a>(lines: &'a [String], what: &str) -> Option<&'a str> {
    lines
        .iter()
        .find_map(|line| line.split_once(what).map(|(_, rest)| rest.trim()))
}

/// What the flood's unit said, judged.
fn judge(lines: &[String]) -> std::result::Result<(), String> {
    if let Some(line) = lines.iter().find(|line| line.contains("lode-test: FAIL")) {
        return Err(line.trim().to_owned());
    }
    if said(lines, "lode-test: killed lode").is_none() {
        return Err("the flood never killed lode".into());
    }
    let verify = said(lines, "lode-test: verify:").ok_or("no verify line")?;
    if !verify.contains("unsaid gaps: none") {
        return Err(format!("kernel records went missing unsaid: {verify}"));
    }
    let restarts: u32 = verify
        .split(", ")
        .find_map(|part| part.strip_suffix(" restart(s)"))
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    let records: u32 = said(lines, "lode-test: restart records:")
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    if restarts == 0 || records == 0 {
        return Err(format!("no restart was recorded: {verify}"));
    }
    let stored: u32 = said(lines, "lode-test: stored:")
        .and_then(|text| text.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    let lost = verify.contains(" 0 said lost");
    if stored == 0 || (stored < 3000 && lost) {
        return Err(format!(
            "{stored} of 3000 flood lines stored, and none said lost: {verify}"
        ));
    }
    if said(lines, "lode-test: unit:").is_none_or(|count| count == "0") {
        return Err("the unit's own lines were not stored under lode-test.service".into());
    }
    isolated(lines)?;
    match said(lines, "lode-test: compressed:").and_then(|count| count.parse::<u32>().ok()) {
        Some(count) if count > 0 => {}
        _ => return Err("no sealed segment was compressed".into()),
    }
    held(lines)?;
    attributed(lines)?;
    vacuumed(lines)?;
    match said(lines, "lode-test: live:").and_then(|count| count.parse::<u32>().ok()) {
        Some(count) if count > 0 => {}
        _ => return Err("lode top --live did not show a source writing now".into()),
    }
    forwarded(lines)?;
    fast(lines)
}

/// Step 6: a collector sent every record once, through its own outage and
/// the uploader's restart, and the failures said and waited out.
fn forwarded(lines: &[String]) -> std::result::Result<(), String> {
    if said(lines, "lode-test: uploaded:") != Some("5") {
        return Err(format!(
            "a collector that came up late was not sent the 5 records: `{}`",
            said(lines, "lode-test: uploaded:").unwrap_or_default()
        ));
    }
    if said(lines, "lode-test: while down:") != Some("5") {
        return Err("a collector that answered 503 was counted as having kept records".into());
    }
    if said(lines, "lode-test: killed the uploader").is_none() {
        return Err("the uploader was never killed".into());
    }
    let counts: Vec<u32> = said(lines, "lode-test: upload:")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|count| count.parse().ok())
        .collect();
    match counts.as_slice() {
        [15, 15, 15, 0, retries] if *retries > 0 => Ok(()),
        other => Err(format!(
            "15 records were not each sent once in the export format, or no failure was said \
             (sent, distinct, identified, unreadable lines, retries said): {other:?}"
        )),
    }
}

/// Step 5: the summaries' read gives what reading everything gives, and
/// no slower; a unit's last lines on the boot's own store.
fn fast(lines: &[String]) -> std::result::Result<(), String> {
    let bench = said(lines, "lode-test: bench:").unwrap_or_default();
    let tail = bench.split_once("-n 50: ").map_or("", |(_, tail)| tail);
    let through: Option<u64> = tail.split_whitespace().next().and_then(|n| n.parse().ok());
    let whole: Option<u64> = tail
        .split(", ")
        .nth(1)
        .and_then(|part| part.split_whitespace().next())
        .and_then(|n| n.parse().ok());
    match (through, whole) {
        (Some(through), Some(whole)) if bench.contains("same answer: yes") && through <= whole => {}
        _ => {
            return Err(format!(
                "the summaries' read was wrong or slower: `{bench}`"
            ));
        }
    }
    if said(lines, "lode-test: newest:") != Some("3") {
        return Err("`-u lode-test -n 3` did not give 3 lines".into());
    }
    Ok(())
}

/// Step 1 of the customer's order of 2026-10-23: units' streams with
/// who wrote them, runs told apart, a user's own records.
fn attributed(lines: &[String]) -> std::result::Result<(), String> {
    for (what, want, why) in [
        (
            "lode-test: stream:",
            "1 1 1",
            "a unit's standard error did not come in once, under its unit with _STREAM, paired \
             with its console copy",
        ),
        (
            "lode-test: runs:",
            "2 1 1 1 1 1",
            "two runs of a oneshot were not told apart by -u UNIT@last, @-1, its invocation id, \
             svc log and svc status",
        ),
        (
            "lode-test: user:",
            "own 2 system 0 stderr 1",
            "a user did not read their own two records, with their stream, and none of the system's",
        ),
        (
            "lode-test: jctl:",
            "1 1 2 0 2 2 1 1",
            "journalctl's -F, -N, -g as a regular expression with smart case and \
             --case-sensitive, a refused pattern, json-pretty or short-full did not answer as \
             journalctl does",
        ),
        (
            "lode-test: jctl admin:",
            "synced flushed rotated 1",
            "--sync, --flush, --rotate or --disk-usage did not answer as journalctl does",
        ),
        (
            "lode-test: streams:",
            "teed line 1 1 1 1 reloaded",
            "lode cat --tee did not keep the file and the record, an echoed line was not kept \
             once with its kernel sequence number, lode events did not list app-error on, or \
             lode reload failed",
        ),
        (
            "lode-test: names:",
            "1 1",
            "a stream's record did not carry its writer's _CMDLINE, or a unit's line its \
             program's _EXE",
        ),
        (
            "lode-test: crash:",
            "1",
            "a crash record did not name its process: CRASH_EXE, CRASH_MODULE and CRASH_CMDLINE \
             from the kernel's identity line",
        ),
        (
            "lode-test: alerts:",
            "1 1 1 1",
            "a flood alert was not said once to `lode alerts` and in the log, a start alert did not \
             start a unit that allows it, or one that does not was not refused and said",
        ),
        (
            "lode-test: top:",
            "SOURCE RECORDS BYTES SHARE 1",
            "lode top did not list the sources",
        ),
        (
            "lode-test: flush:",
            "kept 0 moved before after",
            "--flush did not move the RAM tier to /var/log/journal and go on writing there",
        ),
        (
            "lode-test: users:",
            "1 2",
            "a user's records were not in user-1000/, or root could not read them",
        ),
    ] {
        let got = said(lines, what).unwrap_or_default();
        if got != want {
            return Err(format!("{why}: `{what} {got}`, wanted `{want}`"));
        }
    }
    Ok(())
}

/// journalctl's `--vacuum-time`: sealed segments removed and each said,
/// one closing line a store directory, and the removal in the log.
fn vacuumed(lines: &[String]) -> std::result::Result<(), String> {
    let counts: Vec<u32> = said(lines, "lode-test: vacuum:")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|count| count.parse().ok())
        .collect();
    match counts.as_slice() {
        [deleted, done, 1] if *deleted > 0 && *done > 0 => Ok(()),
        other => Err(format!(
            "--vacuum-time removed no sealed segment, did not say so as journalctl does, or \
             the log did not record it (deleted, done lines, records): {other:?}"
        )),
    }
}

/// Step 2: what the rules, the folding and the limiter held back, each
/// said; an error in its own class; a deletion that removed and said so.
fn held(lines: &[String]) -> std::result::Result<(), String> {
    // 300 lines over `limit 100/2s`: 100 stored and 200 said held.
    let limited: Vec<u32> = said(lines, "lode-test: limited:")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|count| count.parse().ok())
        .collect();
    match limited.as_slice() {
        [100, 200] => {}
        other => {
            return Err(format!(
                "300 lines over a 100/2s limit were not 100 stored and 200 said held: {other:?}"
            ));
        }
    }
    for (what, want, why) in [
        (
            "lode-test: capture:",
            "1 1 1 qoif provide-stub: refused",
            "a crash screenshot was not none without a provider, one stored file from the proven \
             provider, and a refusal for an unproven one",
        ),
        (
            "lode-test: devlog:",
            "/run/systemd/journal/dev-log",
            "/dev/log is not the link to lode's syslog socket",
        ),
        (
            "lode-test: no lode:",
            "returned",
            "logger hung with no lode to take its line",
        ),
        (
            "lode-test: front:",
            "1 1 1 1 0",
            "logger, a stdout stream and a native datagram did not each come in once, with the sender's pid and never a wrong unit",
        ),
        (
            "lode-test: session:",
            "2 0 1",
            "a session's lines were not stamped with its id, across lode's restart, and only while it lasted",
        ),
        (
            "lode-test: folded:",
            "1 1",
            "50 repeats were not one line and one `repeated 49` record",
        ),
        (
            "lode-test: fields:",
            "1 1 1",
            "lines alike but for their numbers were not folded with the last one kept, or a glued \
             number's tail was not taken off a name",
        ),
        (
            "lode-test: levels:",
            "1 0 0 1 1 1 1 1",
            "a line with no level was not shown as unspecified everywhere (json, human, verbose, \
             -p, lode levels), or a level read from words was not marked derived",
        ),
        (
            "lode-test: dropped:",
            "0 1",
            "a dropped source was stored, or its drop not said",
        ),
        (
            "lode-test: classes:",
            "1 1",
            "an error did not go to the errors class",
        ),
        (
            "lode-test: deleted:",
            "0 1",
            "lode delete did not remove the two records and say so",
        ),
    ] {
        let got = said(lines, what).unwrap_or_default();
        if got != want {
            return Err(format!("{why}: `{what} {got}`, wanted `{want}`"));
        }
    }
    Ok(())
}

/// The service's box: its own user, its job under the sealed
/// `drivers.slice`, a move out of it refused; one boot, kernel records.
fn isolated(lines: &[String]) -> std::result::Result<(), String> {
    let uid = said(lines, "lode-test: uid:").unwrap_or_default();
    if uid != "91" {
        return Err(format!("lode runs as uid {uid:?}, not its own user (91)"));
    }
    let cgroup = said(lines, "lode-test: cgroup:").unwrap_or_default();
    if !cgroup.ends_with("/drivers.slice/lode.service") {
        return Err(format!(
            "lode's job is {cgroup:?}, not under the sealed drivers.slice"
        ));
    }
    if said(lines, "lode-test: sealed:") != Some("refused") {
        return Err("lode's process could be moved out of its job".into());
    }
    if said(lines, "lode-test: boots:") != Some("1") {
        return Err("--list-boots did not show the one boot".into());
    }
    match said(lines, "lode-test: kernel records:").and_then(|count| count.parse::<u32>().ok()) {
        Some(count) if count > 0 => Ok(()),
        _ => Err("-k showed no kernel records".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::judge;

    /// A run that passes, every line as the flood's unit says it.
    const CLEAN: &str = "lode-test: killed lode 40
lode-test: verify: kernel seq 0..3500: 3501 held, 0 said lost, 1 restart(s), unsaid gaps: none
lode-test: stored: 3000 of 3000
lode-test: restart records: 1
lode-test: unit: 1
lode-test: uid: 91
lode-test: cgroup: 0::/drivers.slice/lode.service
lode-test: sealed: refused
lode-test: boots: 1
lode-test: kernel records: 300
lode-test: folded: 1 1
lode-test: limited: 100 200
lode-test: dropped: 0 1
lode-test: fields: 1 1 1
lode-test: levels: 1 0 0 1 1 1 1 1
lode-test: classes: 1 1
lode-test: deleted: 0 1
lode-test: session: 2 0 1
lode-test: capture: 1 1 1 qoif provide-stub: refused
lode-test: front: 1 1 1 1 0
lode-test: stream: 1 1 1
lode-test: runs: 2 1 1 1 1 1
lode-test: user: own 2 system 0 stderr 1
lode-test: users: 1 2
lode-test: jctl: 1 1 2 0 2 2 1 1
lode-test: jctl admin: synced flushed rotated 1
lode-test: vacuum: 3 2 1
lode-test: flush: kept 0 moved before after
lode-test: streams: teed line 1 1 1 1 reloaded
lode-test: names: 1 1
lode-test: crash: 1
lode-test: alerts: 1 1 1 1
lode-test: top: SOURCE RECORDS BYTES SHARE 1
lode-test: live: 2
lode-test: devlog: /run/systemd/journal/dev-log
lode-test: no lode: returned
lode-test: bench: 200000 records, 3 segments; -u unit0 -n 50: 4 ms through summaries, 900 ms reading all; same answer: yes
lode-test: newest: 3
lode-test: compressed: 3
lode-test: uploaded: 5
lode-test: while down: 5
lode-test: killed the uploader
lode-test: upload: 15 15 15 0 2";

    /// [`CLEAN`] with the line beginning `what` replaced by `with`.
    fn run(what: &str, with: &str) -> Vec<String> {
        CLEAN
            .lines()
            .map(|line| {
                if line.starts_with(what) {
                    with.to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect()
    }

    #[test]
    fn a_clean_run_passes() {
        assert_eq!(judge(&run("-", "")), Ok(()));
    }

    #[test]
    fn an_unsaid_gap_fails() {
        let lines = run(
            "lode-test: verify:",
            "lode-test: verify: kernel seq 0..3500: 3400 held, 0 said lost, 1 restart(s), unsaid gaps: [(900, 1001)]",
        );
        assert!(judge(&lines).is_err());
    }

    #[test]
    fn missing_lines_said_lost_pass() {
        let mut lines = run(
            "lode-test: verify:",
            "lode-test: verify: kernel seq 0..3500: 3400 held, 101 said lost, 1 restart(s), unsaid gaps: none",
        );
        lines.retain(|line| !line.starts_with("lode-test: stored:"));
        lines.push("lode-test: stored: 2899 of 3000".to_owned());
        assert_eq!(judge(&lines), Ok(()));
    }

    #[test]
    fn no_restart_fails() {
        let lines = run(
            "lode-test: restart records:",
            "lode-test: restart records: 0",
        );
        assert!(judge(&lines).is_err());
    }

    #[test]
    fn a_fold_not_said_fails() {
        assert!(judge(&run("lode-test: folded:", "lode-test: folded: 50 0")).is_err());
    }

    #[test]
    fn a_record_sent_twice_fails() {
        assert!(
            judge(&run(
                "lode-test: upload:",
                "lode-test: upload: 16 15 15 0 2"
            ))
            .is_err()
        );
        assert!(
            judge(&run(
                "lode-test: upload:",
                "lode-test: upload: 15 15 15 0 0"
            ))
            .is_err()
        );
    }

    #[test]
    fn a_root_lode_fails() {
        assert!(judge(&run("lode-test: uid:", "lode-test: uid: 0")).is_err());
    }
}
