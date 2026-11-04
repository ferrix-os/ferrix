//! Fuzz the file server itself (`docs/microkernel/BTRFSD.md` §3, CO.2, L.object.330): a run of
//! requests from a hostile `linuxd`, over a tmpfs, with hostile bytes in the shared slot. Beyond
//! not panicking: each answer encodes and decodes, whatever came before it.

#![no_main]

use std::sync::Arc;

use ferrix_fsproto::server::{Slot, Volume};
use ferrix_fsproto::wire::{Answer, SLOT_BYTES};
use ferrix_vfs::Clock;
use ferrix_vfs::tmpfs::{HeapStorage, Tmpfs};
use libfuzzer_sys::fuzz_target;

#[derive(Debug)]
struct Still;

impl Clock for Still {
    fn now(&self) -> ferrix_vfs::Timespec {
        ferrix_vfs::Timespec::default()
    }
}

struct Memory(Vec<u8>);

impl Slot for Memory {
    fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        let Some(from) = offset.checked_add(out.len()).and_then(|end| self.0.get(offset..end)) else {
            return false;
        };
        out.copy_from_slice(from);
        true
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> bool {
        let Some(to) = offset.checked_add(bytes.len()).and_then(|end| self.0.get_mut(offset..end)) else {
            return false;
        };
        to.copy_from_slice(bytes);
        true
    }
}

fuzz_target!(|bytes: &[u8]| {
    let Ok(filesystem) = Tmpfs::new(1, Arc::new(Still), Arc::new(HeapStorage::new(1 << 24)), 0o755) else {
        return;
    };
    let mut volume = Volume::new(filesystem);
    let mut slot = Memory(vec![0; SLOT_BYTES.min(1 << 16)]);
    let mut rest = bytes;
    // A byte's length, a request of that length, then as many bytes for the slot.
    while let Some((&length, tail)) = rest.split_first() {
        let length = usize::from(length).min(tail.len());
        let (request, tail) = tail.split_at(length);
        let (into_slot, tail) = tail.split_at(length.min(tail.len()));
        let at = slot.0.len().min(into_slot.len());
        slot.0[..at].copy_from_slice(&into_slot[..at]);
        let answer = volume.serve(request, &mut slot);
        assert!(Answer::decode(&answer.encode()).is_some());
        rest = tail;
    }
});
