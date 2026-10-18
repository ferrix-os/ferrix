// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_ring.c and
// gt/intel_ring.h:
// Copyright © 2019 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! A context's ring buffer, as `intel_ring` keeps it: a power-of-two size,
//! the engine's head (how far it has read), the driver's emit point, and
//! the space between, with a cache line kept free ("If the Ring Buffer Head
//! Pointer and the Tail Pointer are on the same cacheline, the Head Pointer
//! must not be greater than the Tail Pointer"). Where i915's
//! `intel_ring_begin` waits for space, this returns [`NoSpace`] and the
//! caller waits for a request to retire.

use crate::lrc::CACHELINE_BYTES;

/// The ring has not room for what was asked; retire requests and ask again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoSpace;

/// `struct intel_ring`'s bookkeeping (the memory is the caller's).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ring {
    /// `size`, a power of two, in bytes.
    pub size: u32,
    /// `effective_size`: `size` less what the hardware may prefetch past the
    /// end (on Gen12 no wa, so `size`).
    pub effective_size: u32,
    /// `head`: where the engine has read to, as last learned.
    pub head: u32,
    /// `emit`: where the next command goes.
    pub emit: u32,
    /// `space`: bytes free.
    pub space: u32,
}

/// What [`Ring::begin`] reserved: the byte offset the commands go at, and
/// how many bytes before it (at the old emit point, up to the end) must be
/// filled with `MI_NOOP` because the commands wrapped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reserved {
    /// The commands' first byte.
    pub at: u32,
    /// The wrap's padding: from `wrap_from` to the end of the ring.
    pub wrap_from: Option<u32>,
}

/// `__intel_ring_space`.
#[must_use]
pub const fn ring_space(head: u32, tail: u32, size: u32) -> u32 {
    head.wrapping_sub(tail).wrapping_sub(CACHELINE_BYTES) & (size - 1)
}

impl Ring {
    /// `intel_engine_create_ring` for a fresh ring of `size` bytes.
    #[must_use]
    pub const fn new(size: u32) -> Self {
        Ring {
            size,
            effective_size: size,
            head: 0,
            emit: 0,
            space: ring_space(0, 0, size),
        }
    }

    /// `intel_ring_wrap`.
    #[must_use]
    pub const fn wrap(&self, pos: u32) -> u32 {
        pos & (self.size - 1)
    }

    /// `intel_ring_update_space` after the engine read to `head`.
    pub fn update_head(&mut self, head: u32) {
        self.head = self.wrap(head);
        self.space = ring_space(self.head, self.emit, self.size);
    }

    /// `intel_ring_begin` for `num_dwords` (even: "Packets must be qword
    /// aligned"), keeping `reserved` bytes for the request's end.
    pub fn begin(&mut self, num_dwords: u32, reserved: u32) -> Result<Reserved, NoSpace> {
        if !num_dwords.is_multiple_of(2) {
            return Err(NoSpace);
        }
        let bytes = num_dwords * 4;
        let remain_usable = self.effective_size - self.emit;
        let mut total = bytes + reserved;
        let mut need_wrap = None;
        if total > remain_usable {
            let remain_actual = self.size - self.emit;
            if bytes > remain_usable {
                total += remain_actual;
                need_wrap = Some(remain_actual);
            } else {
                total = reserved + remain_actual;
            }
        }
        if total > self.space {
            return Err(NoSpace);
        }
        let mut wrap_from = None;
        if let Some(pad) = need_wrap {
            wrap_from = Some(self.emit);
            self.space -= pad;
            self.emit = 0;
        }
        let at = self.emit;
        self.emit += bytes;
        self.space -= bytes;
        Ok(Reserved { at, wrap_from })
    }

    /// `intel_ring_offset` of the emit point: never `size` ("as that hangs
    /// some GPUs").
    #[must_use]
    pub const fn tail(&self) -> u32 {
        self.wrap(self.emit)
    }
}
