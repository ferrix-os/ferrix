//! What the boots of Ferrix's file chooser through the desktop portal
//! share: Yoru's desktop on the Chrome volume, with a session bus -- the
//! volume's `dbus-daemon` -- that portald answers on, and a script saying
//! the windows' classes and titles every two seconds.

use super::Programs;
use super::boot::build_image;
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// Where the session bus listens.
pub(super) const BUS: &str = "unix:path=/tmp/bus";

/// The probe: the windows' classes and titles.
const PROBE: &str = r#"#!/bin/sh
exec 2>&1
until hyprctl version > /dev/null 2>&1; do sleep 0.2; done
while :; do
  echo "portal-probe: windows $(hyprctl clients | grep -E '^[[:space:]]*(class|title):' | tr '\n' ';')"
  sleep 2
done
"#;

/// The class Ferrix's chooser's window has.
pub(super) const CHOOSER_CLASS: &str = "class: rocks.magical.chooser";

/// The image for such a boot, its kernel, and the arguments it boots with;
/// `None`, said, where the machine has no Chrome volume. `lines` are the
/// boot's own configuration after the desktop's, `ports` its own files.
pub(super) fn image(
    arch: Arch,
    programs: &Programs,
    args: &Args,
    name: &str,
    lines: &str,
    ports: Vec<crate::ports::File>,
) -> Result<Option<(std::path::PathBuf, std::path::PathBuf, Args)>> {
    if arch != Arch::X86_64 {
        println!("  {arch}: {name} skipped: the Chrome volume is linux64 alone");
        return Ok(None);
    }
    let Ok(volume) = crate::chrome::volume() else {
        println!("  {arch}: {name} skipped: no Chrome volume (tools/common/fetch/fetch-chrome.sh)");
        return Ok(None);
    };
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str(crate::chrome::WINDOW_ENV);
    config.push_str(&crate::chrome::window_library_path(false));
    config.push_str(&format!("env = DBUS_SESSION_BUS_ADDRESS,{BUS}\n"));
    config.push_str(&format!(
        "exec-once = /data/usr/bin/dbus-daemon --config-file=/data/usr/share/dbus-1/session.conf --address={BUS} --nofork --nopidfile\n"
    ));
    config.push_str("exec-once = /bin/sh /etc/portal-probe.sh\n");
    config.push_str(lines);
    let links = super::desktop::chrome_links(arch, &carried.ports);
    carried.ports.extend(links);
    carried.ports.extend(crate::chrome::window_files());
    carried.busybox = Some(
        crate::busybox::installed_program(arch)
            .ok_or_else(|| Error::new(format!("{arch}: no busybox for the probe")))?,
    );
    carried.ports.push(crate::ports::File {
        path: "etc/portal-probe.sh".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(PROBE.as_bytes().to_vec()),
    });
    carried.ports.extend(ports);
    let mut args = args.clone();
    args.data_image = Some(volume.clone());
    if !args.memory_given {
        args.memory = crate::chrome::MEMORY;
    }
    println!("  {arch}: {name}: the volume {}", volume.display());
    let (image, kernel) = build_image(arch, programs, &config, carried, &args)?;
    Ok(Some((image, kernel, args)))
}
