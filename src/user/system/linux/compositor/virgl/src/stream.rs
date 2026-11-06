//! The stream itself: commands, end to end, as words.

use crate::pipe::{self, command, object};

/// A command buffer being written.
///
/// Every writer appends one whole command, so a stream is always something
/// that could be submitted. Handles -- of state objects, which this context
/// numbers, and of resources, which the render node numbered -- are the
/// caller's to choose and keep apart; nothing here allocates one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    words: Vec<u32>,
}

/// A part of a resource: where a box starts and how big it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    /// Where it starts.
    pub x: u32,
    /// Where it starts.
    pub y: u32,
    /// How wide.
    pub width: u32,
    /// How high.
    pub height: u32,
}

/// How what is drawn is mixed with what is there, for the one colour buffer
/// the compositor draws into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blend {
    /// Whether to mix at all; without it what is drawn replaces.
    pub enable: bool,
    /// `PIPE_BLENDFACTOR_*` for the colour being drawn.
    pub rgb_src: u32,
    /// `PIPE_BLENDFACTOR_*` for the colour that is there.
    pub rgb_dst: u32,
    /// The same for alpha.
    pub alpha_src: u32,
    /// The same for alpha.
    pub alpha_dst: u32,
}

impl Blend {
    /// What is drawn replaces what is there.
    pub const REPLACE: Self = Self {
        enable: false,
        rgb_src: pipe::BLENDFACTOR_ONE,
        rgb_dst: pipe::BLENDFACTOR_ZERO,
        alpha_src: pipe::BLENDFACTOR_ONE,
        alpha_dst: pipe::BLENDFACTOR_ZERO,
    };

    /// Premultiplied source over destination, which is what Wayland's
    /// `ARGB8888` is and what the software renderer composites.
    pub const PREMULTIPLIED_OVER: Self = Self {
        enable: true,
        rgb_src: pipe::BLENDFACTOR_ONE,
        rgb_dst: pipe::BLENDFACTOR_INV_SRC_ALPHA,
        alpha_src: pipe::BLENDFACTOR_ONE,
        alpha_dst: pipe::BLENDFACTOR_INV_SRC_ALPHA,
    };
}

/// How triangles become pixels. What the compositor varies is whether the
/// scissor applies; the rest is what a 2D renderer wants and is fixed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rasterizer {
    /// Whether [`Stream::set_scissor`]'s rectangle clips what is drawn.
    pub scissor: bool,
}

/// How a texture is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sampler {
    /// `PIPE_TEX_FILTER_*`, for both shrinking and growing.
    pub filter: u32,
}

/// A view of a texture for sampling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    /// The resource, by the number the render node gave it.
    pub resource: u32,
    /// `VIRGL_FORMAT_*` to read it as.
    pub format: u32,
    /// Whether alpha reads as one whatever the texture holds, which is what
    /// makes `XRGB8888` opaque.
    pub opaque: bool,
}

/// One attribute of a vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexElement {
    /// Bytes from the vertex's start.
    pub offset: u32,
    /// Which bound vertex buffer.
    pub buffer: u32,
    /// `VIRGL_FORMAT_*` of the attribute.
    pub format: u32,
}

/// A bound vertex buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexBuffer {
    /// Bytes from one vertex to the next.
    pub stride: u32,
    /// Bytes into the resource the first vertex is.
    pub offset: u32,
    /// The resource.
    pub resource: u32,
}

/// A copy from one texture to another, scaled if the boxes differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blit {
    /// The resource written.
    pub dst: u32,
    /// `VIRGL_FORMAT_*` of it.
    pub dst_format: u32,
    /// Where in it.
    pub dst_region: Region,
    /// The resource read.
    pub src: u32,
    /// `VIRGL_FORMAT_*` of it.
    pub src_format: u32,
    /// Where in it.
    pub src_region: Region,
    /// `PIPE_TEX_FILTER_*`.
    pub filter: u32,
}

impl Stream {
    /// An empty stream.
    #[must_use]
    pub const fn new() -> Self {
        Self { words: Vec::new() }
    }

    /// The words written so far.
    #[must_use]
    pub fn words(&self) -> &[u32] {
        &self.words
    }

    /// Whether nothing has been written.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// How many bytes the stream is, which is what a submission is bounded
    /// by.
    #[must_use]
    pub const fn len_bytes(&self) -> usize {
        self.words.len() * 4
    }

    /// Forget everything written, keeping the room.
    pub fn reset(&mut self) {
        self.words.clear();
    }

    /// One command: its header, with the length counted here, and its words.
    ///
    /// A command longer than the header's sixteen bits can say is not
    /// written at all, and the answer says so: half a command would take
    /// every command after it with it.
    fn command(&mut self, number: u32, kind: u32, payload: &[u32]) -> bool {
        let Ok(len) = u16::try_from(payload.len()) else {
            return false;
        };
        self.words
            .push(number | (kind << 8) | (u32::from(len) << 16));
        self.words.extend_from_slice(payload);
        true
    }

    /// `CREATE_OBJECT SURFACE`: a texture's level 0 as something to draw
    /// into.
    pub fn create_surface(&mut self, handle: u32, resource: u32, format: u32) {
        let _ = self.command(
            command::CREATE_OBJECT,
            object::SURFACE,
            // Level 0, and layers 0 to 0.
            &[handle, resource, format, 0, 0],
        );
    }

    /// `CREATE_OBJECT SAMPLER_VIEW`: a texture as something to read from.
    pub fn create_sampler_view(&mut self, handle: u32, view: View) {
        let alpha = if view.opaque {
            pipe::SWIZZLE_ONE
        } else {
            pipe::SWIZZLE_ALPHA
        };
        let swizzle = pipe::SWIZZLE_RED
            | (pipe::SWIZZLE_GREEN << 3)
            | (pipe::SWIZZLE_BLUE << 6)
            | (alpha << 9);
        let _ = self.command(
            command::CREATE_OBJECT,
            object::SAMPLER_VIEW,
            // Layers 0 to 0, levels 0 to 0.
            &[handle, view.resource, view.format, 0, 0, swizzle],
        );
    }

    /// `CREATE_OBJECT SAMPLER_STATE`: clamped to the edge, no mipmaps.
    pub fn create_sampler_state(&mut self, handle: u32, sampler: Sampler) {
        let wrap = pipe::TEX_WRAP_CLAMP_TO_EDGE;
        let s0 = wrap
            | (wrap << 3)
            | (wrap << 6)
            | ((sampler.filter & 1) << 9)
            | (pipe::TEX_MIPFILTER_NONE << 11)
            | ((sampler.filter & 1) << 13);
        let _ = self.command(
            command::CREATE_OBJECT,
            object::SAMPLER_STATE,
            // No bias, levels 0 to 0, and a border nothing reads.
            &[handle, s0, 0, 0, 0, 0, 0, 0, 0],
        );
    }

    /// `CREATE_OBJECT BLEND`, for colour buffer 0.
    pub fn create_blend(&mut self, handle: u32, blend: Blend) {
        let target = u32::from(blend.enable)
            | (pipe::BLEND_ADD << 1)
            | ((blend.rgb_src & 0x1f) << 4)
            | ((blend.rgb_dst & 0x1f) << 9)
            | (pipe::BLEND_ADD << 14)
            | ((blend.alpha_src & 0x1f) << 17)
            | ((blend.alpha_dst & 0x1f) << 22)
            | (pipe::MASK_RGBA << 27);
        let mut payload = [0_u32; 11];
        payload[0] = handle;
        // S0 and S1 are zero: one blend for every buffer, no logic op.
        payload[3] = target;
        let _ = self.command(command::CREATE_OBJECT, object::BLEND, &payload);
    }

    /// `CREATE_OBJECT DSA` with every test off: a compositor has no depth.
    pub fn create_dsa(&mut self, handle: u32) {
        let _ = self.command(command::CREATE_OBJECT, object::DSA, &[handle, 0, 0, 0, 0]);
    }

    /// `CREATE_OBJECT RASTERIZER`.
    pub fn create_rasterizer(&mut self, handle: u32, rasterizer: Rasterizer) {
        // Depth clip on, fill both faces, cull neither, pixel centres at the
        // half, as GL has them.
        let s0 = (1 << 1) | (u32::from(rasterizer.scissor) << 14) | (1 << 29);
        let one = 1.0_f32.to_bits();
        let _ = self.command(
            command::CREATE_OBJECT,
            object::RASTERIZER,
            // Point size and line width of one; nothing else set.
            &[handle, s0, one, 0, 0, one, 0, 0, 0],
        );
    }

    /// `CREATE_OBJECT VERTEX_ELEMENTS`: how a vertex is laid out.
    pub fn create_vertex_elements(&mut self, handle: u32, elements: &[VertexElement]) {
        let mut payload = Vec::with_capacity(1 + elements.len() * 4);
        payload.push(handle);
        for element in elements {
            // Offset, instance divisor, buffer, format.
            payload.extend_from_slice(&[element.offset, 0, element.buffer, element.format]);
        }
        let _ = self.command(command::CREATE_OBJECT, object::VERTEX_ELEMENTS, &payload);
    }

    /// `CREATE_OBJECT SHADER`: TGSI text for `stage`.
    ///
    /// The text goes as it is, a zero after it, padded to words. `tokens` is
    /// how much room virglrenderer gives its parse, so it is an upper bound
    /// and not a count. `false` when the text is longer than one command
    /// carries, which none of the compositor's is.
    pub fn create_shader(&mut self, handle: u32, stage: u32, text: &str, tokens: u32) -> bool {
        let bytes = text.as_bytes();
        // The length virgl is told counts the zero.
        let Ok(len) = u32::try_from(bytes.len() + 1) else {
            return false;
        };
        // No stream output, which is the fifth word's zero.
        let mut payload = vec![handle, stage, len, tokens, 0];
        let (words, rest) = bytes.as_chunks::<4>();
        payload.extend(words.iter().map(|word| u32::from_le_bytes(*word)));
        // The remainder and the zero: always at least the zero, so text
        // that fills its last word gets a word of zeros.
        let mut last = [0_u8; 4];
        for (into, from) in last.iter_mut().zip(rest) {
            *into = *from;
        }
        payload.push(u32::from_le_bytes(last));
        self.command(command::CREATE_OBJECT, object::SHADER, &payload)
    }

    /// `BIND_OBJECT`: make a state object of `kind` the current one.
    pub fn bind_object(&mut self, kind: u32, handle: u32) {
        let _ = self.command(command::BIND_OBJECT, kind, &[handle]);
    }

    /// `DESTROY_OBJECT`.
    pub fn destroy_object(&mut self, kind: u32, handle: u32) {
        let _ = self.command(command::DESTROY_OBJECT, kind, &[handle]);
    }

    /// `BIND_SHADER`: make a shader `stage`'s current one.
    pub fn bind_shader(&mut self, handle: u32, stage: u32) {
        let _ = self.command(command::BIND_SHADER, 0, &[handle, stage]);
    }

    /// `SET_FRAMEBUFFER_STATE`: draw into one surface, with no depth.
    pub fn set_framebuffer(&mut self, surface: u32) {
        let _ = self.command(command::SET_FRAMEBUFFER_STATE, 0, &[1, 0, surface]);
    }

    /// `SET_VIEWPORT_STATE`: map clip space onto a target `width` by
    /// `height`, with `y` down the way rows are counted.
    pub fn set_viewport(&mut self, width: u32, height: u32) {
        let half_width = (width as f32 / 2.0).to_bits();
        let half_height = (height as f32 / 2.0).to_bits();
        let half = 0.5_f32.to_bits();
        let _ = self.command(
            command::SET_VIEWPORT_STATE,
            0,
            // Slot 0, then scale and translate, x y z.
            &[
                0,
                half_width,
                half_height,
                half,
                half_width,
                half_height,
                half,
            ],
        );
    }

    /// `SET_SCISSOR_STATE`: clip what is drawn to `region`, for a
    /// rasterizer that asks for it.
    pub fn set_scissor(&mut self, region: Region) {
        let corner = |x: u32, y: u32| (x & 0xffff) | ((y & 0xffff) << 16);
        let _ = self.command(
            command::SET_SCISSOR_STATE,
            0,
            &[
                0,
                corner(region.x, region.y),
                corner(
                    region.x.saturating_add(region.width),
                    region.y.saturating_add(region.height),
                ),
            ],
        );
    }

    /// `SET_VERTEX_BUFFERS`.
    pub fn set_vertex_buffers(&mut self, buffers: &[VertexBuffer]) {
        let mut payload = Vec::with_capacity(buffers.len() * 3);
        for buffer in buffers {
            payload.extend_from_slice(&[buffer.stride, buffer.offset, buffer.resource]);
        }
        let _ = self.command(command::SET_VERTEX_BUFFERS, 0, &payload);
    }

    /// `SET_SAMPLER_VIEWS`: the textures `stage` reads, from slot 0.
    pub fn set_sampler_views(&mut self, stage: u32, views: &[u32]) {
        let mut payload = vec![stage, 0];
        payload.extend_from_slice(views);
        let _ = self.command(command::SET_SAMPLER_VIEWS, 0, &payload);
    }

    /// `BIND_SAMPLER_STATES`: how `stage` reads them, from slot 0.
    pub fn bind_sampler_states(&mut self, stage: u32, states: &[u32]) {
        let mut payload = vec![stage, 0];
        payload.extend_from_slice(states);
        let _ = self.command(command::BIND_SAMPLER_STATES, 0, &payload);
    }

    /// `SET_CONSTANT_BUFFER`: `stage`'s constants, four floats to a
    /// `CONST[n]`.
    pub fn set_constants(&mut self, stage: u32, values: &[f32]) {
        let mut payload = vec![stage, 0];
        payload.extend(values.iter().map(|value| value.to_bits()));
        let _ = self.command(command::SET_CONSTANT_BUFFER, 0, &payload);
    }

    /// `CLEAR` colour buffer 0 to `color`, red first.
    pub fn clear(&mut self, color: [f32; 4]) {
        let _ = self.command(
            command::CLEAR,
            0,
            &[
                pipe::CLEAR_COLOR0,
                color[0].to_bits(),
                color[1].to_bits(),
                color[2].to_bits(),
                color[3].to_bits(),
                // Depth, a double in two words, and stencil: none is bound.
                0,
                0,
                0,
            ],
        );
    }

    /// `DRAW_VBO`: `count` vertices from `start`, as `mode`.
    pub fn draw(&mut self, mode: u32, start: u32, count: u32) {
        let _ = self.command(
            command::DRAW_VBO,
            0,
            // Not indexed, one instance, and the index range of what is
            // drawn, which virglrenderer passes to `glDrawRangeElements`
            // only for an indexed draw.
            &[
                start,
                count,
                mode,
                0,
                1,
                0,
                0,
                0,
                0,
                0,
                start.saturating_add(count).saturating_sub(1),
                0,
            ],
        );
    }

    /// `RESOURCE_INLINE_WRITE`: bytes into a buffer resource, carried in the
    /// stream itself. For vertices, which are few; pixels go by transfer.
    ///
    /// `false` when `data` is more than one command carries.
    pub fn write_buffer(&mut self, resource: u32, offset: u32, data: &[u8]) -> bool {
        let Ok(len) = u32::try_from(data.len()) else {
            return false;
        };
        let mut payload = vec![
            resource,
            0,
            pipe::TRANSFER_WRITE,
            0,
            0,
            // A buffer's box is a run of bytes: x and width.
            offset,
            0,
            0,
            len,
            1,
            1,
        ];
        let (words, rest) = data.as_chunks::<4>();
        payload.extend(words.iter().map(|word| u32::from_le_bytes(*word)));
        if !rest.is_empty() {
            let mut last = [0_u8; 4];
            for (into, from) in last.iter_mut().zip(rest) {
                *into = *from;
            }
            payload.push(u32::from_le_bytes(last));
        }
        self.command(command::RESOURCE_INLINE_WRITE, 0, &payload)
    }

    /// `BLIT`: copy a box of one texture's colour to a box of another's.
    pub fn blit(&mut self, blit: Blit) {
        let _ = self.command(
            command::BLIT,
            0,
            &[
                pipe::MASK_RGBA | ((blit.filter & 3) << 8),
                // The scissor, which S0 did not ask for.
                0,
                0,
                blit.dst,
                0,
                blit.dst_format,
                blit.dst_region.x,
                blit.dst_region.y,
                0,
                blit.dst_region.width,
                blit.dst_region.height,
                1,
                blit.src,
                0,
                blit.src_format,
                blit.src_region.x,
                blit.src_region.y,
                0,
                blit.src_region.width,
                blit.src_region.height,
                1,
            ],
        );
    }
}
