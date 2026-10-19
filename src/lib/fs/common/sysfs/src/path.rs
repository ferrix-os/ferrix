//! A link's target, spelt as Linux spells it.
//!
//! Every link in sysfs is relative: `kernfs_get_target_path` climbs from the
//! directory the link is in to the deepest directory that holds the target,
//! one `..` a level, and descends from there. So `/sys/block/vda` reads
//! `../devices/pci0000:00/0000:00:04.0/block/vda`, and the tree can be
//! mounted anywhere -- a boot check mounts one under `/tmp` -- with every
//! link still leading where it should.
//!
//! "Holds" is the word that matters: the climb stops at an ancestor of the
//! target's *parent*, never at the target itself. A card's `device` link,
//! in `…/0000:00:02.0/drm/card0`, leads to `…/0000:00:02.0`, which is an
//! ancestor of the link; kernfs still climbs past it and names it,
//! `../../../0000:00:02.0`, and so does this.

use alloc::vec::Vec;

/// The target of a link in the directory `from` leading to `to`, both given
/// as their components beneath the mount's root.
pub fn relative(out: &mut Vec<u8>, from: &[&[u8]], to: &[&[u8]]) {
    let shared = from
        .iter()
        .zip(to)
        .take_while(|(here, there)| here == there)
        .count()
        .min(to.len().saturating_sub(1));
    let mut first = true;
    for _ in from.iter().skip(shared) {
        if !first {
            out.push(b'/');
        }
        first = false;
        out.extend_from_slice(b"..");
    }
    for component in to.iter().skip(shared) {
        if !first {
            out.push(b'/');
        }
        first = false;
        out.extend_from_slice(component);
    }
    if first {
        out.push(b'.');
    }
}
