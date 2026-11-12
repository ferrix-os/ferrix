//! Where an `AF_UNIX` socket's name lives, and how `connect` finds one.
//!
//! A `bind` gives a socket a name, and a `connect` to that name has to arrive
//! at that socket. Linux keeps the two halves in different places and so does
//! this:
//!
//! * **A pathname** is a node in the filesystem. `bind` creates an `S_IFSOCK`
//!   node at the path, exactly as `mknod` would, and a table here maps that
//!   node -- its filesystem's device number and its inode number -- to the
//!   socket. `connect` walks the path like any other, which is what makes the
//!   permissions on the directories above it mean something, and then asks the
//!   table. A program that binds a name that is already a file gets
//!   `EADDRINUSE`, which is why every program that binds one unlinks it first.
//!
//! * **An abstract name** -- a `sun_path` that starts with a NUL -- is in no
//!   filesystem at all. It is a flat namespace of its own, kept here as a map
//!   from the bytes after that NUL to the socket, and it goes when the socket
//!   does.
//!
//! # The node outlives the socket, and that is Linux's behaviour
//!
//! Dropping a socket takes it out of both tables, so a `connect` afterwards
//! finds nothing. For an abstract name that is the whole story: the name is
//! free again. For a pathname the *node* stays, because Linux does not unlink
//! it either -- a socket file left behind by a program that died is why
//! `unlink` before `bind` is the universal idiom -- so a `connect` to it gets
//! `ECONNREFUSED` rather than `ENOENT`. The two answers are different on
//! purpose: `ENOENT` says nobody ever bound this, `ECONNREFUSED` says somebody
//! did and is gone.
//!
//! # The tables hold weak references
//!
//! A table that held a socket alive would keep every bound socket for the life
//! of the machine, and a `Drop` that removes the entry would never run. So
//! each entry is a `Weak`, an entry whose socket has gone counts as no entry
//! at all, and `Drop` removes the entry it knows about by name rather than
//! sweeping.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use ferrix_vfs::{Context, Errno, FileType, Location, NewNode};

use crate::fs;
use crate::fs::socket::Socket;
use crate::sync::SpinLock;

/// A name a socket is bound to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Name {
    /// A filesystem path, and the node the bind made for it: the path is what
    /// `getsockname` answers with, and the node is what the table is keyed by,
    /// because two paths can name one node and a path can come to name
    /// another.
    Path {
        /// What the program passed to `bind`, as it passed it.
        path: Vec<u8>,
        /// The node: its filesystem's device number and its inode number.
        node: (u64, u64),
    },
    /// An abstract name, without its leading NUL.
    Abstract(Vec<u8>),
}

/// Sockets bound to a path, by the node the bind made.
static PATHS: SpinLock<BTreeMap<(u64, u64), Weak<Socket>>> = SpinLock::new(BTreeMap::new());

/// Sockets bound to an abstract name, by the network namespace they were made
/// in and the name: a name is unique within a network namespace and not
/// beyond it, as on Linux (`docs/NETNS.md`, rule NN11).
static ABSTRACT: SpinLock<BTreeMap<AbstractKey, Weak<Socket>>> = SpinLock::new(BTreeMap::new());

/// A network namespace's number and the name.
type AbstractKey = (u64, Vec<u8>);

/// Bind `socket` to `path`, answering the name it now has.
///
/// # Errors
///
/// `EADDRINUSE` if the name is taken -- by a bound socket or by any other
/// file, which is what `mknod` refusing with `EEXIST` means here -- and
/// whatever the walk to the path refuses.
pub(crate) fn bind_path(
    socket: &Arc<Socket>,
    ctx: &Context,
    start: Option<&Location>,
    path: &[u8],
) -> Result<Name, Errno> {
    // The node the bind makes is the name: made here rather than looked up
    // afterwards, so that what goes in the table is what this call created and
    // not whatever is at the path a moment later.
    let made = fs::namespace()
        .mknod_at(ctx, start, path, NewNode::Socket, 0o777)
        .map_err(|error| match error {
            Errno::EEXIST => Errno::EADDRINUSE,
            other => other,
        })?;
    let node = node_of(&made)?;
    let _ = PATHS.lock().insert(node, Arc::downgrade(socket));
    Ok(Name::Path {
        path: path.to_vec(),
        node,
    })
}

/// Bind `socket` to an abstract name, answering the name it now has.
///
/// # Errors
///
/// `EADDRINUSE` if a live socket already holds it.
pub(crate) fn bind_abstract(
    socket: &Arc<Socket>,
    namespace: u64,
    name: &[u8],
) -> Result<Name, Errno> {
    let mut held = ABSTRACT.lock();
    let key = (namespace, name.to_vec());
    // A name whose socket has gone is a free name: the entry is left behind by
    // a `Drop` that has not run yet, or by one that ran after a rebind.
    if held.get(&key).is_some_and(|weak| weak.strong_count() != 0) {
        return Err(Errno::EADDRINUSE);
    }
    let _ = held.insert(key, Arc::downgrade(socket));
    Ok(Name::Abstract(name.to_vec()))
}

/// The socket bound to `path`.
///
/// # Errors
///
/// `ENOENT` if nothing is there, `ECONNREFUSED` if what is there is a socket
/// nobody is bound to any more, and `ECONNREFUSED` if it is not a socket at
/// all -- which is Linux's answer, because a `connect` to a regular file is a
/// connection that was refused and not a path that is missing.
pub(crate) fn socket_at(
    ctx: &Context,
    start: Option<&Location>,
    path: &[u8],
) -> Result<Arc<Socket>, Errno> {
    let found = fs::namespace().resolve(ctx, start, path, true)?;
    if found.inode()?.metadata().kind != FileType::Socket {
        return Err(Errno::ECONNREFUSED);
    }
    let node = node_of(&found)?;
    PATHS
        .lock()
        .get(&node)
        .and_then(Weak::upgrade)
        .ok_or(Errno::ECONNREFUSED)
}

/// The socket bound to an abstract name.
///
/// # Errors
///
/// `ECONNREFUSED` if nobody holds it, which is what Linux answers: an
/// abstract name that is not bound is not a path that does not exist.
pub(crate) fn socket_named(namespace: u64, name: &[u8]) -> Result<Arc<Socket>, Errno> {
    ABSTRACT
        .lock()
        .get(&(namespace, name.to_vec()))
        .and_then(Weak::upgrade)
        .ok_or(Errno::ECONNREFUSED)
}

/// Let go of a name, when the socket that held it goes.
pub(crate) fn forget(name: &Name, namespace: u64) {
    match name {
        Name::Path { node, .. } => {
            let _ = PATHS.lock().remove(node);
        }
        Name::Abstract(bytes) => {
            let _ = ABSTRACT.lock().remove(&(namespace, bytes.clone()));
        }
    }
}

/// The device and inode numbers a location names.
fn node_of(at: &Location) -> Result<(u64, u64), Errno> {
    let stat = fs::namespace().stat(at)?;
    Ok((stat.dev, stat.metadata.ino))
}
