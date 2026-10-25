//! portald on a real bus: a private `dbus-daemon` of the host's, portald
//! on it, and `GLib`'s own `gdbus` reading the settings as GTK would. Said
//! and skipped on a host without the two.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The bus's configuration: a session bus, nothing activated.
const CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy>
</busconfig>
"#;

/// A child killed when dropped.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn found(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn gdbus(address: &str, method: &str, args: &[&str]) -> String {
    let out = Command::new("gdbus")
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.portal.Desktop",
            "--object-path",
            "/org/freedesktop/portal/desktop",
            "--method",
            method,
        ])
        .args(args)
        .output();
    out.map_or_else(
        |error| format!("gdbus: {error}"),
        |out| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        },
    )
}

#[test]
fn gdbus_reads_what_gtk_reads() {
    if !found("dbus-daemon") || !found("gdbus") {
        // Nothing to run against: the answers' own tests still hold.
        return;
    }
    let dir = std::env::temp_dir().join(format!("portald-bus-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("made");
    let config = dir.join("bus.conf");
    std::fs::write(&config, CONFIG).expect("written");
    let mut bus = Killed(
        Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon started"),
    );
    let mut line = String::new();
    // Kept open while the bus runs: a closed pipe ends it.
    let mut out = std::io::BufReader::new(bus.0.stdout.take().expect("its address"));
    {
        use std::io::BufRead as _;
        let _ = out.read_line(&mut line);
    }
    let address = line.trim().to_owned();
    assert!(address.starts_with("unix:"), "the bus's address: {address}");
    let _portald = Killed(
        Command::new(env!("CARGO_BIN_EXE_portald"))
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .stdout(Stdio::null())
            .spawn()
            .expect("portald started"),
    );
    // Until portald has its name.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut said = String::new();
    while Instant::now() < deadline {
        said = gdbus(
            &address,
            "org.freedesktop.portal.Settings.ReadOne",
            &["org.freedesktop.appearance", "color-scheme"],
        );
        if said.contains("uint32") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(said.trim(), "(<uint32 1>,)");
    let theme = gdbus(
        &address,
        "org.freedesktop.portal.Settings.Read",
        &["org.gnome.desktop.interface", "gtk-theme"],
    );
    assert_eq!(theme.trim(), "(<<'Nocturne'>>,)");
    let all = gdbus(&address, "org.freedesktop.portal.Settings.ReadAll", &["[]"]);
    assert!(all.contains("'accent-color': <(0.5686"), "{all}");
    let missing = gdbus(
        &address,
        "org.freedesktop.portal.Settings.ReadOne",
        &["org.freedesktop.appearance", "nope"],
    );
    assert!(
        missing.contains("org.freedesktop.portal.Error.NotFound"),
        "{missing}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
