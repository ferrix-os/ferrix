//! The kernel's FUSE protocol, 7.31 (`linux/fuse.h`): one request's bytes in,
//! one reply's bytes out, with no file descriptor in sight.
use crate::store::{BLOCK, Backend, EIO};
use crate::tree::{Attr, Change, Time, Tree};

const ENOSYS: i32 = 38;
const EINVAL: i32 = 22;
const HEADER: usize = 40;
/// The most a write may carry: 256 pages.
pub const MAX_WRITE: u32 = 1 << 20;
/// How long the kernel may trust a name or a stat, in seconds. The kernel is the
/// only client, and each change goes through it.
const VALID: u64 = 1;
const OPEN_KEEP_CACHE: u32 = 2;

fn take<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N], i32> {
    bytes
        .get(at..at + N)
        .and_then(|b| b.try_into().ok())
        .ok_or(EINVAL)
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32, i32> {
    take(bytes, at).map(u32::from_le_bytes)
}
fn u64_at(bytes: &[u8], at: usize) -> Result<u64, i32> {
    take(bytes, at).map(u64::from_le_bytes)
}
/// The NUL-terminated name at the start of `bytes`, and what follows it.
fn name(bytes: &[u8]) -> Result<(&[u8], &[u8]), i32> {
    let end = bytes.iter().position(|&b| b == 0).ok_or(EINVAL)?;
    Ok((&bytes[..end], &bytes[end + 1..]))
}

fn attr_bytes(a: &Attr, out: &mut Vec<u8>) {
    for v in [a.ino, a.size, a.blocks, a.mtime.0, a.mtime.0, a.ctime.0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in [
        a.mtime.1,
        a.mtime.1,
        a.ctime.1,
        a.mode,
        a.nlink,
        a.uid,
        a.gid,
        0,
        BLOCK as u32,
        0,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
}
fn entry_bytes(a: &Attr, out: &mut Vec<u8>) {
    for v in [a.ino, 0, VALID, VALID] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&[0; 8]);
    attr_bytes(a, out);
}
fn open_bytes(flags: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
}

/// The kernel's INIT, answered with what this file system does.
fn init<B: Backend>(tree: &mut Tree<B>, body: &[u8], out: &mut Vec<u8>) -> Result<(), i32> {
    const ASYNC_READ: u32 = 1;
    const ATOMIC_O_TRUNC: u32 = 1 << 3;
    const BIG_WRITES: u32 = 1 << 5;
    const WRITEBACK_CACHE: u32 = 1 << 16;
    const MAX_PAGES: u32 = 1 << 22;
    if u32_at(body, 0)? != 7 {
        return Err(EINVAL);
    }
    let offered = u32_at(body, 12)?;
    let wanted = ASYNC_READ | ATOMIC_O_TRUNC | BIG_WRITES | WRITEBACK_CACHE | MAX_PAGES;
    tree.kernel_times = offered & WRITEBACK_CACHE != 0;
    for v in [7, 31, MAX_WRITE, offered & wanted] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&64u16.to_le_bytes()); // max_background
    out.extend_from_slice(&48u16.to_le_bytes()); // congestion_threshold
    out.extend_from_slice(&MAX_WRITE.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // time_gran
    out.extend_from_slice(&256u16.to_le_bytes()); // max_pages
    out.resize(out.len() + 2 + 32, 0);
    Ok(())
}

fn setattr<B: Backend>(
    tree: &mut Tree<B>,
    ino: u64,
    b: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), i32> {
    const MODE: u32 = 1;
    const UID: u32 = 2;
    const GID: u32 = 4;
    const SIZE: u32 = 8;
    const MTIME: u32 = 32;
    const MTIME_NOW: u32 = 256;
    let valid = u32_at(b, 0)?;
    let when = |bit: u32, value: u32| (valid & bit != 0).then_some(value);
    let change = Change {
        mode: when(MODE, u32_at(b, 68)?),
        uid: when(UID, u32_at(b, 76)?),
        gid: when(GID, u32_at(b, 80)?),
        size: (valid & SIZE != 0).then_some(u64_at(b, 16)?),
        mtime: (valid & MTIME != 0 && valid & MTIME_NOW == 0)
            .then_some(Time(u64_at(b, 40)?, u32_at(b, 60)?)),
        mtime_now: valid & MTIME_NOW != 0,
    };
    let attr = tree.change(ino, change)?;
    out.extend_from_slice(&[VALID, 0].map(u64::to_le_bytes).concat());
    attr_bytes(&attr, out);
    Ok(())
}

fn readdir<B: Backend>(
    tree: &Tree<B>,
    ino: u64,
    offset: u64,
    size: usize,
    out: &mut Vec<u8>,
) -> Result<(), i32> {
    for (next, child, mode, entry) in tree.readdir(ino, offset)? {
        let length = 24 + entry.len();
        let padded = length.div_ceil(8) * 8;
        if out.len() - 16 + padded > size {
            break;
        }
        out.extend_from_slice(&child.to_le_bytes());
        out.extend_from_slice(&next.to_le_bytes());
        out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        out.extend_from_slice(&((mode >> 12) & 0o17).to_le_bytes());
        out.extend_from_slice(&entry);
        out.resize(out.len() + padded - length, 0);
    }
    Ok(())
}

/// Serve one request. `None` for those the kernel wants no answer to.
#[must_use]
pub fn serve<B: Backend>(tree: &mut Tree<B>, request: &[u8]) -> Option<Vec<u8>> {
    let opcode = u32_at(request, 4).ok()?;
    let unique = u64_at(request, 8).ok()?;
    let ino = u64_at(request, 16).ok()?;
    let who = (u32_at(request, 24).ok()?, u32_at(request, 28).ok()?);
    let body = request.get(HEADER..)?;
    match opcode {
        // FORGET, BATCH_FORGET, INTERRUPT: no reply. They only let go of things,
        // so they are served on a dead store too.
        2 => {
            tree.forget(ino, u64_at(body, 0).ok()?);
            return None;
        }
        42 => {
            for i in 0..u32_at(body, 0).ok()? as usize {
                tree.forget(
                    u64_at(body, 8 + i * 16).ok()?,
                    u64_at(body, 16 + i * 16).ok()?,
                );
            }
            return None;
        }
        36 => return None,
        _ => {}
    }
    let mut out = vec![0; 16];
    let served = if tree.store.is_dead() && !matches!(opcode, 18 | 29 | 26 | 38) {
        Err(EIO)
    } else {
        answer(tree, opcode, ino, who, body, &mut out)
    };
    let error = match served {
        Ok(()) => 0,
        Err(error) => {
            out.truncate(16);
            -error
        }
    };
    let length = out.len() as u32;
    out[0..4].copy_from_slice(&length.to_le_bytes());
    out[4..8].copy_from_slice(&error.to_le_bytes());
    out[8..16].copy_from_slice(&unique.to_le_bytes());
    Some(out)
}

fn answer<B: Backend>(
    tree: &mut Tree<B>,
    opcode: u32,
    ino: u64,
    who: (u32, u32),
    b: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), i32> {
    match opcode {
        1 => entry_bytes(&tree.lookup(ino, name(b)?.0)?, out),
        3 => {
            let attr = tree.attr(ino)?;
            out.extend_from_slice(&[VALID, 0].map(u64::to_le_bytes).concat());
            attr_bytes(&attr, out);
        }
        4 => setattr(tree, ino, b, out)?,
        5 => out.extend_from_slice(&tree.readlink(ino)?),
        6 => {
            let (link, rest) = name(b)?;
            entry_bytes(&tree.symlink(ino, link, name(rest)?.0, who)?, out);
        }
        8 => entry_bytes(
            &tree.create(
                ino,
                name(b.get(16..).ok_or(EINVAL)?)?.0,
                u32_at(b, 0)? & !u32_at(b, 8)?,
                who,
            )?,
            out,
        ),
        9 => entry_bytes(
            &tree.mkdir(
                ino,
                name(b.get(8..).ok_or(EINVAL)?)?.0,
                u32_at(b, 0)? & !u32_at(b, 4)?,
                who,
            )?,
            out,
        ),
        10 => tree.unlink(ino, name(b)?.0)?,
        11 => tree.rmdir(ino, name(b)?.0)?,
        12 => {
            let (old, rest) = name(b.get(8..).ok_or(EINVAL)?)?;
            tree.rename(ino, old, u64_at(b, 0)?, name(rest)?.0, false)?;
        }
        45 => {
            let flags = u32_at(b, 8)?;
            if flags & !1 != 0 {
                return Err(EINVAL);
            }
            let (old, rest) = name(b.get(16..).ok_or(EINVAL)?)?;
            tree.rename(ino, old, u64_at(b, 0)?, name(rest)?.0, flags & 1 != 0)?;
        }
        13 => entry_bytes(
            &tree.link(u64_at(b, 0)?, ino, name(b.get(8..).ok_or(EINVAL)?)?.0)?,
            out,
        ),
        14 => {
            tree.open(ino)?;
            open_bytes(OPEN_KEEP_CACHE, out);
        }
        27 => {
            tree.open(ino)?;
            open_bytes(0, out);
        }
        15 => {
            let size = u32_at(b, 16)? as usize;
            tree.read(ino, u64_at(b, 8)?, size, out)?;
        }
        16 => {
            let size = u32_at(b, 16)? as usize;
            let data = b.get(40..40 + size).ok_or(EINVAL)?;
            let written = tree.write(ino, u64_at(b, 8)?, data)?;
            out.extend_from_slice(&(written as u32).to_le_bytes());
            out.extend_from_slice(&[0; 4]);
        }
        17 => {
            let (blocks, free) = (tree.store.blocks(), tree.store.free_blocks());
            for v in [blocks, free, free, 1 << 32, 1 << 32] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            for v in [BLOCK as u32, 255, BLOCK as u32, 0, 0, 0, 0, 0, 0, 0] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        18 | 29 => tree.release(ino),
        // FSYNC: the barrier a `verify` stands on, and the only thing a sync can owe here.
        20 => tree.store.barrier()?,
        25 | 30 => {}
        26 => init(tree, b, out)?,
        28 => readdir(tree, ino, u64_at(b, 8)?, u32_at(b, 16)? as usize, out)?,
        35 => {
            let attr = tree.create(
                ino,
                name(b.get(16..).ok_or(EINVAL)?)?.0,
                u32_at(b, 4)? & !u32_at(b, 8)?,
                who,
            )?;
            tree.open(attr.ino)?;
            entry_bytes(&attr, out);
            open_bytes(OPEN_KEEP_CACHE, out);
        }
        38 => {}
        _ => return Err(ENOSYS),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Ram, Store};
    use crate::tree::ROOT;

    fn tree() -> Tree<Ram> {
        Tree::new(Store::new(Ram(vec![0; 4 * BLOCK]), 4), 1000, 1000)
    }
    fn request(opcode: u32, ino: u64, body: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&((HEADER + body.len()) as u32).to_le_bytes());
        r.extend_from_slice(&opcode.to_le_bytes());
        r.extend_from_slice(&77u64.to_le_bytes());
        r.extend_from_slice(&ino.to_le_bytes());
        r.extend_from_slice(&1000u32.to_le_bytes());
        r.extend_from_slice(&1000u32.to_le_bytes());
        r.extend_from_slice(&[0; 8]);
        r.extend_from_slice(body);
        r
    }
    fn error_of(reply: &[u8]) -> i32 {
        i32::from_le_bytes(reply[4..8].try_into().unwrap())
    }

    fn create(t: &mut Tree<Ram>, file: &[u8]) -> (Vec<u8>, u64) {
        let mut body = vec![];
        for v in [0o102u32, 0o644, 0o022, 0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(file);
        body.push(0);
        let reply = serve(t, &request(35, ROOT, &body)).unwrap();
        let ino = u64::from_le_bytes(reply[16..24].try_into().unwrap());
        (reply, ino)
    }
    fn write(t: &mut Tree<Ram>, ino: u64, data: &[u8]) -> Vec<u8> {
        let mut body = vec![0; 40];
        body[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
        serve(t, &request(16, ino, &body)).unwrap()
    }
    fn read(t: &mut Tree<Ram>, ino: u64, n: u32) -> Vec<u8> {
        let mut body = vec![0; 40];
        body[16..20].copy_from_slice(&n.to_le_bytes());
        serve(t, &request(15, ino, &body)).unwrap()
    }

    #[test]
    fn replies_carry_their_length_and_the_requests_unique() {
        let mut t = tree();
        let (reply, _) = create(&mut t, b"f");
        assert_eq!(
            u32::from_le_bytes(reply[0..4].try_into().unwrap()) as usize,
            reply.len()
        );
        assert_eq!(u64::from_le_bytes(reply[8..16].try_into().unwrap()), 77);
        assert_eq!(error_of(&reply), 0);
        // entry_out (40) + attr (88) + open_out (16) after the header.
        assert_eq!(reply.len(), 16 + 128 + 16);
    }

    #[test]
    fn create_write_read_goes_through_the_protocol() {
        let mut t = tree();
        let (_, ino) = create(&mut t, b"f");
        let w = write(&mut t, ino, b"hello");
        assert_eq!(u32::from_le_bytes(w[16..20].try_into().unwrap()), 5);
        assert_eq!(&read(&mut t, ino, 100)[16..], b"hello");
        // The umask came off the mode.
        assert_eq!(t.attr(ino).unwrap().mode & 0o777, 0o644);
    }

    #[test]
    fn init_offers_only_what_the_kernel_offered_and_is_64_bytes() {
        let mut t = tree();
        let mut body = vec![];
        for v in [7u32, 45, 1 << 20, 1 | (1 << 16)] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        let reply = serve(&mut t, &request(26, 0, &body)).unwrap();
        assert_eq!(reply.len(), 16 + 64);
        assert_eq!(
            u32::from_le_bytes(reply[28..32].try_into().unwrap()),
            1 | (1 << 16)
        );
        assert!(
            t.kernel_times,
            "writeback cache agreed: the kernel owns the times"
        );
        let mut old = body.clone();
        old[0] = 6;
        assert_eq!(
            error_of(&serve(&mut t, &request(26, 0, &old)).unwrap()),
            -EINVAL
        );
    }

    #[test]
    fn a_dead_store_answers_eio_to_everything_that_asks_for_data_or_names() {
        let mut t = tree();
        let (_, ino) = create(&mut t, b"f");
        write(&mut t, ino, b"hello");
        t.store.backend_mut().0[0] ^= 1;
        assert_eq!(error_of(&read(&mut t, ino, 5)), -EIO);
        assert_eq!(
            error_of(&serve(&mut t, &request(3, ino, &[0; 16])).unwrap()),
            -EIO
        );
        assert_eq!(
            error_of(&serve(&mut t, &request(1, ROOT, b"f\0")).unwrap()),
            -EIO
        );
        assert_eq!(error_of(&write(&mut t, ino, b"x")), -EIO);
        // What only lets go still works, so the kernel can unmount it.
        assert_eq!(
            error_of(&serve(&mut t, &request(18, ino, &[0; 24])).unwrap()),
            0
        );
        assert!(serve(&mut t, &request(2, ino, &1u64.to_le_bytes())).is_none());
    }

    #[test]
    fn what_it_does_not_do_is_enosys() {
        let mut t = tree();
        assert_eq!(
            error_of(&serve(&mut t, &request(39, ROOT, &[])).unwrap()),
            -ENOSYS
        );
    }

    #[test]
    fn a_short_request_is_an_error_and_not_a_panic() {
        let mut t = tree();
        assert_eq!(
            error_of(&serve(&mut t, &request(16, ROOT, &[0; 4])).unwrap()),
            -EINVAL
        );
        assert_eq!(
            error_of(&serve(&mut t, &request(1, ROOT, b"no end")).unwrap()),
            -EINVAL
        );
        assert!(serve(&mut t, &[0; 3]).is_none());
    }

    #[test]
    fn readdir_stops_at_the_size_asked_and_goes_on() {
        let mut t = tree();
        for file in [b"a", b"b", b"c"] {
            create(&mut t, file);
        }
        let mut body = vec![0; 40];
        body[16..20].copy_from_slice(&64u32.to_le_bytes());
        let first = serve(&mut t, &request(28, ROOT, &body)).unwrap();
        assert_eq!(first.len(), 16 + 64); // "." and ".." at 32 each.
        body[8..16].copy_from_slice(&2u64.to_le_bytes());
        let next = serve(&mut t, &request(28, ROOT, &body)).unwrap();
        assert_eq!(&next[16 + 24..16 + 25], b"a");
    }
}
