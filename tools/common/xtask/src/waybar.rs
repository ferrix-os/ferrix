//! waybar on the desktop: the files its boot carries.
//!
//! The boot runs `/bin/waybar` against a config of the tree's own
//! ([`BOOT_CONFIG`]: the user's bar, module for module, with each script
//! replaced by one that prints a fixed answer) and the user's own
//! stylesheet -- `~/.config/waybar/style.css` and the `icons/` beside it,
//! read from this machine when the image is built and never committed. A
//! machine without that file takes [`FALLBACK_STYLE`], and says so. The
//! fonts are the ones the stylesheet names, resolved here as
//! `run-compositor` resolves them (`crate::dotfiles`).
//!
//! Everything goes under [`HOME_DIR`] in the image, and into a directory on
//! this machine as well, where the `x86_64` build of the same program draws
//! the picture the guest's screen must show (`waybar --render`).

use std::path::{Path, PathBuf};

use crate::ports::{Content, File};
use crate::{Error, Result};

/// The boot's config, in the tree.
const BOOT_CONFIG: &str = "data/boot/config.jsonc";

/// The stylesheet a machine without the user's takes, in the tree.
const FALLBACK_STYLE: &str = "data/boot/style.css";

/// Where the boot's files go in the image: under the desktop's `HOME`, `/`.
pub(crate) const HOME_DIR: &str = ".config/waybar-boot";

/// The output the config asks for, and the size of the screen.
pub(crate) const OUTPUT: &str = "Virtual-1";
/// The screen's size.
pub(crate) const SIZE: (u32, u32) = (1024, 768);

/// The colour hyprix clears to (`misc:background_color`'s default), which
/// the bar's translucent ground is laid over.
pub(crate) const GROUND: &str = "111111";

/// The boot's files: its config, the stylesheet with the icons beside it,
/// and whether the stylesheet is the user's.
///
/// # Errors
///
/// A file of the tree's, or of the user's that is there, that cannot be read.
pub(crate) fn files() -> Result<(Vec<File>, bool)> {
    // The waybar app's own boot configuration, in its folder.
    let root = crate::apps::folder("waybar")?;
    let read = |path: &Path| {
        std::fs::read(path).map_err(|error| Error::new(format!("{}: {error}", path.display())))
    };
    let mut out = vec![File {
        path: format!("{HOME_DIR}/config.jsonc"),
        mode: 0o644,
        content: Content::Bytes(read(&root.join(BOOT_CONFIG))?),
    }];
    let users = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config/waybar"))
        .filter(|dir| dir.join("style.css").is_file());
    match &users {
        Some(dir) => {
            out.push(File {
                path: format!("{HOME_DIR}/style.css"),
                mode: 0o644,
                content: Content::Bytes(read(&dir.join("style.css"))?),
            });
            let icons = dir.join("icons");
            if icons.is_dir() {
                let mut names: Vec<_> = std::fs::read_dir(&icons)
                    .map_err(|error| Error::new(format!("{}: {error}", icons.display())))?
                    .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                    .collect();
                names.sort();
                for name in names {
                    let path = icons.join(&name);
                    if path.is_file() {
                        out.push(File {
                            path: format!("{HOME_DIR}/icons/{}", name.to_string_lossy()),
                            mode: 0o644,
                            content: Content::Bytes(read(&path)?),
                        });
                    }
                }
            }
        }
        None => out.push(File {
            path: format!("{HOME_DIR}/style.css"),
            mode: 0o644,
            content: Content::Bytes(read(&root.join(FALLBACK_STYLE))?),
        }),
    }
    Ok((out, users.is_some()))
}

/// The font families the carried stylesheet names.
#[must_use]
pub(crate) fn families(files: &[File]) -> Vec<String> {
    files
        .iter()
        .filter(|file| file.path.ends_with("/style.css"))
        .filter_map(|file| match &file.content {
            Content::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        })
        .flat_map(|text| crate::dotfiles::families_named("style.css", &text))
        .collect()
}

/// Write `files` under `dir` on this machine, each at its path less
/// `strip`, for the host's render to read.
///
/// # Errors
///
/// The directory or a file that cannot be written.
pub(crate) fn write_here(dir: &Path, files: &[File], strip: &str) -> Result<()> {
    for file in files {
        let Content::Bytes(bytes) = &file.content else {
            continue;
        };
        let relative = file.path.strip_prefix(strip).unwrap_or(&file.path);
        let target = dir.join(relative.trim_start_matches('/'));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::new(format!("{}: {error}", parent.display())))?;
        }
        std::fs::write(&target, bytes)
            .map_err(|error| Error::new(format!("{}: {error}", target.display())))?;
    }
    Ok(())
}

/// A `waybar --render` line saying where a module is --
/// `waybar: module pulseaudio at 888,0 78x40` -- as its name and
/// `[x, y, width, height]`.
#[must_use]
pub(crate) fn module_line(line: &str) -> Option<(String, [f32; 4])> {
    let rest = line.strip_prefix("waybar: module ")?;
    let (name, place) = rest.rsplit_once(" at ")?;
    let (corner, size) = place.split_once(' ')?;
    let (x, y) = corner.split_once(',')?;
    let (width, height) = size.split_once('x')?;
    Some((
        name.to_owned(),
        [
            x.parse().ok()?,
            y.parse().ok()?,
            width.parse().ok()?,
            height.parse().ok()?,
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::module_line;

    #[test]
    fn a_renders_module_line_is_read_back() {
        assert_eq!(
            module_line("waybar: module custom/ws-1 at 736,0 32x40"),
            Some(("custom/ws-1".to_owned(), [736.0, 0.0, 32.0, 40.0]))
        );
        assert_eq!(module_line("waybar: rendered 1024x40 into x.ppm"), None);
    }
}
