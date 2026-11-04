//! The server's side of a volume's channel: a table of nodes, and each request
//! answered against it. Generic over `FileSystem`, so that a host test and the
//! fuzz target run it over a tmpfs, and `btrfsd` over btrfs.
//!
//! Hostile input by design (`docs/microkernel/BTRFSD.md`, T.CLIENT): the bytes on
//! the channel are checked by [`Request::decode`], and the slot's are copied into
//! this side's memory before anything reads them (BD.5).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_vfs::{FileSystem, FileType, Inode, NewNode};

use crate::wire::{
    Answer, FIRST_NODE, MAX_NODES, Op, Request, SET_ATTR_BYTES, SLOT_BYTES, decode_set_attr,
    encode_dirent, encode_statfs,
};

/// The shared VMO, read and written only by copying.
pub trait Slot {
    /// Copy `out.len()` bytes from `offset`. False past its end.
    fn read(&self, offset: usize, out: &mut [u8]) -> bool;
    /// Copy `bytes` to `offset`. False past its end.
    fn write(&mut self, offset: usize, bytes: &[u8]) -> bool;
}

/// One channel's view of one volume.
#[derive(Debug)]
pub struct Volume {
    filesystem: Arc<dyn FileSystem>,
    nodes: Vec<Option<Arc<dyn Inode>>>,
    free: Vec<u32>,
    scratch: Box<[u8]>,
}

impl Volume {
    /// The channel's start: node [`FIRST_NODE`] is the volume's root.
    #[must_use]
    pub fn new(filesystem: Arc<dyn FileSystem>) -> Volume {
        let root = filesystem.root();
        Volume {
            filesystem,
            nodes: vec![None, Some(root)],
            free: Vec::new(),
            scratch: vec![0; SLOT_BYTES].into_boxed_slice(),
        }
    }

    /// The root's answer, for the channel's opening.
    #[must_use]
    pub fn root_answer(&self) -> Answer {
        Answer {
            node: FIRST_NODE,
            attr: self.node(FIRST_NODE).ok().map(|root| root.metadata()),
            ..Answer::OK
        }
    }

    fn node(&self, node: u32) -> Result<&Arc<dyn Inode>, Errno> {
        self.nodes
            .get(node as usize)
            .and_then(Option::as_ref)
            .ok_or(Errno::EBADF)
    }

    fn insert(&mut self, inode: Arc<dyn Inode>) -> Result<u32, Errno> {
        if let Some(free) = self.free.pop() {
            if let Some(slot) = self.nodes.get_mut(free as usize) {
                *slot = Some(inode);
                return Ok(free);
            }
            return Err(Errno::EIO);
        }
        if self.nodes.len() >= MAX_NODES {
            return Err(Errno::ENFILE);
        }
        let index = u32::try_from(self.nodes.len()).map_err(|_| Errno::ENFILE)?;
        self.nodes.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        self.free
            .try_reserve(self.nodes.len() + 1)
            .map_err(|_| Errno::ENOMEM)?;
        self.nodes.push(Some(inode));
        Ok(index)
    }

    fn answer_node(&mut self, inode: Arc<dyn Inode>) -> Result<Answer, Errno> {
        let attr = inode.metadata();
        let node = self.insert(inode)?;
        Ok(Answer {
            node,
            attr: Some(attr),
            ..Answer::OK
        })
    }

    /// Answer the request in `bytes`. Never panics, whatever they are and
    /// whatever the slot holds.
    pub fn serve(&mut self, bytes: &[u8], slot: &mut dyn Slot) -> Answer {
        let Ok(request) = Request::decode(bytes) else {
            return Answer::refused(Errno::EINVAL);
        };
        match self.answer(&request, slot) {
            Ok(answer) => answer,
            Err(errno) => Answer::refused(errno),
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per op of the protocol, each a few lines: read in one place"
    )]
    fn answer(&mut self, request: &Request<'_>, slot: &mut dyn Slot) -> Result<Answer, Errno> {
        let node = request.node;
        let named = |request: &Request<'_>| {
            if request.name.is_empty() {
                Err(Errno::EINVAL)
            } else {
                Ok(())
            }
        };
        match request.op {
            Op::Lookup => {
                named(request)?;
                let found = self.node(node)?.lookup(request.name)?;
                self.answer_node(found)
            }
            Op::Forget => {
                // The root is the channel's; it lives as long as it.
                if node == FIRST_NODE {
                    return Err(Errno::EINVAL);
                }
                let _ = self.node(node)?;
                if let Some(entry) = self.nodes.get_mut(node as usize) {
                    *entry = None;
                }
                self.free.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
                self.free.push(node);
                Ok(Answer::OK)
            }
            Op::GetAttr => Ok(Answer {
                attr: Some(self.node(node)?.metadata()),
                ..Answer::OK
            }),
            Op::SetAttr => {
                let mut form = [0; SET_ATTR_BYTES];
                if !slot.read(0, &mut form) {
                    return Err(Errno::EINVAL);
                }
                let change = decode_set_attr(&form).ok_or(Errno::EINVAL)?;
                let target = self.node(node)?;
                target.set_attributes(&change)?;
                Ok(Answer {
                    attr: Some(target.metadata()),
                    ..Answer::OK
                })
            }
            Op::Read => {
                let length = bounded(request.b)?;
                let target = Arc::clone(self.node(node)?);
                let room = self.scratch.get_mut(..length).ok_or(Errno::EINVAL)?;
                let got = target.read_at(request.a, room)?;
                let bytes = room.get(..got).ok_or(Errno::EIO)?;
                if !slot.write(0, bytes) {
                    return Err(Errno::EINVAL);
                }
                Ok(Answer::value(got as u64))
            }
            Op::Write => {
                let length = bounded(request.b)?;
                let target = Arc::clone(self.node(node)?);
                let room = self.scratch.get_mut(..length).ok_or(Errno::EINVAL)?;
                if !slot.read(0, room) {
                    return Err(Errno::EINVAL);
                }
                let (written, end) = target.write_at(request.a, room, request.c == 1)?;
                Ok(Answer {
                    value: written as u64,
                    end,
                    attr: Some(target.metadata()),
                    ..Answer::OK
                })
            }
            Op::ReadDir => {
                let target = Arc::clone(self.node(node)?);
                let room = &mut *self.scratch;
                let mut used = 0_usize;
                target.read_dir(request.a, &mut |entry| match room.get_mut(used..).and_then(
                    |tail| encode_dirent(entry.ino, entry.next, entry.kind, entry.name, tail),
                ) {
                    Some(took) => {
                        used += took;
                        true
                    }
                    None => false,
                })?;
                let bytes = room.get(..used).ok_or(Errno::EIO)?;
                if !slot.write(0, bytes) {
                    return Err(Errno::EINVAL);
                }
                Ok(Answer::value(used as u64))
            }
            Op::Create => {
                named(request)?;
                let kind = FileType::from_mode(request.c).ok_or(Errno::EINVAL)?;
                let target_len = request.d as usize;
                if target_len > ferrix_vfs::path::PATH_MAX {
                    return Err(Errno::ENAMETOOLONG);
                }
                let mut link = Vec::new();
                link.try_reserve_exact(target_len)
                    .map_err(|_| Errno::ENOMEM)?;
                link.resize(target_len, 0);
                if !slot.read(0, &mut link) {
                    return Err(Errno::EINVAL);
                }
                let link: &[u8] = &link;
                let what = match kind {
                    FileType::Regular => NewNode::Regular,
                    FileType::Directory => NewNode::Directory,
                    FileType::Symlink => {
                        if link.is_empty() || link.contains(&0) {
                            return Err(Errno::EINVAL);
                        }
                        NewNode::Symlink(link)
                    }
                    FileType::CharDevice | FileType::BlockDevice => NewNode::Device {
                        kind,
                        rdev: request.b,
                    },
                    FileType::Fifo => NewNode::Fifo,
                    FileType::Socket => NewNode::Socket,
                };
                let permissions = u32::try_from(request.a).map_err(|_| Errno::EINVAL)?;
                if permissions > 0o7777 {
                    return Err(Errno::EINVAL);
                }
                let parent = Arc::clone(self.node(node)?);
                let made = parent.create(request.name, what, permissions)?;
                self.answer_node(made)
            }
            Op::Link => {
                named(request)?;
                let other = u32::try_from(request.a).map_err(|_| Errno::EBADF)?;
                let target = Arc::clone(self.node(other)?);
                self.node(node)?.link(request.name, &target)?;
                Ok(Answer {
                    attr: Some(target.metadata()),
                    ..Answer::OK
                })
            }
            Op::Unlink => {
                named(request)?;
                // What went, so that the client's copy of it says so.
                let parent = self.node(node)?;
                let gone = parent.lookup(request.name)?;
                parent.unlink(request.name)?;
                Ok(Answer {
                    attr: Some(gone.metadata()),
                    ..Answer::OK
                })
            }
            Op::Rmdir => {
                named(request)?;
                self.node(node)?.rmdir(request.name)?;
                Ok(Answer::OK)
            }
            Op::Rename => {
                named(request)?;
                if request.second.is_empty() {
                    return Err(Errno::EINVAL);
                }
                let other = u32::try_from(request.a).map_err(|_| Errno::EBADF)?;
                let target = Arc::clone(self.node(other)?);
                let replaced = target.lookup(request.second).ok();
                self.node(node)?
                    .rename(request.name, &target, request.second, request.c == 1)?;
                Ok(Answer {
                    attr: replaced.map(|gone| gone.metadata()),
                    ..Answer::OK
                })
            }
            Op::ReadLink => {
                let target = self.node(node)?.read_link()?;
                if !slot.write(0, &target) {
                    return Err(Errno::ENAMETOOLONG);
                }
                Ok(Answer::value(target.len() as u64))
            }
            Op::SetLen => {
                let target = self.node(node)?;
                target.set_len(request.a)?;
                Ok(Answer {
                    attr: Some(target.metadata()),
                    ..Answer::OK
                })
            }
            Op::GrowTo => {
                let target = self.node(node)?;
                target.grow_to(request.a)?;
                Ok(Answer {
                    attr: Some(target.metadata()),
                    ..Answer::OK
                })
            }
            Op::Fsync => {
                self.node(node)?.fsync(request.c == 1)?;
                Ok(Answer::OK)
            }
            Op::Sync => {
                self.filesystem.sync()?;
                Ok(Answer::OK)
            }
            Op::StatFs => {
                if !slot.write(0, &encode_statfs(&self.filesystem.statfs())) {
                    return Err(Errno::EINVAL);
                }
                Ok(Answer::OK)
            }
            // The control channel's; on a volume's they are nothing.
            Op::Hello
            | Op::Open
            | Op::Label
            | Op::BlockRead
            | Op::BlockWrite
            | Op::BlockFlush
            | Op::Close => Err(Errno::EINVAL),
        }
    }
}

/// A slot's length, refused past the slot.
fn bounded(length: u64) -> Result<usize, Errno> {
    usize::try_from(length)
        .ok()
        .filter(|&length| length <= SLOT_BYTES)
        .ok_or(Errno::EINVAL)
}
