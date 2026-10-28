//! Frames drawn by hyprix-gpu, a process of their own on any Vulkan
//! driver (`docs/GPU-COMPOSITOR.md`).
//!
//! hyprix is a static program and cannot load a GPU's Vulkan driver, so a
//! glibc helper does: it is sent each frame's `Painter` calls as ops and
//! answers when the frame is in a buffer the card shows. One helper serves
//! every screen, each a canvas of its own. A helper that fails, hangs or
//! dies is answered the way a GPU that does is: the screen draws in
//! software at once, and looks for the helper again later
//! (`crate::state`'s `give_up_gpu` and `gpu_again`), which starts a new
//! one.
//!
//! Off unless `hyprix:gpu` says `auto` or `on`.

mod canvas;
mod conn;
mod link;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

pub use canvas::{Backdrop, Canvas};
pub use link::Link;

/// Whether screens are drawn by the helper: `hyprix:gpu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Never: what every screen did before the helper.
    Off,
    /// Where virgl draws nothing and a helper starts.
    Auto,
    /// The same, saying why when it does not.
    On,
}

impl Mode {
    /// `hyprix:gpu`'s value; anything else is [`Mode::Off`].
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "auto" => Self::Auto,
            "on" | "1" | "true" | "yes" => Self::On,
            _ => Self::Off,
        }
    }
}

thread_local! {
    static MODE: Cell<Mode> = const { Cell::new(Mode::Off) };
    static LINK: RefCell<Weak<RefCell<Link>>> = const { RefCell::new(Weak::new()) };
    static NEXT_CANVAS: Cell<u32> = const { Cell::new(0) };
}

/// Set from the configuration, at start and at each reload.
pub fn set_mode(mode: Mode) {
    MODE.with(|held| held.set(mode));
}

/// What `hyprix:gpu` says now.
#[must_use]
pub fn mode() -> Mode {
    MODE.with(Cell::get)
}

/// Where the helper is: `HYPRIX_GPU_HELPER`, or the image's.
fn program() -> PathBuf {
    std::env::var_os("HYPRIX_GPU_HELPER")
        .map_or_else(|| PathBuf::from("/usr/libexec/hyprix-gpu"), PathBuf::from)
}

/// The helper every canvas draws through, started if none runs or the one
/// that ran is dead.
fn link() -> Result<Rc<RefCell<Link>>, String> {
    if let Some(held) = LINK.with(|link| link.borrow().upgrade())
        && held.borrow().dead().is_none()
    {
        return Ok(held);
    }
    let program = program();
    if !program.exists() {
        return Err(format!("no {}", program.display()));
    }
    let node = render_node();
    let started = Link::spawn(&program, node.as_deref())
        .map_err(|error| format!("{} did not start: {error}", program.display()))?;
    let held = Rc::new(RefCell::new(started));
    LINK.with(|link| *link.borrow_mut() = Rc::downgrade(&held));
    Ok(held)
}

/// Use `link` for the canvases made from now on: the tests' fake helper.
pub fn adopt_link(link: &Rc<RefCell<Link>>) {
    LINK.with(|held| *held.borrow_mut() = Rc::downgrade(link));
}

/// The first render node there is, which the helper picks its device by.
fn render_node() -> Option<String> {
    (128..136)
        .map(|minor| format!("/dev/dri/renderD{minor}"))
        .find(|path| std::path::Path::new(path).exists())
}

/// A canvas for a `width` × `height` screen on the helper, starting it if
/// it is not running, and what to say of it.
///
/// # Errors
///
/// Why there is none: no helper program, one that would not start, or one
/// that would not make the screen's outputs.
pub fn canvas(width: u32, height: u32) -> Result<(Canvas, String), String> {
    let link = link()?;
    let number = NEXT_CANVAS.with(|next| {
        let number = next.get().wrapping_add(1);
        next.set(number);
        number
    });
    let said = {
        let held = link.borrow();
        format!("{} ({})", held.device, held.driver)
    };
    Canvas::new(link, number, width, height)
        .map(|canvas| (canvas, said))
        .map_err(|error| format!("hyprix-gpu: {error}"))
}

/// The modifiers the running helper samples clients' dmabufs of both
/// formats with, for `zwp_linux_dmabuf_v1`.
#[must_use]
pub fn reported() -> Option<Vec<compositor_server::Reported>> {
    use compositor_server::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888, Reported};

    let link = LINK.with(|link| link.borrow().upgrade())?;
    let held = link.borrow();
    let argb = held.modifiers.get(&DRM_FORMAT_ARGB8888)?;
    let xrgb = held.modifiers.get(&DRM_FORMAT_XRGB8888)?;
    Some(
        argb.iter()
            .filter(|(modifier, _)| xrgb.iter().any(|(other, _)| other == modifier))
            .map(|&(modifier, external_only)| Reported {
                modifier,
                external_only,
            })
            .collect(),
    )
}
