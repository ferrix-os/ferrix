//! A Wayland client that draws one of the test patterns.
//!
//! The compositor's exit tests need clients, and a client built on a toolkit
//! would bring a toolkit's dependencies onto Ferrix's image and a toolkit's
//! opinions into the test. This one is the whole Wayland client in one file,
//! over `src/user/system/linux/compositor/wire` and `src/user/system/linux/compositor/socket` -- the same crates the
//! server uses, which means the test exercises them from both ends.
//!
//! It draws `src/user/system/linux/compositor/render`'s checkerboard or gradient, so what it puts
//! on screen is what that crate's own expected images are made of, and the
//! compositor's pixel test can be compared against a picture built in code.
//!
//! What it does is what any client does: connect, bind, make a surface, give
//! it a window, take the configure, ack it, draw at the size it was given,
//! attach and commit, then redraw whenever it is configured again.

mod av1;
pub mod client;
mod dmabuf;

pub use client::{
    Movie, Picture, Shape, announce_when_drawn, present_through_dmabuf, run, run_announced_on,
    run_on, run_shaped, run_shaped_on, run_video, run_wallpaper,
};
