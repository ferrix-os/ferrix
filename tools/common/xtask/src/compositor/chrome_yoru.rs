//! `test-compositor --boot chrome-yoru`: Google's Chrome on Yoru's desktop
//! in Nocturne. The Chrome volume carries GTK 3 (`fetch-chrome.sh`), which
//! Chrome loads for its system look; Yoru carries Nocturne for GTK and
//! GTK's settings naming it, dark. So Chrome's own frame -- its tab strip
//! and toolbar -- is drawn in Nocturne's dark colours rather than Chrome's
//! light default.
//!
//! The window shows `test-chrome-window`'s yellow page. The host finds the
//! page by its yellow in QEMU's screendump and requires the toolbar above
//! it dark: the mean of a row 20 pixels above the page under 0x80, where Chrome's
//! light default is about 0xc7. The
//! screen is kept as `chrome-yoru.ppm`. Skipped where the machine has no
//! Chrome volume.

use std::time::{Duration, Instant};

use super::boot::{build_image, say_the_marker, with_the_transcript};
use super::{FAILED, MARKER, Programs};
use crate::args::Args;
use crate::display::{DEVICE_ID, Image, Qmp, free_port, parse_ppm};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The page's yellow, `#fc0`.
const YELLOW: [u8; 3] = [0xff, 0xcc, 0x00];

/// Where the page's yellow is: its box `(left, top, right, bottom)` and how
/// many pixels it has.
fn page(screen: &Image) -> (usize, Option<(usize, usize, usize, usize)>) {
    let mut found = 0;
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for y in 0..screen.height {
        for x in 0..screen.width {
            let at = (y * screen.width + x) * 3;
            if screen.pixels.get(at..at + 3) == Some(&YELLOW[..]) {
                found += 1;
                bounds = Some(bounds.map_or((x, y, x, y), |(l, t, r, b)| {
                    (l.min(x), t.min(y), r.max(x), b.max(y))
                }));
            }
        }
    }
    (found, bounds)
}

/// The mean of the three channels along row `y` from `left` to `right`.
fn row_mean(screen: &Image, y: usize, left: usize, right: usize) -> u32 {
    let mut sum = 0_u64;
    let mut n = 0_u64;
    for x in left..=right {
        let at = (y * screen.width + x) * 3;
        if let Some(pixel) = screen.pixels.get(at..at + 3) {
            sum += pixel.iter().map(|c| u64::from(*c)).sum::<u64>();
            n += 3;
        }
    }
    u32::try_from(sum / n.max(1)).unwrap_or(u32::MAX)
}

/// `test-compositor --boot chrome-yoru`.
///
/// # Errors
///
/// Chrome never drew its page, or its toolbar is not dark.
pub(super) fn test_chrome_yoru(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    if arch != Arch::X86_64 {
        println!("  {arch}: chrome-yoru skipped: Chrome for Testing is linux64 alone");
        return Ok(());
    }
    let Ok(volume) = crate::chrome::volume() else {
        println!(
            "  {arch}: chrome-yoru skipped: no Chrome volume (tools/common/fetch/fetch-chrome.sh)"
        );
        return Ok(());
    };
    if !volume.is_file() {
        println!(
            "  {arch}: chrome-yoru skipped: {} is not there",
            volume.display()
        );
        return Ok(());
    }
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str(crate::chrome::WINDOW_ENV);
    config.push_str(&crate::chrome::window_library_path(false));
    config.push_str(&format!(
        "exec-once = {}\n",
        crate::chrome::window_command(super::browser::CHROME_WINDOW_PAGE)
    ));
    // The volume's links beside Yoru's own files: its fonts beside Inter
    // and Phosphor, as the desktop links them.
    let links = super::desktop::chrome_links(arch, &carried.ports);
    carried.ports.extend(links);
    carried.ports.extend(crate::chrome::window_files());
    let mut args = args.clone();
    args.data_image = Some(volume.clone());
    if !args.memory_given {
        args.memory = crate::chrome::MEMORY;
    }
    println!("  {arch}: chrome-yoru: Chrome from {}", volume.display());
    let (image, kernel) = build_image(arch, programs, &config, carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("chrome-yoru.ppm");
    let mut best: Option<Image> = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        // The marker is what started this hook; a failure before it ends here.
        let failed = watching.lines().iter().any(|line| line.contains(FAILED));
        if failed {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: the compositor failed before `{MARKER}`")),
                watching,
            ));
        }
        say_the_marker(watching, arch);
        let deadline = Instant::now() + super::browser::CHROME_WINDOW_PATIENCE;
        loop {
            qmp.screendump(Some(DEVICE_ID), &dump)?;
            let bytes = std::fs::read(&dump)
                .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
            let screen = parse_ppm(&bytes)?;
            let found = page(&screen).0;
            if best.as_ref().is_none_or(|kept| page(kept).0 < found) {
                best = Some(screen);
            }
            if found >= 78_643 || Instant::now() >= deadline {
                break;
            }
            let _ = watching.read_more(Instant::now() + Duration::from_secs(2), |_| false)?;
        }
        // A moment more for the toolbar to settle after the page.
        std::thread::sleep(Duration::from_secs(3));
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        let bytes = std::fs::read(&dump)
            .map_err(|error| Error::new(format!("reading {}: {error}", dump.display())))?;
        best = Some(parse_ppm(&bytes)?);
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    let Some(screen) = best else {
        return Err(Error::new(format!("{arch}: chrome-yoru: no screen")));
    };
    let (found, bounds) = page(&screen);
    let Some((left, top, right, _)) = bounds.filter(|_| found >= 78_643) else {
        return Err(Error::new(format!(
            "{arch}: chrome-yoru: Chrome's page never came: {found} yellow pixels (the screen is {})",
            dump.display()
        )));
    };
    let toolbar = row_mean(&screen, top.saturating_sub(20), left, right);
    println!(
        "  {arch}: chrome-yoru: the page at {left},{top}, the toolbar above it {toolbar:#04x} on average (the screen is {})",
        dump.display()
    );
    if toolbar >= 0x80 {
        return Err(Error::new(format!(
            "{arch}: chrome-yoru: Chrome's toolbar is light ({toolbar:#04x}), not Nocturne: no GTK under it"
        )));
    }
    println!("  {arch}: chrome-yoru: Chrome drew its frame in Nocturne, through GTK");
    Ok(())
}
