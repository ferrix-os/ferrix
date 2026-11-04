//! Inodes over the [`store`](crate::store), answering as the kernel's FUSE client asks.
use crate::store::{BLOCK, Backend, ENOSPC, Store, Tag};
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

const ENOENT: i32 = 2;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const ENOTEMPTY: i32 = 39;
const EPERM: i32 = 1;
const HOLE: u32 = u32::MAX;
/// The root's inode, as FUSE fixes it.
pub const ROOT: u64 = 1;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFMT: u32 = 0o170_000;

/// A time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Time(pub u64, pub u32);

impl Time {
    fn now() -> Self {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self(t.as_secs(), t.subsec_nanos())
    }
}

/// What a stat says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    /// Inode number.
    pub ino: u64,
    /// Length.
    pub size: u64,
    /// In 512-byte units.
    pub blocks: u64,
    /// Modified.
    pub mtime: Time,
    /// Changed.
    pub ctime: Time,
    /// Type and permissions.
    pub mode: u32,
    /// Names it has.
    pub nlink: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
}

enum Body {
    File { blocks: Vec<u32>, size: u64 },
    Dir(BTreeMap<Vec<u8>, u64>),
    Link(Vec<u8>),
}

struct Inode {
    body: Body,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: Time,
    ctime: Time,
    /// The kernel's references ([`Tree::forget`]) and open files.
    held: u64,
}

/// A directory entry: next offset, inode, stat mode, name.
pub type DirEntry = (u64, u64, u32, Vec<u8>);

/// A change `setattr` asks for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Change {
    /// New permission bits.
    pub mode: Option<u32>,
    /// New length.
    pub size: Option<u64>,
    /// New modification time.
    pub mtime: Option<Time>,
    /// Set the modification time to now.
    pub mtime_now: bool,
    /// New owner.
    pub uid: Option<u32>,
    /// New group.
    pub gid: Option<u32>,
}

/// The file system.
pub struct Tree<B> {
    /// The blocks.
    pub store: Store<B>,
    inodes: HashMap<u64, Inode>,
    next: u64,
    scratch: Vec<u8>,
    /// The kernel keeps regular files' times itself (FUSE's writeback cache) and
    /// sends them by `setattr`. A write arriving after that must not move the time
    /// forward: an inode evicted later would show a target newer than it is, and
    /// cargo would take a changed source for an older one.
    pub kernel_times: bool,
}

impl<B> std::fmt::Debug for Tree<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("inodes", &self.inodes.len())
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl<B: Backend> Tree<B> {
    /// An empty tree owned by `uid`.
    #[must_use]
    pub fn new(store: Store<B>, uid: u32, gid: u32) -> Self {
        let mut inodes = HashMap::new();
        let now = Time::now();
        inodes.insert(
            ROOT,
            Inode {
                body: Body::Dir(BTreeMap::new()),
                mode: S_IFDIR | 0o755,
                uid,
                gid,
                nlink: 2,
                mtime: now,
                ctime: now,
                held: 1,
            },
        );
        Self {
            store,
            inodes,
            next: ROOT + 1,
            scratch: vec![0; BLOCK],
            kernel_times: false,
        }
    }

    fn node(&self, ino: u64) -> Result<&Inode, i32> {
        self.inodes.get(&ino).ok_or(ENOENT)
    }
    fn node_mut(&mut self, ino: u64) -> Result<&mut Inode, i32> {
        self.inodes.get_mut(&ino).ok_or(ENOENT)
    }
    fn entries(&self, ino: u64) -> Result<&BTreeMap<Vec<u8>, u64>, i32> {
        match &self.node(ino)?.body {
            Body::Dir(entries) => Ok(entries),
            _ => Err(ENOTDIR),
        }
    }
    fn entries_mut(&mut self, ino: u64) -> Result<&mut BTreeMap<Vec<u8>, u64>, i32> {
        match &mut self.node_mut(ino)?.body {
            Body::Dir(entries) => Ok(entries),
            _ => Err(ENOTDIR),
        }
    }
    fn touch(&mut self, ino: u64) {
        let now = Time::now();
        if let Some(node) = self.inodes.get_mut(&ino) {
            node.mtime = now;
            node.ctime = now;
        }
    }

    /// What a stat of `ino` says.
    ///
    /// # Errors
    /// `ENOENT`.
    pub fn attr(&self, ino: u64) -> Result<Attr, i32> {
        let node = self.node(ino)?;
        let (size, used) = match &node.body {
            Body::File { blocks, size } => {
                (*size, blocks.iter().filter(|&&b| b != HOLE).count() as u64)
            }
            Body::Dir(_) => (4096, 0),
            Body::Link(target) => (target.len() as u64, 0),
        };
        Ok(Attr {
            ino,
            size,
            blocks: used * (BLOCK as u64 / 512),
            mtime: node.mtime,
            ctime: node.ctime,
            mode: node.mode,
            nlink: node.nlink,
            uid: node.uid,
            gid: node.gid,
        })
    }

    /// Find a name; the kernel now holds a reference.
    ///
    /// # Errors
    /// `ENOENT`, `ENOTDIR`.
    pub fn lookup(&mut self, parent: u64, name: &[u8]) -> Result<Attr, i32> {
        let ino = *self.entries(parent)?.get(name).ok_or(ENOENT)?;
        self.node_mut(ino)?.held += 1;
        self.attr(ino)
    }

    /// The kernel lets go of `count` references.
    pub fn forget(&mut self, ino: u64, count: u64) {
        if let Some(node) = self.inodes.get_mut(&ino) {
            node.held = node.held.saturating_sub(count);
        }
        self.reap(ino);
    }

    /// A file was opened; it stays until [`Self::release`], name or none.
    ///
    /// # Errors
    /// `ENOENT`.
    pub fn open(&mut self, ino: u64) -> Result<(), i32> {
        self.node_mut(ino)?.held += 1;
        Ok(())
    }

    /// A file was closed.
    pub fn release(&mut self, ino: u64) {
        self.forget(ino, 1);
    }

    fn reap(&mut self, ino: u64) {
        let gone = ino != ROOT
            && self
                .inodes
                .get(&ino)
                .is_some_and(|n| n.held == 0 && n.nlink == 0);
        if gone
            && let Some(node) = self.inodes.remove(&ino)
            && let Body::File { blocks, .. } = node.body
        {
            for block in blocks {
                if block != HOLE {
                    self.store.give(block);
                }
            }
        }
    }

    fn make(
        &mut self,
        parent: u64,
        name: &[u8],
        body: Body,
        mode: u32,
        who: (u32, u32),
    ) -> Result<Attr, i32> {
        if self.entries(parent)?.contains_key(name) {
            return Err(EEXIST);
        }
        let ino = self.next;
        self.next += 1;
        let now = Time::now();
        let nlink = if matches!(body, Body::Dir(_)) { 2 } else { 1 };
        self.inodes.insert(
            ino,
            Inode {
                body,
                mode,
                uid: who.0,
                gid: who.1,
                nlink,
                mtime: now,
                ctime: now,
                held: 1,
            },
        );
        self.entries_mut(parent)?.insert(name.to_vec(), ino);
        if nlink == 2 {
            self.node_mut(parent)?.nlink += 1;
        }
        self.touch(parent);
        self.attr(ino)
    }

    /// A new regular file; the kernel holds a reference.
    ///
    /// # Errors
    /// `EEXIST`, `ENOENT`, `ENOTDIR`, `EPERM` for a device or a pipe.
    pub fn create(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u32,
        who: (u32, u32),
    ) -> Result<Attr, i32> {
        let kind = mode & S_IFMT;
        if kind != 0 && kind != S_IFREG {
            return Err(EPERM);
        }
        self.make(
            parent,
            name,
            Body::File {
                blocks: Vec::new(),
                size: 0,
            },
            S_IFREG | (mode & 0o7777),
            who,
        )
    }

    /// A new directory; the kernel holds a reference.
    ///
    /// # Errors
    /// As [`Self::create`].
    pub fn mkdir(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u32,
        who: (u32, u32),
    ) -> Result<Attr, i32> {
        self.make(
            parent,
            name,
            Body::Dir(BTreeMap::new()),
            S_IFDIR | (mode & 0o7777),
            who,
        )
    }

    /// A new symbolic link; the kernel holds a reference.
    ///
    /// # Errors
    /// As [`Self::create`].
    pub fn symlink(
        &mut self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        who: (u32, u32),
    ) -> Result<Attr, i32> {
        self.make(
            parent,
            name,
            Body::Link(target.to_vec()),
            S_IFLNK | 0o777,
            who,
        )
    }

    /// What a link points at.
    ///
    /// # Errors
    /// `ENOENT`, `EINVAL` for what is no link.
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>, i32> {
        match &self.node(ino)?.body {
            Body::Link(target) => Ok(target.clone()),
            _ => Err(EINVAL),
        }
    }

    /// One more name for a file; the kernel holds a reference.
    ///
    /// # Errors
    /// `EEXIST`, `ENOENT`, `ENOTDIR`, `EPERM` for a directory.
    pub fn link(&mut self, ino: u64, parent: u64, name: &[u8]) -> Result<Attr, i32> {
        if matches!(self.node(ino)?.body, Body::Dir(_)) {
            return Err(EPERM);
        }
        if self.entries(parent)?.contains_key(name) {
            return Err(EEXIST);
        }
        self.entries_mut(parent)?.insert(name.to_vec(), ino);
        let node = self.node_mut(ino)?;
        node.nlink += 1;
        node.held += 1;
        node.ctime = Time::now();
        self.touch(parent);
        self.attr(ino)
    }

    fn drop_name(&mut self, ino: u64) {
        if let Some(node) = self.inodes.get_mut(&ino) {
            node.nlink = if matches!(node.body, Body::Dir(_)) {
                0
            } else {
                node.nlink.saturating_sub(1)
            };
            node.ctime = Time::now();
        }
        self.reap(ino);
    }

    /// Remove a file's name.
    ///
    /// # Errors
    /// `ENOENT`, `ENOTDIR`, `EISDIR`.
    pub fn unlink(&mut self, parent: u64, name: &[u8]) -> Result<(), i32> {
        let ino = *self.entries(parent)?.get(name).ok_or(ENOENT)?;
        if matches!(self.node(ino)?.body, Body::Dir(_)) {
            return Err(EISDIR);
        }
        self.entries_mut(parent)?.remove(name);
        self.touch(parent);
        self.drop_name(ino);
        Ok(())
    }

    /// Remove an empty directory.
    ///
    /// # Errors
    /// `ENOENT`, `ENOTDIR`, `ENOTEMPTY`.
    pub fn rmdir(&mut self, parent: u64, name: &[u8]) -> Result<(), i32> {
        let ino = *self.entries(parent)?.get(name).ok_or(ENOENT)?;
        if !self.entries(ino)?.is_empty() {
            return Err(ENOTEMPTY);
        }
        self.entries_mut(parent)?.remove(name);
        self.node_mut(parent)?.nlink -= 1;
        self.touch(parent);
        self.drop_name(ino);
        Ok(())
    }

    /// Move a name, replacing what is there; `no_replace` refuses that.
    ///
    /// # Errors
    /// `ENOENT`, `ENOTDIR`, `EEXIST`, `ENOTEMPTY`, `EISDIR`.
    pub fn rename(
        &mut self,
        from: u64,
        name: &[u8],
        to: u64,
        new: &[u8],
        no_replace: bool,
    ) -> Result<(), i32> {
        let ino = *self.entries(from)?.get(name).ok_or(ENOENT)?;
        let moving_dir = matches!(self.node(ino)?.body, Body::Dir(_));
        if let Some(&old) = self.entries(to)?.get(new) {
            if no_replace {
                return Err(EEXIST);
            }
            if old == ino {
                return Ok(());
            }
            match (&self.node(old)?.body, moving_dir) {
                (Body::Dir(entries), true) if !entries.is_empty() => return Err(ENOTEMPTY),
                (Body::Dir(_), false) => return Err(EISDIR),
                (Body::File { .. } | Body::Link(_), true) => return Err(ENOTDIR),
                _ => {}
            }
            let old_is_dir = matches!(self.node(old)?.body, Body::Dir(_));
            self.entries_mut(to)?.remove(new);
            if old_is_dir {
                self.node_mut(to)?.nlink -= 1;
            }
            self.drop_name(old);
        }
        self.entries_mut(from)?.remove(name);
        self.entries_mut(to)?.insert(new.to_vec(), ino);
        if moving_dir && from != to {
            self.node_mut(from)?.nlink -= 1;
            self.node_mut(to)?.nlink += 1;
        }
        self.touch(from);
        self.touch(to);
        self.node_mut(ino)?.ctime = Time::now();
        Ok(())
    }

    /// A directory's entries from `offset` on.
    ///
    /// # Errors
    /// `ENOENT`, `ENOTDIR`.
    pub fn readdir(&self, ino: u64, offset: u64) -> Result<Vec<DirEntry>, i32> {
        let entries = self.entries(ino)?;
        let mut out = Vec::new();
        let dots = [(b".".to_vec(), ino), (b"..".to_vec(), ino)];
        let all = dots
            .iter()
            .map(|(n, i)| (n, *i))
            .chain(entries.iter().map(|(n, i)| (n, *i)));
        for (at, (name, child)) in all
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            let mode = self.node(child).map_or(0, |n| n.mode);
            out.push((at as u64 + 1, child, mode, name.clone()));
        }
        Ok(out)
    }

    /// Apply a `setattr`.
    ///
    /// # Errors
    /// `ENOENT`, `EIO`, `ENOSPC`, `EISDIR` for the length of a directory.
    pub fn change(&mut self, ino: u64, change: Change) -> Result<Attr, i32> {
        if let Some(size) = change.size {
            self.truncate(ino, size)?;
        }
        let now = Time::now();
        let node = self.node_mut(ino)?;
        if let Some(mode) = change.mode {
            node.mode = (node.mode & S_IFMT) | (mode & 0o7777);
        }
        if let Some(uid) = change.uid {
            node.uid = uid;
        }
        if let Some(gid) = change.gid {
            node.gid = gid;
        }
        if let Some(mtime) = change.mtime {
            node.mtime = mtime;
        }
        if change.mtime_now {
            node.mtime = now;
        }
        node.ctime = now;
        self.attr(ino)
    }

    fn truncate(&mut self, ino: u64, size: u64) -> Result<(), i32> {
        let want = usize::try_from(size.div_ceil(BLOCK as u64)).map_err(|_| EINVAL)?;
        let tail = (size % BLOCK as u64) as usize;
        let Body::File { blocks, size: old } = &mut self.node_mut(ino)?.body else {
            return Err(EISDIR);
        };
        let shrinking = size < *old;
        let last = if shrinking && tail != 0 {
            blocks.get(want - 1).copied()
        } else {
            None
        };
        let freed: Vec<u32> = if blocks.len() > want {
            blocks.split_off(want)
        } else {
            Vec::new()
        };
        blocks.resize(want, HOLE);
        *old = size;
        for block in freed {
            if block != HOLE {
                self.store.give(block);
            }
        }
        // What was past the new end must read as zeros if the file grows again.
        if let Some(block) = last.filter(|&b| b != HOLE) {
            let tag = Tag {
                ino,
                index: want as u64 - 1,
            };
            let mut buffer = std::mem::take(&mut self.scratch);
            let done = self.store.read(block, tag, &mut buffer).and_then(|()| {
                buffer[tail..].fill(0);
                self.store.write(block, tag, &buffer)
            });
            self.scratch = buffer;
            done?;
        }
        if !self.kernel_times {
            self.touch(ino);
        }
        Ok(())
    }

    /// Read up to `length` bytes at `offset`, appended to `to`.
    ///
    /// # Errors
    /// `ENOENT`, `EISDIR`, `EIO`.
    pub fn read(
        &mut self,
        ino: u64,
        offset: u64,
        length: usize,
        to: &mut Vec<u8>,
    ) -> Result<(), i32> {
        let Body::File { blocks, size } = &self.node(ino)?.body else {
            return Err(EISDIR);
        };
        let size = *size;
        let first = offset / BLOCK as u64;
        let list: Vec<u32> = blocks
            .iter()
            .skip(usize::try_from(first).unwrap_or(usize::MAX))
            .take(length / BLOCK + 2)
            .copied()
            .collect();
        if offset >= size {
            return Ok(());
        }
        let mut left = usize::try_from((size - offset).min(length as u64)).unwrap_or(length);
        let mut within = (offset % BLOCK as u64) as usize;
        let mut buffer = std::mem::take(&mut self.scratch);
        let mut result = Ok(());
        for (index, block) in (first..).zip(list) {
            if left == 0 {
                break;
            }
            let here = left.min(BLOCK - within);
            if block == HOLE {
                to.resize(to.len() + here, 0);
            } else {
                result = self.store.read(block, Tag { ino, index }, &mut buffer);
                if result.is_err() {
                    break;
                }
                to.extend_from_slice(&buffer[within..within + here]);
            }
            left -= here;
            within = 0;
        }
        self.scratch = buffer;
        result?;
        // Past the last block but inside the file: a hole.
        to.resize(to.len() + left, 0);
        Ok(())
    }

    /// Write `data` at `offset`.
    ///
    /// # Errors
    /// `ENOENT`, `EISDIR`, `EIO`, `ENOSPC`.
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, i32> {
        let mut buffer = std::mem::take(&mut self.scratch);
        let result = self.write_with(ino, offset, data, &mut buffer);
        self.scratch = buffer;
        let written = match result {
            Ok(n) => n,
            Err((0, error)) => return Err(error),
            Err((n, error)) if error == ENOSPC => n,
            Err((_, error)) => return Err(error),
        };
        if let Body::File { size, .. } = &mut self.node_mut(ino)?.body {
            *size = (*size).max(offset + written as u64);
        }
        if !self.kernel_times {
            self.touch(ino);
        }
        Ok(written)
    }

    fn write_with(
        &mut self,
        ino: u64,
        offset: u64,
        data: &[u8],
        buffer: &mut [u8],
    ) -> Result<usize, (usize, i32)> {
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let index = usize::try_from(at / BLOCK as u64).map_err(|_| (done, EINVAL))?;
            let within = (at % BLOCK as u64) as usize;
            let here = (BLOCK - within).min(data.len() - done);
            let tag = Tag {
                ino,
                index: index as u64,
            };
            let Some(Inode {
                body: Body::File { blocks, .. },
                ..
            }) = self.inodes.get_mut(&ino)
            else {
                return Err((done, EISDIR));
            };
            if blocks.len() <= index {
                blocks.resize(index + 1, HOLE);
            }
            let held = blocks[index];
            let block = if held == HOLE {
                buffer.fill(0);
                self.store.take().map_err(|e| (done, e))?
            } else {
                if here < BLOCK {
                    self.store.read(held, tag, buffer).map_err(|e| (done, e))?;
                }
                held
            };
            if held == HOLE
                && let Some(Inode {
                    body: Body::File { blocks, .. },
                    ..
                }) = self.inodes.get_mut(&ino)
            {
                blocks[index] = block;
            }
            buffer[within..within + here].copy_from_slice(&data[done..done + here]);
            self.store
                .write(block, tag, buffer)
                .map_err(|e| (done, e))?;
            done += here;
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EIO, Ram};

    const WHO: (u32, u32) = (1000, 1000);

    fn tree(blocks: u32) -> Tree<Ram> {
        Tree::new(
            Store::new(Ram(vec![0; blocks as usize * BLOCK]), blocks),
            WHO.0,
            WHO.1,
        )
    }
    fn read_all(t: &mut Tree<Ram>, ino: u64) -> Vec<u8> {
        let size = t.attr(ino).unwrap().size as usize;
        let mut out = Vec::new();
        t.read(ino, 0, size, &mut out).unwrap();
        out
    }

    #[test]
    fn what_is_written_reads_back_across_blocks_and_at_odd_offsets() {
        let mut t = tree(8);
        let f = t.create(ROOT, b"f", 0o644, WHO).unwrap().ino;
        let data: Vec<u8> = (0..3 * BLOCK + 17).map(|i| (i % 251) as u8).collect();
        assert_eq!(t.write(f, 0, &data).unwrap(), data.len());
        assert_eq!(read_all(&mut t, f), data);
        t.write(f, BLOCK as u64 - 2, b"abcd").unwrap();
        let mut out = Vec::new();
        t.read(f, BLOCK as u64 - 3, 6, &mut out).unwrap();
        assert_eq!(out[1..5], *b"abcd");
        assert_eq!(out[0], data[BLOCK - 3]);
    }

    #[test]
    fn a_hole_reads_as_zeros_and_holds_no_block() {
        let mut t = tree(4);
        let f = t.create(ROOT, b"f", 0o644, WHO).unwrap().ino;
        t.write(f, 2 * BLOCK as u64 + 1, b"x").unwrap();
        assert_eq!(t.store.free_blocks(), 3);
        let all = read_all(&mut t, f);
        assert_eq!(all.len(), 2 * BLOCK + 2);
        assert!(all[..2 * BLOCK + 1].iter().all(|&b| b == 0));
        assert_eq!(all[2 * BLOCK + 1], b'x');
        // A truncate past the end grows a hole too.
        t.change(
            f,
            Change {
                size: Some(5 * BLOCK as u64),
                ..Change::default()
            },
        )
        .unwrap();
        assert_eq!(read_all(&mut t, f).len(), 5 * BLOCK);
    }

    #[test]
    fn a_truncated_tail_does_not_come_back_when_the_file_grows() {
        let mut t = tree(4);
        let f = t.create(ROOT, b"f", 0o644, WHO).unwrap().ino;
        t.write(f, 0, &vec![9; 2 * BLOCK]).unwrap();
        t.change(
            f,
            Change {
                size: Some(10),
                ..Change::default()
            },
        )
        .unwrap();
        assert_eq!(t.store.free_blocks(), 3);
        t.change(
            f,
            Change {
                size: Some(BLOCK as u64),
                ..Change::default()
            },
        )
        .unwrap();
        let all = read_all(&mut t, f);
        assert_eq!(all[..10], [9; 10]);
        assert!(all[10..].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_file_unlinked_while_open_keeps_its_blocks_until_the_last_reference_goes() {
        let mut t = tree(2);
        let f = t.create(ROOT, b"f", 0o644, WHO).unwrap().ino;
        t.open(f).unwrap();
        t.write(f, 0, &[1; 10]).unwrap();
        t.unlink(ROOT, b"f").unwrap();
        assert_eq!(read_all(&mut t, f), [1; 10]);
        assert_eq!(t.store.free_blocks(), 1);
        t.release(f);
        assert_eq!(
            t.store.free_blocks(),
            1,
            "the kernel's lookup still holds it"
        );
        t.forget(f, 1);
        assert_eq!(t.store.free_blocks(), 2);
        assert_eq!(t.attr(f), Err(ENOENT));
    }

    #[test]
    fn hard_links_share_blocks_and_the_last_name_frees_them() {
        let mut t = tree(2);
        let f = t.create(ROOT, b"a", 0o644, WHO).unwrap().ino;
        t.write(f, 0, b"data").unwrap();
        assert_eq!(t.link(f, ROOT, b"b").unwrap().nlink, 2);
        t.unlink(ROOT, b"a").unwrap();
        t.forget(f, 2);
        assert_eq!(read_all(&mut t, f), b"data");
        t.lookup(ROOT, b"b").unwrap();
        t.unlink(ROOT, b"b").unwrap();
        t.forget(f, 1);
        assert_eq!(t.store.free_blocks(), 2);
    }

    #[test]
    fn rename_replaces_a_file_and_refuses_what_it_must() {
        let mut t = tree(4);
        let a = t.create(ROOT, b"a", 0o644, WHO).unwrap().ino;
        let b = t.create(ROOT, b"b", 0o644, WHO).unwrap().ino;
        t.write(b, 0, b"old").unwrap();
        t.forget(b, 1);
        assert_eq!(t.rename(ROOT, b"a", ROOT, b"b", true), Err(EEXIST));
        t.rename(ROOT, b"a", ROOT, b"b", false).unwrap();
        assert_eq!(t.lookup(ROOT, b"b").unwrap().ino, a);
        assert_eq!(t.lookup(ROOT, b"a"), Err(ENOENT));
        assert_eq!(
            t.store.free_blocks(),
            4,
            "the file replaced gave its block back"
        );
        let d = t.mkdir(ROOT, b"d", 0o755, WHO).unwrap().ino;
        t.create(d, b"inside", 0o644, WHO).unwrap();
        assert_eq!(t.rename(ROOT, b"b", ROOT, b"d", false), Err(EISDIR));
        assert_eq!(t.rmdir(ROOT, b"d"), Err(ENOTEMPTY));
        assert_eq!(t.attr(ROOT).unwrap().nlink, 3);
        let e = t.mkdir(ROOT, b"e", 0o755, WHO).unwrap().ino;
        t.rename(ROOT, b"d", e, b"d", false).unwrap();
        assert_eq!(t.attr(ROOT).unwrap().nlink, 3);
        assert_eq!(t.attr(e).unwrap().nlink, 3);
    }

    #[test]
    fn a_short_write_when_full_says_how_much_and_nothing_at_all_says_enospc() {
        let mut t = tree(1);
        let f = t.create(ROOT, b"f", 0o644, WHO).unwrap().ino;
        assert_eq!(t.write(f, 0, &vec![1; 2 * BLOCK]), Ok(BLOCK));
        assert_eq!(t.attr(f).unwrap().size, BLOCK as u64);
        assert_eq!(t.write(f, BLOCK as u64, b"x"), Err(ENOSPC));
        assert_eq!(t.store.health.no_space, 2);
    }

    #[test]
    fn a_read_of_a_corrupted_block_is_an_error_never_wrong_bytes() {
        let mut t = tree(1);
        let f = t.create(ROOT, b"f", 0o644, WHO).unwrap().ino;
        t.write(f, 0, b"data").unwrap();
        t.store.backend_mut().0[1] ^= 4;
        let mut out = Vec::new();
        assert_eq!(t.read(f, 0, 4, &mut out), Err(EIO));
        assert!(out.is_empty());
        assert!(t.store.is_dead());
    }

    #[test]
    fn readdir_goes_on_from_its_offset() {
        let mut t = tree(1);
        t.create(ROOT, b"a", 0o644, WHO).unwrap();
        t.mkdir(ROOT, b"b", 0o755, WHO).unwrap();
        let all = t.readdir(ROOT, 0).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[2].3, b"a");
        assert_eq!(t.readdir(ROOT, all[2].0).unwrap()[0].3, b"b");
        assert!(t.readdir(ROOT, 4).unwrap().is_empty());
    }

    #[test]
    fn with_the_kernel_keeping_times_a_late_write_does_not_move_them() {
        let mut t = tree(2);
        let f = t.create(ROOT, b"f", 0o644, WHO).unwrap().ino;
        t.kernel_times = true;
        let stamp = Time(1_000, 5);
        t.change(
            f,
            Change {
                mtime: Some(stamp),
                ..Change::default()
            },
        )
        .unwrap();
        t.write(f, 0, b"written back later").unwrap();
        assert_eq!(t.attr(f).unwrap().mtime, stamp);
        assert_eq!(t.attr(f).unwrap().size, 18);
        // A truncate too, when it carries no time of its own.
        t.change(
            f,
            Change {
                size: Some(4),
                ..Change::default()
            },
        )
        .unwrap();
        assert_eq!(t.attr(f).unwrap().mtime, stamp);
        // Without that mode a write is what stamps the file.
        t.kernel_times = false;
        t.write(f, 0, b"x").unwrap();
        assert_ne!(t.attr(f).unwrap().mtime, stamp);
    }

    #[test]
    fn a_link_says_where_it_points() {
        let mut t = tree(1);
        let l = t.symlink(ROOT, b"l", b"target", WHO).unwrap();
        assert_eq!(l.size, 6);
        assert_eq!(t.readlink(l.ino).unwrap(), b"target");
        assert_eq!(t.readlink(ROOT), Err(EINVAL));
    }
}
