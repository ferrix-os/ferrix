//! The dispatchers a binding names, parsed from the strings the
//! configuration's `Bind` carries.
//!
//! Names and argument forms follow Hyprland's `KeybindManager.cpp`. Only
//! the forms listed on each variant are accepted; anything else, such as a
//! window selector after a comma or a named workspace, is an
//! [`Error::BadArgument`] rather than a guess.

use crate::{Error, WorkspaceId};

/// Where a floating window is snapped: its work area whole, or one half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap {
    /// The whole work area: maximized.
    Maximized,
    /// The left half.
    Left,
    /// The right half.
    Right,
}

/// What `snapwindow` asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapTarget {
    /// `max`, `left`, `right`: snap there. Asking a window snapped to one
    /// half for the other half restores it, as a floating window manager's
    /// `SUPER+Left` on a window tiled right does.
    To(Snap),
    /// `restore`: back to the rectangle it had before it was snapped.
    Restore,
    /// `toggle`: maximized, or restored when it is: a title bar's double
    /// click and its maximize button.
    Toggle,
}

impl SnapTarget {
    /// Parse `snapwindow`'s argument.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "max" | "maximize" | "u" => Some(Self::To(Snap::Maximized)),
            "left" | "l" => Some(Self::To(Snap::Left)),
            "right" | "r" => Some(Self::To(Snap::Right)),
            "restore" | "d" => Some(Self::Restore),
            "toggle" | "" => Some(Self::Toggle),
            _ => None,
        }
    }
}

/// A direction, from `l`, `r`, `u` or `t`, and `d` or `b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `l`.
    Left,
    /// `r`.
    Right,
    /// `u` or `t`.
    Up,
    /// `d` or `b`.
    Down,
}

impl Direction {
    /// Parse a direction argument.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "l" => Some(Self::Left),
            "r" => Some(Self::Right),
            "u" | "t" => Some(Self::Up),
            "d" | "b" => Some(Self::Down),
            _ => None,
        }
    }
}

/// Which monitor a dispatcher's argument names, by
/// `CMonitorQueryCore::fromConfigString`'s rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorTarget {
    /// `current`: the focused monitor.
    Current,
    /// `l`, `r`, `u`/`t`, `d`/`b`: the monitor that way from the focused
    /// one.
    Direction(Direction),
    /// `+N` or `-N`: N places along the monitors in their own order,
    /// wrapping around.
    Relative(i64),
    /// A number: the monitor with that id, which Hyprland counts from zero.
    Id(i64),
    /// Anything else: a monitor's name, as the connector is called.
    Named(String),
}

impl MonitorTarget {
    /// Parse a monitor argument. Nothing at all names no monitor, as
    /// Hyprland's empty string does.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if text == "current" {
            return Some(Self::Current);
        }
        // A direction first: `l` and `r` are names nothing should be called,
        // and Hyprland reads them as directions.
        if let Some(direction) = Direction::parse(text) {
            return Some(Self::Direction(direction));
        }
        if text.starts_with(['+', '-']) {
            return text.parse::<i64>().ok().map(Self::Relative);
        }
        if let Ok(id) = text.parse::<i64>() {
            return Some(Self::Id(id));
        }
        Some(Self::Named(text.to_owned()))
    }
}

/// Which workspace a `workspace` or `movetoworkspace` argument names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceTarget {
    /// `N`: workspace N; 0 means 1, as Hyprland clamps it.
    Id(WorkspaceId),
    /// `+N` or `-N`: the workspace N numbers after or before the current
    /// one, never below 1, created if need be.
    Relative(i64),
    /// `e+N` or `e-N`: N places along the workspaces that exist on every
    /// monitor, in number order, wrapping around (`m+N`, the same on the
    /// focused monitor only, is not accepted).
    Open(i64),
    /// `special` or `special:NAME`: a workspace shown over the monitor's own
    /// rather than instead of it. Bare `special` is Hyprland's
    /// `special:special`, which is what `togglespecialworkspace` with no
    /// argument toggles.
    Special(String),
    /// `name:NAME`: a workspace by the name it was given, made with the
    /// first free number if it does not exist yet.
    Named(String),
    /// `previous`, and `previous_per_monitor` which is the same thing on a
    /// compositor whose history is per monitor: the workspace the monitor
    /// showed before this one.
    Previous,
    /// `empty`, `emptym`, `emptyn`: the lowest-numbered workspace with
    /// nothing on it. `m` keeps to the focused monitor and `n` counts up
    /// from the workspace it is showing rather than from one.
    Empty {
        /// `emptyn`: start counting above the workspace shown now.
        after_current: bool,
    },
    /// `next`: the workspace one above the one shown now, whether or not
    /// it exists. `+1` in every way but the name.
    Next,
}

impl WorkspaceTarget {
    /// Parse a workspace argument.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let signed = |rest: &str| {
            if rest.starts_with(['+', '-']) {
                rest.parse::<i64>().ok()
            } else {
                None
            }
        };
        // `special` before `e`, because `special` does not start with one
        // and a name might.
        if text == "special" {
            return Some(Self::Special("special".to_owned()));
        }
        if let Some(name) = text.strip_prefix("special:") {
            return (!name.is_empty()).then(|| Self::Special(name.to_owned()));
        }
        if let Some(name) = text.strip_prefix("name:") {
            return (!name.is_empty()).then(|| Self::Named(name.to_owned()));
        }
        // `previous` and `prev`, as `getWorkspaceIDNameFromString` reads
        // them; `_per_monitor` after either is the same here, because this
        // compositor's history is a monitor's own.
        if text.starts_with("prev") {
            return Some(Self::Previous);
        }
        if let Some(rest) = text.strip_prefix("empty") {
            return Some(Self::Empty {
                after_current: rest.contains('n'),
            });
        }
        if text == "next" {
            return Some(Self::Next);
        }
        if let Some(rest) = text.strip_prefix('e') {
            return signed(rest).map(Self::Open);
        }
        if let Some(offset) = signed(text) {
            return Some(Self::Relative(offset));
        }
        if !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse::<i64>()
            .ok()
            .map(|id| Self::Id(WorkspaceId(id.max(1))))
    }
}

/// How a window is fullscreen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullscreenMode {
    /// `fullscreen, 0`: the whole monitor, no gaps and no border.
    Fullscreen,
    /// `fullscreen, 1`: the workspace's work area, as a lone tiled window
    /// would have it, gaps and border kept. As in Hyprland, the other
    /// windows on the workspace are hidden in this mode too.
    Maximized,
}

/// A dispatcher and its argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatcher {
    /// `movefocus`: focus the neighbour in a direction, or the monitor
    /// there if there is no window, or else, unless
    /// `general:no_focus_fallback` is set, the window at the far edge of
    /// the focused monitor.
    MoveFocus(Direction),
    /// `movewindow`: move the focused tiled window past its neighbour in a
    /// direction, the way the layout does it, or to the monitor there if
    /// there is no window.
    MoveWindow(Direction),
    /// `workspace`: show a workspace, creating it if need be.
    Workspace(WorkspaceTarget),
    /// `movetoworkspace`: move the focused window to a workspace and follow
    /// it there.
    MoveToWorkspace(WorkspaceTarget),
    /// `movetoworkspacesilent`: move the focused window to a workspace and
    /// stay.
    MoveToWorkspaceSilent(WorkspaceTarget),
    /// `killactive`: ask the focused window to close.
    KillActive,
    /// `togglefloating`: float the focused window, or tile it again.
    ToggleFloating,
    /// `fullscreen`: toggle the focused window's fullscreen in a mode.
    Fullscreen(FullscreenMode),
    /// `togglespecialworkspace`: show the named special workspace over the
    /// monitor's own, or hide it if it is already showing.
    ToggleSpecialWorkspace(String),
    /// `togglegroup`: make the focused window a group, or dissolve the one
    /// it is in.
    ToggleGroup,
    /// `changegroupactive`: show another member of the focused window's
    /// group.
    ChangeGroupActive(GroupMember),
    /// `moveintogroup`: put the focused window into the group in a
    /// direction.
    MoveIntoGroup(Direction),
    /// `moveoutofgroup`: take the focused window out of its group.
    MoveOutOfGroup,
    /// `lockgroups`: whether a window may be added to a group.
    LockGroups(Locking),
    /// `focusmonitor`: focus a monitor, and the window last focused on it.
    FocusMonitor(MonitorTarget),
    /// `movewindow mon:<monitor>`: move the focused window to a monitor's
    /// active workspace, and follow it there unless `silent` was asked for.
    MoveWindowToMonitor {
        /// Which monitor.
        monitor: MonitorTarget,
        /// Whether the focus stays where it is.
        silent: bool,
    },
    /// `movecurrentworkspacetomonitor`: move the focused monitor's active
    /// workspace to another monitor.
    MoveCurrentWorkspaceToMonitor(MonitorTarget),
    /// `moveworkspacetomonitor`: move a workspace to a monitor.
    MoveWorkspaceToMonitor {
        /// Which workspace.
        workspace: WorkspaceTarget,
        /// Which monitor.
        monitor: MonitorTarget,
    },
    /// `swapactiveworkspaces`: exchange what two monitors are showing.
    SwapActiveWorkspaces {
        /// One monitor.
        one: MonitorTarget,
        /// The other.
        other: MonitorTarget,
    },
    /// `setfloating`: float the focused window, whether or not it already
    /// does. `togglefloating` is the one that turns it over.
    SetFloating,
    /// `settiled`: tile it, the same way round.
    SetTiled,
    /// `snapwindow max|left|right|restore|toggle`, hyprix's own
    /// (`docs/DESKTOP.md` §1.2): a floating window maximized to its work
    /// area or tiled to one half of it, keeping the rectangle it had to go
    /// back to.
    SnapWindow(SnapTarget),
    /// `minimize`, hyprix's own: the focused window to the special
    /// workspace minimized windows wait on until they are focused again.
    Minimize,
    /// `centerwindow`: put a floating window in the middle of its monitor.
    /// `1` centres it on the monitor including the reserved strips, which
    /// is what Hyprland's argument means.
    CenterWindow {
        /// Whether to ignore what the bars reserved.
        whole: bool,
    },
    /// `pin`: keep a floating window on every workspace of its monitor.
    Pin,
    /// `pseudo`: a tiled window drawn at the size it asked for, in the
    /// middle of the slot the tiling gave it.
    Pseudo,
    /// `resizeactive`: make the focused window larger or smaller.
    ResizeActive(Move),
    /// `moveactive`: move a floating window.
    MoveActive(Move),
    /// `swapwindow`: exchange the focused window with its neighbour in a
    /// direction, leaving the focus on the window that moved.
    SwapWindow(Direction),
    /// `swapnext`: exchange it with the next window in the tiling.
    SwapNext {
        /// Whether to take the one before instead.
        back: bool,
    },
    /// `cyclenext`: focus the next window on the workspace.
    CycleNext {
        /// Whether to take the one before instead.
        back: bool,
        /// `tiled`: skip the floating ones.
        tiled_only: bool,
    },
    /// `bringactivetotop`: put the focused floating window above the rest.
    BringActiveToTop,
    /// `alterzorder`: put a floating window at the top or the bottom.
    AlterZOrder {
        /// Whether it goes to the top.
        top: bool,
    },
    /// `focuswindow`: focus the window a rule-shaped expression matches.
    FocusWindow(String),
    /// `closewindow`: ask that window to close.
    CloseWindow(String),
    /// `focuscurrentorlast`: swap between the focused window and the one
    /// before it.
    FocusCurrentOrLast,
    /// `fullscreenstate`: the two fullscreen states Hyprland keeps -- what
    /// the compositor does, and what the client is told -- set separately.
    FullscreenState {
        /// The compositor's, or -1 for "leave it".
        internal: i64,
        /// The client's, or -1 for "leave it".
        client: i64,
    },
    /// `renameworkspace`: give a workspace a name.
    RenameWorkspace {
        /// Which workspace, by id.
        id: i64,
        /// Its new name; empty puts the number back.
        name: String,
    },
    /// `workspaceopt`: turn every window on the workspace floating or
    /// pseudotiled.
    WorkspaceOpt(WorkspaceOption),
    /// `movegroupwindow`: move the focused window inside its group.
    MoveGroupWindow {
        /// Whether to move it back rather than forward.
        back: bool,
    },
    /// `lockactivegroup`: whether the focused window's group takes any more
    /// windows.
    LockActiveGroup(Locking),
    /// `denywindowfromgroup`: whether a window opened by the focused one
    /// joins its group.
    DenyWindowFromGroup(Locking),
    /// `tagwindow`: add, take away or turn over one of a window's tags,
    /// which a `windowrule` can match on.
    TagWindow(String),
    /// `focusworkspaceoncurrentmonitor`: show a workspace on the focused
    /// monitor, bringing it over from another monitor if that is where it
    /// is.
    FocusWorkspaceOnCurrentMonitor(WorkspaceTarget),
    /// `moveintoorcreategroup`: put the focused window into the group in a
    /// direction, making the window there into one if it is not already.
    MoveIntoOrCreateGroup(Direction),
    /// `movewindoworgroup`: move the focused window into the group in a
    /// direction if there is one there, and past that window otherwise.
    MoveWindowOrGroup(Direction),
    /// `setignoregrouplock`: deprecated in Hyprland, where it does nothing.
    SetIgnoreGroupLock,
    /// `layoutmsg`: a message to the layout itself, which each layout
    /// reads its own way.
    LayoutMessage(String),
    /// `movewindowpixel`: move a named window, which the compositor picks
    /// out.
    MoveWindowPixel {
        /// How far, or where to.
        by: Move,
        /// The window expression naming it.
        window: String,
    },
    /// `resizewindowpixel`: resize a named window, the same way.
    ResizeWindowPixel {
        /// How much, or what size.
        by: Move,
        /// The window expression naming it.
        window: String,
    },
}

impl Move {
    /// Parse `resizeactive`'s and `moveactive`'s two numbers.
    ///
    /// `exact` before them makes them a size or a position; a number ending
    /// in `%` is that fraction of the monitor, which is kept as a negative
    /// sentinel nowhere -- percentages are resolved against the monitor by
    /// the caller, and this keeps the pixels a `%` names once the monitor is
    /// known. Hyprland's own parser takes both, and so does this.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (text, exact) = match text.strip_prefix("exact") {
            Some(rest) => (rest.trim(), true),
            None => (text, false),
        };
        let mut numbers = text.split_whitespace();
        let value = |word: Option<&str>| -> Option<i64> {
            let word = word?;
            // A percentage is resolved against the monitor later; here it
            // is kept as the number it names.
            word.strip_suffix('%').unwrap_or(word).parse::<i64>().ok()
        };
        let x = value(numbers.next())?;
        let y = value(numbers.next())?;
        Some(Self { x, y, exact })
    }
}

/// How far `resizeactive` or `moveactive` asks a window to move.
///
/// Hyprland takes two numbers, each a count of pixels or a percentage of
/// the monitor, and `exact` before them makes them a position or a size
/// rather than a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    /// Across.
    pub x: i64,
    /// Down.
    pub y: i64,
    /// Whether the two are where to put it rather than how far to move it.
    pub exact: bool,
}

/// What `workspaceopt` turns on for every window on the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOption {
    /// `allfloat`.
    AllFloat,
    /// `allpseudo`.
    AllPseudo,
}

/// Which member of a group `changegroupactive` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMember {
    /// `f`, or nothing at all: the next, wrapping around.
    Forward,
    /// `b` or `p`: the one before, wrapping around.
    Back,
    /// A number, which Hyprland counts from one.
    Index(i64),
}

/// What `lockgroups` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locking {
    /// `lock`.
    Lock,
    /// `unlock`.
    Unlock,
    /// `toggle`.
    Toggle,
}

impl Locking {
    /// Parse one of the three words, an empty argument being `lock`, as
    /// Hyprland's `lockgroups` reads it.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "lock" | "" => Some(Self::Lock),
            "unlock" => Some(Self::Unlock),
            "toggle" => Some(Self::Toggle),
            _ => None,
        }
    }
}

impl Dispatcher {
    /// Parse a dispatcher from its name, in any case, and its argument.
    pub fn parse(name: &str, arg: &str) -> Result<Self, Error> {
        let name = name.trim().to_ascii_lowercase();
        let arg = arg.trim();
        let bad = || Error::BadArgument {
            dispatcher: name.clone(),
            arg: arg.to_owned(),
        };
        let direction = || Direction::parse(arg).ok_or_else(bad);
        let workspace = || WorkspaceTarget::parse(arg).ok_or_else(bad);
        let monitor = || MonitorTarget::parse(arg).ok_or_else(bad);
        match name.as_str() {
            "movefocus" => direction().map(Self::MoveFocus),
            // `movewindow` takes a direction or `mon:<monitor>`, with
            // `silent` after it leaving the focus where it is.
            "movewindow" => match arg.strip_prefix("mon:") {
                Some(rest) => {
                    let (text, silent) = match rest.trim().strip_suffix("silent") {
                        Some(head) => (head.trim(), true),
                        None => (rest.trim(), false),
                    };
                    MonitorTarget::parse(text)
                        .map(|monitor| Self::MoveWindowToMonitor { monitor, silent })
                        .ok_or_else(bad)
                }
                None => direction().map(Self::MoveWindow),
            },
            "focusmonitor" => monitor().map(Self::FocusMonitor),
            "movecurrentworkspacetomonitor" => monitor().map(Self::MoveCurrentWorkspaceToMonitor),
            "moveworkspacetomonitor" => {
                let (first, rest) = arg.split_once(char::is_whitespace).ok_or_else(bad)?;
                let workspace = WorkspaceTarget::parse(first).ok_or_else(bad)?;
                let monitor = MonitorTarget::parse(rest).ok_or_else(bad)?;
                Ok(Self::MoveWorkspaceToMonitor { workspace, monitor })
            }
            "swapactiveworkspaces" => {
                let (first, rest) = arg.split_once(char::is_whitespace).ok_or_else(bad)?;
                let one = MonitorTarget::parse(first).ok_or_else(bad)?;
                let other = MonitorTarget::parse(rest).ok_or_else(bad)?;
                Ok(Self::SwapActiveWorkspaces { one, other })
            }
            "workspace" => workspace().map(Self::Workspace),
            "movetoworkspace" => workspace().map(Self::MoveToWorkspace),
            "movetoworkspacesilent" => workspace().map(Self::MoveToWorkspaceSilent),
            // Hyprland's killactive ignores its argument.
            "killactive" => Ok(Self::KillActive),
            "togglefloating" => match arg {
                "" | "active" => Ok(Self::ToggleFloating),
                _ => Err(bad()),
            },
            "setfloating" => Ok(Self::SetFloating),
            "settiled" => Ok(Self::SetTiled),
            "centerwindow" => Ok(Self::CenterWindow { whole: arg == "1" }),
            "minimize" => Ok(Self::Minimize),
            "snapwindow" => SnapTarget::parse(arg).map(Self::SnapWindow).ok_or_else(bad),
            "pin" => Ok(Self::Pin),
            "pseudo" => Ok(Self::Pseudo),
            "resizeactive" => Move::parse(arg).map(Self::ResizeActive).ok_or_else(bad),
            "moveactive" => Move::parse(arg).map(Self::MoveActive).ok_or_else(bad),
            "swapwindow" => direction().map(Self::SwapWindow),
            "swapnext" => Ok(Self::SwapNext {
                back: matches!(arg, "b" | "prev"),
            }),
            "cyclenext" => {
                let words: Vec<&str> = arg.split_whitespace().collect();
                Ok(Self::CycleNext {
                    back: words.iter().any(|word| matches!(*word, "prev" | "last")),
                    tiled_only: words.contains(&"tiled"),
                })
            }
            "bringactivetotop" => Ok(Self::BringActiveToTop),
            "alterzorder" => Ok(Self::AlterZOrder {
                top: !arg.starts_with("bottom"),
            }),
            "focuswindow" => Ok(Self::FocusWindow(arg.to_owned())),
            "closewindow" | "killwindow" => Ok(Self::CloseWindow(arg.to_owned())),
            "focuscurrentorlast" => Ok(Self::FocusCurrentOrLast),
            "fullscreenstate" => {
                let mut numbers = arg.split_whitespace();
                let number = |text: Option<&str>| match text {
                    None | Some("") => Ok(-1),
                    Some(text) => text.parse::<i64>().map_err(|_| bad()),
                };
                Ok(Self::FullscreenState {
                    internal: number(numbers.next())?,
                    client: number(numbers.next())?,
                })
            }
            "renameworkspace" => {
                let (first, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
                Ok(Self::RenameWorkspace {
                    id: first.parse::<i64>().map_err(|_| bad())?,
                    name: rest.trim().to_owned(),
                })
            }
            "workspaceopt" => match arg {
                "allfloat" => Ok(Self::WorkspaceOpt(WorkspaceOption::AllFloat)),
                "allpseudo" => Ok(Self::WorkspaceOpt(WorkspaceOption::AllPseudo)),
                _ => Err(bad()),
            },
            "movegroupwindow" => Ok(Self::MoveGroupWindow {
                back: arg == "b" || arg == "prev",
            }),
            "lockactivegroup" => Locking::parse(arg)
                .map(Self::LockActiveGroup)
                .ok_or_else(bad),
            "denywindowfromgroup" => Locking::parse(arg)
                .map(Self::DenyWindowFromGroup)
                .ok_or_else(bad),
            "tagwindow" => Ok(Self::TagWindow(arg.to_owned())),
            "focusworkspaceoncurrentmonitor" => {
                workspace().map(Self::FocusWorkspaceOnCurrentMonitor)
            }
            "moveintoorcreategroup" => direction().map(Self::MoveIntoOrCreateGroup),
            "movewindoworgroup" => direction().map(Self::MoveWindowOrGroup),
            // Hyprland kept the name and took the behaviour out.
            "setignoregrouplock" => Ok(Self::SetIgnoreGroupLock),
            "layoutmsg" => Ok(Self::LayoutMessage(arg.to_owned())),
            // Both take the change first and the window after a comma,
            // which is the one place Hyprland puts the window last.
            "movewindowpixel" | "resizewindowpixel" => {
                let (how, window) = arg.split_once(',').ok_or_else(bad)?;
                let by = Move::parse(how.trim()).ok_or_else(bad)?;
                let window = window.trim().to_owned();
                if name == "movewindowpixel" {
                    Ok(Self::MoveWindowPixel { by, window })
                } else {
                    Ok(Self::ResizeWindowPixel { by, window })
                }
            }
            "togglegroup" => Ok(Self::ToggleGroup),
            "moveoutofgroup" => Ok(Self::MoveOutOfGroup),
            "moveintogroup" => direction().map(Self::MoveIntoGroup),
            "changegroupactive" => match arg {
                "" | "f" | "forward" => Ok(Self::ChangeGroupActive(GroupMember::Forward)),
                "b" | "p" | "back" | "prev" => Ok(Self::ChangeGroupActive(GroupMember::Back)),
                other => other
                    .parse::<i64>()
                    .map(|index| Self::ChangeGroupActive(GroupMember::Index(index)))
                    .map_err(|_| bad()),
            },
            "lockgroups" => match arg {
                "lock" | "" => Ok(Self::LockGroups(Locking::Lock)),
                "unlock" => Ok(Self::LockGroups(Locking::Unlock)),
                "toggle" => Ok(Self::LockGroups(Locking::Toggle)),
                _ => Err(bad()),
            },
            "togglespecialworkspace" => Ok(Self::ToggleSpecialWorkspace(if arg.is_empty() {
                "special".to_owned()
            } else {
                arg.to_owned()
            })),
            "fullscreen" => match arg {
                "" | "0" => Ok(Self::Fullscreen(FullscreenMode::Fullscreen)),
                "1" => Ok(Self::Fullscreen(FullscreenMode::Maximized)),
                _ => Err(bad()),
            },
            _ => Err(Error::UnknownDispatcher(name.clone())),
        }
    }
}
