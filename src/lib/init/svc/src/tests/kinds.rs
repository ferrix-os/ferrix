//! The kinds' keys: §4.4's service keys with systemd's parsing, the
//! resource keys, `[Unit]`'s conditions and limits, mounts and sockets.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use crate::kind::{
    Config, Device, Input, KillMode, Listen, OomPolicy, Output, Restart, Service, ServiceType,
};
use crate::limits::{Controller, CpuWeight, Limits, Memory, Tasks};
use crate::source::{Entry, Layer, LoadError, Source, Unit};
use crate::unit::{Condition, Dependency, Test, conditions_hold};
use crate::value::{Signal, Span};

fn one(name: &str, text: &str) -> Source {
    let mut source = Source::new();
    let added = source.add(Layer::Image, name, Entry::File(text.as_bytes().to_vec()));
    assert!(added.is_ok(), "{name} is a unit path");
    source
}

fn load(name: &str, text: &str) -> Result<Unit, LoadError> {
    one(name, text).load(name)
}

fn service(text: &str) -> (Service, Vec<String>) {
    let unit = load("t.service", text).unwrap();
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

#[test]
fn service_defaults_are_systemds() {
    let (s, warnings) = service("[Service]\nExecStart=/a\n");
    assert!(warnings.is_empty());
    assert_eq!(s.service_type, ServiceType::Simple);
    assert_eq!(s.restart, Restart::No);
    assert_eq!(s.restart_sec, Duration::from_millis(100));
    assert_eq!(s.timeout_stop, Span::Finite(Duration::from_secs(90)));
    assert_eq!(s.kill.mode, KillMode::ControlGroup);
    assert_eq!(s.kill.signal, Signal::TERM);
    assert_eq!(s.oom_policy, OomPolicy::Stop);
    assert_eq!(s.standard_input, Input::Null);
    assert_eq!(s.standard_output, Output::Inherit);
    assert_eq!(s.slice, None);
    assert!(s.limits.is_empty());
    assert_eq!(s.tty(), None);
}

#[test]
fn the_service_keys_of_section_4_4() {
    let (s, warnings) = service(
        "[Service]\nType=notify\nExecStartPre=-/bin/prep\nExecStart=/bin/d --flag\nExecStop=/bin/stop\n\
         ExecReload=/bin/kill -HUP $MAINPID\nRestart=on-failure\nRestartSec=5s\n\
         KillMode=mixed\nKillSignal=SIGINT\nTimeoutStopSec=1min 45s\nSlice=user-1000.slice\n\
         MemoryMax=64M\nMemoryHigh=50%\nTasksMax=512\nCPUWeight=idle\nCPUQuota=150%\nIOWeight=200\n\
         OOMPolicy=continue\nDelegate=yes\nUser=daemon\nGroup=daemon\nWorkingDirectory=-/var/lib/d\n\
         Environment=A=1 \"B=2 3\"\nEnvironment=C=4\nEnvironmentFile=-/etc/default/d\n\
         StandardInput=tty\nStandardOutput=journal+console\nStandardError=file:/var/log/d\n\
         TTYPath=/dev/ttyS1\nOffers=ferrix.clipboard\nUses=ferrix.vfs ferrix.log\nNotifyFd=3\nPIDFile=/run/d.pid\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(s.service_type, ServiceType::Notify);
    assert!(s.exec_start_pre[0].ignore_failure);
    assert_eq!(s.exec_start[0].argv, ["/bin/d", "--flag"]);
    assert_eq!(s.exec_reload[0].argv, ["/bin/kill", "-HUP", "$MAINPID"]);
    assert_eq!(s.restart, Restart::OnFailure);
    assert_eq!(s.restart_sec, Duration::from_secs(5));
    assert_eq!(s.kill.mode, KillMode::Mixed);
    assert_eq!(s.kill.signal, Signal::INT);
    assert_eq!(s.timeout_stop, Span::Finite(Duration::from_secs(105)));
    assert_eq!(
        s.slice.as_ref().map(super::super::name::UnitName::as_str),
        Some("user-1000.slice")
    );
    assert_eq!(
        s.limits,
        Limits {
            memory_max: Some(Memory::Bytes(64 << 20)),
            memory_high: Some(Memory::Share(5_000)),
            tasks_max: Some(Tasks::Count(512)),
            cpu_weight: Some(CpuWeight::Idle),
            cpu_quota: Some(15_000),
            io_weight: Some(200),
        }
    );
    let controllers: Vec<Controller> = s.limits.controllers().collect();
    assert_eq!(
        controllers,
        [
            Controller::Memory,
            Controller::Pids,
            Controller::Cpu,
            Controller::Io
        ]
    );
    assert_eq!(s.oom_policy, OomPolicy::Continue);
    assert!(s.delegate);
    assert_eq!(s.user.as_deref(), Some("daemon"));
    let directory = s.working_directory.as_ref().unwrap();
    assert!(directory.missing_ok && directory.path == "/var/lib/d");
    let environment: Vec<String> = s
        .environment
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    assert_eq!(environment, ["A=1", "B=2 3", "C=4"]);
    assert_eq!(
        s.environment_files,
        [(String::from("/etc/default/d"), true)]
    );
    assert_eq!(s.standard_input, Input::Tty);
    assert_eq!(s.standard_output, Output::Log);
    assert_eq!(s.standard_error, Output::File(String::from("/var/log/d")));
    assert_eq!(s.tty(), Some("/dev/ttyS1"));
    assert_eq!(s.offers, ["ferrix.clipboard"]);
    assert_eq!(s.uses, ["ferrix.vfs", "ferrix.log"]);
    assert_eq!(s.notify_fd, Some(3));
    assert_eq!(s.pid_file.as_deref(), Some("/run/d.pid"));
}

#[test]
fn later_assignments_win_and_empty_ones_reset() {
    let (s, _) = service(
        "[Service]\nExecStart=/a\nRestart=always\nRestart=no\nMemoryMax=1G\nMemoryMax=\n\
         Environment=A=1\nEnvironment=\nEnvironment=B=2\nTimeoutStopSec=0\nTasksMax=infinity\n",
    );
    assert_eq!(s.restart, Restart::No);
    assert_eq!(s.limits.memory_max, None);
    assert_eq!(s.environment, [(String::from("B"), String::from("2"))]);
    assert_eq!(s.timeout_stop, Span::Infinity, "zero means no timeout");
    assert_eq!(s.limits.tasks_max, Some(Tasks::Infinity));
}

#[test]
fn bad_values_warn_and_keep_the_previous_value() {
    let (s, warnings) = service(
        "[Service]\nExecStart=/a\nRestart=sometimes\nRestartSec=soon\nKillSignal=SIGNOPE\n\
         CPUWeight=0\nIOWeight=10001\nMemoryMax=lots\nCPUQuota=0%\nSlice=system.service\n\
         Type=dbus\nWorkingDirectory=relative\nStandardOutput=fd:3\nOffers=a/b\nNotifyFd=1\n",
    );
    assert_eq!(warnings.len(), 13, "{warnings:?}");
    assert!(warnings[0].starts_with("Failed to parse Restart= value 'sometimes'"));
    assert_eq!(s.restart, Restart::No);
    assert_eq!(s.kill.signal, Signal::TERM);
    assert!(s.limits.is_empty());
    assert_eq!(s.slice, None);
    assert_eq!(s.service_type, ServiceType::Simple);
}

#[test]
fn what_service_verify_refuses() {
    let refused = |text: &str| match load("t.service", text) {
        Err(LoadError::Refused(error)) => error.message,
        other => panic!("{other:?}"),
    };
    assert!(refused("[Service]\nType=simple\n").contains("no ExecStart="));
    assert!(
        refused("[Service]\nExecStart=/a\nExecStart=/b\n").contains("more than one ExecStart=")
    );
    assert!(refused("[Service]\nExecStart=/a ; /b\n").contains("more than one ExecStart="));
    assert!(refused("[Service]\nType=oneshot\nExecStart=/a\nRestart=always\n").contains("oneshot"));
    let (oneshot, _) =
        service("[Service]\nType=oneshot\nExecStart=/a\nExecStart=/b\nRemainAfterExit=yes\n");
    assert_eq!(oneshot.exec_start.len(), 2);
    assert!(oneshot.remain_after_exit);
    let (stop_only, _) = service("[Service]\nType=oneshot\nExecStop=/a\n");
    assert!(stop_only.exec_start.is_empty());
}

#[test]
fn a_native_service_may_name_its_user() {
    let (native, warnings) = service(
        "[Service]\nType=native\nExecStart=/a\nUser=ferrix\nGroup=ferrix\n\
         SupplementaryGroups=wheel\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(native.service_type, ServiceType::Native);
    assert_eq!(native.user.as_deref(), Some("ferrix"));
    assert_eq!(native.group.as_deref(), Some("ferrix"));
    assert_eq!(native.supplementary_groups, ["wheel"]);
}

#[test]
fn unit_section_dependencies_conditions_and_limits() {
    let unit = load(
        "t.service",
        "[Unit]\nDescription=T\nRequires=a.service b.mount\nWants=c.service\nWants=c.service\n\
         Requires=\nBindsTo=d.service\nPartOf=e.target\nConflicts=shutdown.target\nBefore=f.target\n\
         After=g.target not-a-unit\nOnFailure=rescue.target\nDefaultDependencies=no\n\
         ConditionPathExists=!/etc/nope\nConditionPathExists=|/a\nConditionPathIsDirectory=|/b\n\
         ConditionKernelCommandLine=quiet\nAssertFileNotEmpty=/etc/x\nConditionPathExists=relative\n\
         StartLimitIntervalSec=30s\nStartLimitBurst=3\n[Service]\nExecStart=/a\n",
    )
    .unwrap();
    let u = &unit.unit;
    let named = |dependency| {
        u.named(dependency)
            .iter()
            .map(super::super::name::UnitName::as_str)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        named(Dependency::Requires),
        ["a.service", "b.mount"],
        "dependencies cannot be reset"
    );
    assert_eq!(named(Dependency::Wants), ["c.service"]);
    assert_eq!(named(Dependency::BindsTo), ["d.service"]);
    assert_eq!(named(Dependency::PartOf), ["e.target"]);
    assert_eq!(named(Dependency::Conflicts), ["shutdown.target"]);
    assert_eq!(named(Dependency::Before), ["f.target"]);
    assert_eq!(named(Dependency::After), ["g.target"]);
    assert_eq!(named(Dependency::OnFailure), ["rescue.target"]);
    assert!(!u.default_dependencies);
    assert_eq!(u.conditions.len(), 4);
    assert_eq!(
        u.conditions[0],
        Condition {
            test: Test::PathExists,
            argument: String::from("/etc/nope"),
            negate: true,
            trigger: false
        }
    );
    assert!(u.conditions[1].trigger && u.conditions[2].trigger);
    assert_eq!(u.asserts.len(), 1);
    assert_eq!(u.start_limit.interval, Duration::from_secs(30));
    assert_eq!(u.start_limit.burst, 3);
    assert_eq!(
        unit.warnings.len(),
        2,
        "the bad name and the relative path: {:?}",
        unit.warnings
    );
}

#[test]
fn start_limits_are_taken_from_service_too() {
    let unit = load(
        "t.service",
        "[Service]\nExecStart=/a\nStartLimitBurst=2\nStartLimitIntervalSec=0\n",
    )
    .unwrap();
    assert_eq!(unit.unit.start_limit.burst, 2);
    assert_eq!(unit.unit.start_limit.interval, Duration::ZERO);
    assert!(unit.warnings.is_empty());
}

#[test]
fn an_empty_condition_resets_the_list() {
    let unit = load("t.target", "[Unit]\nConditionPathExists=/a\nConditionHost=x\nConditionPathExists=\nConditionUser=root\n").unwrap();
    assert_eq!(unit.unit.conditions.len(), 1);
    assert_eq!(unit.unit.conditions[0].test, Test::User);
}

#[test]
fn how_conditions_combine() {
    let c = |negate, trigger| Condition {
        test: Test::PathExists,
        argument: String::from("/x"),
        negate,
        trigger,
    };
    assert!(conditions_hold(&[], |_| false));
    assert!(conditions_hold(&[c(false, false)], |_| true));
    assert!(!conditions_hold(&[c(false, false)], |_| false));
    assert!(conditions_hold(&[c(true, false)], |_| false), "negated");
    let mut answers = [false, true].into_iter();
    assert!(
        conditions_hold(&[c(false, true), c(false, true)], |_| answers
            .next()
            .unwrap()),
        "one trigger is enough"
    );
    assert!(
        !conditions_hold(&[c(false, true), c(false, true)], |_| false),
        "but one is needed"
    );
}

#[test]
fn install_section() {
    let unit = load(
        "t@.service",
        "[Service]\nExecStart=/a\n[Install]\nWantedBy=multi-user.target\nWantedBy=graphical.target\nRequiredBy=x.target\nAlias=u@.service\nDefaultInstance=tty1\n",
    );
    assert!(
        matches!(unit, Err(LoadError::Name(_))),
        "a template itself does not load"
    );
    let mut source = Source::new();
    source
        .add(Layer::Image, "t.service", Entry::File(b"[Service]\nExecStart=/a\n[Install]\nWantedBy=multi-user.target graphical.target\nWantedBy=\nWantedBy=x.target\nAlso=y.service\n".to_vec()))
        .unwrap();
    let unit = source.load("t.service").unwrap();
    assert_eq!(
        unit.install
            .wanted_by
            .iter()
            .map(super::super::name::UnitName::as_str)
            .collect::<Vec<_>>(),
        ["x.target"]
    );
    assert_eq!(unit.install.also.len(), 1);
}

#[test]
fn slices_and_scopes() {
    let slice = load("system.slice", "[Slice]\nMemoryMax=1G\nCPUWeight=50\n").unwrap();
    match slice.config {
        Config::Slice(slice) => {
            assert_eq!(slice.limits.memory_max, Some(Memory::Bytes(1 << 30)));
            assert_eq!(slice.limits.cpu_weight, Some(CpuWeight::Weight(50)));
        }
        other => panic!("{other:?}"),
    }
    let scope = load(
        "app-foot-12.scope",
        "[Scope]\nSlice=user-1000.slice\nKillMode=process\nTimeoutStopSec=5\nTasksMax=10%\n",
    )
    .unwrap();
    match scope.config {
        Config::Scope(scope) => {
            assert_eq!(
                scope
                    .slice
                    .as_ref()
                    .map(super::super::name::UnitName::as_str),
                Some("user-1000.slice")
            );
            assert_eq!(scope.kill.mode, KillMode::Process);
            assert_eq!(scope.timeout_stop, Span::Finite(Duration::from_secs(5)));
            assert_eq!(scope.limits.tasks_max, Some(Tasks::Share(1_000)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn mounts_are_named_after_where_they_mount() {
    let unit = load(
        "sys-fs-cgroup.mount",
        "[Mount]\nWhat=cgroup2\nWhere=/sys/fs/cgroup\nType=cgroup2\nOptions=nsdelegate\n",
    )
    .unwrap();
    match unit.config {
        Config::Mount(mount) => {
            assert_eq!(mount.r#where, "/sys/fs/cgroup");
            assert_eq!(mount.fs_type.as_deref(), Some("cgroup2"));
            assert_eq!(mount.options, "nsdelegate");
        }
        other => panic!("{other:?}"),
    }
    let derived = load("run.mount", "[Mount]\nWhat=tmpfs\nType=tmpfs\n").unwrap();
    assert!(matches!(derived.config, Config::Mount(ref m) if m.r#where == "/run"));
    let root = load("-.mount", "[Mount]\nWhat=/dev/vda\n").unwrap();
    assert!(matches!(root.config, Config::Mount(ref m) if m.r#where == "/"));
    assert!(matches!(
        load("run.mount", "[Mount]\nWhat=tmpfs\nWhere=/tmp\n"),
        Err(LoadError::Refused(_))
    ));
    assert!(matches!(
        load("run.mount", "[Mount]\nWhere=/run\n"),
        Err(LoadError::Refused(_))
    ));
}

#[test]
fn sockets_parse_for_version_two() {
    let unit = load(
        "sshd.socket",
        "[Socket]\nListenStream=22\nAccept=yes\nSocketMode=0600\nService=sshd.service\n",
    )
    .unwrap();
    match unit.config {
        Config::Socket(socket) => {
            assert_eq!(socket.listen, [Listen::Stream(String::from("22"))]);
            assert!(socket.accept);
            assert_eq!(socket.mode, 0o600);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        load("x.socket", "[Socket]\nAccept=no\n"),
        Err(LoadError::Refused(_))
    ));
}

#[test]
fn a_target_has_only_unit_and_install() {
    let unit = load("multi-user.target", "[Unit]\nDescription=Multi-User System\nRequires=basic.target\nAfter=basic.target\nAllowIsolate=yes\n").unwrap();
    assert!(matches!(unit.config, Config::Target));
    assert!(unit.unit.allow_isolate);
    assert!(unit.warnings.is_empty());
}

#[test]
fn a_device_needs_no_file_and_its_name_is_its_node() {
    let source = Source::new();
    let card = source.load("dev-dri-card0.device").unwrap();
    assert_eq!(
        card.config,
        Config::Device(Device {
            path: String::from("/dev/dri/card0")
        })
    );
    assert!(
        crate::kind::of(card.name.unit_type())
            .implied(&card, &|_| true)
            .is_empty(),
        "no default dependencies"
    );
    assert!(matches!(
        source.load("sys-fs-cgroup.device"),
        Err(LoadError::Refused(_))
    ));
    let described = load("dev-sda.device", "[Unit]\nDescription=The disk\n").unwrap();
    assert_eq!(described.unit.description.as_deref(), Some("The disk"));
}
