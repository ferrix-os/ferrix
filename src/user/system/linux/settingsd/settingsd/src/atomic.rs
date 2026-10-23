//! Writing a file so that a crash leaves the old one or the new one, never
//! half of either (`docs/SETTINGS-WRITER.md` §6).

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Replace `path` with `bytes`, mode `mode`: a temporary file beside it,
/// made with the mode before any byte is in it, written, `fsync`ed, renamed
/// over `path`, and the directory `fsync`ed. On an error the temporary file
/// is removed and `path` is as it was.
///
/// # Errors
///
/// The first call that failed, with the path it was about.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a path with no directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a path with no file name"))?;
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".settingsd-{}", std::process::id()));
    let temporary = dir.join(temporary_name);
    let _ = std::fs::remove_file(&temporary);
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        // The umask may have taken bits off: the mode is what was asked.
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn the_file_is_replaced_whole_and_nothing_is_left_beside_it() {
        let dir = std::env::temp_dir().join(format!("settingsd-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hostname");
        std::fs::write(&path, "old\n").unwrap();
        write_atomic(&path, b"new\n", 0o644).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // A directory that is not there: an error, and nothing made.
        assert!(write_atomic(&dir.join("no/such"), b"x", 0o644).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
