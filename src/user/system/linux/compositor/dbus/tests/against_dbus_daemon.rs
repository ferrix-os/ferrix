//! The crate against a real `dbus-daemon` on the development host: it
//! authenticates, says `Hello`, owns a name, and answers a call `dbus-send`
//! makes to it. Skipped where the host has no `dbus-daemon`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use compositor_dbus::{Connection, Message, Writer, session_path};

#[test]
fn a_real_bus_takes_us_and_a_call_reaches_us() {
    if Command::new("dbus-daemon")
        .arg("--version")
        .output()
        .is_err()
    {
        // Nothing to run against: the wire tests still hold.
        return;
    }
    let dir = std::env::temp_dir().join(format!("compositor-dbus-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let socket = dir.join("bus");
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address"])
        .arg(format!("--address=unix:path={}", socket.display()))
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon starts");
    let mut address = String::new();
    let _ = BufReader::new(daemon.stdout.take().expect("its output"))
        .read_line(&mut address)
        .expect("its address");
    let path = session_path(address.trim()).expect("a unix path");

    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc_getuid() };
    let mut bus = Connection::open(&path, uid).expect("authenticated and said Hello");
    assert!(bus.unique.starts_with(':'), "a unique name: {}", bus.unique);
    assert!(bus.request_name("org.ferrix.Test").expect("RequestName"));

    let caller = std::thread::spawn({
        let address = address.trim().to_owned();
        move || {
            Command::new("dbus-send")
                .args([
                    &format!("--bus={address}"),
                    "--print-reply",
                    "--dest=org.ferrix.Test",
                    "/org/ferrix/Test",
                    "org.ferrix.Test.Echo",
                    "string:hello",
                ])
                .output()
                .expect("dbus-send runs")
        }
    });
    let call = loop {
        let message = bus.receive().expect("a message");
        if message.is_call("org.ferrix.Test", "Echo") {
            break message;
        }
    };
    let said = call.reader().string().expect("a string").to_owned();
    let mut body = Writer::new();
    body.string(&format!("{said} back"));
    let _ = bus
        .send(&Message::reply_to(&call).with_body("s", body))
        .expect("the reply sent");
    let output = caller.join().expect("dbus-send finished");
    let printed = String::from_utf8_lossy(&output.stdout);
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        printed.contains("\"hello back\""),
        "dbus-send printed {printed}"
    );
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}
