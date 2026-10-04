# The desktop's launcher entries

The system's `.desktop` entries and their icons, which every desktop installs
in `/usr/share/applications` and the `hicolor` theme whatever launcher it runs
(`tools/common/xtask/src/fuzzel.rs`): the terminal, the pattern client, top,
vkgears and Chrome. They were fuzzel's data until fuzzel became an app
(ferrix-os/apps, 2026-10-04); the fuzzel app's boot-frame test reads them here.
An app ships its own entries in its package (`docs/APPS.md`).
