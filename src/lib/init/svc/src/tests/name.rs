//! Unit names and specifiers, pinned against `unit-name.c` and
//! `unit-printf.c`.

use crate::UnitName;
use crate::name::{NameError, UnitType, escape_path, unescape};
use crate::specifier::{SpecifierError, expand};

fn name(text: &str) -> UnitName {
    UnitName::parse(text).unwrap()
}

#[test]
fn valid_and_invalid_names() {
    assert_eq!(name("sshd.service").unit_type(), UnitType::Service);
    assert_eq!(name("a.b.c.target").stem(), "a.b.c");
    assert_eq!(name("sys-fs-cgroup.mount").unit_type(), UnitType::Mount);
    assert_eq!(name("net.builtin").unit_type(), UnitType::Builtin);
    assert_eq!(UnitName::parse(""), Err(NameError::Length));
    assert_eq!(UnitName::parse("sshd"), Err(NameError::Suffix));
    assert_eq!(UnitName::parse("sshd.timer"), Err(NameError::Suffix));
    assert_eq!(UnitName::parse(".service"), Err(NameError::Prefix));
    assert_eq!(UnitName::parse("@x.service"), Err(NameError::Prefix));
    assert_eq!(UnitName::parse("a b.service"), Err(NameError::Character));
    assert_eq!(UnitName::parse("a/b.service"), Err(NameError::Character));
    let long = alloc::format!("{}.service", "x".repeat(248));
    assert_eq!(UnitName::parse(&long), Err(NameError::Length), "256 bytes");
    let longest = alloc::format!("{}.service", "x".repeat(247));
    assert!(UnitName::parse(&longest).is_ok());
}

#[test]
fn templates_and_instances() {
    let template = name("getty@.service");
    assert!(template.is_template() && !template.is_instance());
    assert_eq!(template.instance(), Some(""));
    let instance = template.instantiate("ttyS0").unwrap();
    assert_eq!(instance.as_str(), "getty@ttyS0.service");
    assert_eq!(instance.prefix(), "getty");
    assert_eq!(instance.instance(), Some("ttyS0"));
    assert_eq!(instance.template(), Some(template.clone()));
    assert_eq!(name("sshd.service").instance(), None);
    assert_eq!(name("sshd.service").template(), None);
    assert_eq!(
        name("a@b@c.service").instance(),
        Some("b@c"),
        "the first @ splits"
    );
    assert_eq!(
        name("sshd.service").instantiate("x"),
        Err(NameError::Template)
    );
    assert_eq!(template.instantiate(""), Err(NameError::Template));
}

#[test]
fn slices_nest_by_dashes() {
    assert_eq!(
        name("user-1000.slice").slice_parent(),
        Some(name("user.slice"))
    );
    assert_eq!(name("user.slice").slice_parent(), Some(name("-.slice")));
    assert_eq!(name("-.slice").slice_parent(), None);
    assert_eq!(UnitName::root_slice(), name("-.slice"));
    assert_eq!(name("sshd.service").slice_parent(), None);
    for bad in ["a-.slice", "-a.slice", "a--b.slice", "a@b.slice"] {
        assert_eq!(UnitName::parse(bad), Err(NameError::Slice), "{bad}");
    }
}

#[test]
fn paths_escape_as_systemd_escapes_them() {
    assert_eq!(escape_path("/").unwrap(), "-");
    assert_eq!(escape_path("/sys/fs/cgroup").unwrap(), "sys-fs-cgroup");
    assert_eq!(escape_path("//sys//fs/./cgroup/").unwrap(), "sys-fs-cgroup");
    assert_eq!(escape_path("/home/a-b").unwrap(), "home-a\\x2db");
    assert_eq!(escape_path("/.hidden/x.y").unwrap(), "\\x2ehidden-x.y");
    assert_eq!(escape_path("/a b").unwrap(), "a\\x20b");
    assert_eq!(escape_path("relative"), Err(NameError::Path));
    assert_eq!(escape_path("/a/../b"), Err(NameError::Path));
    assert_eq!(
        UnitName::for_path("/sys/fs/cgroup", UnitType::Mount).unwrap(),
        name("sys-fs-cgroup.mount")
    );
    assert_eq!(unescape("home-a\\x2db"), "home/a-b");
    assert_eq!(
        unescape("x\\x2"),
        "x\\x2",
        "a short escape stays as written"
    );
}

#[test]
fn specifiers_expand_from_the_name() {
    let unit = name("getty@tty\\x2d1.service");
    assert_eq!(expand("%i", &unit).unwrap(), "tty\\x2d1");
    assert_eq!(expand("%I", &unit).unwrap(), "tty-1");
    assert_eq!(expand("%n", &unit).unwrap(), "getty@tty\\x2d1.service");
    assert_eq!(expand("%N", &unit).unwrap(), "getty@tty\\x2d1");
    assert_eq!(expand("%p", &unit).unwrap(), "getty");
    assert_eq!(expand("100%%", &unit).unwrap(), "100%");
    assert_eq!(
        expand("/dev/%i on %t", &unit).unwrap(),
        "/dev/tty\\x2d1 on /run"
    );
    let plain = name("user-1000.slice");
    assert_eq!(expand("[%i]", &plain).unwrap(), "[]");
    assert_eq!(expand("%j", &plain).unwrap(), "1000");
    assert_eq!(expand("%f", &name("dev-sda.service")).unwrap(), "/dev/sda");
    assert_eq!(expand("%H", &unit), Err(SpecifierError::Unknown('H')));
    assert_eq!(expand("50%", &unit), Err(SpecifierError::Dangling));
}

#[test]
fn device_names_map_to_nodes_below_dev() {
    let card = UnitName::for_path("/dev/dri/card0", UnitType::Device).unwrap();
    assert_eq!(card, name("dev-dri-card0.device"));
    assert_eq!(card.device_path().unwrap(), "/dev/dri/card0");
    let label = UnitName::for_path("/dev/disk/by-label/root", UnitType::Device).unwrap();
    assert_eq!(label.as_str(), "dev-disk-by\\x2dlabel-root.device");
    assert_eq!(label.device_path().unwrap(), "/dev/disk/by-label/root");
    for refused in [
        "sys-fs-cgroup.device",
        "dev.device",
        "-.device",
        "dev--dri.device",
        "dev-dri-.device",
        "dev-a@b.device",
        "dev-\\x2e\\x2e-sda.device",
        "dev-\\x2fsda.device",
    ] {
        assert_eq!(
            name(refused).device_path(),
            Err(NameError::Path),
            "{refused}"
        );
    }
    assert_eq!(
        name("dev-sda.mount").device_path(),
        Err(NameError::Path),
        "not a device unit"
    );
}
