//! Unit kinds (§4.2): each suffix is one implementation of [`Kind`].
//!
//! A kind owns its section's keys and what they mean. The graph and the
//! operations deal in units whatever their kind, so a new kind, a `.timer`
//! say, is a new implementation of the trait and a variant of [`Config`],
//! and nothing else changes.
//!
//! | Kind | Section | Needs a file | |
//! |---|---|---|---|
//! | `.service` | `[Service]` | yes | [`Service`] |
//! | `.slice` | `[Slice]` | no | [`Slice`] |
//! | `.scope` | `[Scope]` | no | [`Scope`] |
//! | `.target` | none | yes | |
//! | `.mount` | `[Mount]` | yes | [`Mount`] |
//! | `.socket` | `[Socket]` | yes | [`Socket`], version 2 |
//! | `.builtin` | none | no | |
//! | `.device` | none | no | [`Device`] |
//!
//! Slices and scopes need no file because the manager makes them: a slice
//! for every level of a `Slice=` path, a scope when a program asks. A
//! builtin needs none because its name is the contract (§7.2): `net.builtin`
//! is active whether or not anything describes it. A device needs none for
//! the same reason: `dev-dri-card0.device` is `/dev/dri/card0`, active while
//! the node exists, so that a service can be `Requires=` and `After=` it.

mod mount;
mod sandbox;
mod service;
mod socket;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub use mount::Mount;
pub use sandbox::{FilterRule, GROUPS, ProtectSystem, Sandbox, SystemCallFilter};
pub use service::{
    Input, Kill, KillMode, OomPolicy, Output, Restart, Service, ServiceType, WorkingDirectory,
};
pub use socket::{Listen, Socket};

use crate::Warnings;
use crate::ini::Section;
use crate::keys::{self, Setter};
use crate::limits::{self, Limits};
use crate::name::{UnitName, UnitType};
use crate::source::Unit;
use crate::unit::Dependency;
use crate::value::Span;

/// Why a unit's settings refuse it: they parse, but contradict each other
/// or leave out what the kind cannot do without. systemd's load state for
/// this is `bad-setting`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitError {
    /// Why, in systemd's words where it has them.
    pub message: String,
}

impl UnitError {
    pub(crate) fn new(message: &str) -> Self {
        Self {
            message: String::from(message),
        }
    }
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// What one kind knows.
pub trait Kind: Sync {
    /// The type it implements.
    fn unit_type(&self) -> UnitType;

    /// The section its keys are in, if it has one.
    fn section(&self) -> Option<&'static str>;

    /// Whether a unit of the kind must have a file to load.
    fn needs_file(&self) -> bool;

    /// The keys of its own section, parsed; an unknown key is a warning.
    /// `section` is empty when the files had none.
    ///
    /// # Errors
    ///
    /// When the settings refuse the unit.
    fn parse(
        &self,
        name: &UnitName,
        section: &Section,
        warnings: &mut Warnings,
    ) -> Result<Config, UnitError>;

    /// The dependencies the kind implies (a service wants its slice, a mount
    /// the mounts above its mount point) and, unless the unit says
    /// `DefaultDependencies=no`, its default ones (§4.3). `exists` says
    /// whether the unit directories have a unit of a name.
    fn implied(&self, unit: &Unit, exists: &dyn Fn(&UnitName) -> bool) -> Edges;
}

/// Dependencies by name, as [`Kind::implied`] gives them.
pub type Edges = Vec<(Dependency, UnitName)>;

/// The root slice.
pub const ROOT_SLICE: &str = "-.slice";
/// Where services and scopes go when they name no slice.
pub const SYSTEM_SLICE: &str = "system.slice";
/// Init's own cgroup, beside every slice.
pub const INIT_SCOPE: &str = "init.scope";
/// `devmgr`'s drivers, which init adopts and never makes, stops or kills
/// (§7.3).
pub const DRIVERS_SLICE: &str = "drivers.slice";
/// What services are after by default.
pub const SYSINIT: &str = "sysinit.target";
/// What services are after by default, besides [`SYSINIT`].
pub const BASIC: &str = "basic.target";
/// What sockets are before by default, and `basic.target` wants.
pub const SOCKETS: &str = "sockets.target";
/// What every unit with default dependencies conflicts with, so that
/// shutdown stops it.
pub const SHUTDOWN: &str = "shutdown.target";
/// The targets that end the machine, each of which pulls in [`SHUTDOWN`].
pub const POWER_TARGETS: [&str; 3] = ["poweroff.target", "reboot.target", "halt.target"];

/// Add a dependency on the unit `name`.
fn edge(edges: &mut Edges, dependency: Dependency, name: &str) {
    if let Ok(name) = UnitName::parse(name) {
        edges.push((dependency, name));
    }
}

/// `Requires=` and `After=` a unit: how a unit is in its slice.
fn within(edges: &mut Edges, name: &str) {
    edge(edges, Dependency::Requires, name);
    edge(edges, Dependency::After, name);
}

/// `Conflicts=` and `Before=` `shutdown.target`: what makes shutdown stop a
/// unit without the unit saying so.
fn stopped_at_shutdown(edges: &mut Edges) {
    edge(edges, Dependency::Conflicts, SHUTDOWN);
    edge(edges, Dependency::Before, SHUTDOWN);
}

/// A unit's kind-specific settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Config {
    /// A service.
    Service(Box<Service>),
    /// A slice.
    Slice(Slice),
    /// A scope.
    Scope(Scope),
    /// A target, which has no settings of its own.
    Target,
    /// A mount.
    Mount(Mount),
    /// A socket.
    Socket(Socket),
    /// A builtin, which has no settings of its own.
    Builtin,
    /// A device node.
    Device(Device),
}

/// The kind a type is implemented by.
pub fn of(unit_type: UnitType) -> &'static dyn Kind {
    match unit_type {
        UnitType::Service => &service::ServiceKind,
        UnitType::Slice => &SliceKind,
        UnitType::Scope => &ScopeKind,
        UnitType::Target => &Plain(UnitType::Target),
        UnitType::Mount => &mount::MountKind,
        UnitType::Socket => &socket::SocketKind,
        UnitType::Builtin => &Plain(UnitType::Builtin),
        UnitType::Device => &DeviceKind,
    }
}

/// Targets and builtins: no section, no settings.
struct Plain(UnitType);

impl Kind for Plain {
    fn unit_type(&self) -> UnitType {
        self.0
    }

    fn section(&self) -> Option<&'static str> {
        None
    }

    fn needs_file(&self) -> bool {
        self.0 == UnitType::Target
    }

    fn parse(&self, _: &UnitName, _: &Section, _: &mut Warnings) -> Result<Config, UnitError> {
        Ok(match self.0 {
            UnitType::Builtin => Config::Builtin,
            _ => Config::Target,
        })
    }

    /// A builtin has none. A power target pulls in `shutdown.target`, and
    /// every other target stops at shutdown. That a target is also after
    /// what it wants needs the other units loaded, so the manager adds it.
    fn implied(&self, unit: &Unit, _: &dyn Fn(&UnitName) -> bool) -> Edges {
        let mut edges = Edges::new();
        let name = unit.name.as_str();
        if self.0 != UnitType::Target {
            return edges;
        }
        let power = POWER_TARGETS.contains(&name);
        if power {
            within(&mut edges, SHUTDOWN);
        }
        if unit.unit.default_dependencies && !power && name != SHUTDOWN {
            stopped_at_shutdown(&mut edges);
        }
        edges
    }
}

/// A device unit's settings, which its name gives: the node it waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The node, absolute and below `/dev`: [`UnitName::device_path`].
    pub path: String,
}

/// The device kind.
struct DeviceKind;

impl Kind for DeviceKind {
    fn unit_type(&self) -> UnitType {
        UnitType::Device
    }

    fn section(&self) -> Option<&'static str> {
        None
    }

    fn needs_file(&self) -> bool {
        false
    }

    /// The node, from the name; a name that is no node's refuses the unit.
    fn parse(&self, name: &UnitName, _: &Section, _: &mut Warnings) -> Result<Config, UnitError> {
        match name.device_path() {
            Ok(path) => Ok(Config::Device(Device { path })),
            Err(_) => Err(UnitError::new(
                "a device unit's name must be a node below /dev, escaped as systemd-escape --path does",
            )),
        }
    }

    /// None: as under systemd, a device has no default dependencies, and
    /// nothing stops it at shutdown.
    fn implied(&self, _: &Unit, _: &dyn Fn(&UnitName) -> bool) -> Edges {
        Edges::new()
    }
}

/// A slice's settings: the limits over everything beneath it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Slice {
    /// The resource keys.
    pub limits: Limits,
}

/// The slice kind.
struct SliceKind;

impl Kind for SliceKind {
    fn unit_type(&self) -> UnitType {
        UnitType::Slice
    }

    fn section(&self) -> Option<&'static str> {
        Some("Slice")
    }

    fn needs_file(&self) -> bool {
        false
    }

    fn parse(
        &self,
        _: &UnitName,
        section: &Section,
        warnings: &mut Warnings,
    ) -> Result<Config, UnitError> {
        let mut slice = Slice::default();
        keys::apply(section, &limits::KEYS, &mut slice.limits, warnings);
        Ok(Config::Slice(slice))
    }

    /// A slice is in its parent slice, and stops at shutdown unless it is
    /// the root.
    fn implied(&self, unit: &Unit, _: &dyn Fn(&UnitName) -> bool) -> Edges {
        let mut edges = Edges::new();
        if let Some(parent) = unit.name.slice_parent() {
            within(&mut edges, parent.as_str());
        }
        if unit.unit.default_dependencies && unit.name.as_str() != ROOT_SLICE {
            stopped_at_shutdown(&mut edges);
        }
        edges
    }
}

/// A scope's settings: where it goes, what bounds it, and how it is
/// stopped, since init did not start what is in it (§5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// `Slice=`; `None` is the manager's default.
    pub slice: Option<UnitName>,
    /// The resource keys.
    pub limits: Limits,
    /// `KillMode=`, `KillSignal=` and `SendSIGHUP=`.
    pub kill: Kill,
    /// `TimeoutStopSec=`.
    pub timeout_stop: Span,
    /// `Delegate=`.
    pub delegate: bool,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            slice: None,
            limits: Limits::default(),
            kill: Kill::default(),
            timeout_stop: service::DEFAULT_TIMEOUT,
            delegate: false,
        }
    }
}

/// The scope kind.
struct ScopeKind;

/// The `[Scope]` keys that are not resource or kill keys.
const SCOPE_KEYS: [(&str, Setter<Scope>); 3] = [
    ("Slice", |scope, a, w| {
        if let Some(slice) = service::slice(a, w) {
            scope.slice = slice;
        }
    }),
    ("TimeoutStopSec", |scope, a, w| {
        if let Some(timeout) = keys::span(a, w) {
            scope.timeout_stop = service::zero_is_infinity(timeout);
        }
    }),
    ("Delegate", |scope, a, w| {
        if let Some(delegate) = service::delegate(a, w) {
            scope.delegate = delegate;
        }
    }),
];

impl Kind for ScopeKind {
    fn unit_type(&self) -> UnitType {
        UnitType::Scope
    }

    fn section(&self) -> Option<&'static str> {
        Some("Scope")
    }

    fn needs_file(&self) -> bool {
        false
    }

    fn parse(
        &self,
        _: &UnitName,
        section: &Section,
        warnings: &mut Warnings,
    ) -> Result<Config, UnitError> {
        let mut scope = Scope::default();
        for assignment in &section.assignments {
            let key = assignment.key.as_str();
            if let Some(setter) = keys::find(&SCOPE_KEYS, key) {
                setter(&mut scope, assignment, warnings);
            } else if let Some(setter) = keys::find(&limits::KEYS, key) {
                setter(&mut scope.limits, assignment, warnings);
            } else if let Some(setter) = keys::find(&service::KILL_KEYS, key) {
                setter(&mut scope.kill, assignment, warnings);
            } else {
                keys::unknown(&section.name, assignment, warnings);
            }
        }
        Ok(Config::Scope(scope))
    }

    /// A scope is in its slice, `system.slice` if it names none and the
    /// root for `init.scope`, and stops at shutdown unless it is init's.
    fn implied(&self, unit: &Unit, _: &dyn Fn(&UnitName) -> bool) -> Edges {
        let mut edges = Edges::new();
        let init = unit.name.as_str() == INIT_SCOPE;
        let named = match &unit.config {
            Config::Scope(scope) => scope.slice.as_ref().map(UnitName::as_str),
            _ => None,
        };
        within(
            &mut edges,
            named.unwrap_or(if init { ROOT_SLICE } else { SYSTEM_SLICE }),
        );
        if unit.unit.default_dependencies && !init {
            stopped_at_shutdown(&mut edges);
        }
        edges
    }
}

impl Kind for service::ServiceKind {
    fn unit_type(&self) -> UnitType {
        UnitType::Service
    }

    fn section(&self) -> Option<&'static str> {
        Some("Service")
    }

    fn needs_file(&self) -> bool {
        true
    }

    fn parse(
        &self,
        name: &UnitName,
        section: &Section,
        warnings: &mut Warnings,
    ) -> Result<Config, UnitError> {
        service::parse(name, section, warnings)
    }

    /// A service is in its slice, `system.slice` if it names none; by
    /// default it is after `sysinit.target` and `basic.target`, as under
    /// systemd, and stops at shutdown -- before `basic.target` does, which
    /// is what the second `After=` gives it.
    fn implied(&self, unit: &Unit, _: &dyn Fn(&UnitName) -> bool) -> Edges {
        let mut edges = Edges::new();
        let named = match &unit.config {
            Config::Service(service) => service.slice.as_ref().map(UnitName::as_str),
            _ => None,
        };
        within(&mut edges, named.unwrap_or(SYSTEM_SLICE));
        if unit.unit.default_dependencies {
            edge(&mut edges, Dependency::After, SYSINIT);
            edge(&mut edges, Dependency::After, BASIC);
            stopped_at_shutdown(&mut edges);
        }
        edges
    }
}

impl Kind for socket::SocketKind {
    fn unit_type(&self) -> UnitType {
        UnitType::Socket
    }

    fn section(&self) -> Option<&'static str> {
        Some("Socket")
    }

    fn needs_file(&self) -> bool {
        true
    }

    fn parse(
        &self,
        name: &UnitName,
        section: &Section,
        warnings: &mut Warnings,
    ) -> Result<Config, UnitError> {
        socket::parse(name, section, warnings)
    }

    /// Before the service it activates, as systemd orders them, so the
    /// sockets exist when it starts; and with default dependencies, after
    /// `sysinit.target`, before `sockets.target`, and stopped at shutdown.
    fn implied(&self, unit: &Unit, exists: &dyn Fn(&UnitName) -> bool) -> Edges {
        let mut edges = Edges::new();
        if let Config::Socket(socket) = &unit.config
            && !socket.accept
        {
            let service = match &socket.service {
                Some(service) => Some(service.clone()),
                None => unit
                    .name
                    .as_str()
                    .strip_suffix(".socket")
                    .and_then(|stem| UnitName::parse(&format!("{stem}.service")).ok()),
            };
            if let Some(service) = service.filter(|name| exists(name)) {
                edges.push((Dependency::Before, service));
            }
        }
        if unit.unit.default_dependencies {
            edge(&mut edges, Dependency::After, SYSINIT);
            edge(&mut edges, Dependency::Before, SOCKETS);
            stopped_at_shutdown(&mut edges);
        }
        edges
    }
}

impl Kind for mount::MountKind {
    fn unit_type(&self) -> UnitType {
        UnitType::Mount
    }

    fn section(&self) -> Option<&'static str> {
        Some("Mount")
    }

    fn needs_file(&self) -> bool {
        true
    }

    fn parse(
        &self,
        name: &UnitName,
        section: &Section,
        warnings: &mut Warnings,
    ) -> Result<Config, UnitError> {
        mount::parse(name, section, warnings)
    }

    /// A mount is after, and requires, the mounts of the paths above its
    /// mount point that the unit directories have units for. It has no
    /// default dependencies: the last step of a shutdown unmounts (§8.2).
    fn implied(&self, unit: &Unit, exists: &dyn Fn(&UnitName) -> bool) -> Edges {
        let mut edges = Edges::new();
        let Config::Mount(mount) = &unit.config else {
            return edges;
        };
        let parts = crate::name::path_components(&mount.r#where).unwrap_or_default();
        for depth in 0..parts.len() {
            let prefix = parts.get(..depth).unwrap_or_default().join("/");
            let path = alloc::format!("/{prefix}");
            if let Ok(above) = UnitName::for_path(&path, UnitType::Mount)
                && exists(&above)
            {
                within(&mut edges, above.as_str());
            }
        }
        edges
    }
}
