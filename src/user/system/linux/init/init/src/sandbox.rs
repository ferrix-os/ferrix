//! The sandboxing keys (`docs/INIT.md` §4.5): a [`Sandbox`] worked out in
//! the parent into a [`Plan`], and carried out by the child between `clone3`
//! and `execve`.
//!
//! The child's order is the design's. First, while it is still root and
//! before anything drops a privilege, [`enter`]: a mount namespace of its
//! own, its mounts made slaves, a fresh tmpfs on `/tmp` and `/var/tmp` for
//! `PrivateTmp=`, and every place `ProtectSystem=` names remounted
//! read-only. Then the child changes directory and user as before. Last,
//! after init's go-ahead and just before `execve`, [`lock`]:
//! `PR_SET_NO_NEW_PRIVS` for `NoNewPrivileges=`. A seccomp filter for
//! `SystemCallFilter=` goes after it, as the very last step, when the
//! kernel has filters (S3).
//!
//! Everything that needs a decision or an allocation is in [`plan`], in the
//! parent, where a failure is the unit's `SpawnFailed` with a line saying
//! why. That includes the two keys whose kernel half is not on `main` yet:
//! `PrivateNetwork=` and `SystemCallFilter=` refuse to start the unit, with
//! the reason, rather than run it without what it asked for.

use std::ffi::CString;
use std::fs;
use std::io;

use ferrix_svc::kind::{ProtectSystem, Sandbox};

use crate::spawn::Unprepared;
use crate::sys;

/// What `ProtectSystem=yes` makes read-only: systemd's `/usr`, `/boot` and
/// `/efi`, and the directories a merged `/usr` would have put under `/usr`,
/// which Ferrix's images keep at the top (§4.5).
const SYSTEM: [&str; 7] = ["/usr", "/boot", "/efi", "/bin", "/sbin", "/lib", "/lib64"];

/// What `ProtectSystem=strict` leaves writable: the API filesystems.
const API: [&str; 3] = ["/dev", "/proc", "/sys"];

/// What `PrivateTmp=` puts a tmpfs on.
const TEMPORARY: [&str; 2] = ["/tmp", "/var/tmp"];

/// One place to make read-only.
#[derive(Debug)]
struct ReadOnly {
    path: CString,
    /// The place is not a mount's root: bind it onto itself first, with
    /// what is mounted beneath it, so there is a mount to remount.
    bind_first: bool,
    /// The flags the mount has now, kept: a bind remount sets the mount's
    /// flags to exactly what it is given.
    flags: libc::c_ulong,
}

/// A [`Sandbox`] worked out for the child, which allocates nothing.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// `unshare(CLONE_NEWNS)` first.
    mount_namespace: bool,
    /// Directories to make (if missing) before the tmpfs mounts.
    make: Vec<CString>,
    /// Where a fresh tmpfs goes.
    tmpfs: Vec<CString>,
    /// What to remount read-only, in order.
    read_only: Vec<ReadOnly>,
    /// `PR_SET_NO_NEW_PRIVS`.
    no_new_privileges: bool,
}

impl Plan {
    /// Whether [`lock`] has anything to do.
    pub(crate) fn locks(&self) -> bool {
        self.no_new_privileges
    }
}

/// Work out `sandbox`, or refuse it. `None` when it asks for nothing.
pub(crate) fn plan(sandbox: &Sandbox) -> Result<Option<Plan>, Unprepared> {
    if sandbox.is_empty() {
        return Ok(None);
    }
    if sandbox.private_network {
        return Err(Unprepared {
            errno: libc::EOPNOTSUPP,
            why: "PrivateNetwork= needs network namespaces, which this kernel does not have \
                  yet; refusing to start the unit without it"
                .to_owned(),
        });
    }
    if sandbox.system_call_filter.is_some() {
        return Err(Unprepared {
            errno: libc::EOPNOTSUPP,
            why: "SystemCallFilter= needs seccomp filters, which this kernel does not have \
                  yet; refusing to start the unit without it"
                .to_owned(),
        });
    }
    let mut plan = Plan {
        mount_namespace: sandbox.needs_mount_namespace(),
        no_new_privileges: sandbox.no_new_privileges,
        ..Plan::default()
    };
    if sandbox.private_tmp {
        for dir in ["/tmp", "/var", "/var/tmp"] {
            plan.make.push(c_string(dir)?);
        }
        for dir in TEMPORARY {
            plan.tmpfs.push(c_string(dir)?);
        }
    }
    if sandbox.protect_system != ProtectSystem::No {
        let table = fs::read_to_string("/proc/self/mountinfo").map_err(|error| Unprepared {
            errno: error.raw_os_error().unwrap_or(libc::EIO),
            why: format!("ProtectSystem= reads /proc/self/mountinfo: {error}"),
        })?;
        let mounts = mounts(&table);
        let roots = roots(sandbox.protect_system, |path| {
            fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
        });
        let mut exempt: Vec<&str> = Vec::new();
        if sandbox.protect_system == ProtectSystem::Strict {
            exempt.extend(API);
        }
        if sandbox.private_tmp {
            exempt.extend(TEMPORARY);
        }
        for (path, bind_first, flags) in read_only(&roots, &mounts, &exempt) {
            plan.read_only.push(ReadOnly {
                path: c_string(&path)?,
                bind_first,
                flags,
            });
        }
    }
    Ok(Some(plan))
}

/// A C string, or the reason it cannot be one.
fn c_string(text: &str) -> Result<CString, Unprepared> {
    CString::new(text).map_err(|_| Unprepared {
        errno: libc::EINVAL,
        why: format!("{text:?} holds a NUL"),
    })
}

/// The places `level` makes read-only that are directories here, as
/// `is_dir` says; a link is left alone, since what it names is covered
/// where it is.
fn roots(level: ProtectSystem, is_dir: impl Fn(&str) -> bool) -> Vec<&'static str> {
    match level {
        ProtectSystem::No => Vec::new(),
        ProtectSystem::Strict => vec!["/"],
        ProtectSystem::Yes | ProtectSystem::Full => SYSTEM
            .into_iter()
            .chain((level == ProtectSystem::Full).then_some("/etc"))
            .filter(|path| is_dir(path))
            .collect(),
    }
}

/// Every mount of `/proc/self/mountinfo`: its place, unescaped, and its
/// per-mount flags.
fn mounts(table: &str) -> Vec<(String, libc::c_ulong)> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let place = fields.nth(4)?;
            let options = fields.next()?;
            Some((unescape(place), flags(options)))
        })
        .collect()
}

/// `mountinfo`'s octal escapes (`\040` for a space) undone.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let octal = bytes
            .get(at + 1..at + 4)
            .filter(|digits| byte == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d)));
        match octal {
            Some(digits) => {
                let value = digits
                    .iter()
                    .fold(0_u32, |value, d| value * 8 + u32::from(d - b'0'));
                out.push(u8::try_from(value).unwrap_or(b'?'));
                at += 4;
            }
            None => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount flags a `mountinfo` options field names, read-only aside.
fn flags(options: &str) -> libc::c_ulong {
    options
        .split(',')
        .map(|option| match option {
            "nosuid" => libc::MS_NOSUID,
            "nodev" => libc::MS_NODEV,
            "noexec" => libc::MS_NOEXEC,
            "noatime" => libc::MS_NOATIME,
            "nodiratime" => libc::MS_NODIRATIME,
            "relatime" => libc::MS_RELATIME,
            _ => 0,
        })
        .fold(0, |all, flag| all | flag)
}

/// Whether `path` is `under` or beneath it.
fn beneath(path: &str, under: &str) -> bool {
    under == "/"
        || path == under
        || path
            .strip_prefix(under)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// What to remount read-only for `roots`, given the mounts there are: each
/// root (bound onto itself first if no mount starts there), then every
/// mount beneath it, each once, leaving out what is `exempt` or beneath
/// it. Each with the flags its mount has now; a root bound onto itself
/// takes those of the mount it is in.
fn read_only(
    roots: &[&str],
    mounts: &[(String, libc::c_ulong)],
    exempt: &[&str],
) -> Vec<(String, bool, libc::c_ulong)> {
    let flags_at = |path: &str| {
        mounts
            .iter()
            .filter(|(place, _)| beneath(path, place))
            .max_by_key(|(place, _)| place.len())
            .map_or(0, |(_, flags)| *flags)
    };
    let mut out: Vec<(String, bool, libc::c_ulong)> = Vec::new();
    for root in roots {
        if exempt.iter().any(|skip| beneath(root, skip)) {
            continue;
        }
        let mounted = mounts.iter().any(|(place, _)| place == root);
        out.push(((*root).to_owned(), !mounted, flags_at(root)));
        for (place, _) in mounts {
            let wanted = place != root
                && beneath(place, root)
                && !exempt.iter().any(|skip| beneath(place, skip))
                && !out.iter().any(|(done, _, _)| done == place);
            if wanted {
                out.push((place.clone(), false, flags_at(place)));
            }
        }
    }
    out
}

/// The child's first sandboxing step, while it is still root: the mount
/// namespace and everything in it. Allocates nothing.
pub(crate) fn enter(plan: &Plan) -> io::Result<()> {
    if !plan.mount_namespace {
        return Ok(());
    }
    sys::unshare(libc::CLONE_NEWNS)?;
    // As systemd: nothing done here may reach the namespace it came from.
    sys::mount(c"none", c"/", c"none", libc::MS_REC | libc::MS_SLAVE, None)?;
    for dir in &plan.make {
        sys::make_directory(dir, 0o755)?;
    }
    for place in &plan.tmpfs {
        sys::mount(
            c"tmpfs",
            place,
            c"tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            Some(c"mode=1777"),
        )?;
        // The options string is not read by every kernel's tmpfs (Ferrix's
        // reads none), so the mode is set as well.
        sys::change_mode(place, 0o1777)?;
    }
    for place in &plan.read_only {
        if place.bind_first {
            sys::mount(
                &place.path,
                &place.path,
                c"none",
                libc::MS_BIND | libc::MS_REC,
                None,
            )?;
        }
        sys::mount(
            c"none",
            &place.path,
            c"none",
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | place.flags,
            None,
        )?;
    }
    Ok(())
}

/// The child's last sandboxing step before `execve`: no new privileges. A
/// seccomp filter goes after it, when there is one to install (S3).
pub(crate) fn lock(plan: &Plan) -> io::Result<()> {
    if plan.no_new_privileges {
        sys::no_new_privileges()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
1 0 0:1 / / rw,relatime - tmpfs none rw
2 1 0:2 / /proc rw,nosuid,nodev,noexec - proc proc rw
3 1 0:3 / /dev rw,nosuid - devtmpfs dev rw
4 1 0:4 / /sys rw,nosuid,nodev,noexec - sysfs sys rw
5 4 0:5 / /sys/fs/cgroup rw,nosuid,nodev,noexec - cgroup2 cgroup rw
6 1 0:6 / /tmp rw,nosuid,nodev - tmpfs tmp rw
7 1 0:7 / /run rw,nosuid,nodev - tmpfs tmpfs rw
8 1 0:8 / /data rw - btrfs /dev/vdb rw
9 1 0:9 / /my\\040disk rw,noexec - tmpfs x rw
10 1 0:10 / /usr/local rw,nodev - tmpfs y rw
";

    #[test]
    fn mountinfo_places_are_unescaped_with_their_flags() {
        let mounts = mounts(TABLE);
        assert_eq!(mounts.len(), 10);
        assert_eq!(mounts[0], ("/".to_owned(), libc::MS_RELATIME));
        assert_eq!(mounts[8], ("/my disk".to_owned(), libc::MS_NOEXEC));
        assert_eq!(
            mounts[1].1,
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC
        );
    }

    #[test]
    fn strict_is_every_mount_but_the_api_filesystems_and_private_tmp() {
        let mounts = mounts(TABLE);
        let places: Vec<String> = read_only(&["/"], &mounts, &["/dev", "/proc", "/sys", "/tmp"])
            .into_iter()
            .map(|(place, bind, _)| {
                assert!(!bind, "{place} is a mount already");
                place
            })
            .collect();
        assert_eq!(places, ["/", "/run", "/data", "/my disk", "/usr/local"]);
    }

    #[test]
    fn a_root_that_is_no_mount_is_bound_first_and_keeps_its_mounts_flags() {
        let mounts = mounts(TABLE);
        let out = read_only(&["/usr", "/etc"], &mounts, &[]);
        assert_eq!(
            out,
            [
                ("/usr".to_owned(), true, libc::MS_RELATIME),
                ("/usr/local".to_owned(), false, libc::MS_NODEV),
                ("/etc".to_owned(), true, libc::MS_RELATIME),
            ]
        );
    }

    #[test]
    fn yes_and_full_take_only_the_directories_there_are() {
        let here = |path: &str| ["/usr", "/bin", "/etc", "/lib"].contains(&path);
        assert_eq!(roots(ProtectSystem::Yes, here), ["/usr", "/bin", "/lib"]);
        assert_eq!(
            roots(ProtectSystem::Full, here),
            ["/usr", "/bin", "/lib", "/etc"]
        );
        assert_eq!(roots(ProtectSystem::Strict, here), ["/"]);
        assert!(roots(ProtectSystem::No, here).is_empty());
    }

    #[test]
    fn beneath_is_by_whole_components() {
        assert!(beneath("/sys/fs/cgroup", "/sys"));
        assert!(beneath("/sys", "/sys"));
        assert!(!beneath("/system", "/sys"));
        assert!(beneath("/anything", "/"));
    }

    #[test]
    fn the_two_keys_without_a_kernel_half_refuse_by_name() {
        let network = Sandbox {
            private_network: true,
            ..Sandbox::default()
        };
        let refused = plan(&network).err().map(|why| why.why).unwrap_or_default();
        assert!(refused.starts_with("PrivateNetwork= needs network namespaces"));
        let filter = Sandbox {
            system_call_filter: Some(ferrix_svc::kind::SystemCallFilter::default()),
            ..Sandbox::default()
        };
        let refused = plan(&filter).err().map(|why| why.why).unwrap_or_default();
        assert!(refused.starts_with("SystemCallFilter= needs seccomp filters"));
        assert!(plan(&Sandbox::default()).is_ok_and(|plan| plan.is_none()));
    }
}
