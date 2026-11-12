//! The EDID each connector carries: the file `drm.edid_firmware=` names for
//! it, read from `/lib/firmware` when the card is taken up, or else the one
//! its driver read from the display (displayctl v10's EDID).
//!
//! `docs/DISPLAY.md` §7 says why here and not in the driver. Linux's DRM
//! core does the same in `drm_edid_load.c`; the grammar and the checks are
//! `ferrix_displayctl::edid`'s, host-tested, and this is the reading and the
//! boot lines. A connector the command line names no file for, or whose file
//! cannot be read or is not an EDID, has none, which is what every connector
//! had before: the card never refuses a driver over it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use ferrix_bootinfo::option_in;
use ferrix_displayctl::edid::{self, FIRMWARE_DIR, PARAMETER};
use ferrix_sysfs::drm::connector_type_name;

use crate::console::println;
use crate::fs;

/// A connector's name, as Linux's `connector->name` is: the type's and its
/// number among the card's connectors of that type, `Virtual-1`.
pub(crate) fn connector_name(kind: u32, head: usize) -> String {
    format!("{}-{}", connector_type_name(kind), head.saturating_add(1))
}

/// Each head's EDID, for card `card`'s connectors of types `kinds`, one a
/// head: what the command line's `drm.edid_firmware` names for each, read
/// and checked, with a boot line for each file named.
pub(crate) fn load(card: u32, kinds: &[u32]) -> Vec<Option<Vec<u8>>> {
    let setting = core::str::from_utf8(fs::procfs::command_line())
        .ok()
        .and_then(|line| option_in(line, PARAMETER));
    kinds
        .iter()
        .enumerate()
        .map(|(head, &kind)| {
            let connector = connector_name(kind, head);
            let file = setting.and_then(|setting| edid::firmware_for(setting, &connector))?;
            let found = read(file);
            match &found {
                Ok((bytes, checked)) => {
                    let said = edid::identity(bytes).map(|monitor| {
                        let text = |part: Option<edid::Text>| {
                            part.map(|text| format!(" {}", text.as_str()))
                                .unwrap_or_default()
                        };
                        format!(
                            "{}{}{}",
                            monitor.manufacturer(),
                            text(monitor.name),
                            text(monitor.serial)
                        )
                    });
                    println!(
                        "  display  card{card} {connector}: EDID from \"{file}\": {}, {} \
                         extension{}{}{}",
                        said.as_deref().unwrap_or("?"),
                        checked.extensions,
                        if checked.extensions == 1 { "" } else { "s" },
                        if checked.dropped > 0 {
                            format!(", {} dropped as not valid", checked.dropped)
                        } else {
                            String::new()
                        },
                        if checked.repaired {
                            ", its header put right"
                        } else {
                            ""
                        }
                    );
                }
                Err(why) => println!(
                    "  display  card{card} {connector}: {PARAMETER} names \"{file}\", {why}; \
                     the connector has no EDID"
                ),
            }
            found.ok().map(|(bytes, _)| bytes)
        })
        .collect()
}

/// The EDID card `card`'s driver read from the display on `connector`,
/// checked as a file is, with a line saying what it is: `None` for none, for
/// bytes that are not an EDID, and for one a file `drm.edid_firmware` names
/// wins over (`overridden`), which is kept rather than this.
pub(crate) fn from_driver(
    card: u32,
    connector: &str,
    bytes: &[u8],
    overridden: bool,
) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        println!("  display  card{card} {connector}: its driver says the display has no EDID");
        return None;
    }
    let mut taken = bytes.to_vec();
    let checked = match edid::check(&mut taken) {
        Ok(checked) => checked,
        Err(why) => {
            println!(
                "  display  card{card} {connector}: its driver's EDID is not an EDID: {why}; \
                 the connector has none from it"
            );
            return None;
        }
    };
    taken.truncate(checked.len);
    let said = edid::identity(&taken).map(|monitor| {
        let text = |part: Option<edid::Text>| {
            part.map(|text| format!(" {}", text.as_str()))
                .unwrap_or_default()
        };
        format!(
            "{}{}{}",
            monitor.manufacturer(),
            text(monitor.name),
            text(monitor.serial)
        )
    });
    println!(
        "  display  card{card} {connector}: EDID from its driver: {}, {} extension{}{}",
        said.as_deref().unwrap_or("?"),
        checked.extensions,
        if checked.extensions == 1 { "" } else { "s" },
        if overridden {
            format!("; the file {PARAMETER} names is kept")
        } else {
            String::new()
        }
    );
    Some(taken)
}

/// Read `/lib/firmware/<file>` and check it, or say why not.
///
/// The name must stay beneath `/lib/firmware` (`edid::confined`), and the
/// read follows no symbolic link there (`fs::read_file_beneath`): what is
/// read is handed to every program that opens the card, so it must be the
/// file the name names and no other (the certification consultant's
/// condition, 2026-09-26).
fn read(file: &str) -> Result<(Vec<u8>, edid::Checked), String> {
    let name = edid::confined(file)
        .map_err(|why| format!("which is not a file under {FIRMWARE_DIR}: {why}"))?;
    let mut bytes = fs::read_file_beneath(
        &fs::namespace().context(),
        FIRMWARE_DIR.trim_end_matches('/').as_bytes(),
        name.as_bytes(),
    )
    .map_err(|error| {
        if error == ferrix_linux_abi::errno::Errno::ELOOP {
            String::from("which is a symbolic link or under one, and is not followed (err=-40)")
        } else {
            format!("which cannot be read (err=-{})", error.0)
        }
    })?;
    let checked = edid::check(&mut bytes).map_err(|why| format!("which is not an EDID: {why}"))?;
    bytes.truncate(checked.len);
    Ok((bytes, checked))
}
