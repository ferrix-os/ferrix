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
row adds; §10 is the certification consultant's review; §11 is where it
stands.

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
| `user-add` | name, full name, type (standard/administrator) | name as `useradd`'s; not taken; uid from the free range | `/etc/passwd`, `/etc/group` (both atomic, one lock) | none | `users` | B4 |
| `user-remove` | name | exists, uid >= 1000, not the caller's own last administrator | the same | none | `users` | B4 |
| `user-set-type` | name, type | as above; never removes the last member of `wheel` | `/etc/group` | none | `users` | B4 |
| `user-auto-login` | name or none | exists, uid >= 1000 | sessiond's file (B4 names it) | none | `users` | B4 |
| `set-boot-word` | key, value or none | key in the compiled list a Settings page may set; value permitted for that key by the **`present` profile** (`tools/common/data/profiles/present.toml`, carried in the image); anything else refused, so a Settings page never leaves the certified configuration | `\FERRIX\CMDLINE.TXT` on the EFI system partition, the one key replaced, every other word kept | none: next boot | `boot` | B5 |
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

**"A password prompt when the policy says so."** `authd` today always asks
for a password. B1 adds one value to its policy syntax, **`Methods=none`**:
the conversation is ACCEPTED once `TargetGroup=` holds, with no prompt. It
is honoured **only** for a policy with `Callers=root`, a `TargetGroup=`,
`Purpose=authenticate` and no `Grant=` or `FirstPassword=`; anywhere else
it is warned about and read as `password`, the stricter reading, as every
unreadable policy value is. So it can loosen a root mechanism's
group-gated action and nothing else: never the lock screen, `login`, `su`
or `passwd`. An administrator who wants polkit's "administrators without a
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
  own (at most 8; the ninth is refused `unavailable`), so a prompt waiting
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
    match talk.next()? {
        Step::Prompt { visible, text } => talk.answer(read_password(&text, visible))?,
        Step::Info(text) => show_under_the_field(&text),
        Step::Done { changed, text } => break show_done(&text),
        Step::Refused(refusal) => break show_refusal(&refusal),
    }
}
```

* **Step-wise**, so a tole app can keep drawing: `Conversation` is
  `AsRawFd`, and `next()` returns at each record. `Client::apply(request,
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
* **`settingsctl`**, a command line over the same crate:
  `settingsctl [--dry-run] set-hostname NAME`,
  `settingsctl [--dry-run] service-enable-persistent UNIT on|off`. It prints
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
settings: request=set-hostname peer=pid:812,uid:1000 account=ferrix auth=password result=done before=ferrix after=box
settings: request=set-hostname peer=pid:830,uid:1001 account=guest auth=refused result=not-permitted
```

It never holds a secret. Refusals for `invalid` and dry runs are not
recorded: nothing was asked and nothing changed.

## 9. Building on it: what B1 built and what each row adds

**B1 built** (2026-10-24):

* `src/user/system/linux/settingsd/`, a workspace of its own with the
  auth workspace's lints: `proto` (the records and the requests'
  validation, pure, host-tested), `client`, `settingsd`, `settingsctl`.
* `authd`: `Methods=none` as §5 bounds it, with tests; the five
  `settings.*` policies.
* `settings.socket` and `settings.service`, wanted by `sockets.target`.
* xtask: `settingsd::carried(arch)` puts it in every image that carries
  `authd` and init; `cargo xtask test-settings` (§9.1).

**Adding a request** (B2-B6), all in `src/user/system/linux/settingsd/`:

1. `proto/src/request.rs`: a variant of `Request` with typed fields, its
   wire name in `Request::NAMES`, its encode and decode arms, its
   `validate` rules, its `area()`.
2. `settingsd/src/requests/<name>.rs`: `plan(&Request, &System) ->
   Result<Plan, Refusal>`, which reads the system and says what would
   change (the dry run's text, the audit's before and after), and
   `apply(Plan) -> Result<Outcome, Refusal>`, which writes with
   `atomic::write_atomic` and makes the live call. Register both in
   `requests::plan`.
3. If the area is new, its policy in `src/user/system/linux/auth/services/settings.<area>`.
4. `settingsctl`'s verb, and the client's re-export if it adds a type.
5. Its line in `test-settings`: refused for `guest`, done for `ferrix`,
   the file checked.

### 9.1 How it is tested

* **Host tests** (`cargo xtask check`'s `settingsd` unit): every record
  round-trips and every truncated or overlong one is refused; every
  validation rule has a passing and a failing case; `settingsd`'s
  conversation runs over a socket pair against a scripted authority and a
  scripted init, and shows that a refusal writes nothing, a dry run writes
  nothing, an invalid request is refused before the authority is asked,
  an ACCEPTED for another uid is refused, and `write_atomic` leaves no
  temporary file. `authd`'s tests show `Methods=none` honoured only where
  §5 says.
* **`cargo xtask test-settings`** boots init, `authd` and `settingsd`
  with three accounts: `root`, `ferrix` (uid 1000, in `wheel`) and `guest`
  (uid 1001, not), both with seeded passwords. At the console:
  `guest` is refused `set-hostname` and `service-enable-persistent` with
  its own right password, and `/etc/hostname` and the links are
  unchanged; `ferrix` with a wrong password is refused; `ferrix` with the
  right one renames the machine (`/etc/hostname` and `hostname` agree) and
  enables `sshd.service` (the link exists); a unit not on the list is
  refused `invalid`; a dry run as `guest` changes nothing; the audit lines
  are in `svc log settings.service` and hold no password.
* **Negative control.** `test-settings --sabotage skip-authorisation`
  builds `settingsd` with step 4 of §5 skipped (`FERRIX_SETTINGS_SABOTAGE`,
  as `authd`'s sabotages are built); the gate must fail on the `guest`
  line. xtask refuses to package a sabotaged `settingsd` anywhere else.

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

The verdict, its conditions and the ledger line are recorded here when
given.

## 11. Where it stands

2026-10-24: design written; review and build in progress on
`feature/settings-writer`.
