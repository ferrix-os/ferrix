# init

`/sbin/init`, `/sbin/getty`, the getty generator and `/bin/svc`: the service
manager `docs/INIT.md` designs, landings L1 to L13 (§16 records each).
Every image that boots `/sbin/init` runs it, the desktop's included, where
the compositor is `hyprix.service`.

The manager itself is `src/lib/init/svc`, a pure `no_std` crate of the Ferrix
workspace: unit files in systemd's syntax, the dependency graph, operations,
the slice tree and every kind's state machine, as `Manager::step(event, now)
-> actions`. Its restart policy is `src/lib/init/restart`, which allocates
nothing, so that `devmgr` restarts drivers by the same rules. What is here is
everything around it that makes system calls:

* `init/` -- pid 1. It mounts `/run` and cgroup2, moves itself into
  `init.scope`, runs the generators, reads the three unit directories, and
  then waits in one `epoll_wait` on a signalfd, each child's exec report,
  each cgroup's `cgroup.events`, the control socket and the sockets it
  listens on for `.socket` units. What it finds becomes the manager's
  events; the manager's actions become `mkdir`, `clone3(CLONE_INTO_CGROUP)`,
  `kill`, `cgroup.kill` and, at the end, `reboot(2)`. It carries out
  `notify` readiness and `forking`, socket activation, the resource keys,
  `Type=native` services in their cgroup's job with the directory of
  `docs/INIT.md` §6, `User=` on a native service through a helper that has
  become that user, and the audit record. On an image booted with
  `ferrix.devmgr=init` it starts `devmgr` itself, through the starter the
  kernel gives pid 1 (§7.3, `units/devmgr.service`). `init/src/sys.rs`
  holds the system calls' `unsafe` blocks, and `sockets.rs` and
  `directory.rs` the few that bind a listener or end a helper.
* `getty/` -- `getty TTY` gives a terminal a session and a login shell;
  `getty-generator DIR` links a getty into `multi-user.target` for each
  console, a console named by its address included.
* `svc/` -- `/bin/svc`, systemctl's verbs over `/run/ferrix/control`:
  `svc status`, `svc list`, `svc log <unit>`, `set-property`, `top`,
  `poweroff` (§10). `/bin/poweroff`, `/bin/reboot` and `/bin/shutdown` are
  links to it, each the verb of its name (§8.2).
* `dirclient/` -- a Linux program using init's directory, as `test-init`
  runs it.
* `drvupdate/` -- `/bin/drvupdate DRIVER IMAGE [LOCATION]`, which sends a
  new driver image to `drvupdated` and prints `devmgr`'s answer
  (`docs/DEVMGR.md` §4.1).
* `units/` -- the targets, `getty@.service`, `rescue.service`,
  `devmgr.service`, and the services of `lode` (`lode.service`,
  `lode-crashd.service`, `lode-pstore.service`, `lode-upload.service`) and
  of the RAM scrubber (`memscrub.service`), installed in
  `/lib/ferrix/units`.

It is a workspace of its own, as zinc and the compositor are, because these
are Linux programs built with std, and the Ferrix workspace's dependency
policy is written for a kernel.

## Building and testing

`cargo xtask test-init --arch all` builds it for each architecture, boots it
as pid 1 and types at the shell its getty gives (`docs/INIT.md` §15).
`cargo xtask check` runs its formatting, clippy and host tests. To build it
alone, from this directory:

```
cargo build --release --target x86_64-unknown-linux-musl
```
