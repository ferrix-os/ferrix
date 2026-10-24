# The settings writer: how a user's Settings app changes the system

Written on 2026-10-24 for Settings backend row **B1**. The customer's new
Settings app (`docs/SETTINGS.md`, its tole UI built beside this) has pages
that change the whole machine: the computer's name, the time, accounts, the
power-mode boot word, remote login. The app runs as the person at the
desktop, who may not write `/etc`. This document is the one way such a
change is made: a small privileged helper, `settingsd`, that takes a
**typed request**, asks `authd` whether the person may make it, writes it
atomically and records it.

Sections 1 to 8 are the design; §9 is what B1 built and what each later
row adds; §10 is the certification consultant's review (OK IF, ledger line
3482); §11 is where it stands. §12 is the trusted prompt and the per-area
allow-lists, the customer's review fix of 2026-10-24; §13 is the image
manifest a Settings app reads to show a page fixed by its build (ledger
3558).

## 1. In one page

```
Settings app (uid 1000)          settingsd (root)                authd (uid 90)
  ferrix-settings-client
  REQUEST set-hostname "box" -->  validate (refuse before asking)
                                  peer uid from SO_PEERCRED
                                  account of that uid
                                  BEGIN settings.hostname, account ->
                                                       policy: wheel? password?
            <-- PROMPT "Password:"  <-- relayed --------- PROMPT
  RESPOND secret ------------------ relayed --------------> RESPOND
                                  <----------------------- ACCEPTED ferrix 1000
                                  verdict names exactly the peer? apply:
                                  write /etc/hostname.tmp, fsync, rename,
                                  fsync /etc, sethostname
                                  audit line -> init -> lode
            <-- DONE "the computer is called box (was ferrix)"
```

* **One helper, one socket.** `/sbin/settingsd`, root, started by init on
  the first connection to `/run/ferrix/settings` (`settings.socket`,
  `SOCK_SEQPACKET`, mode 0666: anyone may ask, `SO_PEERCRED` says who).
* **A closed list of typed requests.** Each is a variant of one Rust enum
  with typed fields; there is no path, no file content, no command and no
  shell anywhere in the protocol. An unknown request name is refused.
* **Validated first.** A request is checked against its own rules before
  anyone is asked for anything, so a malformed one costs no prompt and
  reaches no file.
* **Authorised by `authd`.** The decision is `authd`'s, from a service
  policy per area (`settings.hostname`, `settings.services`, ...), re-read on
  every conversation as every policy is: membership of `wheel`, then a
  password when the policy says so. `settingsd` relays the conversation and
  never decides who may.
* **Written atomically, applied, recorded.** Temporary file, `fsync`,
  `rename`, `fsync` of the directory; then the live effect; then one audit
  record to lode with who, what, before, after and the verdict.
* **A client crate** (`ferrix-settings-client`) gives the app a step-wise
  conversation it can drive from its event loop, a dry run, and errors in
  kinds the UI can word.

## 2. Why a helper, and why not the alternatives

| Way | Why not |
|---|---|
| The app runs as root, or set-uid | the whole UI, tole and the compositor's libraries become root; one bug in a text field is root |
| A "write this file" helper (`pkexec tee`) | a request names a path and bytes, so the helper's policy is "anything an admin may write": the narrowness is gone |
| `su -c 'svc enable ...'` from the app | needs a terminal for the password (U5), a shell, and a command line built from strings |
| One set-uid program per setting | as many programs to audit as settings; each repeats `su`'s U1-U5 |
| polkit + D-Bus | Ferrix has `authd` and its policy files already (`docs/AUTH.md` §4.5 named this shape for phase 3); no system bus exists |

`settingsd` is the mechanism half of polkit's split: the mechanism is small,
privileged and typed; the policy is `authd`'s.

## 3. The protocol

Records on a `SOCK_SEQPACKET` connection, one per packet, at most 4 KiB.
One connection carries one request. Little-endian, every string a `u16`
length and UTF-8, every field read with its bounds; a packet with bytes left
over is refused. The layouts are in `ferrix-settings-proto`
(`src/user/system/linux/settingsd/proto`).

```
client -> settingsd
  REQUEST   version, flags (bit 0: dry run), name, the request's own fields
  RESPOND   secret                      the answer to the last PROMPT
  CANCEL

settingsd -> client
  PROMPT    visible, text               relayed from authd
  INFO      text                        relayed from authd ("wait 16 s")
  DONE      changed, text               the conversation is over: done, or what a dry run would do
  REFUSED   kind, retry_after_ms, text  the conversation is over: nothing was written
```

`REFUSED`'s kinds are what the UI words (§7):

| Kind | When | Written? |
|---|---|---|
| `invalid` | the request broke its own rules: a name with a space, a unit no Settings page may switch | no; no one was asked |
| `not-permitted` | `authd` refused before any prompt: not in the policy's group, the service missing | no |
| `authentication-failed` | the password was wrong, or the account must wait (`retry_after_ms`) | no |
| `cancelled` | the person gave no answer, or the client went away | no |
| `unavailable` | `authd` or init could not be reached | no |
| `failed` | authorised, but the write or the live step failed; the text says which and what is left | the file is either the old one or the new one, never half |
| `unsupported` | an unknown request name or a newer protocol version | no |

A secret is zeroed in every buffer that held it, as `authd`'s client does.

## 4. The requests

Each request names its **area**, and the area is the `authd` service that
authorises it: `settings.<area>`. An area groups what one Settings page
changes, as polkit groups actions, so an administrator can loosen "names
and clocks" without loosening "accounts".

| Request | Fields | Validation | Writes | Live effect | Area | Row |
|---|---|---|---|---|---|---|
| `set-hostname` | name | 1-64 bytes; dot-separated labels of 1-63 ASCII letters, digits and `-`, no label starting or ending with `-` (RFC 1123) | `/etc/hostname` (`name\n`, 0644) | `sethostname` | `hostname` | **B1 (built)** |
| `service-enable-persistent` | unit, enabled | the unit is one of the compiled list a Settings page may switch: `sshd.service`, `sshd.socket` | init's `enable`/`disable` (links under `/etc/ferrix/units`) | none: next boot (`now` is B6's to add) | `services` | **B1 (built)** |
| `set-time` | seconds since the epoch | within 2000..2200 | none | `clock_settime(CLOCK_REALTIME)` | `time` | B3 |
| `set-timezone` | zone name | a file under `/usr/share/zoneinfo` by name, no `..`, no `/` at the start | `/etc/localtime` link, `/etc/TZ` | none | `time` | B3 |
| `user-add` | name, full name, type (standard/administrator) | name as `useradd`'s; not taken; uid from the free range, never reused; never uid 0, a system account or the auth store | `/etc/passwd`, `/etc/group` (both atomic, one lock) | none | `users` | B4, design review first |
| `user-remove` | name | exists, uid >= 1000, not the caller's own last administrator | the same | none | `users` | B4 |
| `user-set-type` | name, type | as above; never removes the last member of `wheel` | `/etc/group` | none | `users` | B4 |
| `user-auto-login` | name or none | exists, uid >= 1000 | sessiond's file (B4 names it) | none | `users` | B4 |
| `set-boot-word` | key, value or none | a closed list of keys and values generated at build time from the **`present` profile** (`tools/common/data/profiles/present.toml`), with a check that fails when they disagree; **no key whose permitted value is `*`** (`ferrix.init`, `ferrix.root`, `ferrix.onexit`, `console`, `drm.edid_firmware`) can be set; "none" only for a key whose default is the reference value; no permit table, no request. So a Settings page sets no boot word `present` does not permit | `\FERRIX\CMDLINE.TXT` on the EFI system partition, the one key replaced, every other word kept; B5's design says what the loader does with a truncated or missing file after a crash | none: next boot | `boot` | B5, design review first |
| `service-enable-persistent` for `sshd` | as B1 | as B1 | as B1 | B6 adds `now` (start or stop as well) | `services` | B6 |

Rules every request keeps:

* **No request names a path.** Paths are constants of the request's code.
* **No request runs a program.** A live step is a system call or a record
  to init; `settingsd` never `exec`s.
* **A request changes one thing.** Pages that change two settings send two
  requests and get two audit records.
* **Reading is not a request.** The current hostname, the enabled units,
  the users are world-readable already; the app reads them itself. A dry
  run (§7) is how the app asks "what would this do".

## 5. Authorisation, through `authd`

`settingsd` decides nothing about people. For each request:

1. **Root asks nothing.** A peer whose uid is 0 is authorised without
   `authd`, as root's `su` asks nothing: root can write the file anyway. The
   audit record says `auth=root`.
2. **Otherwise the peer's account** is the `/etc/passwd` name of its
   `SO_PEERCRED` uid. A uid with no account is refused (`not-permitted`).
3. **`settingsd` checks who listens at `authd`'s socket**: uid 0 (init's
   activation) or 90 (`auth`), as `su` does (U1), before sending a byte.
4. **It runs one `authd` conversation** for service `settings.<area>`,
   naming the peer's account, and relays each PROMPT, INFO and ERROR to the
   client and each RESPOND back. A client that does not answer within
   120 s is cancelled.
5. **The verdict must be ACCEPTED for exactly the peer**: its uid and its
   account (U3). Any other verdict, or an ACCEPTED for anyone else, refuses
   the request; nothing is written.

The shipped policies, one per area, in `/lib/ferrix/auth/services/`:

```ini
# settings.hostname (and settings.services, settings.time, settings.users, settings.boot)
[Service]
Description=Change the computer's name in Settings
Account=any        # settingsd names the person who asked
Callers=root       # only settingsd (root) may ask; a user cannot use it directly
TargetGroup=wheel  # an administrator, asked before any password
Methods=password   # the person's own password, every time
FailDelaySec=2
```

**"A password prompt when the policy says so."** `authd` always asked for
a password. B1 adds one value to its policy syntax, **`Methods=none`**: the
conversation is ACCEPTED with no prompt. It is bounded **by name and by
place** (the consultant's condition 1):

* **By name and shape.** Only a service whose name starts with
  `settings.`, and whose merged policy says `Callers=root`, a
  `TargetGroup=`, `Purpose=authenticate` and no `Grant=` or
  `FirstPassword=`. Anywhere else it is warned about and read as
  `password`, the stricter reading, as every unreadable policy value is. A
  later root mechanism of the same shape is not loosened by accident, and
  the lock screen, `login`, `su` and `passwd` never are.
* **By place.** The acceptance is issued where the `Password: ` prompt
  would be sent: after the caller, the target, the group, the throttle and
  the account's record (damaged, locked, no password) have been checked,
  and only for an account that exists. `authd`'s audit line says
  `method=none`.

It means **any process of a group member, with no check that a person is
there** (`docs/AUTH.md` §4.2, TA.CLIENT). An administrator who wants polkit's "administrators without a
password" for, say, the time writes a drop-in:

```ini
# /etc/ferrix/auth/services/settings.time.d/10-no-password.conf
[Service]
Methods=none
```

Since `authd` reads policy on every conversation, that takes effect on the
next request without restarting anything. The shipped default for every
area is `Methods=password`.

What B1 does not do: **no caching** of an authorisation (polkit's
`auth_admin_keep`): each request is a conversation, so a page that changes
three things asks three times unless its area says `Methods=none`. A
"keep for five minutes" would need a token bound to the session, which
`authd` deliberately does not issue (`docs/AUTH.md` §3.3); it is a decision
for later, not a default.

## 6. Writing

* **One writer at a time.** Connections are served on threads of their
  own (at most 8, and 2 for any one uid; past either the answer is
  `unavailable`), so a prompt waiting
  on one person does not hold up another; the apply step takes one lock.
* **Atomically.** `write_atomic(path, bytes, mode)`: a temporary file in
  the same directory (`O_CREAT|O_EXCL`, the mode set before any byte),
  write, `fsync`, `rename` over the target, `fsync` the directory. A crash
  leaves the old file or the new one.
* **Checked against the system at apply time**, not only at validation:
  `set-hostname` reads the old name for the record; `user-remove` (B4)
  re-reads `/etc/passwd` under the lock.
* **Then the live effect**, which may fail after the file is written. The
  answer is then `failed` with the text saying the file was written and
  what was not applied; the audit record says the same.

## 7. The client, the UI and dry runs

`ferrix-settings-client` (`src/user/system/linux/settingsd/client`), which
the Settings app reaches by path as it reaches tole:

```rust
use ferrix_settings_client::{Client, Request, Step, Refusal, RefusalKind};

let request = Request::SetHostname { name: "box".into() };
request.validate()?;                         // the same rules settingsd applies; no socket
let mut talk = Client::new().begin(&request, false)?;  // false: really do it
loop {
    match talk.step() {
        Step::Prompt { visible, text } => talk.answer(read_password(&text, visible))?,
        Step::Info(text) => show_under_the_field(&text),
        Step::Done { changed, text } => break show_done(&text),
        Step::Refused(refusal) => break show_refusal(&refusal),
    }
}
```

* **Step-wise**, so a tole app can keep drawing: `Conversation` is
  `AsRawFd`, and `step()` returns at each record. `Client::apply(request,
  &mut person)` is the blocking form for command lines, with `authd`'s own
  `Person` trait answering (`settingsctl` uses it with `Terminal`).
* **Errors in the UI.** `Refusal { kind, text, retry_after_ms }`;
  `kind.title()` is the heading ("Not allowed", "Wrong password", ...), and
  `text` is the line under it, written for a person by `settingsd` or
  `authd` ("ferrix is not in wheel"). `authentication-failed` with a
  `retry_after_ms` greys the button out for that long.
* **Dry run.** `begin(&request, true)` sends the request with the dry-run
  flag: `settingsd` validates it, reads the system, and answers DONE with
  what it would do ("would call the computer box; it is called ferrix")
  and `changed` saying whether anything would change. A dry run asks no
  one, writes nothing, records nothing, and is allowed to anyone, since it
  reveals only what the files it reads already show. A page uses it to
  grey out "Apply" when nothing would change and to show the effect
  before the password is asked.
* **Who listens.** `Client::new()` refuses to send anything to a listener
  that is not uid 0 (`settingsd`, or the init that activated it), so a
  server a user started is never told a password; `Client::at(path, uid)`
  names another, for tests. `begin_as_given` skips the client's own check
  of the request's rules, so that `settingsd`'s refusal is what answers:
  for gates.
* **`settingsctl`**, a command line over the same crate:
  `settingsctl [--dry-run] [--as-given] set-hostname NAME`,
  `settingsctl [--dry-run] [--as-given] service-enable-persistent UNIT on|off`. It prints
  `settingsctl: done: TEXT`, `settingsctl: would: TEXT` or
  `settingsctl: refused: KIND: TEXT`, and exits 0, 0 or 1. Scripts and the
  gate use it; the password comes from the terminal or one line of
  standard input, as `authctl try` takes it.

## 8. The audit record

One line per request that reached authorisation, on `settingsd`'s
standard output, which init attributes to `settings.service` and hands to
lode (`docs/LOGGING.md` §5; `svc log settings.service` reads it back, from
init's own ring when lode is not running):

```
settings: request=set-hostname peer-pid=678 peer-uid=1000 comm=settingsctl account=ferrix auth=password result=done before=ferrix after=box
settings: request=set-hostname peer-pid=662 peer-uid=1001 comm=settingsctl account=guest auth=- result=not-permitted before=- after=-
```

`auth` is `root`, `password` (a question was relayed), `none` (accepted
with none) or `-`; `comm` is the asking process's name, root's included.
Every value is escaped to one field. It never holds a secret. Refusals for `invalid` and dry runs are not
recorded: nothing was asked and nothing changed.

## 9. Building on it: what B1 built and what each row adds

**B1 built** (2026-10-24):

* `src/user/system/linux/settingsd/`, a workspace of its own with the
  auth workspace's lints: `proto` (`ferrix-settings-proto`: the records,
  the `Request` enum and its rules, pure, host-tested), `client`
  (`ferrix-settings-client`), `settingsd` and `settingsctl`, and
  `units/settings.{socket,service}`.
* `authd`: `Methods=none` as §5 bounds it, with host tests; the two
  policies `settings.hostname` and `settings.services` in
  `src/user/system/linux/auth/services`; `talks_to_authd` (U1) moved into
  `ferrix-auth-client`, so `su` and `settingsd` share one check.
* xtask: `settingsd::carried(arch)` in the desktop's images, beside authd;
  `cargo xtask test-settings`; `check`'s `settingsd` unit; `gate-rows`.

**Adding a request** (B2-B6), all in `src/user/system/linux/settingsd/`:

1. `proto/src/request.rs`: a variant of `Request` with typed fields, its
   wire name in `Request::NAMES` and `name()`, its `validate()` rules, its
   `area()` (a new `Area` variant and its `service()` if the page is new);
   `proto/src/record.rs`: its encode and decode arms; `proto/src/tests.rs`:
   its round trip and its rules each way.
2. `settingsd/src/requests/<name>.rs`: `plan(...) -> Result<Plan,
   Refusal>`, which reads the system and fills `Plan { action, changed,
   would, before, after }` (the dry run's text, the audit's before and
   after), and `apply(...) -> Result<String, Refusal>`, which writes with
   `atomic::write_atomic` and makes the live call. An `Action` variant and
   an arm in each of `requests::plan` and `requests::apply`.
3. A new area's policy, `src/user/system/linux/auth/services/settings.<area>`
   (`Account=any`, `Callers=root`, `TargetGroup=wheel`, `Methods=password`),
   and its line in authd's `the_shipped_policies_read_cleanly`.
4. `settingsctl`'s verb in `parse`.
5. Its lines in `test-settings`: refused for `guest`, done for `ferrix`,
   the file checked; and a control for each new refusal.

### 9.1 How it is tested

* **Host tests** (`cargo xtask check`'s `settingsd` and `auth` units):
  every record round-trips and every cut, extended or damaged one is
  refused; each validation rule has a case each way; `settingsd`'s
  conversation runs over a socket pair against a scripted authority and a
  scripted init, and shows that ACCEPTED for another uid or another name,
  FAILED, UNAVAILABLE, an unreachable `authd`, a client that cancels or
  leaves, and an invalid request (refused before anyone is asked) all
  write nothing; that a dry run asks nobody and writes nothing; that BEGIN
  names the account of the peer's uid; and that the password is in no
  audit line and no answer. The client refuses a listener of the wrong
  uid before sending a byte. `write_atomic` leaves no temporary file.
  `authd`'s tests show `Methods=none` honoured only where §5 says, a
  `Methods=none` drop-in on `login`, `su`, `passwd` and `hyprlock` still
  prompting, locked, password-less and non-member accounts still refused,
  and settings failures counting against the throttle `su` shares.
* **`cargo xtask test-settings`** boots init, `authd` and `settingsd`
  with three accounts: `root`, `ferrix` (uid 1000, in `wheel`) and `guest`
  (uid 1001, not), both with seeded passwords. At the console: `guest` is
  refused `set-hostname` and `service-enable-persistent` with its own right
  password, and `/etc/hostname`, `hostname` and the links are unchanged;
  `ferrix` with a wrong password is refused; `ferrix` with the right one
  renames the machine (`/etc/hostname` and `hostname` agree) and enables
  `sshd.service` (the link exists); a unit off the list, sent as given, is
  refused `invalid` by `settingsd`; a dry run as `guest` changes nothing; a
  `Methods=none` drop-in on `settings.hostname` lets `ferrix` in with no
  prompt and still refuses `guest`, and one on `gate-mechanism`, a root-only
  wheel service that is not a settings one, still asks for the password;
  the audit lines are in `svc log settings.service` and hold no password.
* **Negative controls** (condition 4), each `gate.sh control
  --expect-line` with that step's own sentence, the edit undone after:

  | | Edit | Sentence it must make |
  |---|---|---|
  | N1 | `settingsd/src/conversation.rs`: `if peer.uid == 0 {` becomes `if peer.uid < u32::MAX {`, every peer root | `guest, outside wheel, was not refused with its own password` |
  | N2 | `settingsd/src/authority.rs`: FAILED taken as ACCEPTED | `ferrix's wrong password was not refused` |
  | N3 | `proto/src/request.rs`: the unit list skipped | `settingsd did not refuse a unit off its list` |
  | N4 | `auth/authd/src/policy.rs`: `Methods=none`'s bound removed | `Methods=none loosened gate-mechanism` |

  All four **FIRED** (`control: FIRED (line, no panic)`) on d549d2e92,
  2026-10-24, logs `~/.local/share/ferrix/logs/queue/settings-b1-n{1,2,3,4}.log`
  on the gate host; the same commit's `test-settings --arch x86_64`
  passed unedited.

  No sabotaged build of `settingsd` exists: a control is a source edit
  that `gate.sh` makes and undoes, so there is nothing for xtask to refuse
  to package.

## 10. Certification

`settingsd`, its client and `settingsctl` are ring-3 programs outside the
certified item (`tools/common/data/certification-item.json`); `authd` is
too. The review is asked for because the helper writes accounts and boot
words (B4, B5). What it must keep:

* **No upward reference.** Nothing in the item learns of `settingsd`.
* **The certified configuration stays reachable and is never left
  silently.** B5's `set-boot-word` refuses a value the `present` profile
  does not permit; leaving the reference configuration is not something a
  Settings page can do.
* **`authd`'s refusals keep their negative controls**; `Methods=none` is a
  new loosening and its bounds have tests.

**The review** (design before code, 2026-10-24, ledger line 3482):
**OK IF (1)-(9)**. No ring hit (`item-hits.py`: 0 core, 0 item); no finding,
no requirement id. The relayed password is accepted on `login`'s and
`su`'s terms: `settingsd` acts on ACCEPTED itself, so `authd`'s "no
token" stands. Its conditions, and where each is met:

1. `Methods=none` bounded by name and place, with host tests, and stated
   in `docs/AUTH.md` (§5 here; authd's `policy.rs`, `engine.rs`, `tests.rs`).
2. The authorisation chain as `login` and `su` have it: the shared U1
   check, U3, the secret in `Secret` and in no log (§5, §9.1).
3. Settings failures share the account's throttle, and TA.CLIENT names
   `settingsd` (`docs/AUTH.md` §2.2; authd's
   `settings_failures_share_the_accounts_throttle`).
4. Controls N1-N4 through `gate.sh control` (§9.1).
5. Dry runs and refusals write nothing (§9.1); B4's and B5's dry runs are
   checked when those rows come.
6. **B4 and B5 come back for a design review before code**; B3 and B6
   may land on a code review alone.
7. B5's bounds (§4's table).
8. This section says that `set-boot-word` **sets no boot word `present`
   does not permit**; it does not make the machine certified, which the
   kernel's build line and the audit SYSTEM record show. When B5 lands,
   `docs/certification/SAFETY-MANUAL.md` AoU-8 names `settingsd` as a
   writer of `CMDLINE.TXT`, reachable by wheel members under
   `settings.boot`; wheel membership and any `Methods=none` drop-in are the
   integrator's to control.
9. Rows `check`, `test-settings` and `test-init --arch all` PASSED, the
   landing message citing ledger line 3482.

Lesser points taken: one uid holds at most two conversations at once
(C1); the audit line names root's pid and program too (C2). A polkit-style
"keep for five minutes" stays out unless designed as a session-bound grant
`authd` issues, with its own review (C3).

## 11. Where it stands

2026-10-24: **B1 built** on `feature/settings-writer`: the design review
is OK IF (1)-(9), ledger line 3482; `set-hostname` and
`service-enable-persistent` work end to end in `test-settings`.
B3 to B6 build on §9's recipe; B4 and B5 start with a design review.

2026-10-24: **B2 built**: init sets the running name from `/etc/hostname`
once `/` is settled, beside `/etc/machine-id`
(`src/user/system/linux/init/init/src/hostname.rs`). It reads at most 256
bytes and takes the first line, trimmed, and only a name
`ferrix-settings-proto::valid_hostname` takes -- the function
`set-hostname` is checked with -- is set; a missing, empty or invalid file
leaves the kernel's default name and one line, "the computer keeps its
default name: ...", and never stops the boot. `test-settings`' image is
called `settings-gate`, and its first step requires the file and
`hostname` to agree; its control (the call removed) FIRED. Code review OK
IF (1)-(4), ledger line 3686, on design review 3636 (18).

2026-10-24: **the trusted prompt and the per-area lists built** (§12):
design OK IF (1)-(20), ledger 3636; code OK IF (A1)-(A8), ledger 3779.
The customer answered 3636 (3) on 2026-10-24: the password relay for
accounts with no seat stays, as built (§12.7).

## 12. The trusted prompt and the per-area allow-lists

The customer's review fix of 2026-10-24, about 02:40: **before `settingsd`
acts on a request from an app, a prompt the requesting app cannot draw or
fake asks the person to confirm, and each area has a list of the programs
allowed to ask for it at all.** Designed against B1 as built; the
consultant's design review is §12.10.

### 12.1 What it closes

B1 relays `authd`'s questions to the client (§5 step 4). So the program
that asked draws the password field and reads the password, and the
consultant's residual (ledger 3482 (a), `docs/AUTH.md` TA.CLIENT) is that
any process of a wheel member's uid can drive `settingsd` with a phished
password, or with none under `Methods=none`, and nobody need be at the
screen. Three gaps, each closed below:

| Gap | Closed by |
|---|---|
| the requesting program sees the password | the question is put in the prompt, which hyprix draws; the program sees only the outcome (§12.3) |
| no one has to be there (`Methods=none`, a password the program already has) | a change asked from the desktop's account is made only once a person answers the prompt with the seat's own keyboard or pointer (§12.4 P3) |
| any program may ask for any area | each area names the programs that may ask; any other is refused before anything is shown (§12.5) |

### 12.2 How others do it, and what is taken

* **polkit and its agents.** The mechanism (here `settingsd`) asks
  `polkitd`, which asks the *session's registered agent* to authenticate;
  the agent is a session process (gnome-shell, `polkit-kde-agent`), so the
  calling app never sees the password. Identity is the caller's
  `unix-process` subject, pid and start time, which CVE-2013-4288 and
  CVE-2021-4034's family showed is racy when read by pid. *Taken:* the
  mechanism never draws or reads; the session's own trusted component
  asks. *Not taken:* a registrable agent (any session process could
  register one on a bus); here there is one asker, wired by `sessiond`.
* **GNOME Shell's system prompts** (`ShellPolkitAuthenticationAgent`,
  gcr's `SystemPrompter`): drawn by the compositor process itself as a
  system-modal dialog, with a pointer and keyboard grab and the screen
  dimmed. A fullscreen client can still paint a look-alike, which GNOME
  accepts. *Taken:* the compositor draws it and holds the input while it
  is up.
* **macOS TCC** (`tccd`, prompts drawn by `UserNotificationCenter`, a
  system process): the requester is named by its **code signature taken
  from the kernel's audit token** of the message, not by anything the app
  says or its pid; the prompt's text is the system's, with the app's
  stated reason quoted as such. Password fields use *secure event input*,
  which no other process can read. *Taken:* the requester named by the
  kernel's record of the connection, the text written by the system;
  input no client can read.
* **Windows UAC on the secure desktop**: consent is shown on a desktop
  no user-mode app of the session can draw on or send input to, over a
  dimmed still of the screen; the **secure attention sequence**
  (Ctrl+Alt+Delete), which no app can intercept, is the trusted path to
  the real credential prompt ("require trusted path for credential entry").
  *Taken:* while a prompt is up nothing of any client is drawn or receives
  input, and synthetic input is not input; and one key combination only
  hyprix answers, which tells a real prompt from a picture of one.
* **Firefox's `security.dialog_enable_delay`** (1 s): a dialog's buttons
  ignore input for its first second, so a click or key aimed at what was
  there before cannot answer it. *Taken* as P5.

### 12.3 Who draws it, and how the request reaches it

**hyprix draws it**, as part of its own frame, not as any client's surface.
In a session hyprix runs as the person's uid (`docs/AUTH.md` §6), but it is
the one process of that uid that already sees every key and owns every
pixel; a client cannot write to it, read its memory (no `ptrace`, no
`/proc/<pid>/mem`) or reopen its inherited sockets (§3.7).

```
settingsd (root)                    sessiond (root, the seat's owner)          hyprix (uid 1000)
  peer is the seat's account?
  connect /run/ferrix/prompt -----> listener 0600 root, peer uid must be 0
                                    "seat <uid>" for the session's account
  ASK id, area, program, what ----> uid is the session's? ----fd 5------>  draws the prompt on every screen
  (authd's PROMPT "Password:")      relays, line for line                  physical keys and pointer only
  QUESTION id, visible, text -----> -------------------------------------> a field in the prompt
                                                                  <------  ANSWER id secret | ALLOW id | DENY id why
  <------------------------------- relays, and nothing of its own
  RESPOND to authd; on ACCEPTED: apply, audit
  CLOSE id, outcome --------------> --------------------------------------> the prompt goes
```

* **settingsd to sessiond** on a `SOCK_SEQPACKET` socket at
  `/run/ferrix/prompt`, mode 0600, owned by root, which `sessiond` binds
  when the session starts and removes when it ends. Each side checks the
  other by `SO_PEERCRED`: `sessiond` serves only uid 0, `settingsd` talks
  only to a listener of uid 0 (U1's shape). One connection carries one
  prompt.
* **sessiond to hyprix** on a third socket pair, descriptor 5
  (`compositor_seat::prompt`, `FERRIX_PROMPT_FD`), beside the devices' 3
  and the lock's 4 and made the same way: no path, close-on-exec in hyprix
  at once, not reopenable through `/proc`. A separate pair, so §3.7's rule
  that any line on 4 but `locked`/`unlocked` ends the session stands.
* **`sessiond` relays and decides nothing about the change**: it checks
  that the uid `settingsd` names is the session's account, keeps one prompt
  at a time (a second is answered `busy`), and passes each line to the
  other side unchanged. What it takes from hyprix is only `ALLOW`, `ANSWER`
  or `DENY` for the open prompt's id; anything else from hyprix ends the
  prompt as denied.
* `authd` is unchanged: `settingsd` still runs the conversation and acts on
  ACCEPTED itself (§5); only the *person* it relays to changes. The
  password now passes through `sessiond` and hyprix, both of which see the
  keyboard anyway, and never through the requesting program.

### 12.4 The prompt's rules, in hyprix

* **P1. Its own frame.** While a prompt is up, every screen is drawn
  from hyprix alone: the background colour, and the prompt centred on each
  screen. No window, layer surface, popup, lock surface, drag icon or
  client cursor is drawn (the screen is drawn as a lock with no lock
  surface, §3.7, plus the prompt); the pointer is hyprix's own arrow. So no
  client can draw on, beside or over it, and nothing a client drew is in
  view to dress it up.
* **P2. Its text is `settingsd`'s.** The heading names the program as the
  kernel recorded it (P-S2 below: its executable's path and the one that
  started it), and the body is the dry run's own `would` line ("would
  call the computer box; it is called ferrix") with the account's name.
  Nothing in it is the requesting program's words.
* **P3. Physical input only.** Keys and pointer events from the seat's
  devices go to the prompt and to nothing else: no bind fires (a
  `bindl` on a letter would be a keylogger written into the user's own
  `hyprland.conf`), no client is sent a key or a button, and the keyboard
  leaves every client while it is up. What `zwp_virtual_keyboard_v1`,
  `zwlr_virtual_pointer_v1`, `sendshortcut` and `pass` inject is dropped
  while a prompt is up and never reaches it.
* **P4. Answers.** With a question: the field takes the typed characters
  (the seat's layout), Backspace, and Return or **Allow** answers with it;
  with none (`Methods=none`, or root's own requests never come here),
  **Allow** or Return. Escape or **Cancel** denies. The secret is held in
  one buffer, zeroed when sent or the prompt goes, and never drawn (dots).
* **P5. A second before it answers.** Input in the prompt's first second
  is ignored, and a key or button counts only when it went down after the
  prompt was shown, so Return or a click aimed at a window cannot answer a
  prompt that came up under it.
* **P6. Not in any picture.** While a prompt is up every screenshot
  (`zwlr_screencopy`, `ext-image-copy-capture`, lode's crash screenshots)
  is refused or blacked out whole, so no program can read the prompt or
  the dots, or capture a real one to imitate it.
* **P7. Not over a lock, not without a screen.** A prompt asked for while
  the session is locked, or with no screen on, is denied at once (`DENY id
  locked`); one up when a lock is taken is denied.
* **P8. The trusted key.** **Ctrl+Alt+Delete** is hyprix's alone, at all
  times: never a bind, never sent to a client. With a prompt up it does
  nothing but show that the prompt is real (the prompt's footer says so);
  with none, hyprix shows for three seconds, drawn as P1, "No system
  prompt is waiting — a window asking for your password now is not
  Ferrix". So a person unsure of a dialog presses the key: a real prompt
  stays, a picture of one is replaced by hyprix's own screen. Every prompt
  says "Ctrl+Alt+Delete shows that this prompt is real" in its footer.
* **P9. One at a time, and ended by its owner.** One prompt is up at once.
  `CLOSE` from `settingsd` ends it; so do a `DENY`, two minutes with no
  answer (`settingsd`'s §5 timeout), the session's end, and hyprix's
  channel closing.

### 12.5 The rules, in `settingsd`

The order of the checks for a request (`conversation::answer`), each
before the next:

1. **S1. Valid** (§4), as B1: refused `invalid`, unrecorded, nothing
   shown.
2. **S2. Root asks nothing**, as B1. A root peer is neither listed nor
   prompted: root could write the file itself.
3. **S3. On the area's list.** The requester is the executable of the
   peer's pid as `SO_PEERCRED` recorded it at `connect`, read by root from
   `/proc/<pid>/exe` with the pid's start time (`/proc/<pid>/stat` field
   22) at accept, and read again once the request has arrived: a pid that
   is gone, whose executable or start time changed, or that started after
   the accept is refused (ledger 3636 (4)). The parent's executable is
   read beside it for the prompt's text. The area's list names the
   programs, by absolute path, that may ask for it; any other is refused
   `not-permitted` ("… may not ask to change the computer's name"),
   recorded `result=not-listed`, with nothing shown. The lists are image
   content, one file `/lib/ferrix/settings/apps` (`area = /bin/settings
   /bin/settingsctl`, a line an area), read on every request as authd's
   policies are. An entry counts only if it is an absolute path to a
   root-owned file that is not group- or world-writable, in directories
   that are likewise; any other entry is ignored with a warning. A line
   that does not parse makes the file allow nothing, and a missing or
   unreadable file, or an area it does not name, allows no program (fail
   closed). Shipped: `hostname`, `time` and `services` each allow
   `/bin/settings` and `/bin/settingsctl`.
4. **S4. The seat's accounts ask on the seat, from the image's word, not
   the socket's** (ledger 3636 (1)). `settingsd` reads which accounts have
   the seat from the configuration `sessiond` is started from: the
   `hyprix.service` unit (`/etc/ferrix/units`, else `/lib/ferrix/units`),
   whose `ExecStart=` runs `sessiond --user NAME`. Then:
   * **a seat account** is served only through the prompt: `settingsd`
     connects to `/run/ferrix/prompt`, and the conversation's person is
     the prompt (§12.3); the client is sent only `INFO` ("confirm on the
     screen") and the end. With no session of it up (no socket, or one
     whose session is another account's) it is refused `unavailable`,
     "confirm on the desktop; no desktop is running". A listener that is
     not uid 0, one that is busy or silent, a broken line: refused
     `unavailable`. **A seat account is never relayed to its client**, so
     killing the compositor (§6.4, E6) leaves a program nothing to ask.
   * **an account the image gives no seat** (a console or ssh user on an
     image whose desktop is another account's, or that has none) is
     relayed to its client as B1 does, the password typed at a terminal
     as `su`'s is; but **a relayed ACCEPTED with no question** (`Methods=none`)
     is refused `not-permitted`, "needs the desktop: this area asks no
     password, so it must be confirmed on the seat", recorded
     `result=needs-seat` (ledger 3636 (2)). The relay never applies a
     change nobody typed anything for.
   * **a configuration that cannot be read** (the unit there but
     unreadable, or its `ExecStart=` naming sessiond without an account)
     refuses every non-root request `unavailable`.
5. **S5. The conversation**, as §5, with the prompt as the person.
   `authd`'s PROMPT opens the prompt with its question; an ACCEPTED with
   no question (`Methods=none`) opens it then with Allow and Cancel and
   waits for the answer. A refusal before any question (not in the
   group, a locked account) shows no prompt. Only after ACCEPTED *and* the
   prompt's answer does `settingsd` apply.
6. **S6. After a Cancel, a pause.** A person's Cancel refuses that program's
   requests for the same area for 30 s (`cancelled`, "you said no a
   moment ago"), so a program cannot put the prompt back up as fast as
   it is dismissed.

The audit record (§8) gains `program=` (the requester's path), and
`auth=` gains `seat` beside `password`, `none` and `root`:
`auth=seat+password` when the prompt asked a question, `auth=seat` when it
only confirmed. A refusal by the list is recorded with `result=not-listed`;
a Cancel with `result=cancelled`.

### 12.6 The lines

`compositor_seat::prompt`, one line each way per record, fields
space-separated, each text field percent-escaped (`%20`, `%25`, nothing
below 0x20 unescaped), every line at most 2 KiB. The same records on both
hops; `seat` is the listener's first line on settingsd's.

```
sessiond -> settingsd  seat UID                       the session's account
settingsd -> hyprix    ask ID AREA PROGRAM PARENT ACCOUNT WHAT
                       question ID VISIBLE TEXT       a field, with authd's text
                       info ID TEXT                   authd's INFO or ERROR, under the field
                       confirm ID                     Allow and Cancel, no field
                       close ID TEXT                  the end; TEXT is shown for a second
hyprix -> settingsd    answer ID SECRET               the field's text, with Allow
                       allow ID                       Allow, with no field
                       deny ID WHY                    cancelled | locked | timeout | busy
```

### 12.7 What it leaves

* **A look-alike can still be painted** while no prompt is up, as a fake
  lock screen can (`docs/AUTH.md` §2.3). It gains nothing from
  `settingsd` (only the real prompt's answer counts), but it can phish a
  password that `su` would take. P8 is the trusted path §2.3 asked for:
  it helps a person who uses it.
* **The list is not a boundary against the user's own code.** A program
  of the account can run `/bin/settings` or `/bin/settingsctl`, or drive
  their windows; the list keeps every other program (a browser, a game)
  from raising a prompt at all, and the prompt names who asked and what
  would change. Reading the executable by pid has polkit's race (a
  process that forks and execs after `connect`); its worst case is a
  wrong name on a prompt the person still has to answer.
* **The relay for an account with no seat** is a prompt the terminal
  program draws, not one the requesting program cannot: it is `su`'s
  shape, a password typed at a terminal, and a program of that account
  that has phished the password can drive it as it can drive `su`. It
  never applies a change with no password typed (S4). **The customer's
  decision, 2026-10-24** (ledger 3636 (3)): keep it, as built -- an
  account with no seat is asked at its terminal, as `su` asks, and an
  acceptance with no password typed is refused.
* **Physical input** is what arrives on the evdev descriptors `sessiond`
  opened for hyprix (descriptor 3). Ferrix has no `uinput`; a
  user-creatable input device would need its own review. The machine's
  host -- QEMU's or crosvm's window, a KVM switch -- is in the trusted
  base, as the keyboard is; a VNC or RDP server *inside* the session is a
  client and cannot answer.
* **One prompt at a time** on the seat: a program can keep its own
  request up for two minutes and so hold the seat's prompt; S6 and the
  per-uid cap of two (§6) bound it, and Cancel ends it.

### 12.8 Absent where `settings_writer = absent` (ledger 3558 (13))

What the writer key leaves out of an image, this included: `settingsd`,
`settingsctl`, the `settings.*` policies, `/lib/ferrix/settings/apps`, and
**the prompt itself** -- hyprix's, `sessiond`'s and `compositor_seat`'s
prompt code is the cargo feature `settings-prompt` of each, which xtask
alone sets from the image's writer (ledger 3636 (10)): the image build
refuses an image whose `settingsd` and prompt feature disagree, either
way, and an image check (`settingsd::check_prompt`) finds no `FERRIX_PROMPT_FD` mark
in an absent-writer image's hyprix and sessiond, and finds it in a
present-writer image's. Without the
feature hyprix opens no descriptor 5 and has no prompt to draw; P8's key is
then plain input again; `sessiond` binds no `/run/ferrix/prompt` and makes
no fifth descriptor. xtask's `settingsd::writer()` reads the key from
the image manifest's own source (`flags::this_image`, `docs/BUILD-FLAGS.md`
§5.1).

### 12.9 How it is tested

* **Host tests.** `compositor_seat::prompt`: every record round-trips, an
  over-long, unescaped or unknown one is refused, a secret is in no
  `Debug`. hyprix's prompt (`prompt/tests.rs`): P4, P5 (a key held when
  the prompt came up, and one in its first second, answer nothing), P7,
  P8, P9, P2's escaping (U+202E, U+2028, format and tag characters; the
  asker's paths shown as printable ASCII), the field one fixed block that
  never moves as it fills, nothing of it in `Debug`, the control socket
  refusing a dispatcher, a keyword and a reload while a prompt is up, and
  every reader of a screen's pixels listed, so a new one must be guarded
  (P6). P3 -- injected input, binds, no key to a client -- is the boot's,
  through T1 and T8. `sessiond`'s relay: only the session's account, one
  prompt at a time, an answer only for the open prompt, anything else
  ending it denied. `settingsd`: S3 (a program off the list, or not the
  one that connected, refused before anyone is asked), S4 (a seat account
  with no desktop, a busy seat, a listener that is not root and a session
  unit that cannot be read -- a drop-in, a mask, a wrapped `sessiond` --
  each refused before `authd` is asked, never relayed), S5 (ACCEPTED with
  no question waits for Allow; a Cancel or a denied field makes nothing),
  S6, a relayed acceptance with no question refused `needs-seat`, an
  unknown request `unsupported` with words, and the client's words when
  there is no `settingsd`.
* **`cargo xtask test-compositor --boot settings-prompt`**, a session
  desktop as `ferrix` (wheel) with authd, settingsd and sessiond; the
  person's keys are QEMU's. A bind runs `/etc/ask.sh`, a program of
  `ferrix`'s, which runs `settingsctl` and, while its prompt is up, types
  the password and Return with `vkbd` and takes a screenshot with `shot`.
  Required: the prompt comes up; a bind pressed while it is up does not
  fire; QEMU's own screendump shows the prompt's background at every
  corner (no client); the injected keys answer nothing within ten
  seconds; the screenshot is black; a wrong password typed in it is
  refused and changes nothing; the right one renames the machine; in an
  Allow-only prompt (`Methods=none` for `settings.services`) Return in its
  first second is nothing and Return after it turns sshd on; Escape in one
  makes nothing; `/tmp/unlisted` (settingsctl's bytes at another path) is
  refused with no prompt; `/proc/<hyprix>/fd/5` cannot be opened ("No such
  device or address"); Ctrl+Alt+Delete with no prompt up is hyprix's.
* **`test-settings`** stays the console's test of B1's relay path for an
  account with no seat, with two more lines: a program off the list
  refused, and a `Methods=none` relay refused (`needs-seat`).
* **Negative controls**, each through `fleet/gate.sh control` with its
  step's own sentence (T5, T9 and TF on the host):

  | | Edit | Sentence it must make |
  |---|---|---|
  | T1 | hyprix: injected input reaches the prompt | `an injected key answered the trusted prompt` |
  | T2 | settingsd: the seat routing skipped | `a request from the desktop was answered without the trusted prompt` |
  | T3 | settingsd: the list check skipped | `a program off the list was not refused` |
  | T4 | hyprix: screenshots not blacked out | `a screenshot showed the trusted prompt` |
  | T5 | settingsd: a seat account relayed when its desktop's socket is gone (host, `check-side`) | `a_seat_account_with_no_seat_to_ask_is_refused ... FAILED` |
  | T6 | settingsd: the needs-seat refusal removed | `a Methods=none relay changed the computer's name` |
  | T7 | hyprix: the frame not the prompt's while one is up | `a client was drawn while the trusted prompt was up` |
  | T8 | hyprix: input goes the usual way while a prompt is up | `a bind fired while the trusted prompt was up` |
  | T9 | hyprix: P5's first second removed (host, `check-side`: the boot's Return lands after the first second) | `input_before_it_is_armed_or_held_from_before_answers_nothing ... FAILED` |
  | T10 | settingsd: applies after a Cancel of an Allow-only prompt | `a cancelled prompt changed something` |
  | TF | xtask: hyprix built without the prompt on a present-writer image (host) | `has no trusted prompt, but this image's settings writer is present` |

  The other direction of TF, the feature planted on an absent-writer
  image, waits for the first `max` or `fast` image that builds
  (`docs/BACKLOG.md`); until then the host test
  `a_prompt_that_disagrees_with_the_writer_is_refused` holds both.

### 12.10 The design review

Design review before code, 2026-10-24: **OK IF (1)-(20)**, ledger line
3636 (`~/.local/share/ferrix/cert-consultant/reviews.md`). S4's `ENOENT`
fallback is replaced by routing from the image's seat configuration, with
no passwordless relay ((1)-(2)); whether the console/ssh relay stays is
the customer's question ((3)); controls T1-T10 ((11)). §13: OK IF
(14)-(16). B2, B3 and B6 may land on a code review alone under (17)-(20).
Internal review, not independent verification (F-27).

Code review, 2026-10-24: **OK IF (A1)-(A8)**, ledger line 3779 (verdict
3782): the rows and T1-T10, TF FIRED on the landing hash; condition (3)
answered by the customer on 2026-10-24 (keep the relay, §12.7); the field one block never grown
(A3); `shown()` escaping every invisible character and the asker's paths
as printable ASCII (A4) -- the values inside the dry run's line are each
bounded by its request's validator (a host name's letters, a zone's name,
a unit off the compiled list), so they are shown as they are; the session
unit read as init reads it or refused (A5); the host tests above (A6);
TF's other direction in `docs/BACKLOG.md` (A7); these docs (A8).

The conditions this section does not already state in its own words:

* **(5) The prompt's text.** Every field the requester or its parent
  influences (the parent's path, the values inside the dry run's line) is
  shown quoted, control and bidirectional characters escaped, cut to a
  length; the line is built from the same decoded request that is applied,
  one request a connection.
* **(6) Not configurable.** No configuration key, `hyprctl keyword`,
  dispatcher or plugin reaches P1-P9; a reload that changes the layout,
  the devices or the outputs waits until the prompt closes; dispatchers
  that move focus, send keys or answer are refused while it is up; the
  control socket and the event stream show at most that a prompt is up,
  never the field's length; Ctrl+Alt+Delete is matched before the binds
  and before any client.
* **(7) Every pixel path** -- screencopy, image-copy-capture, lode's
  screenshots, the frame dumps -- is black whole while a prompt is up and
  on the frame it closes on, and a host test lists the paths, so a new one
  must join it.
* **(8) The secret** is held in a zeroing buffer in hyprix and `sessiond`,
  line and relay buffers included; hyprix is not dumpable
  (`PR_SET_DUMPABLE` 0) from the prompt's opening until its buffer is
  zeroed; no trace or log line written while it is up records keys or
  their count; descriptor 5 is close-on-exec, never passed on, and
  reopening `/proc/<hyprix>/fd/5` is `ENXIO`, as descriptor 4's is.
* **(11) Controls** T5 (a seat account relayed while the desktop is down
  or the socket gone), T6 (a `Methods=none` relay applied), T7 (a client
  drawn while the prompt is up, read from QEMU's own screendump on the
  host), T8 (a key or bind reaching a client or firing while the prompt is
  up), T9 (P5's delay removed) and T10 (`settingsd` applying after a
  `deny`), beside T1-T4, each through `fleet/gate.sh control` reading
  FIRED.
* **(12) Rows**: `check`, `test-compositor --boot settings-prompt`,
  `test-settings`, `--boot hyprlock-session` and `--boot session-end`.
* **(13) Docs**: `docs/AUTH.md` TA.CLIENT rewritten for this residual,
  §2.3 and §6.5 saying the trusted key exists, §4.5 saying phase 3's
  privilege prompts reuse this one.

## 13. The image manifest: a page fixed by its build

Ledger 3558 (14): a Settings app "reads a machine-readable image manifest
(profile, runtime_settings, settings_writer) and shows such pages
read-only, 'fixed by this build (profile max)'". The flag and the key are
another agent's (the `ferrix-kernel-settings` crate, `runtime_settings` in
`build-flags.toml` and the profiles, `settings_writer` beside
`driver_updates`); this is the small interface between them and the app,
so that whichever lands second connects to the other.

* **The file** is the settings crate's (`docs/BUILD-FLAGS.md` §5.1, on
  its branch): `/etc/ferrix/image-manifest`, `key=value` lines --
  `format=1`, `profile`, `level`, `claim`, `runtime_settings`, then the
  image keys, `settings_writer` among them -- written by xtask into every
  image, `max` included, from the values that write the build line and
  the image key (ledger 3636 (15)). This side only reads it.
* **The reader.** `ferrix_settings_client::manifest`: `Manifest::read()`
  (`read_at(path)` for tests) gives the keys. **A missing file, or one
  whose `format` is not 1, reads as unknown**, and unknown is fixed: the
  pages a request backs are shown read-only. `Manifest::fixed()` is
  `runtime_settings == fixed`, `settings_writer == absent`, or unknown;
  `Manifest::fixed_text()` is "Fixed by this build (profile max)", or
  "Fixed: this build does not say what it allows" when unknown.
* **Information only** (ledger 3636 (14)): `settingsd` decides nothing from
  it; a wrong manifest can only mislabel a page, and `settingsd` still
  refuses what its build does not offer.
* **The app.** The System pages a request backs (Name and time, Remote
  login) show their values read-only with that line in place of their
  buttons when `fixed()`; otherwise they send requests. A request that
  `settingsd` does not offer answers `unsupported` with its text, which
  the page shows, never a silent no-op; with no `settingsd` at all, the
  client says the fixed text when the manifest says the writer is absent,
  and "the settings service is not running" otherwise (ledger 3636 (16)).
