//! hyprix-gpu: the compositor's frames drawn by Vulkan in a process of its
//! own (`docs/GPU-COMPOSITOR.md`).
//!
//! hyprix is a static program and cannot load a Vulkan driver. This glibc
//! program can: hyprix starts it with one end of a socket pair, sends it
//! each frame's display list -- the `Painter` calls of
//! `compositor_render::render_onto`, one op each
//! (`compositor_gpu_protocol`) -- and gets the frame back in a dmabuf the
//! card shows. The shapes are the GPU painter's
//! (`compositor_render::gpu::Renderer`), the device is tole's
//! (`tole::gpu::Core`), and [`vulkan::Vulkan`] runs the one on the other.

pub mod server;
pub mod vulkan;
pub mod wire;

#[cfg(test)]
mod tests;
