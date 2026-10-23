//! `wl_shm`: the memory a client draws into.
//!
//! A client makes a file, maps it, and hands the compositor its descriptor
//! as a `wl_shm_pool`. Each `wl_buffer` is a rectangle cut from that pool,
//! with a format and a stride. The compositor above maps the same file and
//! reads the pixels when a surface commits the buffer, so nothing is copied
//! across the socket.
//!
//! # The memory outlives the pool
//!
//! Destroying a pool does not take its memory away while buffers cut from
//! it live: the protocol says so, and `grim` relies on it. So a destroyed
//! pool is reported as retired rather than gone, and
//! [`Client::pool_in_use`] is what the compositor asks before it lets the
//! mapping go.

use compositor_protocol::core::{self, wl_shm, wl_shm_pool};
use compositor_wire::{Arg, ObjectId};

use crate::client::{Client, Event, Fatal};
use crate::role::Role;
use crate::shm::{Buffer, Pool, PoolKey};

impl Client {
    /// `wl_shm`: `create_pool`.
    pub(super) fn shm(&mut self, opcode: u16, args: &[Arg<'_>]) {
        if opcode != wl_shm::request::CREATE_POOL {
            return;
        }
        let (Some(id), Some(fd), Some(size)) = (
            args.first().and_then(Arg::as_object),
            args.get(1).and_then(Arg::as_fd),
            args.get(2).and_then(Arg::as_int),
        ) else {
            return;
        };
        if size <= 0 {
            // wl_shm has no error for it, and libwayland's mmap of a
            // zero-length pool fails, which it answers with invalid_fd.
            self.fail(Fatal::Interface {
                object: id,
                code: wl_shm::error::INVALID_FD,
                text: format!("a pool of {size} bytes"),
            });
            return;
        }
        if !self.make(id, &core::WL_SHM_POOL, 1, Role::ShmPool) {
            return;
        }
        self.pools_made += 1;
        let memory = Pool::new(fd, size, PoolKey(self.pools_made));
        let _ = self.pools.insert(id, memory);
        self.events.push(Event::PoolCreated {
            pool: memory.key,
            memory,
        });
    }

    /// `wl_shm_pool`: `create_buffer` and `resize`.
    pub(super) fn shm_pool(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        match opcode {
            wl_shm_pool::request::CREATE_BUFFER => {
                let Some(id) = args.first().and_then(Arg::as_object) else {
                    return;
                };
                let numbers: Vec<i32> = (1..5)
                    .filter_map(|index| args.get(index).and_then(Arg::as_int))
                    .collect();
                let (Some(pool), [offset, width, height, stride], Some(format)) = (
                    self.pools.get(&sender),
                    numbers.as_slice(),
                    args.get(5).and_then(Arg::as_uint),
                ) else {
                    return;
                };
                match pool.buffer(pool.key, *offset, *width, *height, *stride, format) {
                    Ok(buffer) => {
                        if self.make(id, &core::WL_BUFFER, 1, Role::Buffer) {
                            let _ = self.buffers.insert(id, buffer);
                        }
                    }
                    Err(error) => self.fail(Fatal::Interface {
                        object: sender,
                        code: error.code(),
                        text: error.message(),
                    }),
                }
            }
            wl_shm_pool::request::RESIZE => {
                let Some(size) = args.first().and_then(Arg::as_int) else {
                    return;
                };
                let Some(pool) = self.pools.get_mut(&sender) else {
                    return;
                };
                if pool.resize(size) {
                    let pool = pool.key;
                    self.events.push(Event::PoolResized { pool, size });
                }
            }
            _ => {}
        }
    }

    /// The pool `id` names, if it is one.
    #[must_use]
    pub fn pool(&self, id: ObjectId) -> Option<&Pool> {
        self.pools.get(&id)
    }

    /// The buffer `id` names, if it is one.
    #[must_use]
    pub fn buffer(&self, id: ObjectId) -> Option<&Buffer> {
        self.buffers.get(&id)
    }

    /// Whether any `wl_buffer` this client still owns was made from `pool`.
    ///
    /// `wl_shm_pool.destroy` does not take the memory away: "the mmapped
    /// memory will be released when all buffers that have been created from
    /// this pool are gone". A client is entitled to make its buffers, throw
    /// the pool away and go on drawing with them, and most toolkits do --
    /// `grim` does it between asking for a screenshot and taking it. So the
    /// compositor above keeps the mapping until this says no.
    #[must_use]
    pub fn pool_in_use(&self, pool: PoolKey) -> bool {
        self.buffers.values().any(|buffer| buffer.pool == pool)
            || self
                .orphans
                .values()
                .any(|orphan| orphan.buffer.pool == pool)
    }

    /// Tell the client a buffer is its own again.
    ///
    /// The compositor above calls this once it has finished reading a buffer
    /// a commit replaced. Until it does, the client may not draw into that
    /// memory, so a compositor that forgets is a client that stalls.
    pub fn release_buffer(&mut self, buffer: ObjectId) {
        if !self.buffers.contains_key(&buffer) {
            return;
        }
        let _ = self
            .out
            .write(buffer, core::wl_buffer::event::RELEASE, &[], &[]);
    }
}
