//! Which kernel mappings of the device's apertures are alive: the
//! bookkeeping under `os_map_kernel_space` and `os_unmap_kernel_space`
//! (`device.rs`), apart from the native calls so the host can test it.
//!
//! The rule is Linux's `ioremap` and `iounmap`, which RM was written to:
//! every map is a mapping of its own, even of a range already mapped, and an
//! unmap ends exactly the mapping whose address it names. Nothing is shared
//! between two maps, so nothing RM unmaps can take a mapping away from
//! another holder -- BAR0, mapped once for nvrm's life and never unmapped,
//! stays whatever is mapped and unmapped inside it.
//!
//! A mapping costs the kernel a region and a handle, both of which go when
//! it is unmapped, so what bounds the table is how many are alive at once,
//! not how many were ever made.

/// A page.
const PAGE: usize = 4096;

/// One mapping that is alive.
pub(crate) struct Entry<W> {
    /// The physical address of its first byte, page-aligned.
    pub(crate) phys: u64,
    /// Its length, whole pages.
    pub(crate) len: u64,
    /// Where it is mapped, page-aligned.
    pub(crate) address: usize,
    /// What keeps it: the `IoMapping`, closed when the entry is dropped.
    pub(crate) window: W,
}

/// The mappings alive, at most `N`.
pub(crate) struct Table<W, const N: usize> {
    /// Each alive mapping, in no order.
    slots: [Option<Entry<W>>; N],
    /// How many slots are used.
    live: usize,
    /// The most that ever were at once.
    peak: usize,
}

impl<W, const N: usize> Table<W, N> {
    /// No mappings.
    pub(crate) const fn new() -> Self {
        Table {
            slots: [const { None }; N],
            live: 0,
            peak: 0,
        }
    }

    /// How many mappings are alive.
    pub(crate) const fn live(&self) -> usize {
        self.live
    }

    /// The most that were ever alive at once.
    pub(crate) const fn peak(&self) -> usize {
        self.peak
    }

    /// Whether one more mapping can be remembered: asked before the kernel
    /// is asked for it.
    pub(crate) const fn has_room(&self) -> bool {
        self.live < N
    }

    /// Remember `entry`. Handed back if the table is full.
    pub(crate) fn insert(&mut self, entry: Entry<W>) -> Result<(), Entry<W>> {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
            return Err(entry);
        };
        *slot = Some(entry);
        self.live += 1;
        self.peak = self.peak.max(self.live);
        Ok(())
    }

    /// Forget the mapping `address` was handed out from, and hand it back to
    /// be unmapped: the one that starts on `address`'s page, since a map of
    /// a range that starts inside a page returns an address inside the
    /// mapping's first page. `None` for an address no alive mapping starts
    /// at, as `iounmap` leaves alone an address that is no mapping's.
    pub(crate) fn remove(&mut self, address: usize) -> Option<Entry<W>> {
        let base = address & !(PAGE - 1);
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|entry| entry.address == base))?;
        let entry = slot.take()?;
        self.live -= 1;
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, PAGE, Table};

    /// The table size the 3060 ran out of.
    const OLD: usize = 64;

    /// A mapping of one page at `phys`, mapped at `address`.
    fn page(phys: u64, address: usize) -> Entry<u32> {
        Entry {
            phys,
            len: PAGE as u64,
            address,
            window: 0,
        }
    }

    #[test]
    fn unmapped_slots_are_used_again() {
        // What a desktop does: channels come and go, each with its page,
        // far more over time than the table holds at once.
        let mut table: Table<u32, OLD> = Table::new();
        let mut alive = std::collections::VecDeque::new();
        for index in 0..10 * OLD {
            let address = 0x7000_0000 + index * PAGE;
            assert!(table.has_room(), "full at map {index}");
            assert!(table.insert(page(0x38_1000_0000, address)).is_ok());
            alive.push_back(address);
            if alive.len() > 29 {
                let oldest = alive.pop_front();
                let gone = oldest.and_then(|oldest| table.remove(oldest));
                assert_eq!(gone.map(|gone| gone.address), oldest);
            }
        }
        assert_eq!(table.live(), 29);
        assert_eq!(table.peak(), 30);
        for address in alive {
            assert!(table.remove(address).is_some());
        }
        assert_eq!(table.live(), 0);
    }

    #[test]
    fn the_bound_is_how_many_are_alive() {
        let mut table: Table<u32, OLD> = Table::new();
        for index in 0..OLD {
            assert!(table.has_room());
            assert!(
                table
                    .insert(page(index as u64 * 4096, 0x1000 * (index + 1)))
                    .is_ok()
            );
        }
        assert!(!table.has_room());
        let refused = table.insert(page(0, 0x100_0000)).err();
        assert_eq!(refused.map(|refused| refused.address), Some(0x100_0000));
        assert_eq!((table.live(), table.peak()), (OLD, OLD));
        // One unmap makes room for one map, and no more.
        assert!(table.remove(0x1000 * 7).is_some());
        assert!(table.has_room());
        assert!(table.insert(page(0, 0x100_0000)).is_ok());
        assert!(!table.has_room());
        assert_eq!((table.live(), table.peak()), (OLD, OLD));
    }

    #[test]
    fn an_unmap_ends_the_mapping_it_names_and_no_other() {
        let mut table: Table<u32, 8> = Table::new();
        // BAR0 whole, and one page of it mapped again beside it.
        let bar0 = Entry {
            phys: 0xf000_0000,
            len: 16 << 20,
            address: 0x4000_0000,
            window: 1,
        };
        assert!(table.insert(bar0).is_ok());
        assert!(table.insert(page(0xf000_0000, 0x5000_0000)).is_ok());
        // The same page twice is two mappings.
        assert!(table.insert(page(0xf000_0000, 0x5000_1000)).is_ok());
        assert_eq!(table.live(), 3);

        // An address inside BAR0's mapping but not on its first page is no
        // mapping's: BAR0 stays.
        assert!(table.remove(0x4000_0000 + 0x8_8000).is_none());
        assert!(table.remove(0).is_none());
        assert!(table.remove(0x6000_0000).is_none());
        assert_eq!(table.live(), 3);

        // A map that began inside a page is unmapped by the address it got.
        let gone = table.remove(0x5000_0000 + 0x40);
        assert_eq!(
            gone.map(|gone| (gone.phys, gone.len, gone.window)),
            Some((0xf000_0000, 4096, 0))
        );
        // Unmapped twice, the second finds nothing.
        assert!(table.remove(0x5000_0000).is_none());
        assert!(table.remove(0x5000_1000).is_some());

        let left = table.remove(0x4000_0000);
        assert_eq!(
            left.map(|left| (left.len, left.window)),
            Some((16 << 20, 1))
        );
        assert_eq!(table.live(), 0);
    }
}
