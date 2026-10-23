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
3482); §11 is where it stands.

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
`service-enable-persistent` work end to end in `test-settings`. Nothing
reads `/etc/hostname` at boot yet, so a renamed machine keeps its name only
until the next boot: making init (or a unit) set it from the file is B2's.
B3 to B6 build on §9's recipe; B4 and B5 start with a design review.
