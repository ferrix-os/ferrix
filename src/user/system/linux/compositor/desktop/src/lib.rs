//! What the desktop knows of its applications.
//!
//! Three things a launcher and the Settings app must agree on, kept in one
//! place so that they cannot disagree:
//!
//! * [`entry`]: the `.desktop` files, found and read as fuzzel finds and
//!   reads them. It was the fuzzel app's `desktop` module until Settings
//!   needed the same list; "all applications" on Ferrix is what
//!   [`entry::find_programs`] answers for [`entry::Search::listing`].
//! * [`icon`]: the icon themes, and the file an entry's `Icon` names. It was
//!   the fuzzel app's too.
//! * [`game_mode`]: the set of applications the user starts in game mode, a
//!   file of their own (`~/.config/ferrix/game-mode.conf`), and the mark a
//!   launcher gives a program it starts from that set (`docs/SETTINGS.md`).
//!
//! Nothing here holds a socket or draws a pixel, so all of it is host-tested.
//! The directory also keeps the system's own entries and icons
//! (`applications/`, `icons/`, see `README.md`), which the tests read.

pub mod entry;
pub mod game_mode;
pub mod icon;

#[cfg(test)]
mod testdir;
