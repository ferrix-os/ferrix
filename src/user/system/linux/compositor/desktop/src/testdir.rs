//! A directory of files for a test, removed when the test is done.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// How many have been made, so two tests never share one.
static MADE: AtomicUsize = AtomicUsize::new(0);

/// A fresh directory under the temporary directory.
#[derive(Debug)]
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    /// Make one, named after `what`.
    pub(crate) fn new(what: &str) -> Self {
        let number = MADE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "desktop-test-{}-{what}-{number}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        // A test that cannot make its files fails on the first file it
        // reads back, which says why well enough.
        let _ = std::fs::create_dir_all(&path);
        Self(path)
    }

    /// Where it is.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Write `text` to `relative`, making the directories it needs.
    pub(crate) fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, text);
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
