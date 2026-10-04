# Continuously, from stage 1

* Every stage's exit criterion joins the CI boot test and stays there. As
  it stands on 2026-09-23, CI runs `test-boot` on all three architectures,
  `test-rustc` and `test-selfhost`; the exits that need a binary the repository does not carry,
  a disk judged on the host or a screendump — `test-shell`, `test-vfs`,
  `test-btrfs`, `test-powerfail`, `test-display`, `test-input`, `test-seat`
  and `test-compositor` — run in the landing gates of `docs/BACKLOG.md`, not
  in CI.
* Since 2026-10-03 the tree is the `ferrix-os` GitHub organisation's
  `ferrix`, and ferrousli, zinc, the Pixel 7 tools, the website and the
  apps are repositories of their own: `components.toml` pins each at a
  commit, every xtask command clones a missing one at its old path
  (`cargo xtask components`, `pin-components`), and a change to one lands
  in its repository first and then as a pin here (`docs/CONVENTIONS.md`).
  Since 2026-10-04 every program that starts on its own is an app in
  `ferrix-os/apps`, with an `app.toml` naming its licence, and a
  screenshot and README each (900c2e8c6). CI's self-hosting job went red
  with the split; its fix landed on 2026-10-04 (cb872a732, stage 20).
* The assembly allow-list is not added to without an argument in the diff.
* Anything expressible as a pure function of bytes goes to `src/lib/` and gets a
  fuzz target and a Miri run — before it is called from the kernel, not after.
