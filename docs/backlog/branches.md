# Backlog detail: Branches that still hold unlanded work

Detail kept out of [`docs/BACKLOG.md`](../BACKLOG.md), which holds a short line per item linking here. Each item's text is as it stood in the backlog when it was split out (2026-10-16); add new findings here, under the item's id, and keep the backlog line short.

### br-001

- `os-35/ipc-wake`, two "WIP:" commits.
  - 87675432, part B, a sync wake onto the waker's processor. Its relay and
    affinity checks and controls fired. The effect is bimodal until the
    ring's spurious `wake_all` is gone (part A removes it). Left: rebase
    onto the ring's parts, re-apply `Wake::Sync` at the new bell and
    completion wakes, the full row, the consultant.
  - dbd39280, part C: an adaptive halt-poll under a hypervisor (at most
    50 us, idle-task time), and targeted SGIs on GICv2 and GICv3. Checks
    and controls fired. Left: the full row, and F-50's line confirmed.
  - MSI affinity is noted only: it needs a native call to set a thread's
    affinity, vector retargeting, and devmgr to choose the processor.

### br-002

- `guest-frame-time`: diverged from its own name on GitHub. Origin carries
  six newer `WIP` commits on terminal and scroll rendering
  (`c1534cfaa`..`3f382700b`); this checkout has one unreviewed commit,
  `d33a86602`, a guest-compositor frame-cost fix (musl's `f32::round` and
  `memcmp` costing 30-40x glibc's in a guest). The two overlap in
  `compositor/render/exact.rs` and `compositor/term/*`, so neither a
  force-push nor a blind merge is safe. Left: whoever owns the current WIP
  decides which stands and reconciles the files that overlap. `d33a86602`
  is on GitHub as `guest-frame-time-local`.

### br-003

- `clipboard-vdagent`: also diverged under its own name — origin had landed
  a `user/vport`-based clipboard agent (`c7d7b93fe`, from the 2026-09-27
  wind-down) while this checkout independently built a `compositor/vdagent`
  crate for the same feature. Rather than merge two different
  implementations of one feature, the `vdagent` attempt is kept as
  `wip/clipboard-vdagent-compositor-approach` (pushed), and the
  `clipboard-vdagent` branch itself now matches origin. Left: decide which
  approach to keep, or whether `vport` already covers what `vdagent` did.

### br-004

- `stage13-timens` (worktree `timens`): most of stage 13's time namespaces
  staged (30 files, `fs/timens_check.rs`, `syscall/timens.rs`, vdso, futex,
  timerfd and procfs integration), blocked on an unresolved merge conflict
  in `src/kernel/src/panic/catalog.rs` — both sides add an adjacent catalog
  entry, FX-0907 and FX-0910. Left: take both entries, then regenerate
  `docs/generated/PANICS.md` and the two certification coverage JSONs that
  depend on it, before this can be committed.

### br-005

- A detached-HEAD worktree (`display-show`, at `d9c5658ed`) has one
  uncommitted line: `compositor/blank/src/modeset.rs`'s `BACKGROUND`
  changed from a dark slate to pure blue, while the adjacent comment still
  reads "a dark slate, so a screen that is merely black is not mistaken for
  success." Reads as a leftover debug probe, not an intended change. Left:
  confirm and discard, or finish it and fix the comment.
