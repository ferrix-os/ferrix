<!-- docs/REPO-FLOW.md says what happens to this pull request. In short: the
owner's and the fleet's merge themselves when CI, flow/lane and
certification/verdict pass; an outside contribution is reviewed by the
certification consultant, a security review and the owner before it merges. -->

## What and why

<!-- What changes, and the reason. A commit message argues the why too
(docs/CONVENTIONS.md): one author per commit, no Co-authored-by trailer. -->

## How it was checked

<!-- The commands you ran and what they said: `cargo xtask check`, and for a
kernel change `cargo xtask test-boot --arch all`. -->

## Checklist

- [ ] `git config core.hooksPath .githooks` is set in my clone, and no commit carries a `Co-authored-by:` trailer.
- [ ] `cargo xtask check` passes.
- [ ] If this touches the certified item (`tools/common/data/certification-item.json`, docs/CONVENTIONS.md "Changes to the certified item"), each such commit names its review or says `Certification: <why none is needed>`.
- [ ] No new binary files, vendored archives, minified or encoded blobs, and no new download in a build script; a new dependency is named and argued above.
