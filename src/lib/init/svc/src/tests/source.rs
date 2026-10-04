//! Loading: layers, drop-ins, masking, aliases, templates and links (§4.1).

use alloc::borrow::ToOwned;
use alloc::vec::Vec;

use crate::UnitName;
use crate::kind::Config;
use crate::name::NameError;
use crate::source::{Entry, Layer, LoadError, Source, Unit};
use crate::unit::Dependency;

fn file(text: &str) -> Entry {
    Entry::File(text.as_bytes().to_vec())
}

fn source(entries: &[(Layer, &str, Entry)]) -> Source {
    let mut source = Source::new();
    for (layer, path, entry) in entries {
        source.add(*layer, path, entry.clone()).unwrap();
    }
    source
}

fn service(unit: &Unit) -> &crate::kind::Service {
    match &unit.config {
        Config::Service(service) => service,
        other => panic!("not a service: {other:?}"),
    }
}

fn argv(unit: &Unit) -> Vec<&str> {
    service(unit).exec_start[0]
        .argv
        .iter()
        .map(alloc::string::String::as_str)
        .collect()
}

fn names(list: &[UnitName]) -> Vec<&str> {
    list.iter().map(UnitName::as_str).collect()
}

const GETTY: &str = "[Unit]\nDescription=Login prompt on %i\nAfter=basic.target\n\n\
    [Service]\nType=exec\nExecStart=/sbin/getty %i\nRestart=always\nRestartSec=0\n\
    TTYPath=/dev/%i\nTasksMax=512\n\n[Install]\nWantedBy=multi-user.target\n";

#[test]
fn the_design_documents_getty_template_loads() {
    let source = source(&[(Layer::Image, "getty@.service", file(GETTY))]);
    let unit = source.load("getty@console.service").unwrap();
    assert!(unit.warnings.is_empty(), "{:?}", unit.warnings);
    assert_eq!(unit.name.as_str(), "getty@console.service");
    assert_eq!(
        unit.fragment.as_deref(),
        Some("/lib/ferrix/units/getty@.service")
    );
    assert_eq!(
        unit.unit.description.as_deref(),
        Some("Login prompt on console")
    );
    assert_eq!(names(unit.unit.named(Dependency::After)), ["basic.target"]);
    assert_eq!(argv(&unit), ["/sbin/getty", "console"]);
    assert_eq!(service(&unit).tty_path.as_deref(), Some("/dev/console"));
    assert_eq!(names(&unit.install.wanted_by), ["multi-user.target"]);
    assert_eq!(
        source.load("getty@.service"),
        Err(LoadError::Name(NameError::Template))
    );
}

#[test]
fn a_higher_layer_replaces_the_whole_file() {
    let source = source(&[
        (
            Layer::Image,
            "a.service",
            file("[Service]\nExecStart=/lib-a\nUser=x\n"),
        ),
        (
            Layer::Admin,
            "a.service",
            file("[Service]\nExecStart=/etc-a\n"),
        ),
    ]);
    let unit = source.load("a.service").unwrap();
    assert_eq!(argv(&unit), ["/etc-a"]);
    assert_eq!(
        service(&unit).user,
        None,
        "nothing of the lower file survives"
    );
    assert_eq!(
        unit.fragment.as_deref(),
        Some("/etc/ferrix/units/a.service")
    );
}

#[test]
fn an_instance_file_beats_its_template_in_any_layer() {
    let source = source(&[
        (
            Layer::Image,
            "getty@console.service",
            file("[Service]\nExecStart=/own\n"),
        ),
        (
            Layer::Runtime,
            "getty@.service",
            file("[Service]\nExecStart=/template\n"),
        ),
    ]);
    assert_eq!(
        argv(&source.load("getty@console.service").unwrap()),
        ["/own"]
    );
    assert_eq!(
        argv(&source.load("getty@ttyS0.service").unwrap()),
        ["/template"]
    );
}

#[test]
fn drop_ins_apply_in_name_order_across_layers() {
    let source = source(&[
        (
            Layer::Image,
            "a.service",
            file("[Service]\nExecStart=/a\nRestart=no\n"),
        ),
        (
            Layer::Image,
            "a.service.d/20-lib.conf",
            file("[Service]\nRestart=always\n"),
        ),
        (
            Layer::Admin,
            "a.service.d/10-etc.conf",
            file("[Service]\nRestart=on-failure\nUser=etc\n"),
        ),
        (
            Layer::Image,
            "a.service.d/30-hidden.conf",
            file("[Service]\nUser=lib\n"),
        ),
        (
            Layer::Runtime,
            "a.service.d/30-hidden.conf",
            file("[Service]\nUser=run\n"),
        ),
        (
            Layer::Image,
            "a.service.d/40-exec.conf",
            file("[Service]\nExecStart=\nExecStart=/b\n"),
        ),
    ]);
    let unit = source.load("a.service").unwrap();
    let service = service(&unit);
    assert_eq!(service.restart, crate::kind::Restart::Always, "20 after 10");
    assert_eq!(
        service.user.as_deref(),
        Some("run"),
        "30 from /run hides /lib's"
    );
    assert_eq!(service.exec_start.len(), 1, "an empty ExecStart= clears");
    assert_eq!(service.exec_start[0].path, "/b");
    let drop_ins: Vec<&str> = unit.drop_ins.iter().map(|path| &**path).collect();
    assert_eq!(
        drop_ins,
        [
            "/etc/ferrix/units/a.service.d/10-etc.conf",
            "/lib/ferrix/units/a.service.d/20-lib.conf",
            "/run/ferrix/units/a.service.d/30-hidden.conf",
            "/lib/ferrix/units/a.service.d/40-exec.conf",
        ]
    );
}

#[test]
fn a_template_drop_in_applies_to_its_instances() {
    let source = source(&[
        (Layer::Image, "getty@.service", file(GETTY)),
        (
            Layer::Admin,
            "getty@.service.d/baud.conf",
            file("[Service]\nExecStart=\nExecStart=/sbin/agetty 115200 %I\n"),
        ),
        (
            Layer::Admin,
            "getty@ttyS0.service.d/only.conf",
            file("[Service]\nUser=serial\n"),
        ),
    ]);
    let console = source.load("getty@console.service").unwrap();
    assert_eq!(argv(&console), ["/sbin/agetty", "115200", "console"]);
    assert_eq!(service(&console).user, None);
    let serial = source.load("getty@ttyS0.service").unwrap();
    assert_eq!(service(&serial).user.as_deref(), Some("serial"));
}

#[test]
fn masking() {
    let source = source(&[
        (Layer::Image, "a.service", file("[Service]\nExecStart=/a\n")),
        (Layer::Admin, "a.service", Entry::Masked),
        (Layer::Image, "b.service", file("[Service]\nExecStart=/b\n")),
        (Layer::Runtime, "b.service", file("")),
        (Layer::Image, "c.service", file("[Service]\nExecStart=/c\n")),
        (
            Layer::Image,
            "c.service.d/x.conf",
            file("[Service]\nUser=x\n"),
        ),
        (Layer::Admin, "c.service.d/x.conf", Entry::Masked),
        (Layer::Image, "t@.service", Entry::Masked),
    ]);
    assert_eq!(source.load("a.service"), Err(LoadError::Masked));
    assert_eq!(
        source.load("b.service"),
        Err(LoadError::Masked),
        "an empty file masks"
    );
    let c = source.load("c.service").unwrap();
    assert_eq!(
        service(&c).user,
        None,
        "a masked drop-in hides its namesake"
    );
    assert!(c.drop_ins.is_empty());
    assert_eq!(
        source.load("t@x.service"),
        Err(LoadError::Masked),
        "a masked template masks its instances"
    );
}

#[test]
fn aliases_lead_to_the_unit() {
    let source = source(&[
        (
            Layer::Image,
            "multi-user.target",
            file("[Unit]\nDescription=Multi-user\n"),
        ),
        (
            Layer::Admin,
            "default.target",
            Entry::Alias("multi-user.target".to_owned()),
        ),
        (
            Layer::Image,
            "tty@.service",
            Entry::Alias("getty@.service".to_owned()),
        ),
        (Layer::Image, "getty@.service", file(GETTY)),
        (
            Layer::Image,
            "loop1.target",
            Entry::Alias("loop2.target".to_owned()),
        ),
        (
            Layer::Image,
            "loop2.target",
            Entry::Alias("loop1.target".to_owned()),
        ),
        (
            Layer::Image,
            "multi-user.target.d/x.conf",
            file("[Unit]\nAfter=a.service\n"),
        ),
        (
            Layer::Image,
            "default.target.d/y.conf",
            file("[Unit]\nAfter=b.service\n"),
        ),
    ]);
    let default = source.load("default.target").unwrap();
    assert_eq!(default.name.as_str(), "multi-user.target");
    assert_eq!(names(&default.aliases), ["default.target"]);
    assert_eq!(
        names(default.unit.named(Dependency::After)),
        ["a.service", "b.service"],
        "drop-ins of the unit and of its alias both apply"
    );
    let tty = source.load("tty@ttyS1.service").unwrap();
    assert_eq!(tty.name.as_str(), "getty@ttyS1.service");
    assert_eq!(
        argv(&tty),
        ["/sbin/getty", "ttyS1"],
        "specifiers use the name the file is under"
    );
    assert_eq!(source.load("loop1.target"), Err(LoadError::LinkLoop));
}

#[test]
fn wants_and_requires_directories_add_dependencies() {
    let source = source(&[
        (
            Layer::Image,
            "multi-user.target",
            file("[Unit]\nWants=sshd.service\n"),
        ),
        (
            Layer::Runtime,
            "multi-user.target.wants/getty@console.service",
            Entry::Alias("getty@.service".to_owned()),
        ),
        (
            Layer::Admin,
            "multi-user.target.wants/sshd.service",
            Entry::Alias("sshd.service".to_owned()),
        ),
        (
            Layer::Admin,
            "multi-user.target.wants/gone.service",
            Entry::Masked,
        ),
        (
            Layer::Image,
            "multi-user.target.requires/basic.target",
            file("x"),
        ),
    ]);
    let unit = source.load("multi-user.target").unwrap();
    assert_eq!(
        names(unit.unit.named(Dependency::Wants)),
        ["sshd.service", "getty@console.service"]
    );
    assert_eq!(
        names(unit.unit.named(Dependency::Requires)),
        ["basic.target"]
    );
}

#[test]
fn kinds_that_need_no_file_load_without_one() {
    let source = Source::new();
    assert!(matches!(
        source.load("net.builtin").unwrap().config,
        Config::Builtin
    ));
    assert!(matches!(
        source.load("system.slice").unwrap().config,
        Config::Slice(_)
    ));
    assert!(matches!(
        source.load("session-1.scope").unwrap().config,
        Config::Scope(_)
    ));
    assert!(source.load("-.slice").unwrap().fragment.is_none());
    assert_eq!(source.load("sshd.service"), Err(LoadError::NotFound));
    assert_eq!(source.load("basic.target"), Err(LoadError::NotFound));
}

#[test]
fn add_takes_only_unit_paths() {
    let mut source = Source::new();
    for good in [
        "a.service",
        "a.service.d/x.conf",
        "a.target.wants/b.service",
        "a@.service.requires/b.target",
    ] {
        assert!(source.add(Layer::Image, good, file("")).is_ok(), "{good}");
    }
    for bad in [
        "README",
        "a.service.d/x.txt",
        "a.service.d/.conf",
        "a.target.wants/README",
        "x/y.service",
        "a.service.d/b/c.conf",
        "a.timer",
    ] {
        assert!(source.add(Layer::Image, bad, file("")).is_err(), "{bad}");
    }
    assert_eq!(names(&source.names()), ["a.service"]);
}

#[test]
fn unknown_sections_keys_and_specifiers_warn_and_load() {
    let source = source(&[(
        Layer::Image,
        "a.service",
        file(
            "[Unit]\nFrobnicate=yes\nX-Mine=1\n[Service]\nExecStart=/a %H\nExecStart=/a\nProtectHome=yes\n[Timer]\nOnBoot=1\n[X-Vendor]\nA=1\n",
        ),
    )]);
    let unit = source.load("a.service").unwrap();
    let messages: Vec<&str> = unit
        .warnings
        .list()
        .iter()
        .map(|w| w.message.as_str())
        .collect();
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert!(messages.contains(&"Unknown key name 'Frobnicate' in section 'Unit', ignoring."));
    assert!(messages.contains(&"Unknown section 'Timer'. Ignoring."));
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("Failed to resolve unit specifiers in '/a %H'"))
    );
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("ProtectHome= is not built"))
    );
    assert_eq!(argv(&unit), ["/a"]);
}

#[test]
fn a_file_that_does_not_parse_does_not_load() {
    let source = source(&[
        (Layer::Image, "a.service", file("[Service\nExecStart=/a\n")),
        (Layer::Image, "b.service", file("[Service]\nExecStart=/b\n")),
        (Layer::Image, "b.service.d/bad.conf", file("[Unit")),
    ]);
    assert!(matches!(source.load("a.service"), Err(LoadError::Syntax(e)) if e.line == 1));
    assert!(
        matches!(source.load("b.service"), Err(LoadError::Syntax(e)) if e.file.ends_with("bad.conf"))
    );
}
