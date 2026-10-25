//! The person's answers, carried from the live session to the installed
//! system's first boot in `FERRIX/SETUP.TXT` on the installed disk's EFI
//! system partition (`docs/INSTALLER.md` §11, the live ISO).
//!
//! The live medium's volume carries the file as a placeholder of
//! [`SETUP_BYTES`] bytes, so the installer fills it in place in its byte
//! copy of the volume, as it rewrites `CMDLINE.TXT`, with no FAT writer.
//! The installed system's `ferrix-setup.service` runs `ferrix-install
//! --apply` at every boot, before the session: it reads the file back and
//! makes the system the person's -- the account uid 1000 renamed to their
//! user name with their password seeded for `authd`, the computer's name,
//! the keyboard, the time zone, and the lock screen at the session's start
//! -- each step idempotent, because the kernel puts the image's own
//! `/etc/passwd` back whenever it installs a new image onto the volume.
//!
//! The password is never written: the installer hashes it with Argon2id at
//! the parameters `xtask`'s seeds use, and the PHC string goes in the file
//! and in `/lib/ferrix/auth/seed/<user>`, which `authd` imports once, when
//! the account has no record yet (`docs/AUTH.md` §5.3). The partition is
//! read only by root.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, chown, symlink};
use std::path::Path;

use crate::{SECTOR, fat_in_place, live_disk};
use ferrix_install::disk;
use ferrix_partition::ESP;

/// The placeholder's size, and the most the answers take.
pub const SETUP_BYTES: usize = 4096;
/// The file's first line once filled in.
const MAGIC: &str = "ferrix-setup 1";
/// The account the image's session runs as, by uid (`sessiond --user 1000`).
const UID: u32 = 1000;

/// What the person chose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answers {
    pub user: String,
    pub fullname: String,
    pub password: String,
    pub hostname: String,
    pub keyboard: String,
    pub variant: String,
    pub timezone: String,
    pub language: String,
}

/// Names the system has for itself, which a person's account may not take.
const RESERVED: &[&str] = &[
    "root", "auth", "nobody", "daemon", "bin", "sys", "wheel", "audio", "video", "input",
    "render", "users", "admin", "ferrix-setup",
];

impl Answers {
    /// Refuse what the installed system could not take, in words the page
    /// can show.
    pub fn check(&self) -> Result<(), String> {
        let user = self.user.as_bytes();
        let user_ok = !user.is_empty()
            && user.len() <= 32
            && (user[0].is_ascii_lowercase() || user[0] == b'_')
            && user
                .iter()
                .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !user_ok {
            return Err(format!(
                "{:?} is not a user name: lowercase letters, digits, _ and -, starting with a letter",
                self.user
            ));
        }
        if RESERVED.contains(&self.user.as_str()) {
            return Err(format!("{} is a name the system uses itself", self.user));
        }
        if self.password.is_empty() {
            return Err("the password is empty".into());
        }
        let host = self.hostname.as_bytes();
        if host.is_empty()
            || host.len() > 63
            || host.first() == Some(&b'-')
            || !host
                .iter()
                .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(format!("{:?} is not a computer name", self.hostname));
        }
        let word = |text: &str| {
            text.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-+".contains(&b))
        };
        if self.keyboard.is_empty() || !word(&self.keyboard) || !word(&self.variant) {
            return Err(format!("{:?} is not a keyboard layout", self.keyboard));
        }
        let zone_ok = !self.timezone.is_empty()
            && !self.timezone.starts_with('/')
            && !self.timezone.split('/').any(|part| part.is_empty() || part == "..")
            && self.timezone.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_-+".contains(&b));
        if !zone_ok {
            return Err(format!("{:?} is not a time zone", self.timezone));
        }
        let plain = |text: &str| !text.contains(['\n', '\r', ':', '=']);
        if !plain(&self.fullname) || self.fullname.len() > 128 || !word(&self.language) {
            return Err("the name may not hold a colon, an equals sign or a line break".into());
        }
        Ok(())
    }

    /// The file's text: everything but the password, and its hash, padded
    /// to [`SETUP_BYTES`].
    pub fn file(&self, hash: &str) -> Result<Vec<u8>, String> {
        let fullname = if self.fullname.is_empty() {
            self.user.as_str()
        } else {
            self.fullname.as_str()
        };
        let text = format!(
            "{MAGIC}\nuser={}\nfullname={fullname}\nhash={hash}\nhostname={}\nkeyboard={}\n\
             variant={}\ntimezone={}\nlanguage={}\n",
            self.user, self.hostname, self.keyboard, self.variant, self.timezone, self.language
        );
        let mut bytes = text.into_bytes();
        if bytes.len() > SETUP_BYTES {
            return Err("the answers are larger than the setup file".into());
        }
        bytes.resize(SETUP_BYTES, b'\n');
        Ok(bytes)
    }
}

/// The password's PHC string: Argon2id at the seeds' parameters
/// (`tools/common/xtask/src/auth.rs`), with a random salt.
pub fn hash(password: &str) -> Result<String, String> {
    let params = ferrix_argon2::Params {
        memory_kib: 19_456,
        passes: 2,
        lanes: 1,
    };
    let mut salt = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut salt))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    let blocks = params.blocks().map_err(|why| format!("{why:?}"))?;
    let mut memory = vec![ferrix_argon2::Block::ZERO; blocks];
    let mut tag = [0_u8; 32];
    ferrix_argon2::hash(
        &params,
        &ferrix_argon2::Inputs {
            password: password.as_bytes(),
            salt: &salt,
            secret: &[],
            associated: &[],
        },
        &mut memory,
        &mut tag,
    )
    .map_err(|why| format!("hashing the password: {why:?}"))?;
    let encoded = ferrix_argon2::phc::Encoded::new(params, &salt, &tag)
        .map_err(|why| format!("the password's hash: {why:?}"))?;
    Ok(encoded.to_string())
}

/// What a filled-in file says: the answers, the password's hash in place of
/// the password.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filled {
    pub answers: Answers,
    pub hash: String,
}

/// Read a setup file's text; `None` for the placeholder or anything else.
pub fn parse(bytes: &[u8]) -> Option<Filled> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let mut filled = Filled::default();
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let answers = &mut filled.answers;
        let slot = match key {
            "user" => &mut answers.user,
            "fullname" => &mut answers.fullname,
            "hostname" => &mut answers.hostname,
            "keyboard" => &mut answers.keyboard,
            "variant" => &mut answers.variant,
            "timezone" => &mut answers.timezone,
            "language" => &mut answers.language,
            "hash" => &mut filled.hash,
            _ => continue,
        };
        value.clone_into(slot);
    }
    // The check wants a password; the hash stands for it here.
    let mut checked = filled.answers.clone();
    checked.password = "x".into();
    (checked.check().is_ok() && filled.hash.starts_with("$argon2id$")).then_some(filled)
}

/// `ferrix-install --apply`: find this system's setup file and apply it.
pub fn apply() -> Result<(), String> {
    let Some(filled) = find()? else {
        println!("ferrix-setup: no answers on any EFI system partition; nothing to set up");
        return Ok(());
    };
    apply_to(Path::new("/"), &filled)?;
    set_hostname(&filled.answers.hostname);
    println!(
        "ferrix-setup: this system is {}'s, named {}, keyboard {}, time zone {}",
        filled.answers.user,
        filled.answers.hostname,
        filled.answers.keyboard,
        filled.answers.timezone
    );
    Ok(())
}

/// The filled-in setup file on an EFI system partition of a disk this
/// system can read, never the live medium's.
fn find() -> Result<Option<Filled>, String> {
    let live = live_disk();
    for path in disk::whole_disks() {
        if live.as_deref() == Some(path.as_str()) {
            continue;
        }
        let Ok(mut file) = disk::open(&path) else {
            continue;
        };
        let Ok(Some(table)) = disk::table(&mut file) else {
            continue;
        };
        for (_, partition) in &table.entries {
            if partition.type_guid != ESP || disk::name(partition) == crate::LIVE_NAME {
                continue;
            }
            let Ok(Some(bytes)) = fat_in_place::read(&mut file, partition.first * SECTOR, SETUP)
            else {
                continue;
            };
            if let Some(filled) = parse(&bytes) {
                println!("ferrix-setup: the answers are on {path}'s EFI system partition");
                return Ok(Some(filled));
            }
        }
    }
    Ok(None)
}

/// Where the file is on the volume, as 8.3 names.
pub const SETUP: &[&[u8; 11]] = &[b"FERRIX     ", b"SETUP   TXT"];

/// Apply `filled` to the system whose `/` is `root`. Every step leaves a
/// file as it was when it already says what it should.
pub fn apply_to(root: &Path, filled: &Filled) -> Result<(), String> {
    let answers = &filled.answers;
    let user = answers.user.as_str();
    let at = |relative: &str| root.join(relative);
    let read = |path: &Path| fs::read_to_string(path).unwrap_or_default();
    let write = |path: &Path, text: &str, mode: u32| -> Result<(), String> {
        if fs::read_to_string(path).ok().as_deref() == Some(text) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let new = path.with_extension("ferrix-setup");
        fs::write(&new, text).map_err(|e| format!("{}: {e}", new.display()))?;
        fs::set_permissions(&new, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", new.display()))?;
        fs::rename(&new, path).map_err(|e| format!("{}: {e}", path.display()))
    };

    // The account: uid 1000 renamed, its group, and wheel's member.
    let passwd = read(&at("etc/passwd"));
    let fullname = answers.fullname.replace(',', " ");
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.get(2) == Some(&UID.to_string().as_str()) {
            let shell = fields.get(6).copied().unwrap_or("/bin/zsh");
            lines.push(format!(
                "{user}:x:{UID}:{UID}:{fullname}:/home/{user}:{shell}"
            ));
            found = true;
        } else if fields.first() != Some(&user) {
            lines.push(line.to_owned());
        }
    }
    if !found {
        lines.push(format!("{user}:x:{UID}:{UID}:{fullname}:/home/{user}:/bin/zsh"));
    }
    write(&at("etc/passwd"), &(lines.join("\n") + "\n"), 0o644)?;
    let group = read(&at("etc/group"));
    let old_name = passwd
        .lines()
        .find(|line| line.split(':').nth(2) == Some(&UID.to_string()))
        .and_then(|line| line.split(':').next())
        .unwrap_or("ferrix")
        .to_owned();
    let groups: Vec<String> = group
        .lines()
        .map(|line| {
            let mut fields: Vec<String> = line.split(':').map(str::to_owned).collect();
            if fields.get(2).map(String::as_str) == Some(&UID.to_string()) {
                fields[0] = user.to_owned();
            }
            if let Some(members) = fields.get_mut(3) {
                let renamed: Vec<&str> = members
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(|m| if m == old_name { user } else { m })
                    .collect();
                *members = renamed.join(",");
            }
            fields.join(":")
        })
        .collect();
    write(&at("etc/group"), &(groups.join("\n") + "\n"), 0o644)?;

    // The home, the person's.
    let home = at(&format!("home/{user}"));
    if !home.exists() {
        fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;
        let _ = chown(&home, Some(UID), Some(UID));
        let _ = fs::set_permissions(&home, fs::Permissions::from_mode(0o700));
    }

    // The password, for authd to import once.
    write(
        &at(&format!("lib/ferrix/auth/seed/{user}")),
        &format!("{}\n", filled.hash),
        0o600,
    )?;

    // The computer's name.
    write(&at("etc/hostname"), &format!("{}\n", answers.hostname), 0o644)?;

    // The time zone, as settingsd's set-timezone writes it.
    let zone = Path::new("/usr/share/zoneinfo").join(&answers.timezone);
    if at(&format!("usr/share/zoneinfo/{}", answers.timezone)).is_file() {
        let link = at("etc/localtime");
        if fs::read_link(&link).ok().as_deref() != Some(zone.as_path()) {
            let _ = fs::remove_file(&link);
            symlink(&zone, &link).map_err(|e| format!("{}: {e}", link.display()))?;
        }
        if let Ok(bytes) = fs::read(at(&format!("usr/share/zoneinfo/{}", answers.timezone)))
            && let Some(footer) = footer(&bytes)
        {
            write(&at("etc/TZ"), &format!("{footer}\n"), 0o644)?;
        }
    }

    // The keyboard and the lock at the session's start, in the desktop's
    // configuration: a file of their own, sourced at its end.
    let mut installed = format!(
        "# Written by ferrix-install --apply from the installer's answers.\n\
         input:kb_layout = {}\n",
        answers.keyboard
    );
    if !answers.variant.is_empty() {
        installed.push_str(&format!("input:kb_variant = {}\n", answers.variant));
    }
    installed.push_str(
        "# The session starts locked: the person's password opens it.\n\
         exec-once = /bin/busybox sh /etc/ferrix/login-lock.sh\n",
    );
    write(&at(INSTALLED_CONF), &installed, 0o644)?;
    let config = at("etc/hyprland.conf");
    let text = read(&config);
    let source = format!("source = /{INSTALLED_CONF}");
    if !text.is_empty() && !text.lines().any(|line| line.trim() == source) {
        write(&config, &format!("{}\n{source}\n", text.trim_end()), 0o644)?;
    }
    Ok(())
}

/// The installed system's own desktop lines.
const INSTALLED_CONF: &str = "etc/ferrix/installed-hyprland.conf";

/// A `TZif` file's POSIX footer, as settingsd reads it.
fn footer(bytes: &[u8]) -> Option<String> {
    if bytes.get(4).copied().unwrap_or(0) < b'2' || bytes.last() != Some(&b'\n') {
        return None;
    }
    let body = bytes.get(..bytes.len() - 1)?;
    let start = body.iter().rposition(|&b| b == b'\n')? + 1;
    let line = std::str::from_utf8(body.get(start..)?).ok()?;
    (!line.is_empty() && line.len() <= 64 && line.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| line.to_owned())
}

unsafe extern "C" {
    #[link_name = "sethostname"]
    fn sys_sethostname(name: *const u8, len: usize) -> i32;
}

/// Tell the kernel the computer's name now; `/etc/hostname` says it from the
/// next boot on.
fn set_hostname(name: &str) {
    // SAFETY: the pointer and length are one live byte slice's.
    let _ = unsafe { sys_sethostname(name.as_ptr(), name.len()) };
}

#[cfg(test)]
mod tests {
    use super::{Answers, Filled, apply_to, parse};

    fn answers() -> Answers {
        Answers {
            user: "kira".into(),
            fullname: "Kira Example".into(),
            password: "a password".into(),
            hostname: "kiras-pc".into(),
            keyboard: "de".into(),
            variant: "nodeadkeys".into(),
            timezone: "Europe/Berlin".into(),
            language: "en".into(),
        }
    }

    #[test]
    fn the_file_holds_the_answers_and_never_the_password() {
        let file = answers().file("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA").expect("a file");
        assert_eq!(file.len(), super::SETUP_BYTES);
        let text = String::from_utf8(file.clone()).expect("text");
        assert!(!text.contains("a password"));
        let filled = parse(&file).expect("parsed");
        assert_eq!(filled.answers.user, "kira");
        assert_eq!(filled.answers.variant, "nodeadkeys");
        assert!(filled.answers.password.is_empty());
        // The placeholder is not answers.
        assert!(parse(&vec![b'\n'; 4096]).is_none());
    }

    #[test]
    fn bad_answers_are_refused() {
        for (user, host) in [("Kira", "pc"), ("root", "pc"), ("kira", "My PC"), ("", "pc")] {
            let mut a = answers();
            a.user = user.into();
            a.hostname = host.into();
            assert!(a.check().is_err(), "{user} {host}");
        }
        let mut a = answers();
        a.timezone = "../../etc/shadow".into();
        assert!(a.check().is_err());
    }

    #[test]
    fn applying_renames_the_account_twice_the_same() {
        let root = std::env::temp_dir().join(format!("ferrix-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc")).expect("etc");
        std::fs::write(
            root.join("etc/passwd"),
            "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/home/ferrix:/bin/zsh\nauth:x:90:90:authd:/var/lib/ferrix/auth:/sbin/nologin\n",
        )
        .expect("passwd");
        std::fs::write(root.join("etc/group"), "root:x:0:\nferrix:x:1000:\nauth:x:90:\nwheel:x:10:ferrix\n")
            .expect("group");
        std::fs::write(root.join("etc/hyprland.conf"), "exec-once = /bin/yoru-shell\n").expect("conf");
        let filled = Filled {
            answers: answers(),
            hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
        };
        apply_to(&root, &filled).expect("applied");
        apply_to(&root, &filled).expect("applied again");
        let passwd = std::fs::read_to_string(root.join("etc/passwd")).expect("passwd");
        assert!(passwd.contains("kira:x:1000:1000:Kira Example:/home/kira:/bin/zsh"));
        assert!(!passwd.contains("ferrix:"));
        let group = std::fs::read_to_string(root.join("etc/group")).expect("group");
        assert!(group.contains("kira:x:1000:"));
        assert!(group.contains("wheel:x:10:kira"));
        let conf = std::fs::read_to_string(root.join("etc/hyprland.conf")).expect("conf");
        assert_eq!(conf.matches("source = /etc/ferrix/installed-hyprland.conf").count(), 1);
        let installed =
            std::fs::read_to_string(root.join("etc/ferrix/installed-hyprland.conf")).expect("ours");
        assert!(installed.contains("input:kb_layout = de"));
        assert!(installed.contains("login-lock.sh"));
        let seed = std::fs::read_to_string(root.join("lib/ferrix/auth/seed/kira")).expect("seed");
        assert!(seed.starts_with("$argon2id$"));
        assert_eq!(
            std::fs::read_to_string(root.join("etc/hostname")).expect("hostname"),
            "kiras-pc\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
