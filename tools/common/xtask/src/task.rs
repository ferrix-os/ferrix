//! The commands `cargo xtask` answers, each registered by the module that
//! carries it out.
//!
//! A module with commands holds a table of them beside its own code:
//!
//! ```text
//! /// This module's `cargo xtask` commands (`crate::task`).
//! pub(crate) const TASKS: &[Task] = &[Task {
//!     name: "test-pty",
//!     help_at: 360,
//!     help: &["test-pty      Boot the term app as init, run a program on a pseudoterminal, and require its output back"],
//!     run: |_, args| test_pty(args),
//! }];
//! ```
//!
//! and `main.rs` names the table once, in [`crate::TASKS`]. A new command in
//! a module that has a table changes that module alone; a new module adds its
//! `mod` line and its table's line, both in `main.rs`. Before this, every
//! command was an arm of one `match` and lines of one help text in `main.rs`,
//! so any two features in flight conflicted there (`docs/TECH-DEBT.md` §5.3).
//!
//! `--help` lists the commands by [`Task::help_at`], not by table: the order
//! a reader wants is by topic, and a topic spans modules. A new command takes
//! a number between its neighbours' (they are ten apart); two that take the
//! same one are both listed, in the order of [`crate::TASKS`].

use crate::Result;
use crate::args::Args;

/// One `cargo xtask` command.
pub(crate) struct Task {
    /// The name the command line gives. A name ending in `*` is every name
    /// that starts with what comes before it and no task names exactly:
    /// the module then refuses the ones it does not know itself.
    pub(crate) name: &'static str,
    /// Where its lines go among `--help`'s commands: lowest first.
    pub(crate) help_at: u16,
    /// Its lines in `--help`, without their four spaces of indent, the first
    /// starting with its name and the rest aligned under its description;
    /// none for a command `--help` does not list.
    pub(crate) help: &'static [&'static str],
    /// Carry it out, given the name it was called by.
    pub(crate) run: fn(&str, &Args) -> Result<()>,
}

/// The head of `--help`, before the commands.
const HEAD: &str = "\
Ferrix build driver

USAGE:
    cargo xtask <COMMAND> [OPTIONS]

COMMANDS:
";

/// The task `command` names among `tables`: the one of that name, or else
/// the first whose `*` name it starts with.
pub(crate) fn find<'a>(tables: &[&'a [Task]], command: &str) -> Option<&'a Task> {
    let tasks = || tables.iter().flat_map(|table| table.iter());
    tasks().find(|task| task.name == command).or_else(|| {
        tasks().find(|task| {
            task.name
                .strip_suffix('*')
                .is_some_and(|stem| command.starts_with(stem))
        })
    })
}

/// `--help`: [`HEAD`], every task's lines in [`Task::help_at`]'s order,
/// then `options`.
pub(crate) fn usage(tables: &[&[Task]], options: &str) -> String {
    let mut listed: Vec<&Task> = tables
        .iter()
        .flat_map(|table| table.iter())
        .filter(|task| !task.help.is_empty())
        .collect();
    listed.sort_by_key(|task| task.help_at);
    let mut text = HEAD.to_owned();
    for line in listed.iter().flat_map(|task| task.help.iter()) {
        text.push_str("    ");
        text.push_str(line);
        text.push('\n');
    }
    text.push_str(options);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_taken_once() {
        let mut names: Vec<&str> = crate::TASKS
            .iter()
            .flat_map(|table| table.iter())
            .map(|task| task.name)
            .collect();
        let all = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all, "two tasks share a name");
    }

    #[test]
    fn a_task_s_help_starts_with_its_name_and_has_a_place_of_its_own() {
        let mut places = Vec::new();
        for task in crate::TASKS.iter().flat_map(|table| table.iter()) {
            let Some(first) = task.help.first() else {
                continue;
            };
            assert!(
                first
                    .strip_prefix(task.name)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(' ')),
                "{}'s help starts `{first}`",
                task.name
            );
            places.push(task.help_at);
        }
        let all = places.len();
        places.sort_unstable();
        places.dedup();
        assert_eq!(places.len(), all, "two tasks share a help_at");
    }

    #[test]
    fn a_name_is_found_before_a_stem() {
        fn none(_: &str, _: &Args) -> Result<()> {
            Ok(())
        }
        let table: &[Task] = &[
            Task {
                name: "test-n*",
                help_at: 0,
                help: &[],
                run: none,
            },
            Task {
                name: "test-nine",
                help_at: 0,
                help: &[],
                run: none,
            },
        ];
        let tables = [table];
        assert_eq!(
            find(&tables, "test-nine").map(|t| t.name),
            Some("test-nine")
        );
        assert_eq!(find(&tables, "test-ten").map(|t| t.name), None);
        assert_eq!(find(&tables, "test-nil").map(|t| t.name), Some("test-n*"));
        assert_eq!(find(&tables, "test-n").map(|t| t.name), Some("test-n*"));
    }
}
