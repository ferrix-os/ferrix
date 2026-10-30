//! Suspending a program in RAM, and resuming it where it stopped
//! (`docs/APP-SUSPEND.md` §3.1, S1).
//!
//! `suspend` freezes every process of a window's program with cgroup v2's
//! `cgroup.freeze`, and `resume` thaws them. A frozen process makes no
//! system call: a thread blocked in one is interrupted and makes it again at
//! the thaw (`docs/CGROUPS.md` §11). So everything it holds -- its memory,
//! its files, its sockets, its GPU objects -- stays as it was, and a frozen
//! program resumes from the instruction it stopped at.
//!
//! # Which processes
//!
//! A program is a process and everything it started: the window's process
//! and its descendants, by `/proc`'s parent links. Two cases look further:
//!
//! * **A game Steam started.** Its windows are X windows, whose Wayland
//!   client is yserver, which serves Steam too. Proton names each game
//!   window's class `steam_app_<id>`, and Steam starts every game under a
//!   `reaper` given `AppId=<id>`, which becomes the parent of whatever the
//!   game leaves behind (wineserver included). So such a window means the
//!   reaper of that id and everything beneath it.
//! * **`pid:<n>`** names a process directly, for an X program that is not
//!   Steam's.
//!
//! A program whose processes include the compositor is refused.
//!
//! # Which cgroup
//!
//! The cgroup the program's first process is in is frozen as it is, when
//! nothing else is in it: `app.slice/app-<name>-<pid>.scope`, where a root
//! compositor puts every program it starts. Otherwise -- a game in Steam's
//! scope, or every program of a session in `session-<n>.scope` beside the
//! compositor -- a cgroup is made beneath it, `suspend-<pid>`, frozen
//! first, and the processes moved into it until no new one is left outside.
//! A process forked meanwhile by one already moved is born in it, frozen; one
//! forked by one not yet moved is found by the next look. In a session that
//! needs the scope delegated to its user, which `sessiond` does
//! (`docs/AUTH.md` §6.4).
//!
//! # What the compositor does for a frozen client
//!
//! A frozen client reads nothing, and a client that leaves more than
//! [`compositor_socket::MAX_QUEUED`] unread is dropped. So its connection
//! is *parked* ([`crate::state::Slot::park`]): its frame callbacks wait, as
//! they wait for a surface nobody sees, and no input is sent to it. Its
//! window keeps showing its last frame, at half opacity. An X program's
//! connection is yserver's, which is not frozen and not parked.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the cgroup v2 tree is mounted.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// How many times the processes are looked for and moved before the
/// suspend gives up on the ones still outside. Each look finds only those
/// forked between two looks by a process not yet moved; a program that
/// forks faster than eight looks is not one this is for.
const LOOKS: usize = 8;

/// One process, as `/proc` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    /// Its pid.
    pub pid: u32,
    /// Its parent's.
    pub parent: u32,
    /// `/proc/<pid>/comm`.
    pub comm: String,
    /// `/proc/<pid>/cmdline`, its words.
    pub args: Vec<String>,
    /// Its cgroup, below [`CGROUP_ROOT`], from `/proc/<pid>/cgroup`.
    pub cgroup: Option<String>,
}

/// What `suspend` was asked to freeze.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A process and its descendants.
    Pid(u32),
    /// The game Steam started with this id: its reaper and everything
    /// beneath.
    Steam(u64),
}

/// A program that is frozen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frozen {
    /// The first of its processes.
    pub root: u32,
    /// The cgroup written, below [`CGROUP_ROOT`].
    pub cgroup: String,
    /// Whether the cgroup was made for the suspend.
    pub made: bool,
    /// Its processes, when it was frozen.
    pub pids: BTreeSet<u32>,
}

/// The programs this compositor froze, by their first process.
#[derive(Debug, Default)]
pub struct Suspended {
    frozen: BTreeMap<u32, Frozen>,
}

impl Suspended {
    /// No program frozen.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every program frozen.
    pub fn all(&self) -> impl Iterator<Item = &Frozen> {
        self.frozen.values()
    }

    /// The frozen program `pid` is one of the processes of.
    #[must_use]
    pub fn holding(&self, pid: u32) -> Option<&Frozen> {
        self.frozen
            .values()
            .find(|frozen| frozen.pids.contains(&pid))
    }

    /// Remember a program frozen.
    pub fn insert(&mut self, frozen: Frozen) {
        let _ = self.frozen.insert(frozen.root, frozen);
    }

    /// Forget a program thawed.
    pub fn remove(&mut self, root: u32) -> Option<Frozen> {
        self.frozen.remove(&root)
    }
}

/// The id of a Steam game from its window's class: Proton's `steam_app_<id>`.
#[must_use]
pub fn steam_app(class: &str) -> Option<u64> {
    class.strip_prefix("steam_app_")?.parse().ok()
}

/// The reaper Steam started the game `id` under: a process called `reaper`
/// with `AppId=<id>` among its words.
#[must_use]
pub fn steam_reaper(procs: &[Proc], id: u64) -> Option<u32> {
    let word = format!("AppId={id}");
    procs
        .iter()
        .filter(|proc| proc.comm == "reaper")
        .find(|proc| proc.args.contains(&word))
        .map(|proc| proc.pid)
}

/// `root` and every process beneath it.
#[must_use]
pub fn tree(procs: &[Proc], root: u32) -> BTreeSet<u32> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for proc in procs {
        children.entry(proc.parent).or_default().push(proc.pid);
    }
    let mut found = BTreeSet::new();
    let mut next = vec![root];
    while let Some(pid) = next.pop() {
        if found.insert(pid)
            && let Some(below) = children.get(&pid)
        {
            next.extend(below);
        }
    }
    found
}

/// Whether the program `pids` has its cgroup to itself, so that the cgroup
/// can be frozen as it is: nothing else is in it, and it is not the root.
#[must_use]
pub fn alone_in(procs: &[Proc], pids: &BTreeSet<u32>, cgroup: &str) -> bool {
    cgroup != "/"
        && procs
            .iter()
            .filter(|proc| proc.cgroup.as_deref() == Some(cgroup))
            .all(|proc| pids.contains(&proc.pid))
}

/// The cgroup a `/proc/<pid>/cgroup` names: its unified line, `0::<path>`.
#[must_use]
pub fn unified(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_owned())
}

/// The parent's pid in a `/proc/<pid>/stat`: the field after the state,
/// which follows the command's closing parenthesis (a command may hold
/// spaces and parentheses of its own).
#[must_use]
pub fn parent_in_stat(text: &str) -> Option<u32> {
    let (_, rest) = text.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Every process `/proc` lists now.
#[must_use]
pub fn snapshot() -> Vec<Proc> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(read_proc)
        .collect()
}

/// One process from `/proc`; `None` once it has gone.
fn read_proc(pid: u32) -> Option<Proc> {
    let dir = PathBuf::from(format!("/proc/{pid}"));
    let parent = parent_in_stat(&fs::read_to_string(dir.join("stat")).ok()?)?;
    let comm = fs::read_to_string(dir.join("comm"))
        .map(|text| text.trim_end().to_owned())
        .unwrap_or_default();
    let args = fs::read(dir.join("cmdline"))
        .map(|bytes| {
            bytes
                .split(|&byte| byte == 0)
                .filter(|word| !word.is_empty())
                .map(|word| String::from_utf8_lossy(word).into_owned())
                .collect()
        })
        .unwrap_or_default();
    let cgroup = fs::read_to_string(dir.join("cgroup"))
        .ok()
        .and_then(|text| unified(&text));
    Some(Proc {
        pid,
        parent,
        comm,
        args,
        cgroup,
    })
}

/// The pid `target` names now.
///
/// # Errors
///
/// When no process answers it.
pub fn root_of(target: &Target, procs: &[Proc]) -> Result<u32, String> {
    match *target {
        Target::Pid(pid) if procs.iter().any(|proc| proc.pid == pid) => Ok(pid),
        Target::Pid(pid) => Err(format!("there is no process {pid}")),
        Target::Steam(id) => {
            steam_reaper(procs, id).ok_or_else(|| format!("no Steam reaper says AppId={id}"))
        }
    }
}

/// Freeze the program `root` begins, beside the compositor `own`.
///
/// # Errors
///
/// When it holds the compositor, has no cgroup, or its cgroup cannot be made
/// or written. A program half moved when a later step fails is thawed again.
pub fn freeze(root: u32, own: u32) -> Result<Frozen, String> {
    let procs = snapshot();
    let pids = tree(&procs, root);
    if pids.contains(&own) {
        return Err(format!("process {root} holds the compositor itself"));
    }
    let Some(cgroup) = procs
        .iter()
        .find(|proc| proc.pid == root)
        .and_then(|proc| proc.cgroup.clone())
    else {
        return Err(format!("process {root} is in no cgroup"));
    };
    if alone_in(&procs, &pids, &cgroup) {
        write(&cgroup, "cgroup.freeze", "1")?;
        return Ok(Frozen {
            root,
            cgroup,
            made: false,
            pids,
        });
    }
    let made = format!("{}/suspend-{root}", cgroup.trim_end_matches('/'));
    let dir = path(&made);
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    }
    write(&made, "cgroup.freeze", "1")?;
    match gather(root, own, &made) {
        Ok(pids) => Ok(Frozen {
            root,
            cgroup: made,
            made: true,
            pids,
        }),
        Err(why) => {
            let _ = write(&made, "cgroup.freeze", "0");
            Err(why)
        }
    }
}

/// Move the program `root` begins into the frozen cgroup `into`, looking
/// again until every one of its processes is there: its processes.
fn gather(root: u32, own: u32, into: &str) -> Result<BTreeSet<u32>, String> {
    for _ in 0..LOOKS {
        let procs = snapshot();
        let pids = tree(&procs, root);
        if pids.contains(&own) {
            return Err(format!("process {root} holds the compositor itself"));
        }
        let outside: Vec<u32> = procs
            .iter()
            .filter(|proc| pids.contains(&proc.pid) && proc.cgroup.as_deref() != Some(into))
            .map(|proc| proc.pid)
            .collect();
        if outside.is_empty() {
            return Ok(pids);
        }
        for pid in outside {
            // One that ended meanwhile is no loss; any other refusal is.
            if let Err(why) = write(into, "cgroup.procs", &pid.to_string())
                && fs::metadata(format!("/proc/{pid}")).is_ok()
            {
                return Err(format!("moving {pid}: {why}"));
            }
        }
    }
    Err(format!(
        "process {root}'s program kept starting processes outside {into}"
    ))
}

/// Thaw a program `freeze` froze.
///
/// # Errors
///
/// When its cgroup cannot be written.
pub fn thaw(frozen: &Frozen) -> Result<(), String> {
    write(&frozen.cgroup, "cgroup.freeze", "0")
}

/// The path of `file` in the cgroup `cgroup`.
fn path(cgroup: &str) -> PathBuf {
    Path::new(CGROUP_ROOT).join(cgroup.trim_start_matches('/'))
}

/// Write `value` to `file` of the cgroup `cgroup`.
fn write(cgroup: &str, file: &str, value: &str) -> Result<(), String> {
    let at = path(cgroup).join(file);
    fs::write(&at, value).map_err(|error| format!("{}: {error}", at.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent: u32, comm: &str, args: &[&str], cgroup: &str) -> Proc {
        Proc {
            pid,
            parent,
            comm: comm.to_owned(),
            args: args.iter().map(|&word| word.to_owned()).collect(),
            cgroup: Some(cgroup.to_owned()),
        }
    }

    const SESSION: &str = "/user.slice/user-1000.slice/session-1.scope";

    /// A session as the box has one: the compositor, Steam, the game's
    /// reaper with Proton, the game and wineserver, which Wine left to the
    /// reaper; and a terminal in a scope of its own.
    fn session() -> Vec<Proc> {
        vec![
            proc(30, 1, "hyprix", &["/bin/hyprix"], SESSION),
            proc(40, 30, "steam", &["steam"], SESSION),
            proc(
                50,
                40,
                "reaper",
                &["reaper", "SteamLaunch", "AppId=2234140", "--", "proton"],
                SESSION,
            ),
            proc(51, 50, "python3", &["proton", "waitforexitandrun"], SESSION),
            proc(52, 51, "Endfield.exe", &["Endfield.exe"], SESSION),
            proc(53, 50, "wineserver", &["wineserver"], SESSION),
            proc(60, 30, "foot", &["foot"], "/app.slice/app-foot-60.scope"),
            proc(61, 60, "zinc", &["zinc"], "/app.slice/app-foot-60.scope"),
        ]
    }

    #[test]
    fn a_steam_window_means_its_reaper_and_everything_beneath() {
        let procs = session();
        assert_eq!(steam_app("steam_app_2234140"), Some(2_234_140));
        assert_eq!(steam_app("steam_app_"), None);
        assert_eq!(steam_app("foot"), None);
        let reaper = steam_reaper(&procs, 2_234_140);
        assert_eq!(reaper, Some(50));
        assert_eq!(steam_reaper(&procs, 1), None);
        assert_eq!(
            tree(&procs, 50),
            BTreeSet::from([50, 51, 52, 53]),
            "wineserver, left to the reaper, is the game's"
        );
    }

    #[test]
    fn a_program_alone_in_its_scope_is_frozen_there_and_a_shared_one_is_not() {
        let procs = session();
        let foot = tree(&procs, 60);
        assert!(alone_in(&procs, &foot, "/app.slice/app-foot-60.scope"));
        // The game shares the session's scope with the compositor and Steam.
        let game = tree(&procs, 50);
        assert!(!alone_in(&procs, &game, SESSION));
        // The root cgroup is never frozen as it is.
        let everything: BTreeSet<u32> = procs.iter().map(|proc| proc.pid).collect();
        let at_root: Vec<Proc> = procs
            .iter()
            .cloned()
            .map(|proc| Proc {
                cgroup: Some("/".to_owned()),
                ..proc
            })
            .collect();
        assert!(!alone_in(&at_root, &everything, "/"));
    }

    #[test]
    fn a_program_holding_the_compositor_is_its_tree_too() {
        let procs = session();
        assert!(tree(&procs, 1).contains(&30));
        assert!(!tree(&procs, 40).contains(&30));
    }

    #[test]
    fn proc_files_are_read_as_the_kernel_writes_them() {
        assert_eq!(
            unified("0::/user.slice/user-1000.slice/session-1.scope\n"),
            Some(SESSION.to_owned())
        );
        assert_eq!(unified(""), None);
        // A command with a space and a parenthesis of its own.
        assert_eq!(
            parent_in_stat("52 (Endfield (1).exe) S 51 52 30 0 -1"),
            Some(51)
        );
        assert_eq!(parent_in_stat("garbage"), None);
    }

    #[test]
    fn a_target_is_found_or_said_missing() {
        let procs = session();
        assert_eq!(root_of(&Target::Pid(60), &procs), Ok(60));
        assert!(root_of(&Target::Pid(99), &procs).is_err());
        assert_eq!(root_of(&Target::Steam(2_234_140), &procs), Ok(50));
        assert!(root_of(&Target::Steam(7), &procs).is_err());
    }

    #[test]
    fn the_programs_frozen_are_found_by_any_of_their_processes() {
        let mut suspended = Suspended::new();
        suspended.insert(Frozen {
            root: 50,
            cgroup: format!("{SESSION}/suspend-50"),
            made: true,
            pids: BTreeSet::from([50, 51, 52, 53]),
        });
        assert_eq!(suspended.holding(52).map(|frozen| frozen.root), Some(50));
        assert!(suspended.holding(60).is_none());
        assert_eq!(suspended.all().count(), 1);
        assert!(suspended.remove(50).is_some());
        assert!(suspended.holding(52).is_none());
    }
}
