//! `/etc/machine-id`: the machine's own name for itself, made once.
//!
//! lode stamps it on every record and lode-upload sends it, so a receiver
//! keeps a machine's log in a store named by it rather than by its host
//! name, and two machines both called `ferrix` stay apart. Ferrix images
//! carry none; init makes one when `/` is settled and holds none, as
//! systemd does on a first boot: 128 bits from the kernel's random, marked
//! as a version 4 UUID, written as 32 lowercase hex digits and a newline,
//! mode 0444. A root kept on a disk keeps it from boot to boot; a root in
//! memory has a new one each boot. One that is there is never changed.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;

/// Where it is.
const PATH: &str = "/etc/machine-id";

/// Whether `text` holds a machine id: 32 hex digits, a newline allowed.
#[must_use]
pub(crate) fn valid(text: &str) -> bool {
    let id = text.trim_end_matches('\n');
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) && id != "0".repeat(32)
}

/// The id `bytes` make: a version 4 UUID's bits, as systemd marks its own.
#[must_use]
pub(crate) fn from_random(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Make `/etc/machine-id` when there is none, saying what was done.
pub(crate) fn ensure(say: &dyn Fn(&str)) {
    if fs::read_to_string(PATH).is_ok_and(|text| valid(&text)) {
        return;
    }
    let mut bytes = [0_u8; 16];
    if let Err(error) = fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        say(&format!("no machine id made: /dev/urandom: {error}"));
        return;
    }
    let id = from_random(bytes);
    // Written whole, then renamed over: a reader never sees half of one.
    let partial = "/etc/.machine-id.new";
    let written = fs::write(partial, format!("{id}\n"))
        .and_then(|()| fs::set_permissions(partial, fs::Permissions::from_mode(0o444)))
        .and_then(|()| fs::rename(partial, PATH));
    match written {
        Ok(()) => say(&format!("made {PATH}: {id}")),
        Err(error) => {
            let _ = fs::remove_file(partial);
            say(&format!("no machine id made: {PATH}: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{from_random, valid};

    #[test]
    fn an_id_is_32_hex_digits_marked_as_a_version_4_uuid() {
        let id = from_random([0xff; 16]);
        assert_eq!(id, "ffffffffffff4fffbfffffffffffffff");
        assert!(valid(&id));
        assert!(valid(&format!("{id}\n")));
        let low = from_random([0; 16]);
        assert_eq!(low.chars().nth(12), Some('4'));
        assert_eq!(low.chars().nth(16), Some('8'));
        assert!(valid(&low));
    }

    #[test]
    fn what_is_not_an_id_is_made_again() {
        assert!(!valid(""));
        assert!(!valid("uninitialized\n"));
        assert!(!valid(&"0".repeat(32)));
        assert!(!valid("062359e1029c423c0c0b4c9cf5216a1"));
        assert!(!valid("062359e1029c423c0c0b4c9cf5216a14x"));
        assert!(valid("062359e1029c423c0c0b4c9cf5216a14\n"));
    }
}
