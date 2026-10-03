//! Becoming the account (the certification consultant's F3): its groups,
//! then its gid, then its uid, each checked, and then every id read back.
//! A mismatch is an error, and `login` execs nothing after one.

use crate::Account;

/// Drop root for `account`.
pub fn drop_to(account: &Account) -> Result<(), String> {
    let check = |what: &str, result: libc::c_int| {
        if result == 0 {
            Ok(())
        } else {
            Err(format!("{what}: {}", std::io::Error::last_os_error()))
        }
    };
    let groups: Vec<libc::gid_t> = account.groups.clone();
    // SAFETY: the pointer and length are the vector's own, read only.
    check("setgroups", unsafe {
        libc::setgroups(groups.len(), groups.as_ptr())
    })?;
    let (gid, uid) = (account.gid, account.uid);
    // SAFETY: setresgid takes plain integers.
    check("setresgid", unsafe { libc::setresgid(gid, gid, gid) })?;
    // SAFETY: setresuid takes plain integers.
    check("setresuid", unsafe { libc::setresuid(uid, uid, uid) })?;
    let ids = read_back()?;
    let wanted = ([uid; 3], [gid; 3]);
    if (ids.0, ids.1) != wanted {
        return Err(format!(
            "the ids read back are uid {:?} gid {:?}, not {uid} and {gid}",
            ids.0, ids.1
        ));
    }
    let mut have = ids.2;
    let mut want = groups;
    have.sort_unstable();
    have.dedup();
    want.sort_unstable();
    want.dedup();
    if have != want {
        return Err(format!("the groups read back are {have:?}, not {want:?}"));
    }
    Ok(())
}

/// The real, effective and saved uids, the same of the gids, and the
/// supplementary groups.
type Ids = ([u32; 3], [u32; 3], Vec<u32>);

/// The process's [`Ids`], as the kernel has them now.
fn read_back() -> Result<Ids, String> {
    let (mut ru, mut eu, mut su) = (0, 0, 0);
    let (mut rg, mut eg, mut sg) = (0, 0, 0);
    // SAFETY: three writable uids, alive for the call.
    if unsafe { libc::getresuid(&raw mut ru, &raw mut eu, &raw mut su) } != 0 {
        return Err(format!("getresuid: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: three writable gids, alive for the call.
    if unsafe { libc::getresgid(&raw mut rg, &raw mut eg, &raw mut sg) } != 0 {
        return Err(format!("getresgid: {}", std::io::Error::last_os_error()));
    }
    let mut groups = vec![0 as libc::gid_t; 256];
    let size = libc::c_int::try_from(groups.len()).unwrap_or(0);
    // SAFETY: `groups` is writable for `size` entries.
    let count = unsafe { libc::getgroups(size, groups.as_mut_ptr()) };
    let Ok(count) = usize::try_from(count) else {
        return Err(format!("getgroups: {}", std::io::Error::last_os_error()));
    };
    groups.truncate(count);
    Ok(([ru, eu, su], [rg, eg, sg], groups))
}
