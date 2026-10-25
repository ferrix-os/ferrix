//! A file of a FAT32 volume read whole, or rewritten in place with bytes of
//! its own length, following its cluster chain: how the installer fills in
//! the copied volume's `FERRIX/SETUP.TXT` and the installed system reads it
//! back, with no FAT writer.

use std::fs::File;
use std::io;

use ferrix_install::disk;
use ferrix_install::fat::Fat;

/// The file at `path` in the volume at byte `at` of `file`, if it is there.
pub fn read(file: &mut File, at: u64, path: &[&[u8; 11]]) -> io::Result<Option<Vec<u8>>> {
    let volume = Fat::read(file, at)?;
    let Some(entry) = volume.open_path(file, path)? else {
        return Ok(None);
    };
    if entry.size > 1 << 20 {
        return Err(io::Error::other("a setup file larger than a MiB"));
    }
    volume.read_file(file, &entry).map(Some)
}

/// Write `bytes` over the file at `path`, which must be exactly as long.
/// Whether it was there.
pub fn rewrite(file: &mut File, at: u64, path: &[&[u8; 11]], bytes: &[u8]) -> io::Result<bool> {
    let volume = Fat::read(file, at)?;
    let Some(entry) = volume.open_path(file, path)? else {
        return Ok(false);
    };
    if entry.size as usize != bytes.len() {
        return Err(io::Error::other(format!(
            "the volume's file is {} bytes, not {}",
            entry.size,
            bytes.len()
        )));
    }
    let cluster_bytes = volume.cluster_bytes();
    let mut cluster = Some(entry.cluster);
    for piece in bytes.chunks(cluster_bytes) {
        let this = cluster.ok_or_else(|| io::Error::other("a file's cluster chain ends early"))?;
        disk::write_at(file, volume.cluster_offset(this)?, piece)?;
        cluster = volume.next(file, this)?;
    }
    Ok(true)
}
