//! Who shows things on a card: the check `cargo xtask test-display` makes
//! of the DRM master (`docs/DISPLAY.md` §2.3), on two opens of the card's
//! own, before the program shows anything.
//!
//! 1. A second open succeeds while the first is open and is not master: its
//!    `SETCRTC`, `PAGE_FLIP`, `DIRTYFB` and `CURSOR` are `EACCES`.
//! 2. It cannot map the first open's buffer, whose map offset names nothing
//!    of its own (E-02, across opens).
//! 3. Its `DROP_MASTER` is `EINVAL`, and its `SET_MASTER` `EBUSY` while the
//!    first is master.
//! 4. The first shows [`FIRST`] and drops master: the screen goes off. The
//!    program says [`BLANK`] with the colour and waits for a line on its
//!    console, while the test reads the screen and requires no pixel of it.
//!    Then the second, root, takes master and shows [`SECOND`].
//! 5. The first's `SET_MASTER` is `EBUSY`; once the second drops, the first
//!    takes master back, having been master before.
//! 6. An open that is not master may hold at most `NON_MASTER_BUFFERS`
//!    buffers, and the master still makes one after.
//! 7. With nobody master, a program that is not root cannot take master
//!    through an open that never had it: a child that inherited a third
//!    open and dropped to uid 65534 is refused `EACCES`.
//!
//! The answer is one line, [`PREFIX`] then `ok` or what failed.

use std::io::{self, BufRead, Write};

use ferrix_linux_abi::drm::{self, Field, MapDumb};

use crate::card::{Card, Dumb, ioctl_on, plan};

/// What the line starts with, which the display test looks for.
pub const PREFIX: &str = "card-master: ";

/// What the program says when the first open has dropped master, before it
/// waits: `compositor: master-blank` and the first's colour as `RRGGBB`. It
/// starts with the compositor's prefix, which is what the test's hook runs
/// at.
pub const BLANK: &str = "compositor: master-blank";

/// The first open's colour and the second's, as `XRGB8888`.
pub(crate) const FIRST: u32 = 0x00C0_3030;
/// The second open's.
pub(crate) const SECOND: u32 = 0x0030_C030;

/// How many buffers `NON_MASTER_BUFFERS` is, as the kernel's rules say
/// (`ferrix_displayctl::card`: half the session's 32).
const NON_MASTER_BUFFERS: usize = 16;

/// `SET_MASTER` or `DROP_MASTER`, which take no argument.
fn mastership(card: &Card, request: u32) -> io::Result<()> {
    // SAFETY: neither request reads or writes its argument.
    let result = unsafe { libc::ioctl(card.raw_fd(), request as _, 0) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Close `card`, which `Card` itself never does.
fn close(card: Card) {
    // SAFETY: the descriptor `Card::open` made, closed once; `card` is not
    // used after.
    let _ = unsafe { libc::close(card.raw_fd()) };
}

/// Whether `result` is the error `errno`.
fn refused<T>(result: io::Result<T>, errno: i32) -> bool {
    matches!(result, Err(error) if error.raw_os_error() == Some(errno))
}

/// Fill `dumb` with `colour`.
fn fill(dumb: &mut Dumb, colour: u32) {
    for pixel in dumb.pixels().chunks_exact_mut(4) {
        pixel.copy_from_slice(&colour.to_le_bytes());
    }
}

/// Run the checks: [`PREFIX`] and `ok`, or the first that failed.
#[must_use]
pub fn master() -> String {
    let opened = Card::open().and_then(|first| Ok((first, Card::open()?)));
    let (first, second) = match opened {
        Ok(both) => both,
        Err(why) => return format!("{PREFIX}FAILED: card0 did not open twice: {why}"),
    };
    let said = match check(&first, &second) {
        Ok(()) => format!("{PREFIX}ok"),
        Err(why) => format!("{PREFIX}FAILED: {why}"),
    };
    // Whatever failed, nobody is master after: the program's own screen
    // comes next, from an open of its own.
    let _ = mastership(&first, drm::IOCTL_DROP_MASTER);
    let _ = mastership(&second, drm::IOCTL_DROP_MASTER);
    close(second);
    close(first);
    said
}

fn check(first: &Card, second: &Card) -> Result<(), String> {
    let plan = plan(first).map_err(|why| format!("no plan for the screen: {why}"))?;
    let (width, height) = (u32::from(plan.mode.hdisplay), u32::from(plan.mode.vdisplay));

    // 2, first: the first open's buffer, and its offset tried on the second
    // before the second has any buffer of its own the offset could name.
    let mut shown =
        Dumb::new(first, width, height).map_err(|why| format!("the first open's buffer: {why}"))?;
    fill(&mut shown, FIRST);
    let mut map = MapDumb {
        handle: shown.handle(),
        ..MapDumb::ZERO
    };
    ioctl_on(first.raw_fd(), drm::IOCTL_MODE_MAP_DUMB, &mut map)
        .map_err(|why| format!("MAP_DUMB: {why}"))?;
    let offset = libc::off_t::try_from(map.offset).map_err(|why| why.to_string())?;
    // SAFETY: a mapping asked for and, if made, taken away at once; nothing
    // reads it.
    let stolen = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            4096,
            libc::PROT_READ,
            libc::MAP_SHARED,
            second.raw_fd(),
            offset,
        )
    };
    if stolen != libc::MAP_FAILED {
        // SAFETY: the mapping just made.
        let _ = unsafe { libc::munmap(stolen, 4096) };
        return Err("the second open mapped the first open's buffer".to_owned());
    }

    // 1. The second is not master.
    let theirs = Dumb::new(second, width, height)
        .map_err(|why| format!("the second open's buffer: {why}"))?;
    let eacces = libc::EACCES;
    if !refused(second.set_mode(&plan, &theirs), eacces) {
        return Err("the second open's SETCRTC was not EACCES".to_owned());
    }
    if !refused(second.page_flip(&plan, &theirs), eacces) {
        return Err("the second open's PAGE_FLIP was not EACCES".to_owned());
    }
    if !refused(second.dirty(&theirs, &[]), eacces) {
        return Err("the second open's DIRTYFB was not EACCES".to_owned());
    }
    if !refused(second.set_cursor(&plan, None, (0, 0), (0, 0)), eacces) {
        return Err("the second open's CURSOR was not EACCES".to_owned());
    }

    // 3.
    if !refused(mastership(second, drm::IOCTL_DROP_MASTER), libc::EINVAL) {
        return Err("the second open's DROP_MASTER was not EINVAL".to_owned());
    }
    if !refused(mastership(second, drm::IOCTL_SET_MASTER), libc::EBUSY) {
        return Err(
            "the second open's SET_MASTER was not EBUSY while the first is master".to_owned(),
        );
    }

    // 4. The first shows, drops, and the screen is read with nothing on it.
    let _ = first
        .set_mode(&plan, &shown)
        .map_err(|why| format!("the master's SETCRTC: {why}"))?;
    mastership(first, drm::IOCTL_DROP_MASTER)
        .map_err(|why| format!("the master's DROP_MASTER: {why}"))?;
    let mut out = io::stdout();
    let _ = writeln!(out, "{BLANK} {:06x}", FIRST & 0x00FF_FFFF);
    let _ = out.flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    mastership(second, drm::IOCTL_SET_MASTER)
        .map_err(|why| format!("root's SET_MASTER with no master: {why}"))?;
    let mut theirs = theirs;
    fill(&mut theirs, SECOND);
    let _ = second
        .set_mode(&plan, &theirs)
        .map_err(|why| format!("the new master's SETCRTC: {why}"))?;

    // 5. Taken back by the open that was master before.
    if !refused(mastership(first, drm::IOCTL_SET_MASTER), libc::EBUSY) {
        return Err(
            "the first open's SET_MASTER was not EBUSY while the second is master".to_owned(),
        );
    }
    mastership(second, drm::IOCTL_DROP_MASTER)
        .map_err(|why| format!("the second's DROP_MASTER: {why}"))?;
    mastership(first, drm::IOCTL_SET_MASTER)
        .map_err(|why| format!("the first open did not take master back: {why}"))?;

    // 6. The second, not master, fills its share of the buffers; the master
    // still makes one. Small buffers: the slots run out, not the bytes.
    let mut held = Vec::with_capacity(NON_MASTER_BUFFERS);
    let mut refused_at = None;
    for made in 0..=NON_MASTER_BUFFERS {
        match Dumb::new(second, 16, 16) {
            Ok(dumb) => held.push(dumb),
            Err(why) => {
                refused_at = Some((made, why.raw_os_error()));
                break;
            }
        }
    }
    // The second's first buffer counts against the share too.
    if refused_at.map(|(made, _)| made) != Some(NON_MASTER_BUFFERS - 1) {
        return Err(format!(
            "the second open was refused a buffer at {refused_at:?}, not at its {NON_MASTER_BUFFERS}th"
        ));
    }
    let _master_buffer = Dumb::new(first, 16, 16)
        .map_err(|why| format!("the master was refused a buffer past the others' share: {why}"))?;
    drop(held);

    // 7. A third open, made while the first is master, so never master
    // itself; the first then drops, and a child that is not root asks.
    let third = Card::open().map_err(|why| format!("a third open: {why}"))?;
    mastership(first, drm::IOCTL_DROP_MASTER)
        .map_err(|why| format!("the first's last DROP_MASTER: {why}"))?;
    let asked = as_nobody(&third);
    close(third);
    match asked {
        Some(code) if code == libc::EACCES => Ok(()),
        Some(0) => {
            Err("a program that is not root took master with no master on the card".to_owned())
        }
        Some(code) => Err(format!(
            "a program that is not root was refused master with errno {code}, not EACCES"
        )),
        None => Err("the child that asks as uid 65534 did not run".to_owned()),
    }
}

/// `SET_MASTER` on `card` from a child that dropped to uid 65534 first: the
/// errno it was refused with, 0 if it was not, or `None` if no child ran.
fn as_nobody(card: &Card) -> Option<i32> {
    // SAFETY: the child calls only async-signal-safe functions -- setresgid,
    // setresuid, ioctl, _exit -- before it exits.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return None;
    }
    if child == 0 {
        // SAFETY: async-signal-safe, with constant arguments.
        let group = unsafe { libc::setresgid(65534, 65534, 65534) };
        // SAFETY: as above.
        let user = unsafe { libc::setresuid(65534, 65534, 65534) };
        let code = if group != 0 || user != 0 {
            255
        } else {
            // SAFETY: SET_MASTER reads and writes no argument.
            let asked = unsafe { libc::ioctl(card.raw_fd(), drm::IOCTL_SET_MASTER as _, 0) };
            if asked == 0 {
                0
            } else {
                io::Error::last_os_error().raw_os_error().unwrap_or(254)
            }
        };
        // SAFETY: the child ends here, running nothing of the parent's.
        unsafe { libc::_exit(code) }
    }
    let mut status = 0;
    // SAFETY: the child just made; `status` is a live int.
    if unsafe { libc::waitpid(child, &mut status, 0) } != child || !libc::WIFEXITED(status) {
        return None;
    }
    match libc::WEXITSTATUS(status) {
        255 => None,
        code => Some(code),
    }
}

/// Open the card as an open that is not master and close it again: what a
/// program that only reads the card does. Closing must not take the
/// master's screen off, which the display test's screendump then shows.
pub fn bystander() {
    if let Ok(card) = Card::open() {
        close(card);
    }
}
