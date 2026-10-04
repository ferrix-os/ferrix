//! The sandboxing keys of L13 (`docs/INIT.md` §4.5): systemd's values and
//! warnings, `SystemCallFilter=`'s merging, and the `+` prefix that runs a
//! command without them.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use super::rig::Rig;
use crate::event::{Action, Request, SpawnSpec};
use crate::kind::{Config, FilterRule, ProtectSystem, Sandbox, Service, SystemCallFilter};
use crate::source::{Entry, Layer, Source};

fn service(text: &str) -> (Service, Vec<String>) {
    let mut source = Source::new();
    let added = source.add(
        Layer::Image,
        "t.service",
        Entry::File(text.as_bytes().to_vec()),
    );
    assert!(added.is_ok());
    let unit = source.load("t.service").unwrap();
    let warnings = unit
        .warnings
        .list()
        .iter()
        .map(|w| w.message.clone())
        .collect();
    match unit.config {
        Config::Service(service) => (*service, warnings),
        other => panic!("{other:?}"),
    }
}

fn sandbox(keys: &str) -> (Sandbox, Vec<String>) {
    let (service, warnings) = service(&alloc::format!("[Service]\nExecStart=/a\n{keys}"));
    (service.sandbox, warnings)
}

fn rule(name: &str, add: bool, action: Option<&str>) -> FilterRule {
    FilterRule {
        name: name.to_owned(),
        add,
        action: action.map(str::to_owned),
    }
}

#[test]
fn nothing_asked_is_an_empty_sandbox() {
    let (s, warnings) = sandbox("");
    assert!(warnings.is_empty());
    assert!(s.is_empty());
    assert!(!s.needs_mount_namespace());
}

#[test]
fn the_three_mount_and_privilege_keys_take_systemds_values() {
    let (s, warnings) = sandbox("NoNewPrivileges=yes\nPrivateTmp=true\nProtectSystem=strict\n");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(s.no_new_privileges);
    assert!(s.private_tmp);
    assert_eq!(s.protect_system, ProtectSystem::Strict);
    assert!(s.needs_mount_namespace());

    for (value, level) in [
        ("yes", ProtectSystem::Yes),
        ("on", ProtectSystem::Yes),
        ("full", ProtectSystem::Full),
        ("strict", ProtectSystem::Strict),
        ("no", ProtectSystem::No),
        ("false", ProtectSystem::No),
    ] {
        let (s, warnings) = sandbox(&alloc::format!("ProtectSystem={value}\n"));
        assert!(warnings.is_empty(), "{value}: {warnings:?}");
        assert_eq!(s.protect_system, level, "{value}");
    }
    // systemd 256's `disconnected` is what Ferrix's `yes` does already.
    let (s, warnings) = sandbox("PrivateTmp=disconnected\n");
    assert!(warnings.is_empty());
    assert!(s.private_tmp);
}

#[test]
fn a_bad_value_warns_and_keeps_the_last_good_one() {
    let (s, warnings) = sandbox(
        "ProtectSystem=full\nProtectSystem=sometimes\nNoNewPrivileges=yes\nNoNewPrivileges=maybe\n\
         PrivateTmp=yes\nPrivateTmp=connected\nPrivateNetwork=nope\n",
    );
    assert_eq!(s.protect_system, ProtectSystem::Full);
    assert!(s.no_new_privileges);
    assert!(s.private_tmp);
    assert!(!s.private_network);
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("Failed to parse ProtectSystem= value 'sometimes'"))
    );
}

#[test]
fn a_later_assignment_wins() {
    let (s, _) = sandbox("PrivateTmp=yes\nPrivateTmp=no\nProtectSystem=strict\nProtectSystem=no\n");
    assert!(s.is_empty());
}

#[test]
fn private_network_is_parsed_for_the_backend_to_refuse() {
    let (s, warnings) = sandbox("PrivateNetwork=yes\n");
    assert!(warnings.is_empty());
    assert!(s.private_network);
    assert!(!s.needs_mount_namespace());
}

#[test]
fn an_allow_list_starts_with_default_and_a_later_deny_takes_from_it() {
    let (s, warnings) =
        sandbox("SystemCallFilter=@system-service read\nSystemCallFilter=~@mount write\n");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        s.system_call_filter,
        Some(SystemCallFilter {
            allow_list: true,
            rules: alloc::vec![
                rule("@default", true, None),
                rule("@system-service", true, None),
                rule("read", true, None),
                rule("@mount", false, None),
                rule("write", false, None),
            ],
        })
    );
}

#[test]
fn a_deny_list_takes_actions_and_a_later_allow_takes_from_it() {
    let (s, warnings) =
        sandbox("SystemCallFilter=~@privileged @reboot:EPERM mount:13\nSystemCallFilter=reboot\n");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        s.system_call_filter,
        Some(SystemCallFilter {
            allow_list: false,
            rules: alloc::vec![
                rule("@privileged", true, None),
                rule("@reboot", true, Some("EPERM")),
                rule("mount", true, Some("13")),
                rule("reboot", false, None),
            ],
        })
    );
}

#[test]
fn an_empty_filter_resets_and_the_next_one_decides_again() {
    let (s, _) = sandbox("SystemCallFilter=read\nSystemCallFilter=\nSystemCallFilter=~ptrace\n");
    let filter = s.system_call_filter.unwrap();
    assert!(!filter.allow_list);
    assert_eq!(filter.rules, [rule("ptrace", true, None)]);
    let (s, _) = sandbox("SystemCallFilter=read\nSystemCallFilter=\n");
    assert_eq!(s.system_call_filter, None);
}

#[test]
fn unknown_groups_bad_names_and_bad_actions_warn_and_are_dropped() {
    let (s, warnings) = sandbox(
        "SystemCallFilter=~@frobnicate Open:EPERM ptrace:EPERM bpf:0 kexec_load:kill \
         reboot:eperm\nSystemCallFilter=~init_module\n",
    );
    let filter = s.system_call_filter.unwrap();
    assert_eq!(
        filter.rules,
        [
            rule("ptrace", true, Some("EPERM")),
            rule("kexec_load", true, Some("kill")),
            rule("init_module", true, None),
        ]
    );
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    assert!(warnings.contains(&String::from(
        "Unknown system call group, ignoring: @frobnicate"
    )));
    assert!(warnings.contains(&String::from("Failed to parse system call, ignoring: Open")));
    assert!(warnings.contains(&String::from(
        "Failed to parse error number, ignoring: bpf:0"
    )));

    let (s, warnings) = sandbox("SystemCallFilter=read:EPERM write\n");
    assert_eq!(
        s.system_call_filter.unwrap().rules,
        [rule("@default", true, None), rule("write", true, None)]
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[test]
fn the_keys_not_built_warn_by_name_and_load() {
    let (s, warnings) = sandbox("ProtectHome=yes\nSystemCallArchitectures=native\n");
    assert!(s.is_empty());
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .all(|w| w.contains("= is not built (landing L13 built"))
    );
}

fn spawns(actions: &[Action]) -> Vec<SpawnSpec> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Spawn { spec, .. } => Some((**spec).clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_command_gets_the_sandbox_but_one_with_a_plus() {
    let mut rig = Rig::new(&[(
        "s.service",
        "[Service]\nExecStartPre=+/bin/pre\nExecStart=/bin/main\nPrivateTmp=yes\n\
         NoNewPrivileges=yes\nProtectSystem=full\n",
    )]);
    let pre = spawns(&rig.request(Request::start("s.service")));
    assert_eq!(pre.len(), 1);
    assert!(pre[0].sandbox.is_empty(), "{:?}", pre[0].sandbox);
    let (pre, _) = rig.spawned("s.service");
    let main = spawns(&rig.exit(pre, 0));
    assert_eq!(main.len(), 1);
    assert!(main[0].sandbox.private_tmp);
    assert!(main[0].sandbox.no_new_privileges);
    assert_eq!(main[0].sandbox.protect_system, ProtectSystem::Full);
}
