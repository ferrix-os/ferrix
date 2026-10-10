# The desktop's applications

Two things live here.

**The crate `compositor-desktop`** (`src/`): what a launcher and the Settings
app must agree on. `entry` finds and reads `.desktop` files as fuzzel's
`xdg.c` does, and `icon` finds the file an entry's `Icon` names as fuzzel's
`icon.c` does; both were the fuzzel app's until 2026-10-11, when Settings
came to list the same applications. `game_mode` is the user's
`~/.config/ferrix/game-mode.conf`, the set of applications started in game
mode, and the mark a launcher gives a program it starts from that set
(`docs/SETTINGS.md`). It depends on nothing but std, and all of it is
host-tested.

**The system's launcher entries** (`applications/`, `icons/`): the
`.desktop` entries and their icons, which every desktop installs in
`/usr/share/applications` and the `hicolor` theme whatever launcher it runs
(`tools/common/xtask/src/fuzzel.rs`): the terminal, the pattern client, top,
vkgears and Chrome. They were fuzzel's data until fuzzel became an app
(ferrix-os/apps, 2026-10-04); the fuzzel app's boot-frame test reads them
here, and so do this crate's tests. An app ships its own entries in its
package (`docs/APPS.md`).
