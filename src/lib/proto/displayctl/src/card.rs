//! Who may do what on a card: the rules of `/dev/dri/card<N>`'s DRM master,
//! as pure functions, host-tested, which the kernel's open card calls and
//! does not copy (`docs/DISPLAY.md` §2.3, the certification consultant's
//! condition 6, ledger 2327).
//!
//! A card has at most one master, the open that shows things: it alone may
//! set a mode, flip a page, report damage or place a cursor. Every other
//! open may read the card's objects and make buffers of its own. The first
//! open is master when there is none and its opener is root; `SET_MASTER`
//! and `DROP_MASTER` move it as Linux's `drm_setmaster_ioctl` and
//! `drm_dropmaster_ioctl` do.

use ferrix_linux_abi::drm;
use ferrix_linux_abi::errno::Errno;

/// What an ioctl needs of the open that makes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// Any open: it reads the card, or changes only the open's own objects.
    Any,
    /// The master alone: it changes what the card shows. `EACCES` for any
    /// other open, Linux's `drm_ioctl_permit` answer for a `DRM_MASTER` call.
    Master,
    /// `SET_MASTER` and `DROP_MASTER`, which decide for themselves
    /// ([`set_master`], [`drop_master`]).
    Mastership,
}

/// The class of every ioctl the card answers, by its number; `version` is
/// `DRM_IOCTL_VERSION`'s number for the caller's width. `None` for any other
/// number, which the card refuses (`ENOTTY`): a call added later is in no
/// class until it is put in one here, and one that changes what is shown or
/// the card's state -- `OBJ_SETPROPERTY`, `SETPROPERTY`, `SETGAMMA`,
/// `SETPLANE`, leases -- belongs with [`Class::Master`].
#[must_use]
pub const fn class_of(request: u32, version: u32) -> Option<Class> {
    if request == version {
        return Some(Class::Any);
    }
    Some(match request {
        drm::IOCTL_SET_MASTER | drm::IOCTL_DROP_MASTER => Class::Mastership,
        drm::IOCTL_MODE_SETCRTC
        | drm::IOCTL_MODE_PAGE_FLIP
        | drm::IOCTL_MODE_DIRTYFB
        | drm::IOCTL_MODE_CURSOR
        | drm::IOCTL_MODE_CURSOR2 => Class::Master,
        drm::IOCTL_GET_CAP
        | drm::IOCTL_SET_CLIENT_CAP
        | drm::IOCTL_MODE_GETRESOURCES
        | drm::IOCTL_MODE_GETCONNECTOR
        | drm::IOCTL_MODE_GETENCODER
        | drm::IOCTL_MODE_GETCRTC
        | drm::IOCTL_MODE_CREATE_DUMB
        | drm::IOCTL_MODE_MAP_DUMB
        | drm::IOCTL_PRIME_FD_TO_HANDLE
        | drm::IOCTL_MODE_DESTROY_DUMB
        | drm::IOCTL_MODE_ADDFB
        | drm::IOCTL_MODE_ADDFB2
        | drm::IOCTL_MODE_RMFB
        | drm::IOCTL_MODE_GETPLANERESOURCES
        | drm::IOCTL_MODE_GETPLANE
        | drm::IOCTL_MODE_OBJ_GETPROPERTIES
        | drm::IOCTL_MODE_GETPROPERTY
        | drm::IOCTL_MODE_GETPROPBLOB => Class::Any,
        _ => return None,
    })
}

/// Whether an open of class-`class` ioctl may go on, for an open that `is`
/// master or not: `EACCES` for a master-only call from another open.
///
/// # Errors
///
/// `EACCES`.
pub const fn permit(class: Class, is_master: bool) -> Result<(), Errno> {
    match class {
        Class::Master if !is_master => Err(Errno::EACCES),
        _ => Ok(()),
    }
}

/// An open's number on its card: given at open, never shared.
pub type OpenId = u64;

/// Whether a new open, `me`, is master: when the card has none and its
/// opener is root. Should card0 ever be opened by someone else, a program
/// of a user's could otherwise take master between two sessions and draw a
/// lock screen of its own (the consultant's condition 1, ledger 2290).
#[must_use]
pub const fn first_open_is_master(master: Option<OpenId>, root: bool) -> bool {
    master.is_none() && root
}

/// What `SET_MASTER` did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Became {
    /// The open was master already.
    Already,
    /// The open is master now.
    Master,
}

/// `SET_MASTER` from open `me`, on a card whose master is `master`: as
/// Linux's `drm_setmaster_ioctl` answers since 5.10. A master asking again
/// is answered yes; while another open is master, `EBUSY`; with none, an
/// open that was master before (`was_master`), or one made by root (`root`),
/// becomes it, and any other is `EACCES`.
///
/// # Errors
///
/// `EBUSY`, `EACCES`.
pub const fn set_master(
    master: Option<OpenId>,
    me: OpenId,
    was_master: bool,
    root: bool,
) -> Result<Became, Errno> {
    match master {
        Some(holder) if holder == me => Ok(Became::Already),
        Some(_) => Err(Errno::EBUSY),
        None if was_master || root => Ok(Became::Master),
        None => Err(Errno::EACCES),
    }
}

/// `DROP_MASTER` from open `me`: allowed only to the master, `EINVAL` for
/// any other open, as Linux's `drm_dropmaster_ioctl`.
///
/// # Errors
///
/// `EINVAL`.
pub const fn drop_master(master: Option<OpenId>, me: OpenId) -> Result<(), Errno> {
    match master {
        Some(holder) if holder == me => Ok(()),
        _ => Err(Errno::EINVAL),
    }
}

/// The most attached buffers the opens that are not master may hold
/// together: half the session's [`crate::session::MAX_BUFFERS`], the rest
/// kept for the master.
pub const NON_MASTER_BUFFERS: usize = crate::session::MAX_BUFFERS / 2;

/// What a card's budget holds now, for [`may_take`]: every buffer's bytes,
/// and of those the ones whose open is not master *now*, recounted when the
/// master changes (the consultant's condition 4).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Taken {
    /// Bytes of every buffer, kept ones included.
    pub bytes: u64,
    /// Bytes of the buffers of opens that are not master.
    pub others: u64,
    /// How many buffers opens that are not master hold.
    pub others_buffers: usize,
}

/// Whether a new buffer of `bytes` may be made on a card of `budget` bytes,
/// for an open that `is_master` or not: the whole budget for the master,
/// and for every other open together at most half of it and
/// [`NON_MASTER_BUFFERS`] buffers, so that the master always keeps half the
/// budget -- a double-buffered 4K screen and a cursor at 256 MiB -- whatever
/// the other opens make (the consultant's condition 2, ledger 2290).
#[must_use]
pub const fn may_take(taken: Taken, bytes: u64, is_master: bool, budget: u64) -> bool {
    let Some(total) = taken.bytes.checked_add(bytes) else {
        return false;
    };
    if total > budget {
        return false;
    }
    if is_master {
        return true;
    }
    let Some(others) = taken.others.checked_add(bytes) else {
        return false;
    };
    others <= budget / 2 && taken.others_buffers < NON_MASTER_BUFFERS
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    const VERSION: u32 = drm::IOCTL_VERSION_64;

    /// Every ioctl the card answers, with its class: a call added to the
    /// card and not here, or put in another class, fails this.
    const EVERY: [(u32, Class); 27] = [
        (drm::IOCTL_VERSION_64, Class::Any),
        (drm::IOCTL_SET_MASTER, Class::Mastership),
        (drm::IOCTL_DROP_MASTER, Class::Mastership),
        (drm::IOCTL_MODE_SETCRTC, Class::Master),
        (drm::IOCTL_MODE_PAGE_FLIP, Class::Master),
        (drm::IOCTL_MODE_DIRTYFB, Class::Master),
        (drm::IOCTL_MODE_CURSOR, Class::Master),
        (drm::IOCTL_MODE_CURSOR2, Class::Master),
        (drm::IOCTL_GET_CAP, Class::Any),
        (drm::IOCTL_SET_CLIENT_CAP, Class::Any),
        (drm::IOCTL_MODE_GETRESOURCES, Class::Any),
        (drm::IOCTL_MODE_GETCONNECTOR, Class::Any),
        (drm::IOCTL_MODE_GETENCODER, Class::Any),
        (drm::IOCTL_MODE_GETCRTC, Class::Any),
        (drm::IOCTL_MODE_CREATE_DUMB, Class::Any),
        (drm::IOCTL_MODE_MAP_DUMB, Class::Any),
        (drm::IOCTL_PRIME_FD_TO_HANDLE, Class::Any),
        (drm::IOCTL_MODE_DESTROY_DUMB, Class::Any),
        (drm::IOCTL_MODE_ADDFB, Class::Any),
        (drm::IOCTL_MODE_ADDFB2, Class::Any),
        (drm::IOCTL_MODE_RMFB, Class::Any),
        (drm::IOCTL_MODE_GETPLANERESOURCES, Class::Any),
        (drm::IOCTL_MODE_GETPLANE, Class::Any),
        (drm::IOCTL_MODE_OBJ_GETPROPERTIES, Class::Any),
        (drm::IOCTL_MODE_GETPROPERTY, Class::Any),
        (drm::IOCTL_MODE_GETPROPBLOB, Class::Any),
        (drm::IOCTL_VERSION_32, Class::Any),
    ];

    #[test]
    fn every_ioctl_the_card_answers_has_its_class_and_no_other_has_one() {
        for (request, class) in EVERY {
            let version = if request == drm::IOCTL_VERSION_32 {
                drm::IOCTL_VERSION_32
            } else {
                VERSION
            };
            assert_eq!(class_of(request, version), Some(class), "{request:#x}");
        }
        // Calls that would change what is shown, and the render node's: in
        // no class, so refused.
        for unknown in [
            drm::IOCTL_GEM_CLOSE,
            drm::IOCTL_PRIME_HANDLE_TO_FD,
            0xC060_64B7,
            0,
        ] {
            assert_eq!(class_of(unknown, VERSION), None, "{unknown:#x}");
        }
    }

    #[test]
    fn only_the_master_changes_what_is_shown() {
        assert_eq!(permit(Class::Master, false), Err(Errno::EACCES));
        assert_eq!(permit(Class::Master, true), Ok(()));
        assert_eq!(permit(Class::Any, false), Ok(()));
        assert_eq!(permit(Class::Mastership, false), Ok(()));
    }

    #[test]
    fn the_first_open_is_master_only_when_root_makes_it() {
        assert!(first_open_is_master(None, true));
        assert!(!first_open_is_master(None, false));
        assert!(!first_open_is_master(Some(1), true));
    }

    #[test]
    fn set_master_answers_as_linux_does() {
        assert_eq!(set_master(Some(1), 1, true, false), Ok(Became::Already));
        assert_eq!(set_master(Some(1), 2, true, true), Err(Errno::EBUSY));
        assert_eq!(set_master(None, 2, true, false), Ok(Became::Master));
        assert_eq!(set_master(None, 2, false, true), Ok(Became::Master));
        assert_eq!(set_master(None, 2, false, false), Err(Errno::EACCES));
    }

    #[test]
    fn only_the_master_drops_it() {
        assert_eq!(drop_master(Some(1), 1), Ok(()));
        assert_eq!(drop_master(Some(1), 2), Err(Errno::EINVAL));
        assert_eq!(drop_master(None, 2), Err(Errno::EINVAL));
    }

    #[test]
    fn the_master_keeps_half_the_budget_whatever_the_others_make() {
        const BUDGET: u64 = 256 << 20;
        let others = Taken {
            bytes: 128 << 20,
            others: 128 << 20,
            others_buffers: 4,
        };
        assert!(
            !may_take(others, 4096, false, BUDGET),
            "the others' half is full"
        );
        assert!(
            may_take(others, 66 << 20, true, BUDGET),
            "the master's half is free"
        );
        assert!(
            !may_take(others, (128 << 20) + 4096, true, BUDGET),
            "never past the budget"
        );
        let many = Taken {
            bytes: 32 * 4096,
            others: 32 * 4096,
            others_buffers: NON_MASTER_BUFFERS,
        };
        assert!(
            !may_take(many, 4096, false, BUDGET),
            "the others' slots are full"
        );
        assert!(may_take(many, 4096, true, BUDGET));
        assert!(!may_take(
            Taken {
                bytes: u64::MAX,
                ..Taken::default()
            },
            1,
            true,
            BUDGET
        ));
    }
}
