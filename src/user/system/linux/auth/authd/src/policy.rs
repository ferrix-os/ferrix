//! What each service allows (`docs/AUTH.md` §4.2): a file named after the
//! service, in the unit files' syntax and their three layers.
//!
//! The highest layer that has the file wins, and every `<service>.d/*.conf`
//! in any layer is merged after it, in the order of their names, as a unit's
//! drop-ins are. A service with no file anywhere has no policy, and a
//! conversation for it is UNAVAILABLE: a misspelt name never falls back to a
//! default that opens something.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ferrix_svc::Warnings;
use ferrix_svc::ini::{self, Document};

/// Whose account a service authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountRule {
    /// Only the caller's own, whoever the caller is: the lock screen.
    Caller,
    /// The caller's own, or any account when the caller is root: `passwd`,
    /// `login`.
    Any,
}

/// Who may start a conversation for a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Callers {
    /// Anyone.
    Any,
    /// Only a caller whose effective uid is 0.
    Root,
}

/// What a conversation does once the person is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// Say who they are: the lock screen, `login`, `su`.
    Authenticate,
    /// Change their password: `passwd`.
    ChangePassword,
}

/// One service's policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Policy {
    /// Its name.
    pub(crate) service: String,
    /// Whose account it authenticates.
    pub(crate) account: AccountRule,
    /// Who may ask.
    pub(crate) callers: Callers,
    /// What it does after.
    pub(crate) purpose: Purpose,
    /// How long a failure waits before FAILED is sent.
    pub(crate) fail_delay: Duration,
    /// Whether an acceptance is told to the seat's owner (phase 2).
    pub(crate) grant_seat: bool,
    /// `FirstPassword=local`: an account with no credential may choose one
    /// here, from the console alone (`docs/AUTH.md` §5.4).
    pub(crate) first_password_local: bool,
    /// `TargetGroup=`: the account must be in this group before anything is
    /// asked of it (`su`'s `wheel`, decision 5).
    pub(crate) target_group: Option<String>,
    /// `Methods=none`: ACCEPTED once every other check holds, with no
    /// prompt. Honoured only within [`methods_none_bound`]; anywhere else
    /// the policy asks for a password (`docs/SETTINGS-WRITER.md` §5).
    pub(crate) methods_none: bool,
}

/// The only services `Methods=none` may loosen: the settings writer's
/// (`docs/SETTINGS-WRITER.md` §5; the certification consultant's ledger
/// line 3482, condition 1).
const METHODS_NONE_PREFIX: &str = "settings.";

/// Whether `policy` may skip its prompt: a service of the settings
/// writer's, asked by root alone, for a member of a group, to say who
/// someone is, with nothing granted and no first password offered. Every
/// shipped policy for a person's login, `su`, `passwd` or the lock screen
/// fails it by name as well as by shape.
fn methods_none_bound(policy: &Policy) -> bool {
    policy.service.starts_with(METHODS_NONE_PREFIX)
        && policy.callers == Callers::Root
        && policy.target_group.is_some()
        && policy.purpose == Purpose::Authenticate
        && !policy.grant_seat
        && !policy.first_password_local
}

/// Read `service`'s policy from `layers`, lowest first. Warnings about keys
/// it does not know, or values it cannot read, go to `warn`.
pub(crate) fn load(
    layers: &[PathBuf],
    service: &str,
    warn: &mut dyn FnMut(String),
) -> Option<Policy> {
    let main = layers
        .iter()
        .rev()
        .map(|layer| layer.join(service))
        .find(|path| path.is_file())?;
    let mut warnings = Warnings::new();
    let mut document = read(&main, &mut warnings)?;
    let mut drop_ins: Vec<PathBuf> = layers
        .iter()
        .filter_map(|layer| std::fs::read_dir(layer.join(format!("{service}.d"))).ok())
        .flat_map(|entries| entries.filter_map(Result::ok).map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
        .collect();
    drop_ins.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for path in drop_ins {
        if let Some(later) = read(&path, &mut warnings) {
            document.merge(later);
        }
    }
    for warning in warnings.list() {
        warn(format!("{warning}"));
    }
    Some(interpret(service, &document, warn))
}

fn read(path: &Path, warnings: &mut Warnings) -> Option<Document> {
    let bytes = std::fs::read(path).ok()?;
    let name: Arc<str> = Arc::from(path.display().to_string());
    ini::parse(&name, &bytes, warnings).ok()
}

/// The last value of `key` in `[Service]`, as systemd's scalars read.
fn last<'a>(document: &'a Document, key: &'a str) -> Option<&'a str> {
    document
        .section("Service")?
        .values(key)
        .last()
        .map(|assignment| assignment.value.as_str())
}

fn interpret(service: &str, document: &Document, warn: &mut dyn FnMut(String)) -> Policy {
    let mut policy = Policy {
        service: service.to_owned(),
        account: AccountRule::Caller,
        callers: Callers::Any,
        purpose: Purpose::Authenticate,
        fail_delay: Duration::from_secs(2),
        grant_seat: false,
        first_password_local: false,
        target_group: None,
        methods_none: false,
    };
    let known = [
        "Description",
        "Account",
        "Callers",
        "Methods",
        "Purpose",
        "FailDelaySec",
        "Grant",
        "FirstPassword",
        "TargetGroup",
    ];
    if let Some(section) = document.section("Service") {
        for assignment in &section.assignments {
            if !known.contains(&assignment.key.as_str()) {
                warn(format!(
                    "{}:{}: unknown key {} in the {service} policy, ignored",
                    assignment.file, assignment.line, assignment.key
                ));
            }
        }
    }
    let bad = |warn: &mut dyn FnMut(String), key: &str, value: &str| {
        warn(format!(
            "{service}: {key}={value} is not understood; the stricter reading is kept"
        ));
    };
    match last(document, "Account") {
        None | Some("self" | "caller") => {}
        Some("any") => policy.account = AccountRule::Any,
        Some(other) => bad(warn, "Account", other),
    }
    match last(document, "Callers") {
        None | Some("any") => {}
        Some("root") => policy.callers = Callers::Root,
        Some(other) => {
            // Refusing everyone but root is the stricter reading.
            policy.callers = Callers::Root;
            bad(warn, "Callers", other);
        }
    }
    match last(document, "Purpose") {
        None | Some("authenticate") => {}
        Some("change-password") => policy.purpose = Purpose::ChangePassword,
        Some(other) => bad(warn, "Purpose", other),
    }
    let methods_none = match last(document, "Methods") {
        None | Some("password") => false,
        Some("none") => true,
        Some(other) => {
            warn(format!(
                "{service}: Methods={other}: only password and none exist yet (docs/AUTH.md §4.1); password it is"
            ));
            false
        }
    };
    if let Some(value) = last(document, "FailDelaySec") {
        match value.parse::<u64>() {
            Ok(seconds) if seconds <= 30 => policy.fail_delay = Duration::from_secs(seconds),
            _ => bad(warn, "FailDelaySec", value),
        }
    }
    match last(document, "Grant") {
        None | Some("") => {}
        Some("seat") => policy.grant_seat = true,
        Some(other) => bad(warn, "Grant", other),
    }
    match last(document, "FirstPassword") {
        None | Some("" | "no") => {}
        Some("local") => policy.first_password_local = true,
        Some(other) => bad(warn, "FirstPassword", other),
    }
    match last(document, "TargetGroup") {
        None | Some("") => {}
        Some(group)
            if group
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) =>
        {
            policy.target_group = Some(group.to_owned());
        }
        Some(other) => bad(warn, "TargetGroup", other),
    }
    if methods_none {
        if methods_none_bound(&policy) {
            policy.methods_none = true;
        } else {
            warn(format!(
                "{service}: Methods=none is honoured only for a settings.* service with \
                 Callers=root, a TargetGroup=, Purpose=authenticate and no Grant= or \
                 FirstPassword= (docs/AUTH.md §4.2); password it is"
            ));
        }
    }
    policy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn layers(root: &Path) -> Vec<PathBuf> {
        ["lib", "etc", "run"]
            .iter()
            .map(|top| root.join(top).join("ferrix/auth/services"))
            .collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("authd-policy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_higher_layer_wins_and_drop_ins_merge() {
        let root = scratch("layers");
        let l = layers(&root);
        write(&l[0], "gate", "[Service]\nAccount=any\nFailDelaySec=2\n");
        write(&l[1], "gate", "[Service]\nAccount=self\nFailDelaySec=1\n");
        write(
            &l[0].join("gate.d"),
            "10-root.conf",
            "[Service]\nCallers=root\n",
        );
        let mut warnings = Vec::new();
        let policy = load(&l, "gate", &mut |w| warnings.push(w)).unwrap();
        assert_eq!(
            policy.account,
            AccountRule::Caller,
            "etc's file replaced lib's"
        );
        assert_eq!(policy.fail_delay, Duration::from_secs(1));
        assert_eq!(policy.callers, Callers::Root, "the drop-in applied");
        assert!(warnings.is_empty(), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_file_is_no_policy_and_strange_values_read_strictly() {
        let root = scratch("strict");
        let l = layers(&root);
        assert!(load(&l, "missing", &mut |_| {}).is_none());
        write(
            &l[0],
            "odd",
            "[Service]\nCallers=wheel\nNonsense=1\nMethods=totp\n",
        );
        let mut warnings = Vec::new();
        let policy = load(&l, "odd", &mut |w| warnings.push(w)).unwrap();
        assert_eq!(policy.callers, Callers::Root);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    const SETTINGS: &str =
        "[Service]\nAccount=any\nCallers=root\nTargetGroup=wheel\nMethods=none\n";

    #[test]
    fn methods_none_loosens_a_settings_service_of_the_shape_alone() {
        let root = scratch("none");
        let l = layers(&root);
        write(&l[0], "settings.hostname", SETTINGS);
        let mut warnings = Vec::new();
        let policy = load(&l, "settings.hostname", &mut |w| warnings.push(w)).unwrap();
        assert!(policy.methods_none, "{warnings:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
        // The same shape under another name asks for a password.
        write(&l[0], "mechanism", SETTINGS);
        let mut warnings = Vec::new();
        let policy = load(&l, "mechanism", &mut |w| warnings.push(w)).unwrap();
        assert!(!policy.methods_none);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // A settings.* service that breaks the shape in any one way asks too.
        for broken in [
            "[Service]\nAccount=any\nTargetGroup=wheel\nMethods=none\n",
            "[Service]\nAccount=any\nCallers=root\nMethods=none\n",
            "[Service]\nAccount=any\nCallers=root\nTargetGroup=wheel\nPurpose=change-password\nMethods=none\n",
            "[Service]\nAccount=any\nCallers=root\nTargetGroup=wheel\nGrant=seat\nMethods=none\n",
            "[Service]\nAccount=any\nCallers=root\nTargetGroup=wheel\nFirstPassword=local\nMethods=none\n",
        ] {
            write(&l[0], "settings.broken", broken);
            let policy = load(&l, "settings.broken", &mut |_| {}).unwrap();
            assert!(!policy.methods_none, "{broken}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn methods_none_in_a_drop_in_never_loosens_the_shipped_policies() {
        let root = scratch("dropins");
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../services");
        let etc = root.join("etc/ferrix/auth/services");
        let l = [shipped, etc.clone()];
        for service in ["hyprlock", "passwd", "login", "su"] {
            write(
                &etc.join(format!("{service}.d")),
                "10-none.conf",
                "[Service]\nMethods=none\n",
            );
            let mut warnings = Vec::new();
            let policy = load(&l, service, &mut |w| warnings.push(w)).unwrap();
            assert!(!policy.methods_none, "{service}");
            assert_eq!(warnings.len(), 1, "{service}: {warnings:?}");
        }
        // The shipped settings policies ask for a password, and a drop-in
        // may loosen them.
        for service in ["settings.hostname", "settings.services"] {
            let policy = load(&l, service, &mut |_| {}).unwrap();
            assert!(!policy.methods_none, "{service}");
            assert_eq!(policy.callers, Callers::Root, "{service}");
            assert_eq!(policy.target_group.as_deref(), Some("wheel"), "{service}");
            write(
                &etc.join(format!("{service}.d")),
                "10-none.conf",
                "[Service]\nMethods=none\n",
            );
            let policy = load(&l, service, &mut |_| {}).unwrap();
            assert!(policy.methods_none, "{service}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_shipped_policies_read_cleanly() {
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../services");
        let layers = [shipped];
        for service in ["settings.hostname", "settings.services"] {
            let mut warnings = Vec::new();
            let _ = load(&layers, service, &mut |w| warnings.push(w)).unwrap();
            assert!(warnings.is_empty(), "{service}: {warnings:?}");
        }
        for service in ["hyprlock", "passwd", "login"] {
            let mut warnings = Vec::new();
            let policy = load(&layers, service, &mut |w| warnings.push(w)).unwrap();
            assert!(warnings.is_empty(), "{service}: {warnings:?}");
            let expected = match service {
                "hyprlock" => (
                    AccountRule::Caller,
                    Callers::Any,
                    Purpose::Authenticate,
                    true,
                ),
                "passwd" => (
                    AccountRule::Any,
                    Callers::Any,
                    Purpose::ChangePassword,
                    false,
                ),
                _ => (
                    AccountRule::Any,
                    Callers::Root,
                    Purpose::Authenticate,
                    false,
                ),
            };
            assert_eq!(
                (
                    policy.account,
                    policy.callers,
                    policy.purpose,
                    policy.grant_seat
                ),
                expected,
                "{service}"
            );
        }
    }
}
