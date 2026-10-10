//! virgl's command stream, written as words.
//!
//! A virtio-gpu with a GPU behind it runs *virgl*: gallium's state and draw
//! calls, flattened into 32-bit words, which the host's virglrenderer turns
//! back into OpenGL. `VIRTGPU_EXECBUFFER` on `/dev/dri/renderD128` carries a
//! buffer of them. This crate writes that buffer and nothing else: it opens
//! no device and holds no resource, so every command is host-tested against
//! the words virglrenderer's own header says it must be (`docs/GPU.md`
//! §3.5, step 3).
//!
//! # Where the numbers come from
//!
//! Every layout is `virgl_protocol.h`'s (virglrenderer 1.2.0) and every
//! writer follows Mesa's `virgl_encode.c`, which is the encoder virglrenderer
//! is tested against. The enumerations in [`pipe`] are gallium's
//! `p_defines.h` and virgl's `virgl_hw.h`. virgl renumbers some of gallium's
//! bind flags, so those are taken from virgl's header and not from gallium's.
//!
//! # What a command looks like
//!
//! One header word, `command | object << 8 | length << 16`, with the length
//! in words and not counting the header, and then that many words. There is
//! no framing beyond that: a stream is commands end to end, and a reader
//! that loses its place has no way back, which is why the length is computed
//! here from what was written rather than passed in.
//!
//! # Shaders are text
//!
//! virgl carries a shader as TGSI *assembly text*, which virglrenderer
//! parses and translates to GLSL. [`Stream::create_shader`] pads the text to
//! words and says how long it was; what the text says is [`shaders`]'.

mod device;
pub mod pipe;
pub mod shaders;
mod stream;
pub mod vtest;

#[cfg(test)]
mod tests;

pub use device::{Device, Layout, Texture};
pub use stream::{
    Blend, Blit, Rasterizer, Region, Sampler, Stream, VertexBuffer, VertexElement, View,
};
