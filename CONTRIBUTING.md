# Contributing to Ferrix

Boot Ferrix, try something, and tell us where it breaks. A clear report is as
useful as a patch.

## Report what you find

- **Something broke:** open a bug with the command, the commit and the serial
  log. A boot that fails its own checks says which one.
- **A Linux program that should run:** open a "Linux program" issue. The
  kernel logs every system call it does not answer, and that line is usually
  the whole bug.
- **A security problem:** see [SECURITY.md](SECURITY.md) and do not open a
  public issue.

## Changing the code

1. Read [docs/CONVENTIONS.md](docs/CONVENTIONS.md) first. Every change follows
   it, including the rule that a commit names one author and carries no
   `Co-authored-by:` trailer.
2. Run `git config core.hooksPath .githooks` once in your clone.
3. `cargo xtask check` runs every gate CI runs. A change to the kernel should
   also pass `cargo xtask test-boot --arch all`.
4. A commit message is a subject and a body that argues why, not what.

## Sending a pull request

1. Fork the repository and branch from `develop`, naming the branch for what
   it is: `feature/<what>`, `bugfix/<what>`, `docs/<what>` or `chore/<what>`.
2. Open the pull request against `develop`. Fill in the template.
3. CI runs once a maintainer has approved running workflows for it. GitHub
   asks that of every outside pull request, because CI runs your code.
4. Every outside pull request gets three reviews before it can merge: the
   certification consultant's, a security review of the diff, and the
   owner's approval. Changes to CI, build scripts, `Cargo.toml` and
   `Cargo.lock`, fetched downloads and pins get the closest reading. New
   binary files, vendored archives or minified code will be asked to go.
5. A maintainer merges it by hand. Outside pull requests never merge
   themselves.

[docs/REPO-FLOW.md](docs/REPO-FLOW.md) describes the whole flow: branch names,
the two lanes, and how `develop` reaches `main`.

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) is the map of the tree, and
[the roadmap](docs/roadmap/README.md) says what each stage still needs.

## How Ferrix is made

Most code is written in Claude sessions. The project owner chooses what to
build and reviews the results. Contributions from other people go through the
same checks, and through the reviews above, which the owner's own changes skip.
